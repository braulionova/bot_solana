//! analyze_graduations.rs – Analyze PumpFun graduation tokens to find winning patterns.
//!
//! Takes pool addresses from helios-bot logs and checks current vault balances
//! vs initial graduation reserves to determine winners/losers.
//!
//! Usage: cargo run --release --bin analyze_graduations

use std::time::Duration;

use base64::Engine as _;
use reqwest::blocking::Client;
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;

const PUMPSWAP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOC_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// Typical PumpFun graduation initial reserves: ~79-85 SOL.
const INITIAL_SOL_ESTIMATE: f64 = 82.0;

fn main() {
    let rpc_url = std::env::var("SIM_RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());

    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    // Extract pool addresses from helios-bot logs.
    println!("=== PumpFun Graduation Analysis ===");
    println!("Extracting pools from helios-bot logs...\n");

    let pools = extract_pools_from_logs();
    println!("Found {} unique pools in logs\n", pools.len());

    if pools.is_empty() {
        println!("No pools found in logs. Check helios-bot journal.");
        return;
    }

    let mut winners = 0u32;
    let mut losers = 0u32;
    let mut rugged = 0u32; // <1 SOL remaining
    let mut healthy = 0u32; // >50 SOL remaining
    let mut total_return_pct = 0.0f64;
    let mut stats: Vec<PoolStat> = Vec::new();
    let mut frozen_count = 0u32;
    let mut mint_auth_count = 0u32;

    // Analyze pools in batches (read pool account → parse → read vaults).
    for (i, (pool_str, token_str)) in pools.iter().enumerate() {
        let pool_pk: Pubkey = match pool_str.parse() {
            Ok(pk) => pk,
            Err(_) => continue,
        };
        let token_mint: Pubkey = match token_str.parse() {
            Ok(pk) => pk,
            Err(_) => continue,
        };

        // Read pool account to get vault addresses.
        let pool_data = match read_account_base64(&client, &rpc_url, &pool_pk) {
            Some(d) => d,
            None => continue,
        };

        if pool_data.len() < 243 {
            continue;
        }

        let base_mint = pk_from(&pool_data, 43);
        let quote_mint = pk_from(&pool_data, 75);
        let pool_base_vault = pk_from(&pool_data, 139);
        let pool_quote_vault = pk_from(&pool_data, 171);
        let coin_creator = pk_from(&pool_data, 211);

        let wsol: Pubkey = WSOL_MINT.parse().unwrap();
        let wsol_is_base = base_mint == wsol;

        // Read vault balances in one batch.
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getMultipleAccounts",
            "params": [[
                pool_base_vault.to_string(),
                pool_quote_vault.to_string(),
                token_mint.to_string()
            ], {"encoding": "jsonParsed"}]
        });

        let resp = match client.post(&rpc_url).json(&body).send() {
            Ok(r) => r,
            Err(_) => { std::thread::sleep(Duration::from_millis(500)); continue; },
        };
        let json: Value = match resp.json() {
            Ok(j) => j,
            Err(_) => continue,
        };

        let vals = &json["result"]["value"];
        let base_balance = extract_token_balance(&vals[0]).unwrap_or(0);
        let quote_balance = extract_token_balance(&vals[1]).unwrap_or(0);

        let (wsol_reserve, token_reserve) = if wsol_is_base {
            (base_balance, quote_balance)
        } else {
            (quote_balance, base_balance)
        };

        // Check token mint for freeze/mint authority.
        let mint_info = &vals[2];
        let has_freeze = mint_info["data"]["parsed"]["info"]["freezeAuthority"].as_str()
            .map_or(false, |s| !s.is_empty());
        let has_mint_auth = mint_info["data"]["parsed"]["info"]["mintAuthority"].as_str()
            .map_or(false, |s| !s.is_empty());

        if has_freeze { frozen_count += 1; }
        if has_mint_auth { mint_auth_count += 1; }

        if wsol_reserve == 0 && token_reserve == 0 {
            // Pool drained completely — rug pull.
            rugged += 1;
            losers += 1;
            stats.push(PoolStat {
                pool: pool_str.clone(),
                token: token_str.clone(),
                sol_now: 0.0,
                return_pct: -100.0,
                has_freeze,
                has_mint_auth,
                status: "RUGGED",
            });
            continue;
        }

        let current_wsol = wsol_reserve as f64 / 1e9;
        let return_pct = (current_wsol - INITIAL_SOL_ESTIMATE) / INITIAL_SOL_ESTIMATE * 100.0;

        let status = if current_wsol < 1.0 {
            rugged += 1;
            losers += 1;
            "RUGGED"
        } else if current_wsol < INITIAL_SOL_ESTIMATE {
            losers += 1;
            "LOSER"
        } else {
            winners += 1;
            if current_wsol > INITIAL_SOL_ESTIMATE * 1.5 {
                "BIG_WIN"
            } else {
                "WINNER"
            }
        };

        if current_wsol > 50.0 { healthy += 1; }
        total_return_pct += return_pct;

        stats.push(PoolStat {
            pool: pool_str.clone(),
            token: token_str.clone(),
            sol_now: current_wsol,
            return_pct,
            has_freeze,
            has_mint_auth,
            status,
        });

        if (i + 1) % 10 == 0 {
            print!("\rAnalyzed {}/{}...", i + 1, pools.len());
        }

        // Rate limit: 100ms between requests.
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("\r                                    ");

    // Sort by return descending.
    stats.sort_by(|a, b| b.return_pct.partial_cmp(&a.return_pct).unwrap_or(std::cmp::Ordering::Equal));

    let total = stats.len().max(1) as f64;

    println!("╔════════════════════════════════════════════════════════════╗");
    println!("║           PUMPFUN GRADUATION ANALYSIS                    ║");
    println!("╠════════════════════════════════════════════════════════════╣");
    println!("║ Analyzed: {:<47}║", stats.len());
    println!("║ Winners:  {:<4} ({:.1}%)                                   ║", winners, winners as f64 / total * 100.0);
    println!("║ Losers:   {:<4} ({:.1}%)                                   ║", losers, losers as f64 / total * 100.0);
    println!("║ Rugged:   {:<4} ({:.1}%) — <1 SOL remaining               ║", rugged, rugged as f64 / total * 100.0);
    println!("║ Healthy:  {:<4} ({:.1}%) — >50 SOL remaining              ║", healthy, healthy as f64 / total * 100.0);
    println!("║ Avg return: {:<+.1}%                                       ║", total_return_pct / total);
    println!("╠════════════════════════════════════════════════════════════╣");
    println!("║ SCAM INDICATORS                                          ║");
    println!("║ Freeze authority:  {:<4} ({:.1}%)                          ║", frozen_count, frozen_count as f64 / total * 100.0);
    println!("║ Active mint auth:  {:<4} ({:.1}%)                          ║", mint_auth_count, mint_auth_count as f64 / total * 100.0);
    println!("╚════════════════════════════════════════════════════════════╝");

    println!("\n=== TOP 10 WINNERS ===");
    println!("{:<10} {:<12} {:>10} {:>10} {:>6} {:>6}", "STATUS", "TOKEN", "SOL NOW", "RETURN", "FREEZE", "MINT");
    println!("{}", "-".repeat(60));
    for s in stats.iter().filter(|s| s.return_pct > 0.0).take(10) {
        println!("{:<10} {}.. {:>8.2} {:>+9.1}% {:>6} {:>6}",
            s.status, &s.token[..8], s.sol_now, s.return_pct,
            if s.has_freeze { "YES" } else { "no" },
            if s.has_mint_auth { "YES" } else { "no" },
        );
    }

    println!("\n=== TOP 10 LOSERS ===");
    println!("{:<10} {:<12} {:>10} {:>10} {:>6} {:>6}", "STATUS", "TOKEN", "SOL NOW", "RETURN", "FREEZE", "MINT");
    println!("{}", "-".repeat(60));
    for s in stats.iter().rev().take(10) {
        println!("{:<10} {}.. {:>8.2} {:>+9.1}% {:>6} {:>6}",
            s.status, &s.token[..8], s.sol_now, s.return_pct,
            if s.has_freeze { "YES" } else { "no" },
            if s.has_mint_auth { "YES" } else { "no" },
        );
    }

    // Correlation analysis: do scam indicators predict losers?
    let freeze_losers = stats.iter().filter(|s| s.has_freeze && s.return_pct < 0.0).count();
    let freeze_total = stats.iter().filter(|s| s.has_freeze).count().max(1);
    let no_freeze_losers = stats.iter().filter(|s| !s.has_freeze && s.return_pct < 0.0).count();
    let no_freeze_total = stats.iter().filter(|s| !s.has_freeze).count().max(1);

    let mint_losers = stats.iter().filter(|s| s.has_mint_auth && s.return_pct < 0.0).count();
    let mint_total = stats.iter().filter(|s| s.has_mint_auth).count().max(1);
    let no_mint_losers = stats.iter().filter(|s| !s.has_mint_auth && s.return_pct < 0.0).count();
    let no_mint_total = stats.iter().filter(|s| !s.has_mint_auth).count().max(1);

    println!("\n=== SCAM INDICATOR CORRELATION ===");
    println!("Freeze authority present:  {:.0}% lose value ({}/{})", freeze_losers as f64 / freeze_total as f64 * 100.0, freeze_losers, freeze_total);
    println!("Freeze authority absent:   {:.0}% lose value ({}/{})", no_freeze_losers as f64 / no_freeze_total as f64 * 100.0, no_freeze_losers, no_freeze_total);
    println!("Mint authority present:    {:.0}% lose value ({}/{})", mint_losers as f64 / mint_total as f64 * 100.0, mint_losers, mint_total);
    println!("Mint authority absent:     {:.0}% lose value ({}/{})", no_mint_losers as f64 / no_mint_total as f64 * 100.0, no_mint_losers, no_mint_total);

    // Strategy recommendations.
    let safe_winners = stats.iter().filter(|s| !s.has_freeze && !s.has_mint_auth && s.return_pct > 0.0).count();
    let safe_total = stats.iter().filter(|s| !s.has_freeze && !s.has_mint_auth).count().max(1);

    println!("\n=== STRATEGY RECOMMENDATIONS ===");
    println!("Safe tokens (no freeze, no mint auth) win rate: {:.0}% ({}/{})",
        safe_winners as f64 / safe_total as f64 * 100.0, safe_winners, safe_total);
    println!("");
    println!("1. FILTER: Skip tokens with freeze authority or active mint authority");
    println!("2. TRAILING STOP: Sell when price drops 5% from peak (after 5% gain)");
    println!("3. EMERGENCY STOP: Sell immediately if >50% loss from entry");
    println!("4. MAX HOLD: Never hold >60s — most rug pulls happen in first 2 minutes");
    println!("5. LIQUIDITY CHECK: Only snipe pools with >1 SOL initial WSOL reserve");
}

