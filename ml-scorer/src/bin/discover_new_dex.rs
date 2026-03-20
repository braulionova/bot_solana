//! discover_new_dex — Discover pools for FluxBeam, Saber, Raydium CPMM, and PumpSwap
//! from on-chain data, then merge into mapped_pools.json.
//!
//! Usage:
//!   ./target/release/discover_new_dex
//!
//! Environment overrides:
//!   RPC_URL       — Solana RPC endpoint (default: Helius mainnet)
//!   POOLS_JSON    — path to mapped_pools.json (default: /root/solana-bot/mapped_pools.json)

use solana_account_decoder::UiAccountEncoding;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::RpcFilterType;
use solana_sdk::pubkey::Pubkey;

use std::collections::HashSet;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

/// Pool entry matching the mapped_pools.json schema.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PoolEntry {
    pool_id: String,
    dex: String,
    mint_a: String,
    mint_b: String,
    vault_a: String,
    vault_b: String,
    fee_bps: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    decimals_a: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decimals_b: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
}

const HELIUS_RPC: &str =
    "https://mainnet.helius-rpc.com/?api-key=YOUR_HELIUS_KEY";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| HELIUS_RPC.into());
    let pools_json = std::env::var("POOLS_JSON")
        .unwrap_or_else(|_| "/root/solana-bot/mapped_pools.json".into());

    tracing::info!("discover_new_dex starting");
    tracing::info!("RPC: {rpc_url}");
    tracing::info!("JSON: {pools_json}");

    // Load existing pools
    let data = std::fs::read_to_string(&pools_json)?;
    let existing_entries: Vec<serde_json::Value> = serde_json::from_str(&data)?;
    let existing_ids: HashSet<String> = existing_entries
        .iter()
        .filter_map(|e| {
            e.get("pool_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    tracing::info!(existing = existing_ids.len(), "loaded existing pools");

    let client = RpcClient::new_with_timeout(rpc_url.clone(), Duration::from_secs(300));

    let mut all_new: Vec<PoolEntry> = Vec::new();

    // --- 1. PumpSwap (uses Helius DAS pagination because >5M accounts) ---
    tracing::info!("scanning PumpSwap via paginated RPC...");
    match fetch_pumpswap_paginated(&rpc_url, &existing_ids) {
        Ok(pools) => {
            tracing::info!(found = pools.len(), "PumpSwap pools discovered");
            all_new.extend(pools);
        }
        Err(e) => {
            tracing::error!(error = %e, "PumpSwap scan failed");
        }
    }
    thread::sleep(Duration::from_secs(3));

    // --- 2. Raydium CPMM (637 bytes, no status byte — all accounts are valid) ---
    tracing::info!("scanning Raydium CPMM...");
    match fetch_cpmm_pools(&client, &existing_ids) {
        Ok(pools) => {
            tracing::info!(found = pools.len(), "Raydium CPMM pools discovered");
            all_new.extend(pools);
        }
        Err(e) => {
            tracing::error!(error = %e, "Raydium CPMM scan failed");
        }
    }
    thread::sleep(Duration::from_secs(3));

    // --- 3. FluxBeam (program has 0 accounts on mainnet as of 2026-03) ---
    tracing::info!("scanning FluxBeam...");
    match fetch_fluxbeam_pools(&client, &existing_ids) {
        Ok(pools) => {
            tracing::info!(found = pools.len(), "FluxBeam pools discovered");
            all_new.extend(pools);
        }
        Err(e) => {
            tracing::error!(error = %e, "FluxBeam scan failed");
        }
    }
    thread::sleep(Duration::from_secs(3));

    // --- 4. Saber (395 bytes, corrected layout) ---
    tracing::info!("scanning Saber...");
    match fetch_saber_pools(&client, &existing_ids) {
        Ok(pools) => {
            tracing::info!(found = pools.len(), "Saber pools discovered");
            all_new.extend(pools);
        }
        Err(e) => {
            tracing::error!(error = %e, "Saber scan failed");
        }
    }

    // Summary
    let mut pumpswap_count = 0u64;
    let mut cpmm_count = 0u64;
    let mut fluxbeam_count = 0u64;
    let mut saber_count = 0u64;
    for p in &all_new {
        match p.dex.as_str() {
            "pumpswap" => pumpswap_count += 1,
            "raydium_cpmm" => cpmm_count += 1,
            "fluxbeam" => fluxbeam_count += 1,
            "saber" => saber_count += 1,
            _ => {}
        }
    }

    println!(
        "Discovered {} PumpSwap pools, {} CPMM pools, {} FluxBeam pools, {} Saber pools",
        pumpswap_count, cpmm_count, fluxbeam_count, saber_count
    );

    if all_new.is_empty() {
        println!("No new pools to add.");
        return Ok(());
    }

    // Merge into mapped_pools.json
    let data = std::fs::read_to_string(&pools_json)?;
    let mut entries: Vec<serde_json::Value> = serde_json::from_str(&data)?;
    let current_ids: HashSet<String> = entries
        .iter()
        .filter_map(|e| {
            e.get("pool_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    let mut added = 0u64;
    for pool in &all_new {
        if current_ids.contains(&pool.pool_id) {
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
            "decimals_a": pool.decimals_a.unwrap_or(9),
            "decimals_b": pool.decimals_b.unwrap_or(6),
            "symbol": pool.symbol.as_deref().unwrap_or("UNKNOWN/UNKNOWN"),
        }));
        added += 1;
    }

    if added > 0 {
        let output = serde_json::to_string_pretty(&entries)?;
        std::fs::write(&pools_json, output)?;
        println!("Added {} new pools to mapped_pools.json", added);
    } else {
        println!("All discovered pools already exist in mapped_pools.json");
    }

    println!("Total pools in mapped_pools.json: {}", entries.len());

    Ok(())
}

// ---------------------------------------------------------------------------
// PumpSwap — paginated via Helius getProgramAccounts with cursor
// ---------------------------------------------------------------------------

/// Fetch PumpSwap pools using Helius getProgramAccountsV2 with pagination.
/// PumpSwap has 5M+ accounts; pool accounts are 301 bytes with discriminator f19a6d0411b16dbc.
/// V2 does not support filters, so we fetch all accounts and filter client-side.
fn fetch_pumpswap_paginated(
    rpc_url: &str,
    known: &HashSet<String>,
) -> anyhow::Result<Vec<PoolEntry>> {
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    let pumpswap_pool_disc: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];
    let mut pools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page = 0u64;
    let mut total_scanned = 0u64;
    let max_pages = 5500; // 5.5M accounts / 1000 per page

    loop {
        page += 1;
        if page > max_pages {
            tracing::info!(
                pools = pools.len(),
                scanned = total_scanned,
                "PumpSwap: reached max pages, stopping"
            );
            break;
        }

        let mut opts = serde_json::json!({
            "encoding": "base64",
            "limit": 1000,
        });
        if let Some(ref c) = cursor {
            opts["after"] = serde_json::json!(c);
        }

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getProgramAccountsV2",
            "params": [
                "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",
                opts,
            ],
        });

        if page % 100 == 1 {
            tracing::info!(
                page,
                pools = pools.len(),
                scanned = total_scanned,
                "PumpSwap scanning..."
            );
        }

        let resp = match http.post(rpc_url).json(&body).send() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(page, error = %e, "request failed, retrying after delay");
                thread::sleep(Duration::from_secs(5));
                match http.post(rpc_url).json(&body).send() {
                    Ok(r) => r,
                    Err(e2) => {
                        tracing::error!(page, error = %e2, "retry also failed, stopping");
                        break;
                    }
                }
            }
        };

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tracing::warn!(page, "rate limited, waiting 5s");
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        let json: serde_json::Value = match resp.json() {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(page, error = %e, "json parse failed, stopping");
                break;
            }
        };

        if let Some(err) = json.get("error") {
            tracing::warn!(page, error = %err, "RPC error, stopping");
            break;
        }

        let result = &json["result"];
        let accounts = result["accounts"].as_array();
        let pagination_key = result["paginationKey"].as_str().map(|s| s.to_string());

        let accounts = match accounts {
            Some(a) => a,
            None => {
                tracing::warn!(page, "no accounts array in response, stopping");
                break;
            }
        };

        if accounts.is_empty() && pagination_key.is_none() {
            break;
        }

        for account in accounts {
            total_scanned += 1;
            let pubkey = account["pubkey"].as_str().unwrap_or("");

            let data_b64 = account
                .pointer("/account/data/0")
                .and_then(|d| d.as_str())
                .unwrap_or("");

            if let Some(data) = base64_decode(data_b64) {
                // Filter: must be 301 bytes with correct discriminator
                if data.len() == 301 && data[..8] == pumpswap_pool_disc {
                    if !known.contains(pubkey) {
                        if let Some(pool) = decode_pumpswap(&data, pubkey, 25) {
                            pools.push(pool);
                        }
                    }
                }
            }
        }

        match pagination_key {
            Some(key) => cursor = Some(key),
            None => break,
        }

        // Small delay for rate limiting (Helius allows ~10 req/s on paid plans)
        thread::sleep(Duration::from_millis(200));
    }

    tracing::info!(
        pools = pools.len(),
        total_scanned,
        pages = page,
        "PumpSwap scan complete"
    );

    Ok(pools)
}

