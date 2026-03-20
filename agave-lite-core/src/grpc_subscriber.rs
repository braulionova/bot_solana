//! gRPC account subscriber — connects to Richat/Yellowstone for push-based
//! account updates. Receives is_startup=true snapshot + real-time deltas.
//!
//! Ping/pong keepalive + auto-reconnect with exponential backoff.
//! Writes directly to the shared AccountCache DashMap (<1µs per update).

use crate::account_cache::{AccountCache, CachedAccount};
use futures::StreamExt;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// DEX program IDs to subscribe to.
const DEX_PROGRAMS: &[&str] = &[
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium V4
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  // Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",  // Meteora DLMM
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",  // PumpSwap
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",   // SPL Token (vaults)
];

/// Metrics for the gRPC subscriber.
pub struct GrpcMetrics {
    pub startup_accounts: AtomicU64,
    pub delta_updates: AtomicU64,
    pub reconnects: AtomicU64,
    pub connected: AtomicBool,
}

impl GrpcMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            startup_accounts: AtomicU64::new(0),
            delta_updates: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            connected: AtomicBool::new(false),
        })
    }
}

/// Configuration for the gRPC subscriber.
pub struct GrpcConfig {
    /// Richat/Yellowstone gRPC endpoint (e.g., "http://127.0.0.1:10000").
    pub endpoint: String,
    /// Optional auth token.
    pub x_token: Option<String>,
    /// Additional specific account pubkeys to subscribe to (vaults).
    pub account_pubkeys: Vec<Pubkey>,
    /// Subscribe by owner (DEX programs) — receives ALL accounts owned by these programs.
    pub subscribe_by_owner: bool,
}

/// Spawn the gRPC subscriber on a dedicated thread with its own tokio runtime.
pub fn spawn_grpc_subscriber(
    config: GrpcConfig,
    cache: Arc<AccountCache>,
    metrics: Arc<GrpcMetrics>,
) {
    std::thread::Builder::new()
        .name("al-grpc-sub".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("grpc subscriber runtime");
            rt.block_on(grpc_loop(config, cache, metrics));
        })
        .expect("spawn al-grpc-sub");
}

async fn grpc_loop(
    config: GrpcConfig,
    cache: Arc<AccountCache>,
    metrics: Arc<GrpcMetrics>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        info!("gRPC: connecting to {}", config.endpoint);
        match run_subscription(&config, &cache, &metrics).await {
            Ok(()) => {
                info!("gRPC: stream ended cleanly");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                warn!("gRPC: error: {} — reconnecting in {:?}", e, backoff);
                metrics.reconnects.fetch_add(1, Ordering::Relaxed);
            }
        }
        metrics.connected.store(false, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn run_subscription(
    config: &GrpcConfig,
    cache: &Arc<AccountCache>,
    metrics: &Arc<GrpcMetrics>,
) -> anyhow::Result<()> {
    use yellowstone_grpc_client::GeyserGrpcClient;
    use yellowstone_grpc_proto::geyser::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots,
        SubscribeRequestPing,
    };

    let mut builder = GeyserGrpcClient::build_from_shared(config.endpoint.clone())?;
    if let Some(ref token) = config.x_token {
        builder = builder.x_token(Some(token.clone()))?;
    }
    let mut client = builder
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("gRPC connect: {e}"))?;

    info!("gRPC: connected to {}", config.endpoint);
    metrics.connected.store(true, Ordering::Relaxed);

    // Build subscription request.
    let mut accounts_filter = HashMap::new();

    // Subscribe by owner (DEX programs) — gets ALL accounts owned by these programs.
    if config.subscribe_by_owner {
        let owners: Vec<String> = DEX_PROGRAMS.iter().map(|s| s.to_string()).collect();
        accounts_filter.insert(
            "dex_pools".to_string(),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: owners,
                filters: vec![],
                ..Default::default()
            },
        );
    }

    // Also subscribe to specific vault pubkeys.
    if !config.account_pubkeys.is_empty() {
        let pubkeys: Vec<String> = config.account_pubkeys.iter().map(|p| p.to_string()).collect();
        accounts_filter.insert(
            "vaults".to_string(),
            SubscribeRequestFilterAccounts {
                account: pubkeys,
                owner: vec![],
                filters: vec![],
                ..Default::default()
            },
        );
    }

    let mut slots_filter = HashMap::new();
    slots_filter.insert(
        "slots".to_string(),
        SubscribeRequestFilterSlots {
            filter_by_commitment: Some(true),
            ..Default::default()
        },
    );

    let request = SubscribeRequest {
        accounts: accounts_filter,
        slots: slots_filter,
        commitment: Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: vec![], // Full data
        ping: Some(SubscribeRequestPing { id: 1 }),
        ..Default::default()
    };

    let (mut sub_tx, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .map_err(|e| anyhow::anyhow!("gRPC subscribe: {e}"))?;

    info!("gRPC: subscribed (owner filter + {} specific accounts)", config.account_pubkeys.len());

    let mut ping_interval = tokio::time::interval(Duration::from_secs(10));
    let mut last_update = tokio::time::Instant::now();

    loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(anyhow::anyhow!("gRPC stream: {e}")),
                    None => return Ok(()), // Stream ended
                };

                let update = match msg.update_oneof {
                    Some(u) => u,
                    None => continue,
                };

                match update {
                    UpdateOneof::Account(acc_update) => {
                        if let Some(acc) = acc_update.account {
                            if let Ok(pubkey) = Pubkey::try_from(acc.pubkey.as_slice()) {
                                let owner = Pubkey::try_from(acc.owner.as_slice())
                                    .unwrap_or_default();

                                cache.upsert(pubkey, CachedAccount {
                                    lamports: acc.lamports,
                                    data: acc.data,
                                    owner,
                                    slot: acc_update.slot,
                                });

                                if acc_update.is_startup {
                                    metrics.startup_accounts.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    metrics.delta_updates.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        last_update = tokio::time::Instant::now();
                    }
                    UpdateOneof::Slot(slot_update) => {
                        let _ = cache.slot.fetch_max(slot_update.slot, Ordering::Relaxed);
                    }
                    UpdateOneof::Pong(_) => {
                        debug!("gRPC: pong received");
                    }
                    _ => {}
                }
            }
            _ = ping_interval.tick() => {
                // Send ping keepalive via the subscription sink.
                use futures::SinkExt;
                use yellowstone_grpc_proto::geyser::SubscribeRequest;
                let ping = SubscribeRequest {
                    ping: Some(SubscribeRequestPing { id: 1 }),
                    ..Default::default()
                };
                if let Err(e) = sub_tx.send(ping).await {
                    warn!("gRPC: ping send failed: {}", e);
                    return Err(anyhow::anyhow!("ping failed: {e}"));
                }

                // Log metrics periodically.
                let startup = metrics.startup_accounts.load(Ordering::Relaxed);
                let deltas = metrics.delta_updates.load(Ordering::Relaxed);
                debug!(
                    "gRPC: startup={} deltas={} cached={} slot={}",
                    startup, deltas, cache.len(), cache.current_slot()
                );
            }
        }
    }
}