struct PoolStat {
    pool: String,
    token: String,
    sol_now: f64,
    return_pct: f64,
    has_freeze: bool,
    has_mint_auth: bool,
    status: &'static str,
}

/// Extract pool and token addresses from helios-bot journal logs.
fn extract_pools_from_logs() -> Vec<(String, String)> {
    use std::process::Command;

    let output = Command::new("journalctl")
        .args(["-u", "helios-bot", "--no-pager", "-o", "cat"])
        .output();

    let raw = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => {
            eprintln!("Failed to read journal: {}", e);
            return Vec::new();
        }
    };
    // Strip ANSI escape codes.
    let stdout = strip_ansi(&raw);

    let mut pools = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in stdout.lines() {
        // Match: "SNIPER: graduation detected — building snipe TX token=XXX pool=YYY"
        if line.contains("graduation detected") {
            let token = extract_field(line, "token=");
            let pool = extract_field(line, "pool=");
            if let (Some(t), Some(p)) = (token, pool) {
                if seen.insert(p.clone()) {
                    pools.push((p, t));
                }
            }
        }
    }

    pools
}

fn extract_field(line: &str, prefix: &str) -> Option<String> {
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let end = rest.find(|c: char| c == ' ' || c == '\n' || c == '\t').unwrap_or(rest.len());
    let val = &rest[..end];
    if val.len() >= 32 { Some(val.to_string()) } else { None }
}

fn read_account_base64(client: &Client, rpc_url: &str, pubkey: &Pubkey) -> Option<Vec<u8>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pubkey.to_string(), {"encoding": "base64"}]
    });

    let resp = client.post(rpc_url).json(&body).send().ok()?;
    let json: Value = resp.json().ok()?;

    if json["result"]["value"].is_null() {
        return None;
    }

    let data_b64 = json["result"]["value"]["data"][0].as_str()?;
    base64::engine::general_purpose::STANDARD.decode(data_b64).ok()
}

fn pk_from(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::from(<[u8; 32]>::try_from(&data[offset..offset + 32]).unwrap_or([0u8; 32]))
}

fn extract_token_balance(val: &Value) -> Option<u64> {
    val["data"]["parsed"]["info"]["tokenAmount"]["amount"]
        .as_str()?
        .parse()
        .ok()
}

/// Strip ANSI escape codes from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Consume the '[' and all following digits/semicolons until a letter.
            if let Some(next) = chars.next() {
                if next == '[' {
                    for cc in chars.by_ref() {
                        if cc.is_ascii_alphabetic() { break; }
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
