//! onchain_scanner.rs — Scan on-chain transactions for successful arb patterns.
//!
//! Detects flash-loan arb TXs from other bots by signature:
//!   1. TX contains 2+ DEX swap instructions (different programs or pools)
//!   2. Input token == output token (cyclic route)
//!   3. Output > input (profitable)
//!
//! Feeds learned routes/pools/times into the MlScorer to pre-score our opportunities.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::MlScorer;

/// Known DEX program IDs to detect swap instructions.
const DEX_PROGRAMS: &[(&str, &str)] = &[
    ("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", "raydium_v4"),
    ("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", "raydium_clmm"),
    ("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc", "orca"),
    ("LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS", "meteora"),
    ("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", "pumpswap"),
];

/// Known flash loan program IDs.
const FLASH_PROGRAMS: &[&str] = &[
    "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA", // MarginFi
    "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD", // Kamino
    "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo", // Solend/Save
];

/// A detected on-chain arb pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedArb {
    pub signature: String,
    pub slot: u64,
    pub timestamp: i64,
    pub pools: Vec<String>,
    pub dex_types: Vec<String>,
    pub n_swaps: u32,
    pub uses_flash_loan: bool,
    pub profit_lamports: i64,
    pub borrow_amount: u64,
    pub route_key: String,  // sorted pool addresses joined
    pub hour: u8,
}

/// Scan recent blocks for successful arb transactions.
/// Uses getSignaturesForAddress on DEX programs, then analyzes TX structure.
pub async fn scan_recent_arbs(
    rpc_url: &str,
    limit: usize,
) -> anyhow::Result<Vec<DetectedArb>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let mut all_arbs = Vec::new();

    // Scan tip accounts — arb bots always tip Jito
    let tip_accounts = [
        "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
        "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
        "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
        "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    ];

    for tip_account in &tip_accounts {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getSignaturesForAddress",
            "params": [
                tip_account,
                {"limit": limit, "commitment": "confirmed"}
            ]
        });

        let resp = client.post(rpc_url).json(&body).send().await?;
        let val: serde_json::Value = resp.json().await?;

        let sigs = match val.get("result").and_then(|r| r.as_array()) {
            Some(s) => s,
            None => continue,
        };

        for sig_info in sigs {
            let sig = match sig_info.get("signature").and_then(|s| s.as_str()) {
                Some(s) => s,
                None => continue,
            };
            // Skip failed TXs
            if sig_info.get("err").is_some() && !sig_info["err"].is_null() {
                continue;
            }
            let slot = sig_info.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
            let ts = sig_info.get("blockTime").and_then(|t| t.as_i64()).unwrap_or(0);

            // Fetch full TX to analyze
            if let Ok(Some(arb)) = analyze_tx(&client, rpc_url, sig, slot, ts).await {
                all_arbs.push(arb);
            }

            // Rate limit
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Don't hammer RPC
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    info!(
        arbs_found = all_arbs.len(),
        "on-chain arb scan complete"
    );

    Ok(all_arbs)
}

