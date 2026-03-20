//! tpu_sender.rs — Direct transaction submission to validator TPU ports via QUIC.
//!
//! Bypasses JSON-RPC sendTransaction entirely.
//! Sends raw transactions directly to current slot leader(s).
//! Latency: ~5-20ms vs ~80-150ms for RPC sendTransaction.

use anyhow::Context;
use solana_client::{nonblocking::tpu_client::TpuClient, tpu_client::TpuClientConfig};
use solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool};
use solana_rpc_client::nonblocking::rpc_client::RpcClient as NonblockingRpcClient;
use solana_sdk::transaction::VersionedTransaction;
use std::sync::Arc;
use tracing::{debug, info, warn};

pub struct TpuSender {
    client: Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>,
}

impl TpuSender {
    /// Create a TpuSender connected to the cluster via `rpc_url` + `ws_url`.
    pub async fn new(rpc_url: &str, ws_url: &str) -> anyhow::Result<Self> {
        let rpc_client = Arc::new(NonblockingRpcClient::new(rpc_url.to_string()));

        let config = TpuClientConfig {
            fanout_slots: 2, // send to next 2 leaders for reliability
        };

        let client = TpuClient::new("mini-agave", rpc_client, ws_url, config)
            .await
            .map_err(|e| anyhow::anyhow!("TpuClient init: {}", e))?;

        info!(rpc = %rpc_url, ws = %ws_url, fanout = 2, "TpuClient ready (mini-agave)");
        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Send raw transaction bytes directly to the current leader via QUIC.
    /// Returns the transaction signature as a base58 string.
    pub async fn send_wire(&self, tx_bytes: Vec<u8>) -> anyhow::Result<String> {
        let tx: VersionedTransaction =
            bincode::deserialize(&tx_bytes).context("deserialize tx for sig extraction")?;
        let sig = tx
            .signatures
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let ok = self.client.send_wire_transaction(tx_bytes).await;
        if ok {
            debug!(sig = %sig, "tx dispatched via TPU (QUIC)");
            Ok(sig)
        } else {
            warn!(sig = %sig, "TPU send_wire_transaction returned false");
            Err(anyhow::anyhow!("TpuClient send failed for sig={sig}"))
        }
    }
}
