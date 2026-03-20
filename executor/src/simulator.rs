// simulator.rs – Simulate a transaction via RPC before sending to Jito.
//
// Uses `simulateTransaction` RPC method with `sigVerify: false` so we can
// check whether the transaction would succeed without needing a valid
// blockhash from the very latest slot.

use anyhow::{bail, Context, Result};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
use solana_sdk::{commitment_config::CommitmentConfig, transaction::VersionedTransaction};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// SimulationResult
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct SimulationResult {
    /// `true` if the simulation succeeded (no error).
    pub success: bool,
    /// Compute units consumed by the simulation.
    pub units_consumed: u64,
    /// Simulation error string (if any).
    pub error: Option<String>,
    /// Return data from the program (base64 encoded).
    pub return_data: Option<String>,
    /// Estimated profit extracted from logs (if parseable).
    pub estimated_profit: Option<i64>,
}

// ---------------------------------------------------------------------------
// Simulator
// ---------------------------------------------------------------------------

pub struct Simulator {
    client: RpcClient,
}

impl Simulator {
    /// Create a simulator.  If the `SIM_RPC_URL` environment variable is set
    /// it is used as the RPC endpoint (allows a dedicated high-throughput node
    /// for simulation, independent of the main transaction RPC).
    pub fn new(rpc_url: &str) -> Self {
        let sim_url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| rpc_url.to_string());
        if sim_url != rpc_url {
            tracing::info!(sim_url = %sim_url, "using dedicated simulation RPC endpoint");
        }
        Self {
            client: RpcClient::new_with_commitment(sim_url, CommitmentConfig::processed()),
        }
    }

    /// Simulate `tx` and return a `SimulationResult`.
    pub fn simulate(&self, tx: &VersionedTransaction) -> Result<SimulationResult> {
        let config = RpcSimulateTransactionConfig {
            sig_verify: false,
            replace_recent_blockhash: true,
            commitment: Some(CommitmentConfig::processed()),
            encoding: None,
            accounts: None,
            min_context_slot: None,
            inner_instructions: false,
        };

        let response = self
            .client
            .simulate_transaction_with_config(tx, config)
            .map_err(|e| {
                warn!(error = %e, "simulateTransaction RPC call failed");
                anyhow::anyhow!("simulateTransaction RPC: {}", e)
            })?;

        let value = response.value;

        let success = value.err.is_none();
        let units_consumed = value.units_consumed.unwrap_or(0);
        let error = value.err.map(|e| format!("{:?}", e));

        let return_data = value.return_data.map(|rd| rd.data.0);

        let logs = value.logs.unwrap_or_default();
        let estimated_profit = extract_profit_from_logs(&logs);

        if !success {
            warn!(
                error = error.as_deref().unwrap_or("unknown"),
                ?logs,
                "simulation FAILED"
            );
        } else {
            info!(
                units = units_consumed,
                profit = ?estimated_profit,
                "simulation OK"
            );
        }

        Ok(SimulationResult {
            success,
            units_consumed,
            error,
            return_data,
            estimated_profit,
        })
    }

    /// Simulate and return `Ok(true)` only if simulation succeeded.
    pub fn simulate_ok(&self, tx: &VersionedTransaction) -> Result<bool> {
        let result = self.simulate(tx)?;
        Ok(result.success)
    }
}

// ---------------------------------------------------------------------------
// Log parser
// ---------------------------------------------------------------------------

/// Attempt to extract a profit figure from program log lines.
///
/// Our on-chain Helios Arb program emits: `"profit: <lamports>"`.
fn extract_profit_from_logs(logs: &[String]) -> Option<i64> {
    for line in logs {
        if let Some(rest) = line.strip_prefix("Program log: profit: ") {
            if let Ok(lamports) = rest.trim().parse::<i64>() {
                return Some(lamports);
            }
        }
    }
    None
}