// ---------------------------------------------------------------------------
// Raydium CPMM — 637 bytes, 12 pubkeys, no status byte gate
// ---------------------------------------------------------------------------

fn fetch_cpmm_pools(
    client: &RpcClient,
    known: &HashSet<String>,
) -> anyhow::Result<Vec<PoolEntry>> {
    let program = Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C")?;

    let config = RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::DataSize(637)]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..Default::default()
        },
        ..Default::default()
    };

    let accounts = client.get_program_accounts_with_config(&program, config)?;
    tracing::info!(total = accounts.len(), "fetched CPMM accounts");

    let mut pools = Vec::new();
    for (pubkey, account) in &accounts {
        let pool_id = pubkey.to_string();
        if known.contains(&pool_id) {
            continue;
        }
        if let Some(pool) = decode_raydium_cpmm(&account.data, &pool_id, 25) {
            pools.push(pool);
        }
    }

    Ok(pools)
}

// ---------------------------------------------------------------------------
// FluxBeam — program has 0 accounts on mainnet, try anyway
// ---------------------------------------------------------------------------

fn fetch_fluxbeam_pools(
    client: &RpcClient,
    known: &HashSet<String>,
) -> anyhow::Result<Vec<PoolEntry>> {
    let program = Pubkey::from_str("FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSR1X4")?;

    // Try multiple possible sizes
    for data_size in [389u64, 324, 456, 512] {
        let config = RpcProgramAccountsConfig {
            filters: Some(vec![RpcFilterType::DataSize(data_size)]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                ..Default::default()
            },
            ..Default::default()
        };

        match client.get_program_accounts_with_config(&program, config) {
            Ok(accounts) => {
                if !accounts.is_empty() {
                    tracing::info!(
                        data_size,
                        count = accounts.len(),
                        "found FluxBeam accounts"
                    );
                    let mut pools = Vec::new();
                    for (pubkey, account) in &accounts {
                        let pool_id = pubkey.to_string();
                        if known.contains(&pool_id) {
                            continue;
                        }
                        if let Some(pool) = decode_fluxbeam(&account.data, &pool_id, 30) {
                            pools.push(pool);
                        }
                    }
                    return Ok(pools);
                }
            }
            Err(e) => {
                tracing::warn!(data_size, error = %e, "FluxBeam size query failed");
            }
        }
    }

    // Also try without size filter (will fail if too many accounts)
    match client.get_program_accounts(&program) {
        Ok(accounts) => {
            tracing::info!(
                count = accounts.len(),
                "FluxBeam accounts (no size filter)"
            );
            let mut pools = Vec::new();
            for (pubkey, account) in &accounts {
                let pool_id = pubkey.to_string();
                if known.contains(&pool_id) || account.data.len() < 389 {
                    continue;
                }
                if let Some(pool) = decode_fluxbeam(&account.data, &pool_id, 30) {
                    pools.push(pool);
                }
            }
            Ok(pools)
        }
        Err(_) => {
            tracing::info!("FluxBeam program has no accounts on mainnet");
            Ok(Vec::new())
        }
    }
}

