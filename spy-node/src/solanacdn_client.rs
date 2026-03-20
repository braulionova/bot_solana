//! solanacdn_client.rs — ShredstreamProxy / SolanaCDN gRPC client.
//!
//! Connects to a jito-shredstream-proxy (or SolanaCDN compatible) via gRPC,
//! subscribes to SubscribeEntries, decodes transactions, and:
//!   1. Forwards SpySignal events to the main pipeline (route engine trigger)
//!   2. Feeds decoded TXs to ShredDeltaTracker for real-time vault reserve updates
//!
//! This provides an alternative hydration source from shreds — no RPC needed.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use solana_sdk::transaction::VersionedTransaction;
use tracing::{debug, info, warn};

/// Callback type for processing decoded transactions (e.g., feed to ShredDeltaTracker).
pub type TxCallback = Arc<dyn Fn(u64, &VersionedTransaction) + Send + Sync>;

use crate::signal_bus::SpySignal;
use crate::jito_shredstream_client::proto::shredstream;

/// Stats for ShredstreamProxy source.
pub struct SolanaCdnStats {
    pub shreds_received: AtomicU64,
    pub batches_received: AtomicU64,
    pub bytes_received: AtomicU64,
    pub txs_decoded: AtomicU64,
    pub vault_updates: AtomicU64,
    pub errors: AtomicU64,
    pub reconnects: AtomicU64,
    pub latest_slot: AtomicU64,
}

impl SolanaCdnStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            shreds_received: AtomicU64::new(0),
            batches_received: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            txs_decoded: AtomicU64::new(0),
            vault_updates: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            latest_slot: AtomicU64::new(0),
        })
    }
}

/// Spawn SolanaCDN / ShredstreamProxy gRPC client.
/// Feeds decoded TXs into both signal bus AND tx_callback for vault hydration.
pub fn spawn_solanacdn_client(
    endpoint: String,
    x_token: String,
    sig_tx: Sender<SpySignal>,
    tx_callback: Option<TxCallback>,
    stats: Arc<SolanaCdnStats>,
    exit: Arc<AtomicBool>,
) {
    let endpoint_log = endpoint.clone();
    std::thread::Builder::new()
        .name("solanacdn-shreds".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt solanacdn");

            rt.block_on(async {
                let mut backoff = Duration::from_secs(2);

                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    stats.reconnects.fetch_add(1, Ordering::Relaxed);
                    info!(%endpoint, "SolanaCDN: connecting");

                    match run_session(&endpoint, &x_token, &sig_tx, &tx_callback, &stats, &exit).await {
                        Ok(()) => {
                            warn!(%endpoint, "SolanaCDN: stream ended — reconnecting");
                            backoff = Duration::from_secs(2);
                        }
                        Err(e) => {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            let msg = format!("{e:#}");
                            if msg.contains("PermissionDenied") || msg.contains("Unauthenticated") {
                                warn!("SolanaCDN: auth required — retrying in 60s");
                                tokio::time::sleep(Duration::from_secs(60)).await;
                                continue;
                            }
                            warn!(%endpoint, error = %e, ?backoff, "SolanaCDN: error");
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                    }
                }
            });
        })
        .expect("spawn solanacdn-shreds");

    info!(endpoint = %endpoint_log, "SolanaCDN: client thread started");
}

async fn run_session(
    endpoint: &str,
    x_token: &str,
    sig_tx: &Sender<SpySignal>,
    tx_callback: &Option<TxCallback>,
    stats: &Arc<SolanaCdnStats>,
    exit: &AtomicBool,
) -> anyhow::Result<()> {
    use shredstream::shredstream_proxy_client::ShredstreamProxyClient;
    use futures::StreamExt;

    let channel = tonic::transport::Channel::from_shared(endpoint.to_string())?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("gRPC connect {endpoint}: {e}"))?;

    let mut client = ShredstreamProxyClient::new(channel);
    info!(%endpoint, "SolanaCDN: gRPC connected");

    let mut grpc_request = tonic::Request::new(shredstream::SubscribeEntriesRequest {});
    if !x_token.is_empty() {
        if let Ok(val) = x_token.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>() {
            grpc_request.metadata_mut().insert("x-token", val);
        }
    }

    let response = client.subscribe_entries(grpc_request).await
        .map_err(|e| anyhow::anyhow!("subscribe_entries: {e}"))?;

    let mut stream = response.into_inner();
    info!(%endpoint, "SolanaCDN: subscribed to entries stream");

    let mut debounce_ms: u64 = 0;

    while let Some(entry_result) = stream.next().await {
        if exit.load(Ordering::Relaxed) {
            break;
        }

        let entry = entry_result.map_err(|e| anyhow::anyhow!("stream: {e}"))?;

        stats.batches_received.fetch_add(1, Ordering::Relaxed);
        stats.bytes_received.fetch_add(entry.entries.len() as u64, Ordering::Relaxed);
        stats.latest_slot.fetch_max(entry.slot, Ordering::Relaxed);

        // Decode entries and feed TXs to delta tracker for vault hydration
        if let Ok(entries) = bincode::deserialize::<Vec<solana_entry::entry::Entry>>(&entry.entries) {
            let mut tx_count = 0u64;

            for e in &entries {
                for tx in &e.transactions {
                    tx_count += 1;

                    // Feed to callback (e.g., ShredDeltaTracker for vault hydration)
                    if let Some(ref cb) = tx_callback {
                        cb(entry.slot, tx);
                    }
                }
            }

            stats.txs_decoded.fetch_add(tx_count, Ordering::Relaxed);
            stats.shreds_received.fetch_add(tx_count, Ordering::Relaxed);
        }

        // Signal the route engine (debounced to avoid flood)
        let now = now_ms();
        if now.saturating_sub(debounce_ms) >= 10 {
            debounce_ms = now;
            let signal = SpySignal::LiquidityEvent {
                slot: entry.slot,
                pool: solana_sdk::pubkey::Pubkey::default(),
                event_type: crate::signal_bus::LiquidityEventType::Added {
                    amount_a: 1,
                    amount_b: 0,
                },
            };
            let _ = sig_tx.try_send(signal);
        }
    }

    Err(anyhow::anyhow!("SolanaCDN stream closed"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
