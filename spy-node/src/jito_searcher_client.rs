//! jito_searcher_client.rs
//!
//! Jito Block Engine – Searcher role (NO whitelist required).
//! Subscribes to SubscribeMempoolTransactions for the DEX program IDs.
//! Gives pre-confirmation transaction data at ~50-150ms latency,
//! equivalent to ShredStream but without requiring whitelist registration.
//!
//! Auth flow (same as ShredStream but role = SEARCHER):
//!   GenerateAuthChallenge(role=SEARCHER) → sign → GenerateAuthTokens → Bearer token
//!   Then: SearcherService.SubscribeMempoolTransactions(programs=[raydium, orca, ...])

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use futures::StreamExt;
use solana_sdk::signature::{Keypair, Signer};
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Uri};
use tonic::Request;
use tracing::{error, info, warn};

use crate::signal_bus::SpySignal;

pub mod proto {
    pub mod auth {
        tonic::include_proto!("auth");
    }
    pub mod packet {
        tonic::include_proto!("packet");
    }
    pub mod searcher {
        tonic::include_proto!("searcher");
    }
}

use proto::auth::auth_service_client::AuthServiceClient;
use proto::auth::{GenerateAuthChallengeRequest, GenerateAuthTokensRequest, Role};
use proto::searcher::searcher_service_client::SearcherServiceClient;
use proto::searcher::{mempool_subscription::Msg, MempoolSubscription, ProgramSubscriptionV0};

/// DEX programs to subscribe to for mempool transactions.
const DEX_PROGRAMS: &[&str] = &[
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium AMM v4
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  // Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS",  // Meteora DLMM
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",  // PumpSwap AMM
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  // PumpFun bonding
];

/// Spawn the Jito searcher mempool subscription thread.
/// `block_engine_url` — e.g. "https://frankfurt.mainnet.block-engine.jito.wtf"
pub fn spawn_jito_searcher(
    keypair: Arc<Keypair>,
    block_engine_url: String,
    sig_tx: Sender<SpySignal>,
    exit: Arc<AtomicBool>,
) {
    thread::Builder::new()
        .name("jito-searcher".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt jito-searcher");

            rt.block_on(async {
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    match run_searcher(&keypair, &block_engine_url, &sig_tx, &exit).await {
                        Ok(()) => {
                            warn!("jito searcher session ended — reconnecting in 5s");
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            if msg.contains("PermissionDenied") || msg.contains("Unauthenticated") {
                                warn!(
                                    "Jito Searcher auth denied — retrying in 5min. \
                                     Register keypair at jito.network"
                                );
                                tokio::time::sleep(Duration::from_secs(300)).await;
                            } else if msg.contains("Unimplemented") || msg.contains("unimplemented")
                            {
                                // SubscribeMempoolTransactions not available on this endpoint.
                                warn!(
                                    "SubscribeMempoolTransactions not available on this block \
                                     engine endpoint. Falling back to other signal sources."
                                );
                                tokio::time::sleep(Duration::from_secs(300)).await;
                            } else {
                                warn!(error = %e, "jito searcher error — reconnecting in 10s");
                                tokio::time::sleep(Duration::from_secs(10)).await;
                            }
                        }
                    }
                }
            });
        })
        .expect("spawn jito-searcher");
}

async fn run_searcher(
    keypair: &Keypair,
    block_engine_url: &str,
    sig_tx: &Sender<SpySignal>,
    exit: &AtomicBool,
) -> Result<()> {
    let uri: Uri = block_engine_url.parse().context("parse block_engine_url")?;

    let tls = ClientTlsConfig::new();
    let channel = Channel::builder(uri)
        .tls_config(tls)
        .context("tls_config")?
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .connect()
        .await
        .context("connect to block engine")?;

    info!(
        block_engine_url,
        pubkey = %keypair.pubkey(),
        "connected to Jito block engine (searcher role)"
    );

    // Authenticate as SEARCHER (role 2 — no whitelist required).
    let access_token = authenticate(keypair, channel.clone())
        .await
        .context("authenticate as searcher")?;

    info!(
        pubkey = %keypair.pubkey(),
        "authenticated with Jito block engine (searcher)"
    );

    let bearer = format!("Bearer {}", access_token);
    let mut searcher = SearcherServiceClient::new(channel);

    let mut req = Request::new(MempoolSubscription {
        msg: Some(Msg::Program(ProgramSubscriptionV0 {
            programs: DEX_PROGRAMS.iter().map(|s| s.to_string()).collect(),
        })),
    });
    req.metadata_mut().insert(
        "authorization",
        MetadataValue::try_from(&bearer).context("build auth header")?,
    );

    let mut stream = searcher
        .subscribe_mempool_transactions(req)
        .await
        .context("subscribe_mempool_transactions")?
        .into_inner();

    info!("Jito searcher mempool subscription active (DEX programs filter)");

    while let Some(msg) = stream.next().await {
        if exit.load(Ordering::Relaxed) {
            break;
        }

        let notification: proto::searcher::PendingTxNotification = msg.context("stream error")?;

        // Each PendingTxNotification contains one or more PacketBatches with raw tx data.
        let n_txs: usize = notification
            .transactions
            .iter()
            .map(|b| b.packets.len())
            .sum();
        if n_txs == 0 {
            continue;
        }

        // Emit one signal per batch to trigger the route engine.
        // We use slot=0 because pending txs don't have a slot yet.
        let signal = SpySignal::LiquidityEvent {
            slot: 0,
            pool: solana_sdk::pubkey::Pubkey::default(),
            event_type: crate::signal_bus::LiquidityEventType::Added {
                amount_a: n_txs as u64,
                amount_b: 0,
            },
        };

        if sig_tx.try_send(signal).is_err() {
            // Channel full — route engine still busy with previous signal.
        }
    }

    Ok(())
}

/// Authenticate with the Block Engine as a SEARCHER (role=2).
async fn authenticate(keypair: &Keypair, channel: Channel) -> Result<String> {
    let mut auth = AuthServiceClient::new(channel);

    let pubkey_bytes = keypair.pubkey().to_bytes().to_vec();
    let challenge_resp = auth
        .generate_auth_challenge(GenerateAuthChallengeRequest {
            role: Role::Searcher as i32,
            pubkey: pubkey_bytes.clone(),
        })
        .await
        .context("generate_auth_challenge")?
        .into_inner();

    let to_sign = format!("{}-{}", keypair.pubkey(), challenge_resp.challenge);
    let signature = keypair.sign_message(to_sign.as_bytes());

    let tokens = auth
        .generate_auth_tokens(GenerateAuthTokensRequest {
            challenge: challenge_resp.challenge,
            client_pubkey: pubkey_bytes,
            signed_challenge: signature.as_ref().to_vec(),
        })
        .await
        .context("generate_auth_tokens")?
        .into_inner();

    let token = tokens
        .access_token
        .context("no access_token in response")?
        .value;

    Ok(token)
}
