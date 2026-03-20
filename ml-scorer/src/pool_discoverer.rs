//! pool_discoverer.rs — Auto-discover new pools + vaults from on-chain data.
//!
//! Two modes:
//! 1. **Batch scan**: getProgramAccounts for each DEX program → find all pools
//! 2. **Live stream**: gRPC account subscribe → detect new pool creation in real-time
//!
//! Discovered pools are:
//! - Saved to PostgreSQL (pool_discoveries table)
//! - Added to mapped_pools.json for hydrator
//! - Registered in ShredDeltaTracker for vault updates

use std::collections::HashSet;
use std::time::Duration;
use tracing::{info, warn};

/// DEX program IDs and their pool account sizes for filtering.
const DEX_POOL_CONFIGS: &[(&str, &str, usize)] = &[
    // (program_id, label, min_account_size)
    ("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", "raydium_v4", 752),
    ("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc", "orca", 653),
    ("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo", "meteora", 904),
    ("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", "pumpswap", 210),
];

/// A discovered pool with its vault addresses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoveredPool {
    pub pool_id: String,
    pub dex: String,
    pub mint_a: String,
    pub mint_b: String,
    pub vault_a: String,
    pub vault_b: String,
    pub fee_bps: u64,
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub decimals_a: u8,
    pub decimals_b: u8,
}

/// Batch scan: fetch all pools for a DEX program via getProgramAccounts.
pub async fn scan_dex_pools(
    rpc_url: &str,
    program_id: &str,
    dex_label: &str,
    min_size: usize,
    known_pools: &HashSet<String>,
) -> anyhow::Result<Vec<DiscoveredPool>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getProgramAccounts",
        "params": [
            program_id,
            {
                "encoding": "base64",
                "filters": [
                    {"dataSize": min_size}
                ],
                "withContext": false
            }
        ]
    });

    info!(program = program_id, dex = dex_label, "scanning for pools...");

    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;

    let accounts = json["result"].as_array()
        .ok_or_else(|| anyhow::anyhow!("no result array"))?;

    let mut pools = Vec::new();

    for account in accounts {
        let pubkey = account["pubkey"].as_str().unwrap_or("");
        if known_pools.contains(pubkey) {
            continue;
        }

        let data_b64 = account.pointer("/account/data/0")
            .and_then(|d| d.as_str())
            .unwrap_or("");

        if let Some(data) = base64_decode(data_b64) {
            if let Some(pool) = decode_pool(&data, pubkey, dex_label) {
                pools.push(pool);
            }
        }
    }

    info!(
        dex = dex_label,
        total_accounts = accounts.len(),
        new_pools = pools.len(),
        "scan complete"
    );

    Ok(pools)
}

/// Scan ALL DEX programs and return all new pools.
pub async fn scan_all_dex_pools(
    rpc_url: &str,
    known_pools: &HashSet<String>,
) -> Vec<DiscoveredPool> {
    let mut all = Vec::new();

    for (program, label, min_size) in DEX_POOL_CONFIGS {
        match scan_dex_pools(rpc_url, program, label, *min_size, known_pools).await {
            Ok(pools) => all.extend(pools),
            Err(e) => warn!(dex = label, error = %e, "scan failed"),
        }
        // Rate limit between programs
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    info!(total_new = all.len(), "all DEX scans complete");
    all
}

/// Save discovered pools to PostgreSQL.
pub async fn save_discoveries(db_url: &str, pools: &[DiscoveredPool]) -> anyhow::Result<u64> {
    let (client, connection) =
        tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move { let _ = connection.await; });

    let mut inserted = 0u64;
    for pool in pools {
        match client.execute(
            "INSERT INTO pool_discoveries (pool_address, dex_type, token_a, token_b, \
             initial_reserve_a, initial_reserve_b, discovery_source, discovery_slot) \
             VALUES ($1,$2,$3,$4,$5,$6,'auto_scan',0) ON CONFLICT DO NOTHING",
            &[
                &pool.pool_id, &pool.dex, &pool.mint_a, &pool.mint_b,
                &(pool.reserve_a as i64), &(pool.reserve_b as i64),
            ],
        ).await {
            Ok(n) => inserted += n,
            Err(e) => warn!(pool = %pool.pool_id, error = %e, "insert failed"),
        }
    }

    info!(inserted, total = pools.len(), "discoveries saved to PG");
    Ok(inserted)
}