// ---------------------------------------------------------------------------
// Saber — 395 bytes, corrected layout
// ---------------------------------------------------------------------------

fn fetch_saber_pools(
    client: &RpcClient,
    known: &HashSet<String>,
) -> anyhow::Result<Vec<PoolEntry>> {
    let program = Pubkey::from_str("SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ")?;

    let config = RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::DataSize(395)]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            ..Default::default()
        },
        ..Default::default()
    };

    let accounts = client.get_program_accounts_with_config(&program, config)?;
    tracing::info!(total = accounts.len(), "fetched Saber accounts");

    let mut pools = Vec::new();
    for (pubkey, account) in &accounts {
        let pool_id = pubkey.to_string();
        if known.contains(&pool_id) {
            continue;
        }
        if let Some(pool) = decode_saber(&account.data, &pool_id) {
            pools.push(pool);
        }
    }

    Ok(pools)
}

// ---------------------------------------------------------------------------
// Decoders
// ---------------------------------------------------------------------------

/// PumpSwap pool layout (395 bytes):
///   offset  8: pool_bump (u8)
///   offset  9: index (u16)
///   offset 11: creator (Pubkey 32)
///   offset 43: base_mint (Pubkey 32)
///   offset 75: quote_mint (Pubkey 32)
///   offset 107: lp_mint (Pubkey 32)
///   offset 139: pool_base_token_account / vault_a (Pubkey 32)
///   offset 171: pool_quote_token_account / vault_b (Pubkey 32)
fn decode_pumpswap(data: &[u8], pool_id: &str, fee_bps: u64) -> Option<PoolEntry> {
    // PumpSwap pool accounts are 301 bytes on mainnet (not 395 as initially documented)
    if data.len() < 203 {
        return None;
    }

    // Offsets: 43=base_mint, 75=quote_mint, 107=lp_mint, 139=vault_a, 171=vault_b
    let base_mint = Pubkey::try_from(&data[43..75]).ok()?;
    let quote_mint = Pubkey::try_from(&data[75..107]).ok()?;
    let vault_a = Pubkey::try_from(&data[139..171]).ok()?;
    let vault_b = Pubkey::try_from(&data[171..203]).ok()?;

    if base_mint == Pubkey::default()
        || quote_mint == Pubkey::default()
        || vault_a == Pubkey::default()
        || vault_b == Pubkey::default()
    {
        return None;
    }

    let mint_a_str = base_mint.to_string();
    let mint_b_str = quote_mint.to_string();
    let symbol = format!(
        "{}.../{:.8}",
        &mint_a_str[..8.min(mint_a_str.len())],
        &mint_b_str
    );

    Some(PoolEntry {
        pool_id: pool_id.to_string(),
        dex: "pumpswap".to_string(),
        mint_a: mint_a_str,
        mint_b: mint_b_str,
        vault_a: vault_a.to_string(),
        vault_b: vault_b.to_string(),
        fee_bps,
        decimals_a: Some(9),
        decimals_b: Some(9),
        symbol: Some(symbol),
    })
}

