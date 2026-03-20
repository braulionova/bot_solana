//! ShredstreamProxy / SolanaCDN source — receives decoded entries via gRPC.
//!
//! Connects to jito-shredstream-proxy or compatible (ERPC, SolanaCDN)
//! which exposes a SubscribeEntries gRPC stream of decoded entries.
//! These entries contain pre-confirmation transactions from shreds.

use crate::event_bus::EventBus;
use crate::types::*;
use futures::StreamExt;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub struct ShredstreamConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

/// Run the shredstream source with auto-reconnect.
pub async fn run_shredstream_source(
    config: ShredstreamConfig,
    bus: Arc<EventBus>,
) {
    let mut backoff = Duration::from_secs(2);

    loop {
        info!(endpoint = %config.endpoint, "shredstream: connecting");

        match run_session(&config, &bus).await {
            Ok(()) => {
                warn!("shredstream: stream ended — reconnecting");
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("PermissionDenied") || msg.contains("Unauthenticated") {
                    warn!("shredstream: auth required — waiting 60s");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
                warn!(
                    error = %e,
                    backoff_ms = backoff.as_millis(),
                    "shredstream: error"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn run_session(
    config: &ShredstreamConfig,
    bus: &Arc<EventBus>,
) -> anyhow::Result<()> {
    let channel = tonic::transport::Channel::from_shared(config.endpoint.clone())?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .connect()
        .await?;

    // The ShredstreamProxy uses a custom proto — we use raw tonic client
    // with the known method path.
    let mut client = tonic::client::Grpc::new(channel);
    client.ready().await?;

    let mut request = tonic::Request::new(());
    if let Some(token) = &config.x_token {
        if !token.is_empty() {
            if let Ok(val) = token.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>() {
                request.metadata_mut().insert("x-token", val);
            }
        }
    }

    info!("shredstream: connected, subscribing to entries");

    // For ShredstreamProxy: the method is shredstream.ShredstreamProxy/SubscribeEntries
    // The response is a stream of Entry messages with slot + serialized entries bytes.
    // Since we don't have the proto compiled, we use a simplified approach:
    // parse the raw bytes as bincode-serialized Vec<solana_entry::entry::Entry>.

    // Use raw gRPC streaming with known path
    let response = client
        .server_streaming(
            request,
            tonic::codegen::http::uri::PathAndQuery::from_static(
                "/shredstream.ShredstreamProxy/SubscribeEntries",
            ),
            tonic::codec::ProstCodec::<(), ShredstreamEntry>::default(),
        )
        .await?;

    let mut stream = response.into_inner();

    while let Some(entry_result) = stream.next().await {
        let entry = entry_result?;
        let now = Instant::now();

        // Decode entries from bincode
        if let Ok(entries) =
            bincode::deserialize::<Vec<solana_entry::entry::Entry>>(&entry.entries)
        {
            for e in &entries {
                for tx in &e.transactions {
                    let sig = *tx.signatures.first().unwrap_or(&Signature::default());
                    if sig == Signature::default() {
                        continue;
                    }

                    // Extract account keys
                    let accounts: Vec<Pubkey> = tx
                        .message
                        .static_account_keys()
                        .iter()
                        .copied()
                        .collect();

                    let event = IngestEvent {
                        source: Source::Shredstream,
                        received_at: now,
                        slot: entry.slot,
                        event_type: EventType::Transaction,
                        data: EventData::Transaction(TransactionInfo {
                            signature: sig,
                            is_vote: false,
                            success: true, // shred entries = pre-confirmation, assumed ok
                            program_ids: accounts.clone(),
                            token_deltas: vec![],
                            accounts,
                            raw: vec![],
                        }),
                    };

                    bus.process(event);
                }
            }
        }
    }

    Ok(())
}

/// Proto-compatible struct for ShredstreamProxy entry message.
#[derive(Clone, prost::Message)]
pub struct ShredstreamEntry {
    #[prost(uint64, tag = "1")]
    pub slot: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub entries: Vec<u8>,
}
