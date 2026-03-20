//! helius_rebate.rs – Helius Backrun Rebate sender.
//!
//! Sends transactions via Helius sendTransaction with `rebate-address` query param.
//! Helius backruns the transaction and returns 50% of MEV to the rebate address.
//! This runs in parallel with Jito + TPU fanout for maximum coverage.

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;
use solana_sdk::transaction::VersionedTransaction;
use tracing::{debug, info, warn};

pub struct HeliusRebateSender {
    client: Client,
    /// Full URL with api-key and rebate-address query params.
    url: String,
    enabled: bool,
}

impl HeliusRebateSender {
    /// Create from HELIUS_API_KEY (or RPC_URL) + rebate wallet address.
    /// Reads HELIUS_REBATE_ADDRESS from env (defaults to PAYER pubkey).
    pub fn from_env(payer_pubkey: &str) -> Self {
        // Extract API key from RPC_URL if it contains helius
        let rpc_url = std::env::var("RPC_URL").unwrap_or_default();
        let api_key = std::env::var("HELIUS_API_KEY")
            .ok()
            .or_else(|| {
                // Parse api-key from URL like https://...helius-rpc.com/?api-key=XXX
                rpc_url.split("api-key=").nth(1).map(|s| s.split('&').next().unwrap_or(s).to_string())
            })
            .unwrap_or_default();

        let rebate_address = std::env::var("HELIUS_REBATE_ADDRESS")
            .unwrap_or_else(|_| payer_pubkey.to_string());

        let enabled = !api_key.is_empty() && !rebate_address.is_empty();

        let url = if enabled {
            format!(
                "https://mainnet.helius-rpc.com/?api-key={}&rebate-address={}",
                api_key, rebate_address
            )
        } else {
            String::new()
        };

        if enabled {
            info!(
                rebate_address = %rebate_address,
                "Helius backrun rebate sender enabled (50% MEV rebate)"
            );
        } else {
            warn!("Helius rebate sender disabled — no API key or rebate address");
        }

        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            url,
            enabled,
        }
    }

    /// Send a transaction via Helius with backrun rebate.
    /// Returns the transaction signature on success.
    pub async fn send(&self, tx: &VersionedTransaction) -> Result<String> {
        if !self.enabled {
            anyhow::bail!("Helius rebate sender not enabled");
        }

        let tx_bytes = bincode::serialize(tx).context("serialize tx")?;
        let encoded = bs58::encode(&tx_bytes).into_string();

        let payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [
                encoded,
                {
                    "skipPreflight": true,
                    "preflightCommitment": "processed",
                    "encoding": "base58"
                }
            ]
        });

        let resp = self.client
            .post(&self.url)
            .json(&payload)
            .send()
            .await
            .context("helius rebate send")?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("helius rebate response")?;

        if let Some(error) = body.get("error") {
            anyhow::bail!("Helius sendTransaction error: {}", error);
        }

        if let Some(result) = body.get("result").and_then(|r| r.as_str()) {
            debug!(sig = result, "Helius rebate TX submitted");
            Ok(result.to_string())
        } else {
            anyhow::bail!("Helius sendTransaction: unexpected response status={} body={}", status, body);
        }
    }
}
