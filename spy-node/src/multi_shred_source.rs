//! multi_shred_source.rs — Multi-source shred/signal receiver for max coverage.
//!
//! Runs multiple data sources in parallel threads. Each source forwards signals
//! to the main pipeline via the crossbeam signal channel.
//!
//! Sources (all free, no whitelist):
//!   1. Turbine gossip (already on SPY_TVU_PORT, ~669ms)
//!   2. ERPC gRPC (Yellowstone protocol, ~78ms) — ValidatorsDAO
//!   3. SolanaCDN POP (Pipe Network, ~78ms) — future
//!   4. Shredcaster/XDP (Overclock/Anza, lowest overhead) — future

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use tracing::{error, info, warn};

use crate::signal_bus::SpySignal;

/// Stats for each shred source.
pub struct ShredSourceStats {
    pub erpc_signals: AtomicU64,
    pub erpc_errors: AtomicU64,
    pub erpc_reconnects: AtomicU64,
}

impl ShredSourceStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            erpc_signals: AtomicU64::new(0),
            erpc_errors: AtomicU64::new(0),
            erpc_reconnects: AtomicU64::new(0),
        })
    }
}

/// DEX programs to monitor.
const DEX_PROGRAMS: &[&str] = &[
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium V4
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  // Orca
    "LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS",  // Meteora
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",  // PumpSwap
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  // PumpFun
];

/// Spawn ERPC gRPC source — subscribes to DEX transactions via Yellowstone protocol.
///
/// Available endpoints (no whitelist needed, trial available):
///   - Frankfurt: `https://shreds-fra.erpc.global`
///   - Amsterdam: `https://shreds-ams.erpc.global`
pub fn spawn_erpc_source(
    endpoint: String,
    x_token: String,
    sig_tx: Sender<SpySignal>,
    stats: Arc<ShredSourceStats>,
    exit: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("erpc-source".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt erpc-source");

            rt.block_on(async {
                let mut backoff = Duration::from_secs(2);

                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    stats.erpc_reconnects.fetch_add(1, Ordering::Relaxed);

                    match run_erpc(&endpoint, &x_token, &sig_tx, &stats, &exit).await {
                        Ok(()) => {
                            warn!("ERPC source stream ended — reconnecting in 2s");
                            backoff = Duration::from_secs(2);
                        }
                        Err(e) => {
                            stats.erpc_errors.fetch_add(1, Ordering::Relaxed);
                            let msg = format!("{e:#}");
                            if msg.contains("PermissionDenied")
                                || msg.contains("Unauthenticated")
                            {
                                error!(
                                    "ERPC auth failed — get x-token from ValidatorsDAO Discord. \
                                     Disabling ERPC source."
                                );
                                return; // Stop trying
                            }
                            warn!(error = %e, ?backoff, "ERPC source error");
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                    }
                }
            });
        })
        .expect("spawn erpc-source");
}

async fn run_erpc(
    endpoint: &str,
    x_token: &str,
    sig_tx: &Sender<SpySignal>,
    stats: &Arc<ShredSourceStats>,
    exit: &AtomicBool,
) -> anyhow::Result<()> {
    use futures::StreamExt;

    // Use the same yellowstone gRPC client as grpc_feed in mini-agave
    let mut builder =
        yellowstone_grpc_client::GeyserGrpcClient::build_from_shared(endpoint.to_string())?;
    if !x_token.is_empty() {
        builder = builder.x_token(Some(x_token.to_string()))?;
    }
    let mut client = builder
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("ERPC connect: {e}"))?;

    info!(endpoint, "ERPC gRPC source connected");

    // Subscribe to DEX transactions (same as Jito searcher but via ERPC)
    let mut tx_filter = std::collections::HashMap::new();
    tx_filter.insert(
        "dex".to_string(),
        yellowstone_grpc_proto::geyser::SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            signature: None,
            account_include: DEX_PROGRAMS.iter().map(|s| s.to_string()).collect(),
            account_exclude: vec![],
            account_required: vec![],
        },
    );

    let request = yellowstone_grpc_proto::geyser::SubscribeRequest {
        accounts: std::collections::HashMap::new(),
        slots: std::collections::HashMap::new(),
        transactions: tx_filter,
        transactions_status: std::collections::HashMap::new(),
        blocks: std::collections::HashMap::new(),
        blocks_meta: std::collections::HashMap::new(),
        commitment: Some(
            yellowstone_grpc_proto::geyser::CommitmentLevel::Processed as i32,
        ),
        accounts_data_slice: vec![],
        ping: None,
        entry: std::collections::HashMap::new(),
    };

    let (_sub_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;
    info!("ERPC gRPC subscription active (DEX transactions, Processed commitment)");

    let mut debounce_ms: u64 = 0;

    while let Some(msg) = stream.next().await {
        if exit.load(Ordering::Relaxed) {
            break;
        }

        let msg = msg.map_err(|e| anyhow::anyhow!("stream: {e}"))?;

        if let Some(yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof::Transaction(
            tx,
        )) = msg.update_oneof
        {
            stats.erpc_signals.fetch_add(1, Ordering::Relaxed);

            // Debounce: max 1 signal per 20ms
            let now = now_ms();
            if now.saturating_sub(debounce_ms) >= 20 {
                debounce_ms = now;
                // Emit a LiquidityEvent signal to wake up route engine
                let signal = SpySignal::LiquidityEvent {
                    slot: tx.slot,
                    pool: solana_sdk::pubkey::Pubkey::default(),
                    event_type: crate::signal_bus::LiquidityEventType::Added {
                        amount_a: 1,
                        amount_b: 0,
                    },
                };
                let _ = sig_tx.try_send(signal);
            }
        }
    }

    Err(anyhow::anyhow!("ERPC stream closed"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Log multi-source stats periodically.
pub fn spawn_stats_logger(
    stats: Arc<ShredSourceStats>,
    scdn_stats: Option<Arc<crate::solanacdn_client::SolanaCdnStats>>,
) {
    std::thread::Builder::new()
        .name("shred-src-stats".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(30));

            if let Some(ref s) = scdn_stats {
                info!(
                    erpc_signals = stats.erpc_signals.load(Ordering::Relaxed),
                    erpc_errors = stats.erpc_errors.load(Ordering::Relaxed),
                    scdn_shreds = s.shreds_received.load(Ordering::Relaxed),
                    scdn_batches = s.batches_received.load(Ordering::Relaxed),
                    scdn_bytes = s.bytes_received.load(Ordering::Relaxed),
                    scdn_errors = s.errors.load(Ordering::Relaxed),
                    scdn_latest_slot = s.latest_slot.load(Ordering::Relaxed),
                    "multi-source shred stats"
                );
            } else {
                info!(
                    erpc_signals = stats.erpc_signals.load(Ordering::Relaxed),
                    erpc_errors = stats.erpc_errors.load(Ordering::Relaxed),
                    erpc_reconnects = stats.erpc_reconnects.load(Ordering::Relaxed),
                    "multi-source shred stats"
                );
            }
        })
        .expect("spawn shred-src-stats");
}
