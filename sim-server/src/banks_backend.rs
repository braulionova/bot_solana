//! banks_backend.rs – BanksClient simulation backend.
//!
//! Connects to a Solana BanksServer via tarpc/bincode TCP protocol.
//! Latency: ~30-60ms when server is local/regional vs ~100-200ms for JSON-RPC.
//!
//! BanksServer sources:
//!   - Local Agave validator:  export BANKS_SERVER_ADDR=127.0.0.1:8900
//!   - Triton One RPC+:        export BANKS_SERVER_ADDR=<triton-host>:8900
//!   - Own bare-metal node:    export BANKS_SERVER_ADDR=<your-validator-ip>:8900
//!
//! The validator must have rpc_banks_addr configured (enabled by default in Agave).

use crate::{SimulateResponse, SimulationBackend};
use anyhow::Context;
use solana_banks_client::BanksClient;
use solana_sdk::{
    commitment_config::CommitmentLevel, hash::Hash, message::VersionedMessage,
    transaction::VersionedTransaction,
};
use std::net::ToSocketAddrs;
use tokio::sync::Mutex;
use tracing::{debug, info};

pub struct BanksBackend {
    // BanksClient requires &mut self for all calls; wrap in Mutex.
    // For higher throughput create a Vec<Mutex<BanksClient>> pool.
    client: Mutex<BanksClient>,
}

impl BanksBackend {
    /// Connect to a BanksServer at `addr`.
    pub async fn new(addr: &str) -> anyhow::Result<Self> {
        let sock_addr = addr
            .to_socket_addrs()
            .context("resolve BANKS_SERVER_ADDR")?
            .next()
            .ok_or_else(|| anyhow::anyhow!("no address resolved for {}", addr))?;

        let client: BanksClient = solana_banks_client::start_tcp_client(sock_addr)
            .await
            .with_context(|| format!("BanksClient connect to {}", addr))?;

        info!(addr = %addr, "BanksClient connected");
        Ok(Self {
            client: Mutex::new(client),
        })
    }
}

#[async_trait::async_trait]
impl SimulationBackend for BanksBackend {
    async fn simulate(
        &self,
        tx_bytes: Vec<u8>,
        replace_blockhash: bool,
        recent_blockhash: Hash,
    ) -> anyhow::Result<SimulateResponse> {
        let mut tx: VersionedTransaction =
            bincode::deserialize(&tx_bytes).context("deserialize transaction")?;

        // Replace blockhash if requested (avoids stale-blockhash failures).
        if replace_blockhash {
            match &mut tx.message {
                VersionedMessage::Legacy(msg) => msg.recent_blockhash = recent_blockhash,
                VersionedMessage::V0(msg) => msg.recent_blockhash = recent_blockhash,
            }
        }

        let mut client = self.client.lock().await;

        // `result` is BanksTransactionResultWithSimulation:
        //   .result:             Option<transaction::Result<()>>
        //   .simulation_details: Option<TransactionSimulationDetails>
        let result = client
            .simulate_transaction_with_commitment(tx, CommitmentLevel::Processed)
            .await
            .context("BanksClient simulate_transaction")?;

        // Extract error string from Option<Result<(), TransactionError>>.
        let err_str: Option<String> = result
            .result
            .and_then(|r| r.err())
            .map(|e| format!("{:?}", e));

        let (logs, units_consumed) = match result.simulation_details {
            Some(details) => (details.logs, Some(details.units_consumed)),
            None => (vec![], None),
        };

        debug!(
            err = ?err_str,
            logs = logs.len(),
            units = ?units_consumed,
            "BanksClient simulation complete"
        );

        Ok(SimulateResponse {
            err: err_str,
            logs,
            units_consumed,
        })
    }
}