/// Analyze a single TX to detect if it's an arb.
/// Uses multiple signals: DEX programs in accounts, token balance changes, inner CPIs.
async fn analyze_tx(
    client: &reqwest::Client,
    rpc_url: &str,
    signature: &str,
    slot: u64,
    timestamp: i64,
) -> anyhow::Result<Option<DetectedArb>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [
            signature,
            {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0}
        ]
    });

    let resp = client.post(rpc_url).json(&body).send().await?;
    let val: serde_json::Value = resp.json().await?;

    let tx = match val.get("result") {
        Some(r) if !r.is_null() => r,
        _ => return Ok(None),
    };

    // Get ALL account keys (including loaded from ALTs)
    let mut all_accounts: Vec<String> = Vec::new();

    if let Some(keys) = tx.pointer("/transaction/message/accountKeys").and_then(|a| a.as_array()) {
        for k in keys {
            if let Some(pk) = k.get("pubkey").and_then(|v| v.as_str()).or_else(|| k.as_str()) {
                all_accounts.push(pk.to_string());
            }
        }
    }
    // ALT-loaded accounts
    if let Some(loaded) = tx.pointer("/meta/loadedAddresses") {
        for key in ["writable", "readonly"] {
            if let Some(addrs) = loaded.get(key).and_then(|a| a.as_array()) {
                for addr in addrs {
                    if let Some(s) = addr.as_str() {
                        all_accounts.push(s.to_string());
                    }
                }
            }
        }
    }

    let account_set: HashSet<&str> = all_accounts.iter().map(|s| s.as_str()).collect();
    let dex_map: HashMap<&str, &str> = DEX_PROGRAMS.iter().cloned().collect();

    // Count DEX programs in ALL accounts (outer + ALT loaded)
    let mut dex_hits: HashSet<&str> = HashSet::new();
    for (prog_id, dex_name) in DEX_PROGRAMS {
        if account_set.contains(*prog_id) {
            dex_hits.insert(dex_name);
        }
    }

    // Count swap CPIs in inner instructions
    let mut n_swaps = 0u32;
    let mut swap_programs: Vec<String> = Vec::new();
    let mut pools_found: Vec<String> = Vec::new();

    // Inner instructions (where CPIs from arb programs appear)
    if let Some(inners) = tx.pointer("/meta/innerInstructions").and_then(|i| i.as_array()) {
        for inner_group in inners {
            if let Some(ixs) = inner_group.get("instructions").and_then(|i| i.as_array()) {
                for ix in ixs {
                    let prog = ix.get("programId").and_then(|p| p.as_str()).unwrap_or("");
                    if dex_map.contains_key(prog) {
                        n_swaps += 1;
                        swap_programs.push(dex_map[prog].to_string());
                        // Extract pool from parsed info or accounts
                        if let Some(accs) = ix.get("accounts").and_then(|a| a.as_array()) {
                            // Pool is typically the first non-program account
                            if let Some(pool) = accs.get(0).and_then(|a| a.as_str()) {
                                if !dex_map.contains_key(pool) {
                                    pools_found.push(pool.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Outer instructions
    if let Some(ixs) = tx.pointer("/transaction/message/instructions").and_then(|i| i.as_array()) {
        for ix in ixs {
            let prog = ix.get("programId").and_then(|p| p.as_str()).unwrap_or("");
            if dex_map.contains_key(prog) {
                n_swaps += 1;
                swap_programs.push(dex_map[prog].to_string());
            }
        }
    }

    // Need at least 2 swap interactions OR 2+ different DEX programs
    if n_swaps < 2 && dex_hits.len() < 2 {
        return Ok(None);
    }

    // Detect flash loan usage
    let uses_flash = FLASH_PROGRAMS.iter().any(|p| account_set.contains(*p));

    // --- Profit detection via token balance changes ---
    // Check pre/postTokenBalances for the fee payer (accountIndex 0)
    let mut profit: i64 = 0;

    // Method 1: SOL balance change
    let pre_sol = tx.pointer("/meta/preBalances").and_then(|b| b.as_array())
        .and_then(|b| b.first()).and_then(|b| b.as_i64()).unwrap_or(0);
    let post_sol = tx.pointer("/meta/postBalances").and_then(|b| b.as_array())
        .and_then(|b| b.first()).and_then(|b| b.as_i64()).unwrap_or(0);
    let sol_change = post_sol - pre_sol;

    // Method 2: WSOL/USDC token balance changes for the signer
    let fee_payer = all_accounts.first().map(|s| s.as_str()).unwrap_or("");
    if let (Some(pre_tokens), Some(post_tokens)) = (
        tx.pointer("/meta/preTokenBalances").and_then(|t| t.as_array()),
        tx.pointer("/meta/postTokenBalances").and_then(|t| t.as_array()),
    ) {
        // Build map: (accountIndex, mint) → amount
        let mut pre_map: HashMap<(u64, String), f64> = HashMap::new();
        let mut post_map: HashMap<(u64, String), f64> = HashMap::new();

        for tb in pre_tokens {
            let idx = tb.get("accountIndex").and_then(|i| i.as_u64()).unwrap_or(999);
            let mint = tb.get("mint").and_then(|m| m.as_str()).unwrap_or("").to_string();
            let amount = tb.pointer("/uiTokenAmount/uiAmount")
                .and_then(|a| a.as_f64()).unwrap_or(0.0);
            pre_map.insert((idx, mint), amount);
        }
        for tb in post_tokens {
            let idx = tb.get("accountIndex").and_then(|i| i.as_u64()).unwrap_or(999);
            let mint = tb.get("mint").and_then(|m| m.as_str()).unwrap_or("").to_string();
            let amount = tb.pointer("/uiTokenAmount/uiAmount")
                .and_then(|a| a.as_f64()).unwrap_or(0.0);
            post_map.insert((idx, mint), amount);
        }

        // Check if signer's WSOL increased (arb profit)
        let wsol = "So11111111111111111111111111111111111111112";
        for ((idx, mint), post_amt) in &post_map {
            if *idx <= 1 && mint == wsol {
                let pre_amt = pre_map.get(&(*idx, mint.clone())).copied().unwrap_or(0.0);
                let diff = post_amt - pre_amt;
                if diff > 0.0 {
                    profit = (diff * 1e9) as i64;
                }
            }
        }
    }

    // Use SOL change if no WSOL profit detected
    if profit == 0 && sol_change > 0 {
        profit = sol_change;
    }

    // Only interested in profitable TXs
    if profit <= 0 {
        return Ok(None);
    }

    let unique_dexes: Vec<String> = dex_hits.into_iter().map(|s| s.to_string()).collect();

    pools_found.sort();
    pools_found.dedup();
    let route_key = if pools_found.is_empty() {
        unique_dexes.join("+")
    } else {
        pools_found.join("_")
    };

    let hour = if timestamp > 0 {
        ((timestamp % 86400) / 3600) as u8
    } else {
        0
    };

    Ok(Some(DetectedArb {
        signature: signature.to_string(),
        slot,
        timestamp,
        pools: pools_found,
        dex_types: unique_dexes,
        n_swaps,
        uses_flash_loan: uses_flash,
        profit_lamports: profit,
        borrow_amount: 0,
        route_key,
        hour,
    }))
}

/// Feed detected arbs into the MlScorer.
pub fn feed_arbs_to_scorer(scorer: &Arc<MlScorer>, arbs: &[DetectedArb]) {
    let mut fed = 0u64;
    for arb in arbs {
        // Feed each pool in the arb route as a "success" observation
        for (i, pool) in arb.pools.iter().enumerate() {
            let dex = arb.dex_types.get(i).map(|s| s.as_str()).unwrap_or("unknown");
            let strategy = if arb.uses_flash_loan {
                "cross-dex-flash"
            } else {
                "cross-dex"
            };
            // Attribute profit evenly across pools in the route
            let profit_per_pool = arb.profit_lamports / arb.pools.len().max(1) as i64;
            scorer.observe(pool, dex, strategy, true, profit_per_pool);
            fed += 1;
        }
    }
    info!(
        arbs = arbs.len(),
        pool_observations = fed,
        "fed on-chain arbs into ML scorer"
    );
}

/// Save detected arbs to PostgreSQL for training data.
pub async fn save_arbs_to_pg(db_url: &str, arbs: &[DetectedArb]) -> anyhow::Result<u64> {
    let (client, connection) =
        tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!("onchain-scanner pg error: {e}");
        }
    });

    // Create table if not exists
    client
        .execute(
            "CREATE TABLE IF NOT EXISTS onchain_arbs (
                id BIGSERIAL PRIMARY KEY,
                signature TEXT UNIQUE NOT NULL,
                slot BIGINT NOT NULL,
                block_time BIGINT,
                pools TEXT[] NOT NULL,
                dex_types TEXT[] NOT NULL,
                n_swaps INT NOT NULL,
                uses_flash_loan BOOLEAN NOT NULL,
                profit_lamports BIGINT NOT NULL,
                route_key TEXT NOT NULL,
                hour SMALLINT NOT NULL,
                created_at TIMESTAMPTZ DEFAULT now()
            )",
            &[],
        )
        .await?;

    // Create index for route analysis
    client
        .execute(
            "CREATE INDEX IF NOT EXISTS idx_onchain_arbs_route ON onchain_arbs (route_key)",
            &[],
        )
        .await?;
    client
        .execute(
            "CREATE INDEX IF NOT EXISTS idx_onchain_arbs_profit ON onchain_arbs (profit_lamports DESC)",
            &[],
        )
        .await?;

    let mut inserted = 0u64;
    for arb in arbs {
        let pools: Vec<&str> = arb.pools.iter().map(|s| s.as_str()).collect();
        let dexes: Vec<&str> = arb.dex_types.iter().map(|s| s.as_str()).collect();

        match client
            .execute(
                "INSERT INTO onchain_arbs (signature, slot, block_time, pools, dex_types, \
                 n_swaps, uses_flash_loan, profit_lamports, route_key, hour) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (signature) DO NOTHING",
                &[
                    &arb.signature,
                    &(arb.slot as i64),
                    &arb.timestamp,
                    &pools,
                    &dexes,
                    &(arb.n_swaps as i32),
                    &arb.uses_flash_loan,
                    &arb.profit_lamports,
                    &arb.route_key,
                    &(arb.hour as i16),
                ],
            )
            .await
        {
            Ok(n) => inserted += n,
            Err(e) => debug!(error = %e, sig = %arb.signature, "insert arb failed"),
        }
    }

    info!(inserted, total = arbs.len(), "saved on-chain arbs to PG");
    Ok(inserted)
}

/// Background loop: scan on-chain arbs periodically and feed into scorer.
pub fn spawn_onchain_scanner(
    rpc_url: String,
    db_url: String,
    scorer: Arc<MlScorer>,
    interval: Duration,
) {
    std::thread::Builder::new()
        .name("onchain-arb-scanner".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt onchain-scanner");

            rt.block_on(async {
                info!(
                    interval_secs = interval.as_secs(),
                    "on-chain arb scanner started"
                );

                // Initial scan: fetch more TXs to bootstrap
                match scan_recent_arbs(&rpc_url, 50).await {
                    Ok(arbs) => {
                        if !arbs.is_empty() {
                            feed_arbs_to_scorer(&scorer, &arbs);
                            if let Err(e) = save_arbs_to_pg(&db_url, &arbs).await {
                                warn!(error = %e, "failed to save initial arbs to PG");
                            }
                            log_arb_stats(&arbs);
                        }
                    }
                    Err(e) => warn!(error = %e, "initial on-chain arb scan failed"),
                }

                loop {
                    tokio::time::sleep(interval).await;

                    match scan_recent_arbs(&rpc_url, 20).await {
                        Ok(arbs) => {
                            if !arbs.is_empty() {
                                feed_arbs_to_scorer(&scorer, &arbs);
                                if let Err(e) = save_arbs_to_pg(&db_url, &arbs).await {
                                    warn!(error = %e, "failed to save arbs to PG");
                                }
                            }
                        }
                        Err(e) => warn!(error = %e, "on-chain arb scan failed"),
                    }
                }
            });
        })
        .expect("spawn onchain-arb-scanner");
}

fn log_arb_stats(arbs: &[DetectedArb]) {
    if arbs.is_empty() {
        return;
    }

    let total_profit: i64 = arbs.iter().map(|a| a.profit_lamports).sum();
    let avg_profit = total_profit / arbs.len() as i64;
    let flash_count = arbs.iter().filter(|a| a.uses_flash_loan).count();

    // Route frequency
    let mut route_counts: HashMap<&str, u32> = HashMap::new();
    for arb in arbs {
        *route_counts.entry(&arb.route_key).or_default() += 1;
    }

    // Top DEX combos
    let mut dex_combos: HashMap<String, u32> = HashMap::new();
    for arb in arbs {
        let mut dexes = arb.dex_types.clone();
        dexes.sort();
        *dex_combos.entry(dexes.join("+")).or_default() += 1;
    }

    let top_combo = dex_combos
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(k, v)| format!("{k} ({v}x)"))
        .unwrap_or_default();

    info!(
        arbs = arbs.len(),
        total_profit_sol = format!("{:.4}", total_profit as f64 / 1e9),
        avg_profit_sol = format!("{:.6}", avg_profit as f64 / 1e9),
        flash_loan_pct = format!("{:.0}%", flash_count as f64 / arbs.len() as f64 * 100.0),
        unique_routes = route_counts.len(),
        top_dex_combo = top_combo,
        "on-chain arb analysis"
    );
}

/// Query PostgreSQL for the most profitable routes (for route prioritization).
pub async fn get_hot_routes(db_url: &str, top_n: usize) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let (client, connection) =
        tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let rows = client
        .query(
            "SELECT route_key, SUM(profit_lamports) as total_profit, COUNT(*) as count \
             FROM onchain_arbs \
             WHERE created_at > now() - interval '24 hours' \
             GROUP BY route_key \
             ORDER BY total_profit DESC \
             LIMIT $1",
            &[&(top_n as i64)],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let route: String = r.get(0);
            let profit: i64 = r.get(1);
            let count: i64 = r.get(2);
            (route, profit, count)
        })
        .collect())
}