/// Raydium CPMM pool layout (637 bytes, 12 pubkeys):
///   offset   0: discriminator (8 bytes)
///   offset   8: amm_config (Pubkey 32)
///   offset  40: pool_creator (Pubkey 32)
///   offset  72: token_0_vault (Pubkey 32)
///   offset 104: token_1_vault (Pubkey 32)
///   offset 136: lp_mint (Pubkey 32)
///   offset 168: token_0_mint (Pubkey 32)
///   offset 200: token_1_mint (Pubkey 32)
///   offset 232: token_0_program (Pubkey 32)
///   offset 264: token_1_program (Pubkey 32)
///   offset 296+: authority, padding, config fields
fn decode_raydium_cpmm(data: &[u8], pool_id: &str, fee_bps: u64) -> Option<PoolEntry> {
    if data.len() < 296 {
        return None;
    }

    let vault_0 = Pubkey::try_from(&data[72..104]).ok()?;
    let vault_1 = Pubkey::try_from(&data[104..136]).ok()?;
    let token_0 = Pubkey::try_from(&data[168..200]).ok()?;
    let token_1 = Pubkey::try_from(&data[200..232]).ok()?;

    if token_0 == Pubkey::default()
        || token_1 == Pubkey::default()
        || vault_0 == Pubkey::default()
        || vault_1 == Pubkey::default()
    {
        return None;
    }

    let mint_a_str = token_0.to_string();
    let mint_b_str = token_1.to_string();
    let symbol = format!(
        "{}.../{:.8}",
        &mint_a_str[..8.min(mint_a_str.len())],
        &mint_b_str
    );

    Some(PoolEntry {
        pool_id: pool_id.to_string(),
        dex: "raydium_cpmm".to_string(),
        mint_a: mint_a_str,
        mint_b: mint_b_str,
        vault_a: vault_0.to_string(),
        vault_b: vault_1.to_string(),
        fee_bps,
        decimals_a: Some(9),
        decimals_b: Some(6),
        symbol: Some(symbol),
    })
}

