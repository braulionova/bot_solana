//! helius_fast_sender.rs – Helius Atlas Fast Sender.
//!
//! Sends transactions via `https://sender.helius-rpc.com/fast` for optimized
//! inclusion. This endpoint uses Atlas infrastructure for faster propagation.
//! Runs in parallel with Jito bundles, TPU fanout, and Helius rebate sender.
//!
//! Tip accounts (rotate randomly for load balancing):
//! https://docs.helius.dev/solana-rpc-nodes/sending-transactions#atlas-fast-sender

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, info, warn};

static TIP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Helius Atlas tip accounts — randomly select one per TX for load balancing.
const TIP_ACCOUNTS: &[&str] = &[
    "4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE",
    "D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ",
    "9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta",
    "5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn",
    "2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD",
    "2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ",
    "wyvPkWjVZz1M8fHQnMMCDTQDbkManefNNhweYk5WkcF",
    "3KCKozbAaF75qEU33jtzozcJ29yJuaLJTy2jFdzUY8bT",
    "4vieeGHPYPG2MmyPRcYjdiDmmhN3ww7hsFNap8pVN3Ey",
    "4TQLFNWK8AovT1gFvda5jfw2oJeRMKEmw7aH6MGBJ3or",
];

pub struct HeliusFastSender {
    client: Client,
    url: String,
    enabled: bool,
    tip_lamports: u64,
}

impl HeliusFastSender {
    /// Create from env. Reads HELIUS_FAST_SENDER_URL and HELIUS_FAST_TIP_LAMPORTS.
    pub fn from_env() -> Self {
        let url = std::env::var("HELIUS_FAST_SENDER_URL")
            .unwrap_or_else(|_| "https://sender.helius-rpc.com/fast".to_string());

        let tip_lamports = std::env::var("HELIUS_FAST_TIP_LAMPORTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000); // default 10k lamports = 0.00001 SOL

        let enabled = !url.is_empty();

        if enabled {
            info!(
                url = %url,
                tip_lamports,
                "Helius Atlas fast sender enabled"
            );
        } else {
            warn!("Helius Atlas fast sender disabled");
        }

        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            url,
            enabled,
            tip_lamports,
        }
    }

    /// Return the next tip account pubkey (round-robin for load balancing).
    pub fn next_tip_account() -> Option<Pubkey> {
        let idx = TIP_COUNTER.fetch_add(1, Ordering::Relaxed) % TIP_ACCOUNTS.len();
        Pubkey::from_str(TIP_ACCOUNTS[idx]).ok()
    }

    /// Build a SOL transfer instruction to the next Atlas tip account.
    pub fn tip_instruction(payer: &Pubkey, lamports: u64) -> Option<Instruction> {
        let tip_account = Self::next_tip_account()?;
        Some(Instruction {
            program_id: system_program::id(),
            accounts: vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(tip_account, false),
            ],
            data: solana_sdk::system_instruction::transfer(payer, &tip_account, lamports)
                .data
                .clone(),
        })
    }

    /// Send a pre-signed transaction via the Helius Atlas fast sender.
    pub async fn send(&self, tx: &VersionedTransaction) -> Result<String> {
        if !self.enabled {
            anyhow::bail!("Helius fast sender not enabled");
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
                    "encoding": "base58"
                }
            ]
        });

        let resp = self.client
            .post(&self.url)
            .json(&payload)
            .send()
            .await
            .context("helius fast send")?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("helius fast response")?;

        if let Some(error) = body.get("error") {
            anyhow::bail!("Helius fast sendTransaction error: {}", error);
        }

        if let Some(result) = body.get("result").and_then(|r| r.as_str()) {
            debug!(sig = result, "Helius Atlas fast TX submitted");
            Ok(result.to_string())
        } else {
            anyhow::bail!("Helius fast sendTransaction: unexpected response status={} body={}", status, body);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn tip_lamports(&self) -> u64 {
        self.tip_lamports
    }
}
