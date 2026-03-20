//! train_competition — Analyze on-chain arb TXs from other bots to learn competition patterns.
//!
//! Scans Jito tip accounts for successful arb TXs, identifies:
//! 1. Which pools are most competitive (many bots targeting them)
//! 2. Which token pairs have low competition (exotic/3-hop)
//! 3. What tip levels win auctions
//! 4. Time-of-day patterns for each route
//! 5. Which bots are fastest (by analyzing their TX patterns)
//!
//! Saves competition map to PostgreSQL for the DecisionEngine.

use std::collections::HashMap;
use tracing::info;

const RPC_URL: &str = "https://mainnet.helius-rpc.com/?api-key=YOUR_HELIUS_KEY";
const DB_URL: &str = "postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios";

/// Known DEX program IDs
const DEX_PROGRAMS: &[&str] = &[
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",   // Orca
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",   // Meteora
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",   // PumpSwap
];

/// Jito tip accounts (arb bots always tip here)
const TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| RPC_URL.to_string());
    let db_url = std::env::var("DB_URL").unwrap_or_else(|_| DB_URL.to_string());

    info!("Competition Detector Training — scanning on-chain arb TXs");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut all_arbs: Vec<ArbTx> = Vec::new();

    // Scan each tip account for recent successful TXs
    for tip_account in TIP_ACCOUNTS {
        info!("Scanning tip account: {}...", &tip_account[..8]);

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getSignaturesForAddress",
            "params": [tip_account, {"limit": 100, "commitment": "confirmed"}]
        });

        let resp = match client.post(&rpc_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => { tracing::warn!(error = %e, "RPC failed"); continue; }
        };

        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => continue,
        };

        let sigs = match json["result"].as_array() {
            Some(s) => s,
            None => continue,
        };

        for sig_info in sigs {
            // Only successful TXs
            if !sig_info["err"].is_null() { continue; }

            let sig = sig_info["signature"].as_str().unwrap_or("");
            let slot = sig_info["slot"].as_u64().unwrap_or(0);
            let ts = sig_info["blockTime"].as_i64().unwrap_or(0);

            // Fetch TX details
            let tx_body = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "getTransaction",
                "params": [sig, {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0}]
            });

            let tx_resp = match client.post(&rpc_url).json(&tx_body).send().await {
                Ok(r) => r,
                Err(_) => continue,
            };

            let tx_json: serde_json::Value = match tx_resp.json().await {
                Ok(j) => j,
                Err(_) => continue,
            };

            let tx = match tx_json.get("result") {
                Some(t) if !t.is_null() => t,
                _ => continue,
            };

            // Analyze: count DEX programs, extract pools, compute profit
            let mut dex_count = 0u32;
            let mut pools_found: Vec<String> = Vec::new();
            let mut dex_types: Vec<String> = Vec::new();

            // Check all account keys for DEX programs
            if let Some(keys) = tx.pointer("/transaction/message/accountKeys") {
                let keys_arr = keys.as_array().unwrap_or(&Vec::new()).clone();
                for key in &keys_arr {
                    let pk = key.get("pubkey").and_then(|v| v.as_str())
                        .or_else(|| key.as_str())
                        .unwrap_or("");
                    for (i, dex) in DEX_PROGRAMS.iter().enumerate() {
                        if pk == *dex {
                            dex_count += 1;
                            dex_types.push(["raydium", "orca", "meteora", "pumpswap"][i].to_string());
                        }
                    }
                }
            }

            // Need at least 2 DEX interactions for arb
            if dex_count < 2 { continue; }

            // Extract SOL profit
            let pre_sol = tx.pointer("/meta/preBalances/0").and_then(|b| b.as_i64()).unwrap_or(0);
            let post_sol = tx.pointer("/meta/postBalances/0").and_then(|b| b.as_i64()).unwrap_or(0);
            let profit = post_sol - pre_sol;
            if profit <= 0 { continue; }

            // Extract fee payer (the bot)
            let fee_payer = tx.pointer("/transaction/message/accountKeys/0")
                .and_then(|k| k.get("pubkey").and_then(|v| v.as_str()).or_else(|| k.as_str()))
                .unwrap_or("unknown");

            let hour = if ts > 0 { ((ts % 86400) / 3600) as u8 } else { 0 };

            all_arbs.push(ArbTx {
                signature: sig.to_string(),
                slot,
                timestamp: ts,
                fee_payer: fee_payer.to_string(),
                dex_types: dex_types.clone(),
                n_swaps: dex_count,
                profit_lamports: profit,
                hour,
            });

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    info!("Found {} competitor arb TXs", all_arbs.len());

    // ═══ ANALYSIS ═══

    // Top bots by profit
    let mut bot_profits: HashMap<String, (i64, u32)> = HashMap::new();
    for arb in &all_arbs {
        let entry = bot_profits.entry(arb.fee_payer.clone()).or_insert((0, 0));
        entry.0 += arb.profit_lamports;
        entry.1 += 1;
    }
    let mut bots: Vec<_> = bot_profits.iter().collect();
    bots.sort_by(|a, b| b.1.0.cmp(&a.1.0));

    info!("\n=== Top Competitor Bots ===");
    for (bot, (profit, count)) in bots.iter().take(10) {
        info!("  {}... profit={:.4} SOL txs={}", &bot[..8], *profit as f64 / 1e9, count);
    }

    // DEX combo frequency
    let mut combo_freq: HashMap<String, u32> = HashMap::new();
    for arb in &all_arbs {
        let mut dexes = arb.dex_types.clone();
        dexes.sort();
        dexes.dedup();
        *combo_freq.entry(dexes.join("+")).or_default() += 1;
    }
    info!("\n=== DEX Combos (most competitive) ===");
    let mut combos: Vec<_> = combo_freq.iter().collect();
    combos.sort_by(|a, b| b.1.cmp(a.1));
    for (combo, count) in combos.iter().take(10) {
        info!("  {}: {} arbs", combo, count);
    }

    // Hour distribution
    let mut hour_dist: HashMap<u8, u32> = HashMap::new();
    for arb in &all_arbs {
        *hour_dist.entry(arb.hour).or_default() += 1;
    }
    info!("\n=== Peak Hours ===");
    let mut hours: Vec<_> = hour_dist.iter().collect();
    hours.sort_by(|a, b| b.1.cmp(a.1));
    for (hour, count) in hours.iter().take(5) {
        info!("  {}:00 UTC: {} arbs", hour, count);
    }

    // Save to PG
    let (pg_client, connection) = tokio_postgres::connect(&db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move { let _ = connection.await; });

    // Save competition map
    let competition_data = serde_json::json!({
        "trained_at": chrono::Utc::now().to_rfc3339(),
        "total_arbs": all_arbs.len(),
        "top_bots": bots.iter().take(20).map(|(b, (p, c))| {
            serde_json::json!({"bot": b, "profit": p, "count": c})
        }).collect::<Vec<_>>(),
        "dex_combos": combo_freq,
        "hour_distribution": hour_dist,
        "high_competition_combos": combos.iter().take(5).map(|(c, n)| {
            serde_json::json!({"combo": c, "count": n})
        }).collect::<Vec<_>>(),
    });

    pg_client.execute(
        "INSERT INTO ml_model_state (model_name, weights_json, n_samples) \
         VALUES ('competition_detector', $1, $2) \
         ON CONFLICT (model_name) DO UPDATE SET weights_json = $1, n_samples = $2, updated_at = now()",
        &[&competition_data, &(all_arbs.len() as i64)],
    ).await?;

    info!("\nModel saved to PG (ml_model_state.competition_detector)");
    info!("Total competitor arbs analyzed: {}", all_arbs.len());

    Ok(())
}

struct ArbTx {
    signature: String,
    slot: u64,
    timestamp: i64,
    fee_payer: String,
    dex_types: Vec<String>,
    n_swaps: u32,
    profit_lamports: i64,
    hour: u8,
}