/// FluxBeam pool layout (389+ bytes):
///   offset   8: status (u64)
///   offset 261: token_a_mint (Pubkey 32)
///   offset 293: token_b_mint (Pubkey 32)
///   offset 325: token_a_vault (Pubkey 32)
///   offset 357: token_b_vault (Pubkey 32)
fn decode_fluxbeam(data: &[u8], pool_id: &str, fee_bps: u64) -> Option<PoolEntry> {
    if data.len() < 389 {
        return None;
    }

    let status = u64::from_le_bytes(data[8..16].try_into().ok()?);
    if status == 0 {
        return None;
    }

    let token_a = Pubkey::try_from(&data[261..293]).ok()?;
    let token_b = Pubkey::try_from(&data[293..325]).ok()?;
    let vault_a = Pubkey::try_from(&data[325..357]).ok()?;
    let vault_b = Pubkey::try_from(&data[357..389]).ok()?;

    if token_a == Pubkey::default()
        || token_b == Pubkey::default()
        || vault_a == Pubkey::default()
        || vault_b == Pubkey::default()
    {
        return None;
    }

    let mint_a_str = token_a.to_string();
    let mint_b_str = token_b.to_string();
    let symbol = format!(
        "{}.../{:.8}",
        &mint_a_str[..8.min(mint_a_str.len())],
        &mint_b_str
    );

    Some(PoolEntry {
        pool_id: pool_id.to_string(),
        dex: "fluxbeam".to_string(),
        mint_a: mint_a_str,
        mint_b: mint_b_str,
        vault_a: vault_a.to_string(),
        vault_b: vault_b.to_string(),
        fee_bps,
        decimals_a: Some(9),
        decimals_b: Some(6),
        symbol: Some(symbol),
    })
}

/// Saber StableSwap layout (395 bytes, corrected from on-chain analysis):
///   offset   0: is_initialized (bool)
///   offset   1: is_paused (bool)
///   offset   2: nonce (u8)
///   offset   3: initial_amp_factor (u64)
///   offset  11: target_amp_factor (u64)
///   offset  19: current_ts (i64)
///   offset  27: start_ramp_ts (i64)
///   offset  35: stop_ramp_ts (i64)
///   offset  43: future_admin_deadline (i64)
///   offset  51: future_admin_account (Pubkey 32)
///   offset  83: admin_account (Pubkey 32)
///   offset 115: token_a_mint (Pubkey 32)
///   offset 147: token_a_reserves / vault_a (Pubkey 32)
///   offset 179: token_a_admin_fee (Pubkey 32)
///   offset 211: token_b_mint (Pubkey 32)
///   offset 243: token_b_reserves / vault_b (Pubkey 32)
///   offset 275: token_b_admin_fee (Pubkey 32)
///   offset 307: pool_mint (Pubkey 32)
///   offset 339: fees struct start
///   offset 363: trade_fee_numerator (u64)
///   offset 371: trade_fee_denominator (u64)
fn decode_saber(data: &[u8], pool_id: &str) -> Option<PoolEntry> {
    if data.len() < 379 {
        return None;
    }

    let is_initialized = data[0] != 0;
    if !is_initialized {
        return None;
    }

    let is_paused = data[1] != 0;
    if is_paused {
        return None;
    }

    let token_a = Pubkey::try_from(&data[115..147]).ok()?;
    let vault_a = Pubkey::try_from(&data[147..179]).ok()?;
    let token_b = Pubkey::try_from(&data[211..243]).ok()?;
    let vault_b = Pubkey::try_from(&data[243..275]).ok()?;

    if token_a == Pubkey::default()
        || token_b == Pubkey::default()
        || vault_a == Pubkey::default()
        || vault_b == Pubkey::default()
    {
        return None;
    }

    // Trade fee at offset 363/371 (verified from on-chain data)
    let fee_num = u64::from_le_bytes(data[363..371].try_into().ok()?);
    let fee_den = u64::from_le_bytes(data[371..379].try_into().ok()?);
    let fee_bps = if fee_den > 0 && fee_num <= fee_den {
        (fee_num * 10_000) / fee_den
    } else {
        4 // default 4bps for Saber stable pools
    };

    let mint_a_str = token_a.to_string();
    let mint_b_str = token_b.to_string();
    let symbol = format!(
        "{}.../{:.8}",
        &mint_a_str[..8.min(mint_a_str.len())],
        &mint_b_str
    );

    Some(PoolEntry {
        pool_id: pool_id.to_string(),
        dex: "saber".to_string(),
        mint_a: mint_a_str,
        mint_b: mint_b_str,
        vault_a: vault_a.to_string(),
        vault_b: vault_b.to_string(),
        fee_bps,
        decimals_a: Some(9),
        decimals_b: Some(6),
        symbol: Some(symbol),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}