/// Append discovered pools to mapped_pools.json for hydrator.
pub fn append_to_mapped_pools(
    json_path: &str,
    pools: &[DiscoveredPool],
) -> anyhow::Result<u64> {
    let data = std::fs::read_to_string(json_path)?;
    let mut entries: Vec<serde_json::Value> = serde_json::from_str(&data)?;

    let existing: HashSet<String> = entries.iter()
        .filter_map(|e| e.get("pool_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    let mut added = 0u64;
    for pool in pools {
        if existing.contains(&pool.pool_id) {
            continue;
        }
        entries.push(serde_json::json!({
            "pool_id": pool.pool_id,
            "dex": pool.dex,
            "mint_a": pool.mint_a,
            "mint_b": pool.mint_b,
            "vault_a": pool.vault_a,
            "vault_b": pool.vault_b,
            "fee_bps": pool.fee_bps,
            "decimals_a": pool.decimals_a,
            "decimals_b": pool.decimals_b,
            "symbol": format!("{}.../{}", &pool.mint_a[..8], &pool.mint_b[..8]),
        }));
        added += 1;
    }

    if added > 0 {
        let output = serde_json::to_string_pretty(&entries)?;
        std::fs::write(json_path, output)?;
        info!(added, total = entries.len(), "mapped_pools.json updated");
    }

    Ok(added)
}

/// Decode pool account data based on DEX type.
fn decode_pool(data: &[u8], pubkey: &str, dex: &str) -> Option<DiscoveredPool> {
    match dex {
        "raydium_v4" => decode_raydium_pool(data, pubkey),
        "orca" => decode_orca_pool(data, pubkey),
        "meteora" => decode_meteora_pool(data, pubkey),
        "pumpswap" => decode_pumpswap_pool(data, pubkey),
        _ => None,
    }
}

fn decode_raydium_pool(data: &[u8], pubkey: &str) -> Option<DiscoveredPool> {
    if data.len() < 752 { return None; }
    let mint_a = bs58::encode(&data[400..432]).into_string();
    let mint_b = bs58::encode(&data[432..464]).into_string();
    let vault_a = bs58::encode(&data[336..368]).into_string();
    let vault_b = bs58::encode(&data[368..400]).into_string();
    Some(DiscoveredPool {
        pool_id: pubkey.to_string(),
        dex: "Raydium V4".to_string(),
        mint_a, mint_b, vault_a, vault_b,
        fee_bps: 25, reserve_a: 0, reserve_b: 0,
        decimals_a: 9, decimals_b: 6,
    })
}

fn decode_orca_pool(data: &[u8], pubkey: &str) -> Option<DiscoveredPool> {
    if data.len() < 300 { return None; }
    let mint_a = bs58::encode(&data[101..133]).into_string();
    let mint_b = bs58::encode(&data[181..213]).into_string();
    let vault_a = bs58::encode(&data[133..165]).into_string();
    let vault_b = bs58::encode(&data[213..245]).into_string();
    Some(DiscoveredPool {
        pool_id: pubkey.to_string(),
        dex: "Orca Whirlpool".to_string(),
        mint_a, mint_b, vault_a, vault_b,
        fee_bps: 30, reserve_a: 0, reserve_b: 0,
        decimals_a: 9, decimals_b: 6,
    })
}

fn decode_meteora_pool(data: &[u8], pubkey: &str) -> Option<DiscoveredPool> {
    if data.len() < 300 { return None; }
    let mint_a = bs58::encode(&data[41..73]).into_string();
    let mint_b = bs58::encode(&data[73..105]).into_string();
    let vault_a = bs58::encode(&data[105..137]).into_string();
    let vault_b = bs58::encode(&data[137..169]).into_string();
    Some(DiscoveredPool {
        pool_id: pubkey.to_string(),
        dex: "Meteora DLMM".to_string(),
        mint_a, mint_b, vault_a, vault_b,
        fee_bps: 30, reserve_a: 0, reserve_b: 0,
        decimals_a: 9, decimals_b: 6,
    })
}

fn decode_pumpswap_pool(data: &[u8], pubkey: &str) -> Option<DiscoveredPool> {
    if data.len() < 210 { return None; }
    let mint_a = bs58::encode(&data[43..75]).into_string();
    let mint_b = bs58::encode(&data[75..107]).into_string();
    let vault_a = bs58::encode(&data[139..171]).into_string();
    let vault_b = bs58::encode(&data[171..203]).into_string();
    Some(DiscoveredPool {
        pool_id: pubkey.to_string(),
        dex: "PumpSwap".to_string(),
        mint_a, mint_b, vault_a, vault_b,
        fee_bps: 25, reserve_a: 0, reserve_b: 0,
        decimals_a: 9, decimals_b: 6,
    })
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}
