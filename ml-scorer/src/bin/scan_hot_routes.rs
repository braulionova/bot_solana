//! scan_hot_routes — Scan ALL pools from Agave RPC, find low-competition arb routes,
//! score them with ML, and persist to PostgreSQL.
//!
//! Usage:
//!   RPC_URL=http://127.0.0.1:9000 DB_URL=postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios \
//!     ./target/release/scan_hot_routes
//!
//! Environment:
//!   RPC_URL     — Solana RPC (default: http://127.0.0.1:9000)
//!   DB_URL      — PostgreSQL connection string
//!   POOLS_JSON  — Path to mapped_pools.json (default: /root/spy_node/mapped_pools.json)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use ml_scorer::snapshot_scanner::{
    self, PoolSnapshot, ScoredRoute,
};
use ml_scorer::MlScorer;

const DEFAULT_RPC: &str = "http://127.0.0.1:9000";
const DEFAULT_DB: &str = "postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios";
const DEFAULT_POOLS_JSON: &str = "/root/spy_node/mapped_pools.json";

/// DEX program IDs
const RAYDIUM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const ORCA: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
const METEORA: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const PUMPSWAP: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_string());
    let db_url = std::env::var("DB_URL").unwrap_or_else(|_| DEFAULT_DB.to_string());
    let pools_json = std::env::var("POOLS_JSON").unwrap_or_else(|_| DEFAULT_POOLS_JSON.to_string());

    info!("Hot route scanner starting");
    info!("RPC: {rpc_url}");
    info!("DB: {db_url}");
    info!("Pools: {pools_json}");

    // Load ML scorer from PG (historical data)
    let scorer = MlScorer::new();
    match scorer.load_from_pg(&db_url).await {
        Ok(_) => info!("ML scorer loaded: {}", scorer.stats()),
        Err(e) => warn!("ML scorer load failed (fresh start): {e}"),
    }

    // Load pool metadata from JSON
    let t0 = Instant::now();
    let pools_data = std::fs::read_to_string(&pools_json)?;
    let pool_entries: Vec<serde_json::Value> = serde_json::from_str(&pools_data)?;
    info!(
        pools = pool_entries.len(),
        "loaded pool metadata in {:.1}ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );

    // Build pool address → metadata map
    let mut pool_meta: HashMap<String, PoolMeta> = HashMap::new();
    for entry in &pool_entries {
        let addr = entry
            .get("pool_id")
            .or_else(|| entry.get("pool_address"))
            .or_else(|| entry.get("address"))
            .or_else(|| entry.get("id"))
            .and_then(|v| v.as_str());
        let token_a = entry
            .get("mint_a")
            .or_else(|| entry.get("token_a"))
            .or_else(|| entry.get("baseMint"))
            .or_else(|| entry.get("tokenMintA"))
            .and_then(|v| v.as_str());
        let token_b = entry
            .get("mint_b")
            .or_else(|| entry.get("token_b"))
            .or_else(|| entry.get("quoteMint"))
            .or_else(|| entry.get("tokenMintB"))
            .and_then(|v| v.as_str());
        let dex_type = entry
            .get("dex")
            .or_else(|| entry.get("dex_type"))
            .or_else(|| entry.get("source"))
            .and_then(|v| v.as_str());

        if let (Some(addr), Some(ta), Some(tb)) = (addr, token_a, token_b) {
            let dex = dex_type.unwrap_or("unknown").to_string();
            let va = entry.get("vault_a").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let vb = entry.get("vault_b").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let fee = entry.get("fee_bps").and_then(|v| v.as_u64()).unwrap_or(25);
            pool_meta.insert(
                addr.to_string(),
                PoolMeta {
                    token_a: ta.to_string(),
                    token_b: tb.to_string(),
                    dex_type: dex,
                    vault_a: va,
                    vault_b: vb,
                    fee_bps: fee,
                    decimals_a: 9,
                    decimals_b: 6,
                },
            );
        }
    }

    info!(pools_with_meta = pool_meta.len(), "pool metadata parsed");

    // Fetch vault balances from RPC to get real reserves.
    // Strategy: collect all vault addresses, batch fetch their token balances.
    let t1 = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Collect vaults: vault_address → Vec<(pool_address, is_vault_a)>
    let mut vault_to_pool: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    for (pool_addr, meta) in &pool_meta {
        if !meta.vault_a.is_empty() {
            vault_to_pool
                .entry(meta.vault_a.clone())
                .or_default()
                .push((pool_addr.clone(), true));
        }
        if !meta.vault_b.is_empty() {
            vault_to_pool
                .entry(meta.vault_b.clone())
                .or_default()
                .push((pool_addr.clone(), false));
        }
    }

    info!(vaults = vault_to_pool.len(), "fetching vault balances");

    // Batch fetch vault accounts (token accounts → read amount from data)
    let vault_addrs: Vec<String> = vault_to_pool.keys().cloned().collect();
    let mut vault_balances: HashMap<String, u64> = HashMap::new();
    let batch_size = 100;

    for chunk in vault_addrs.chunks(batch_size) {
        let keys: Vec<serde_json::Value> = chunk
            .iter()
            .map(|addr| serde_json::json!(addr))
            .collect();

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getMultipleAccounts",
            "params": [keys, {"encoding": "base64"}]
        });

        let resp = match client.post(&rpc_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "RPC batch failed, skipping chunk");
                continue;
            }
        };

        let val: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "RPC parse failed");
                continue;
            }
        };

        if let Some(accounts) = val.pointer("/result/value").and_then(|v| v.as_array()) {
            for (i, account) in accounts.iter().enumerate() {
                if account.is_null() {
                    continue;
                }
                let data_b64 = account
                    .pointer("/data/0")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                if let Some(data) = base64_decode(data_b64) {
                    // SPL Token account: amount at offset 64 (u64 LE)
                    if data.len() >= 72 {
                        let amount = u64::from_le_bytes(
                            data[64..72].try_into().unwrap_or([0; 8]),
                        );
                        vault_balances.insert(chunk[i].clone(), amount);
                    }
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    info!(
        vaults_fetched = vault_balances.len(),
        "vault balances in {:.1}s",
        t1.elapsed().as_secs_f64()
    );

    // Build snapshots from vault balances
    let mut snapshots: Vec<PoolSnapshot> = Vec::new();
    for (pool_addr, meta) in &pool_meta {
        let reserve_a = vault_balances.get(&meta.vault_a).copied().unwrap_or(0);
        let reserve_b = vault_balances.get(&meta.vault_b).copied().unwrap_or(0);

        if reserve_a > 0 && reserve_b > 0 {
            snapshots.push(PoolSnapshot {
                address: pool_addr.clone(),
                dex_type: meta.dex_type.clone(),
                token_a: meta.token_a.clone(),
                token_b: meta.token_b.clone(),
                reserve_a,
                reserve_b,
                fee_bps: meta.fee_bps,
                vault_a: meta.vault_a.clone(),
                vault_b: meta.vault_b.clone(),
            });
        }
    }

    info!(
        pools_hydrated = snapshots.len(),
        "fetched reserves in {:.1}s",
        t1.elapsed().as_secs_f64()
    );

    // Run the route scanner
    let routes = snapshot_scanner::scan_all_routes(&snapshots, Some(&scorer));

    // Print summary
    snapshot_scanner::print_summary(&routes);

    // Save to PostgreSQL
    if !routes.is_empty() {
        match snapshot_scanner::save_hot_routes(&db_url, &routes).await {
            Ok(n) => info!(saved = n, "hot routes persisted to PostgreSQL"),
            Err(e) => warn!(error = %e, "failed to save hot routes"),
        }
    }

    // Save ML model state
    if let Err(e) = scorer.save_to_pg(&db_url).await {
        warn!(error = %e, "failed to save ML model state");
    }

    info!(
        "scan complete in {:.1}s",
        t0.elapsed().as_secs_f64()
    );

    Ok(())
}

struct PoolMeta {
    token_a: String,
    token_b: String,
    dex_type: String,
    vault_a: String,
    vault_b: String,
    fee_bps: u64,
    decimals_a: u8,
    decimals_b: u8,
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Decode reserves from raw account data based on DEX type.
fn decode_pool_reserves(data: &[u8], dex_type: &str, _addr: &str) -> (u64, u64, u64, String, String) {
    match dex_type {
        "raydium_v4" | "raydium" | "RaydiumAmmV4" => {
            // Raydium AMM V4 layout
            if data.len() < 752 {
                return (0, 0, 25, String::new(), String::new());
            }
            // pool_coin_amount at offset 200 (u64 LE)
            // pool_pc_amount at offset 208 (u64 LE)
            // Note: real reserves come from vault balances, these are pool-level
            let vault_a = if data.len() > 344 {
                bs58::encode(&data[336..368]).into_string()
            } else {
                String::new()
            };
            let vault_b = if data.len() > 376 {
                bs58::encode(&data[368..400]).into_string()
            } else {
                String::new()
            };
            // Read reserves from pool state
            let r_a = u64::from_le_bytes(data[200..208].try_into().unwrap_or([0; 8]));
            let r_b = u64::from_le_bytes(data[208..216].try_into().unwrap_or([0; 8]));
            (r_a, r_b, 25, vault_a, vault_b) // 25 bps = 0.25%
        }
        "orca" | "OrcaWhirlpool" | "orca_whirlpool" => {
            // Orca Whirlpool layout
            if data.len() < 300 {
                return (0, 0, 30, String::new(), String::new());
            }
            // liquidity at offset 129 (u128 LE)
            // sqrt_price at offset 145 (u128 LE)
            let vault_a = if data.len() > 173 {
                bs58::encode(&data[141..173]).into_string()
            } else {
                String::new()
            };
            let vault_b = if data.len() > 205 {
                bs58::encode(&data[173..205]).into_string()
            } else {
                String::new()
            };
            // For concentrated liquidity, reserves depend on tick — use liquidity as proxy
            let liquidity = u128::from_le_bytes(data[129..145].try_into().unwrap_or([0; 16]));
            // Rough reserve estimate from liquidity
            let reserve_est = (liquidity >> 32) as u64;
            (
                reserve_est.max(1_000_000),
                reserve_est.max(1_000_000),
                30,
                vault_a,
                vault_b,
            )
        }
        "meteora" | "MeteoraDlmm" | "meteora_dlmm" => {
            if data.len() < 300 {
                return (0, 0, 30, String::new(), String::new());
            }
            let vault_a = if data.len() > 105 {
                bs58::encode(&data[73..105]).into_string()
            } else {
                String::new()
            };
            let vault_b = if data.len() > 137 {
                bs58::encode(&data[105..137]).into_string()
            } else {
                String::new()
            };
            // Meteora: reserves not directly in pool state, in vaults
            // Use placeholder — real hydrator fetches vaults separately
            (10_000_000, 10_000_000, 30, vault_a, vault_b)
        }
        "pumpswap" | "PumpSwap" => {
            if data.len() < 210 {
                return (0, 0, 25, String::new(), String::new());
            }
            let vault_a = if data.len() > 171 {
                bs58::encode(&data[139..171]).into_string()
            } else {
                String::new()
            };
            let vault_b = if data.len() > 203 {
                bs58::encode(&data[171..203]).into_string()
            } else {
                String::new()
            };
            (10_000_000, 10_000_000, 25, vault_a, vault_b)
        }
        _ => (0, 0, 30, String::new(), String::new()),
    }
}
