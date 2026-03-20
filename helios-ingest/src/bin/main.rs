//! helios-ingest — Multi-source Solana data aggregator.
//!
//! Spawns all configured sources, deduplicates events, and forwards to
//! the strategy engine via crossbeam channel.
//!
//! Usage:
//!   HELIUS_GRPC_ENDPOINT=https://mainnet.helius-rpc.com \
//!   HELIUS_API_KEY=xxx \
//!   RICHAT_ENDPOINT=http://127.0.0.1:10000 \
//!   SHREDSTREAM_ENDPOINT=http://127.0.0.1:9999 \
//!   ./target/release/helios-ingest

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use helios_ingest::event_bus::EventBus;
use helios_ingest::grpc_source::{self, GrpcSourceConfig};
use helios_ingest::richat_source::{self, RichatSourceConfig};
use helios_ingest::shredstream_source::{self, ShredstreamConfig};
use helios_ingest::types::*;

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    info!("helios-ingest starting — multi-source Solana aggregator");

    // Output channel for strategy engine
    let (output_tx, output_rx) = crossbeam_channel::bounded::<IngestEvent>(10_000);

    // Event bus with 100K dedup capacity
    let bus = Arc::new(EventBus::new(output_tx, 100_000));

    // --- Source 1: Yellowstone gRPC (Helius) ---
    let helius_endpoint = env("HELIUS_GRPC_ENDPOINT", "");
    let helius_key = env("HELIUS_API_KEY", "");
    if !helius_endpoint.is_empty() && !helius_key.is_empty() {
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            grpc_source::run_grpc_source(
                GrpcSourceConfig {
                    endpoint: helius_endpoint,
                    x_token: Some(helius_key),
                    label: "helius".to_string(),
                    source_type: Source::YellowstoneGrpc,
                },
                bus_clone,
            )
            .await;
        });
        info!("source: Helius gRPC ENABLED");
    } else {
        info!("source: Helius gRPC DISABLED (no endpoint/key)");
    }

    // --- Source 2: Richat gRPC (local self-hosted) ---
    let richat_endpoint = env("RICHAT_ENDPOINT", "");
    if !richat_endpoint.is_empty() {
        let richat_token = env("RICHAT_TOKEN", "");
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            richat_source::run_richat_source(
                RichatSourceConfig {
                    endpoint: richat_endpoint,
                    x_token: if richat_token.is_empty() {
                        None
                    } else {
                        Some(richat_token)
                    },
                },
                bus_clone,
            )
            .await;
        });
        info!("source: Richat gRPC ENABLED");
    } else {
        info!("source: Richat gRPC DISABLED (no endpoint)");
    }

    // --- Source 3: ShredstreamProxy / SolanaCDN ---
    let shredstream_endpoint = env("SHREDSTREAM_ENDPOINT", "");
    if !shredstream_endpoint.is_empty() {
        let shredstream_token = env("SHREDSTREAM_TOKEN", "");
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            shredstream_source::run_shredstream_source(
                ShredstreamConfig {
                    endpoint: shredstream_endpoint,
                    x_token: if shredstream_token.is_empty() {
                        None
                    } else {
                        Some(shredstream_token)
                    },
                },
                bus_clone,
            )
            .await;
        });
        info!("source: Shredstream ENABLED");
    } else {
        info!("source: Shredstream DISABLED (no endpoint)");
    }

    // --- Consumer: log events and forward to strategy engine ---
    let bus_metrics = bus.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            bus_metrics.log_metrics();
        }
    });

    // Main consumer loop — reads from output channel
    info!("helios-ingest running — waiting for events");
    let mut total_events = 0u64;
    let mut last_log = std::time::Instant::now();

    loop {
        match output_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(event) => {
                total_events += 1;

                // Log first few events for debugging
                if total_events <= 5 {
                    match &event.data {
                        EventData::Transaction(tx) => {
                            info!(
                                source = %event.source,
                                slot = event.slot,
                                sig = %tx.signature,
                                accounts = tx.accounts.len(),
                                "TX event"
                            );
                        }
                        EventData::AccountUpdate(acct) => {
                            info!(
                                source = %event.source,
                                slot = event.slot,
                                pubkey = %acct.pubkey,
                                owner = %acct.owner,
                                data_len = acct.data.len(),
                                "account update"
                            );
                        }
                        EventData::Slot(s) => {
                            info!(
                                source = %event.source,
                                slot = s.slot,
                                status = ?s.status,
                                "slot update"
                            );
                        }
                    }
                }

                // Periodic summary
                if last_log.elapsed().as_secs() >= 10 {
                    info!(
                        total_events,
                        events_per_sec = total_events as f64
                            / last_log.elapsed().as_secs_f64().max(0.001),
                        "consumer stats"
                    );
                    total_events = 0;
                    last_log = std::time::Instant::now();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                info!("output channel closed — shutting down");
                break;
            }
        }
    }

    Ok(())
}
