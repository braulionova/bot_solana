/// Dual-Scanner Flash Arb: Carbium + Jupiter circular arb detection
/// MarginFi flash loan (0% fee) wraps profitable routes
///
/// Scanner 1: Carbium CQ1 — native circular SOL→X→SOL quotes
/// Scanner 2: Jupiter — 2-leg arb: buy(SOL→token) vs sell(token→SOL), detect spread
///            Also: Jupiter circular via intermediate tokens (USDC, mSOL, JUP, etc.)
///
/// All execution wrapped with MarginFi flash loan = 0 capital, 0 fee, atomic

use anyhow::{anyhow, Result};
use base64::Engine;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
    transaction::VersionedTransaction,
    address_lookup_table::AddressLookupTableAccount,
};

const CARBIUM_API: &str = "https://api.carbium.io/api/v2/quote";
const CARBIUM_RPC: &str = "https://rpc-service.carbium.io/?apiKey=578475ff-1380";
const TX_FEE_LAMPORTS: i64 = 5_000;
const JITO_TIP_LAMPORTS: i64 = 10_000;

// --- MarginFi (0% fee) ---
const MARGINFI_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";
const MARGINFI_GROUP: &str = "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8";
const MARGINFI_ACCOUNT: &str = "Gj3hRZsQCEboWngk64eFGd3ja4JBoAUMYEiQKSFoAEWY";
const MARGINFI_SOL_BANK: &str = "CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAntD6LYGh";
const MARGINFI_SOL_VAULT: &str = "2eicbpitfJXDwqCuFAmPgDP7t2oUotnAzbGzRKLMgSLe";
const MARGINFI_SOL_ORACLE: &str = "4Hmd6PdjVA9auCoScE12iaBogfwS4ZXQ6VZoBeqanwWW";
const MARGINFI_ALT: &str = "83QFhtNV6AUqXCQpiu7JJupt6nwqX7xBsFAehRGbiwPC";

const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

// Jupiter API
const JUPITER_QUOTE: &str = "https://api.jup.ag/swap/v1/quote";
const JUPITER_SWAP: &str = "https://api.jup.ag/swap/v1/swap";

// Tokens to scan — high liquidity + high volatility for cross-DEX spreads
const SCAN_TOKENS: &[(&str, &str)] = &[
    // Stables (tight spreads but high volume — micro arbs)
    ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC"),
    ("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", "USDT"),
    // LSTs (depeg arbs)
    ("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So", "mSOL"),
    ("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", "jitoSOL"),
    ("bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1", "bSOL"),
    ("5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm", "INF"),
    // Blue chips (high volume)
    ("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN", "JUP"),
    ("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs", "ETH"),
    ("27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4", "JLP"),
    // Memecoins (volatile — bigger spreads)
    ("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", "BONK"),
    ("EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", "WIF"),
    ("HeLp6NuQkmYB4pYWo2zYs22mESHXPQYzXbB8n4V98jwC", "TRUMP"),
    ("hntyVP6YFm1Hg25TN9WGLqM12b8TQmcknKrdu1oxWux", "HNT"),
    ("GJALwSLakGkVhCqj1aWGSjzd7KCcmSMpjzBNTBNwvuCe", "ai16z"),
    ("CLoUDKc4Ane7HeQcPpE3YHnznRxhMimJ4MyaUqyHFzAu", "CLOUD"),
    ("rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof", "RENDER"),
    ("SHDWyBxihqiCj6YekG2GUr7wqKLeLAMK1gHZck9pL6y", "SHDW"),
    ("MEW1gQWJ3nEXg2qgERiKu7FAFj79PHvQVREQUzScPP5", "MEW"),
    ("A8C3xuqscfmyLrQ3HJNc7FoyPyiddfNAT7YoANqJPrao", "PNUT"),
    ("Grass7B4RdKfBCjTKgSqnXkqjwiGvQyFbuSCUJr3XXjs", "GRASS"),
];

fn main() -> Result<()> {
    let carbium_key = std::env::var("CARBIUM_API_KEY").unwrap_or("814f09dc-d645".into());
    // 2 Jupiter API keys for parallel scanning
    let jup_keys = vec![
        std::env::var("JUPITER_API_KEY").unwrap_or("a4a6b7e3-12c1-4c44-9e63-f5dee8506c8e".into()),
        std::env::var("JUPITER_API_KEY_2").unwrap_or("3c8663da-9895-493b-9b62-af7ae9404a24".into()),
    ];
    let wb: Vec<u8> = serde_json::from_str(&std::fs::read_to_string("/root/solana-bot/wallet.json")?)?;
    let kp = Keypair::from_bytes(&wb).map_err(|e| anyhow!("{}", e))?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build()?;
    let continuous = std::env::var("CONTINUOUS").map(|v| v == "true").unwrap_or(false);
    let min_profit: i64 = std::env::var("MIN_PROFIT").and_then(|v| Ok(v.parse().unwrap_or(5000))).unwrap_or(5000);
    let taker = kp.pubkey().to_string();
    let amounts: Vec<u64> = vec![100_000_000, 500_000_000, 1_000_000_000]; // 0.1, 0.5, 1.0 SOL
    let mut round = 0u64;
    let mut stats = Stats::default();

    // One-time: withdraw any existing deposit from MarginFi
    withdraw_marginfi_deposit(&client, &kp);

    println!("Flash Arb Scanner | wallet: {} | min_profit: {} lam", taker, min_profit);
    println!("  {} tokens × {} amounts = {} scans/round (parallel batches)",
        SCAN_TOKENS.len(), amounts.len(), SCAN_TOKENS.len() * amounts.len());
    println!("  MarginFi 0% flash loan | Jito bundles (0 cost on fail)");

    loop {
        round += 1;

        // === Carbium circular (quick, 3 amounts) ===
        for &amount in &amounts {
            let amt_str = amount.to_string();
            if let Ok(true) = scan_carbium(&client, &carbium_key, &kp, &amt_str, &taker, min_profit, &mut stats) {
                println!("🎉 Carbium arb executed!");
            }
        }

        // === Jupiter parallel scan: 4 tokens per batch (2 keys × 2 concurrent) ===
        let batch_size = 4;
        for batch_start in (0..SCAN_TOKENS.len()).step_by(batch_size) {
            let batch_end = (batch_start + batch_size).min(SCAN_TOKENS.len());
            let batch = &SCAN_TOKENS[batch_start..batch_end];

            // Scan each token×amount combo in parallel threads
            let mut handles = Vec::new();
            for (i, &(mint_str, name_str)) in batch.iter().enumerate() {
                let api_key = jup_keys[i % jup_keys.len()].clone();
                let mint = mint_str.to_string();
                let name = name_str.to_string();
                let mint_out = mint_str.to_string();
                let name_out = name_str.to_string();
                let amounts_c = amounts.clone();

                let handle = std::thread::spawn(move || {
                    let c = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(4)).build().ok()?;
                    let mut best: Option<(i64, u64, String, String, String)> = None; // (profit, amount, buy_dex, sell_dex, name)

                    for &amount in &amounts_c {
                        let buy_url = format!(
                            "{}?inputMint={}&outputMint={}&amount={}&slippageBps=300&onlyDirectRoutes=true",
                            JUPITER_QUOTE, WSOL_MINT, mint, amount
                        );
                        let buy_resp: serde_json::Value = c.get(&buy_url)
                            .header("x-api-key", &api_key).send().ok()?.json().ok()?;
                        let tokens_out: u64 = buy_resp.get("outAmount")
                            .and_then(|v| v.as_str())?.parse().ok()?;
                        if tokens_out == 0 { continue; }

                        let sell_url = format!(
                            "{}?inputMint={}&outputMint={}&amount={}&slippageBps=300&onlyDirectRoutes=true",
                            JUPITER_QUOTE, mint, WSOL_MINT, tokens_out
                        );
                        let sell_resp: serde_json::Value = c.get(&sell_url)
                            .header("x-api-key", &api_key).send().ok()?.json().ok()?;
                        let sol_back: u64 = sell_resp.get("outAmount")
                            .and_then(|v| v.as_str())?.parse().ok()?;

                        let profit = sol_back as i64 - amount as i64;
                        if profit > best.as_ref().map(|b| b.0).unwrap_or(0) {
                            let buy_dex = buy_resp.get("routePlan").and_then(|r| r.as_array())
                                .and_then(|a| a.first())
                                .and_then(|r| r.get("swapInfo")).and_then(|s| s.get("label"))
                                .and_then(|l| l.as_str()).unwrap_or("?").to_string();
                            let sell_dex = sell_resp.get("routePlan").and_then(|r| r.as_array())
                                .and_then(|a| a.first())
                                .and_then(|r| r.get("swapInfo")).and_then(|s| s.get("label"))
                                .and_then(|l| l.as_str()).unwrap_or("?").to_string();
                            best = Some((profit, amount, buy_dex, sell_dex, name.clone()));
                        }
                    }
                    best
                });
                handles.push((handle, mint_out, name_out));
            }

            // Collect results — execute best opportunity
            for (handle, mint, name) in handles {
                if let Ok(Some((profit, amount, buy_dex, sell_dex, _))) = handle.join() {
                    let net = profit - JITO_TIP_LAMPORTS;
                    stats.jup_opportunities += 1;

                    if net < min_profit {
                        if profit > 0 {
                            let bps = (profit as f64 / amount as f64) * 10_000.0;
                            println!("   [Jup] {} {:.1}bps ({} lam) buy@{} sell@{} @ {} SOL",
                                name, bps, profit, buy_dex, sell_dex, amount as f64 / 1e9);
                        }
                        continue;
                    }

                    let bps = (profit as f64 / amount as f64) * 10_000.0;
                    println!("✅ [Jup] {} +{} net ({:.1}bps) buy@{} sell@{} @ {} SOL",
                        name, net, bps, buy_dex, sell_dex, amount as f64 / 1e9);

                    // Execute: get swap TXs, wrap with MarginFi, send
                    if let Ok(true) = execute_jupiter_arb(&client, &jup_keys[0], &kp, &mint, amount, &mut stats) {
                        println!("🎉 Jupiter arb executed! ({})", name);
                    }
                }
            }

            // Brief pause between batches to respect rate limits
            std::thread::sleep(std::time::Duration::from_millis(300));
        }

        if round % 3 == 0 {
            println!("📊 R{}: sent={} mfi={} direct={} sim_fail={} too_big={} jup_opp={} | bal={}",
                round, stats.sent, stats.mfi_sent, stats.direct_sent,
                stats.sim_fail, stats.too_big, stats.jup_opportunities,
                std::fs::read_to_string("/proc/self/status").ok()
                    .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).map(|l| l.to_string()))
                    .unwrap_or_default());
        }
        if !continuous { break; }
    }
    Ok(())
}

/// Execute a Jupiter 2-leg arb: get swap TXs, wrap with MarginFi flash loan, send
fn execute_jupiter_arb(client: &reqwest::blocking::Client, api_key: &str, kp: &Keypair,
    token_mint: &str, amount: u64, stats: &mut Stats) -> Result<bool> {

    // Get buy+sell quotes fresh (the parallel scan may be stale)
    let buy_url = format!(
        "{}?inputMint={}&outputMint={}&amount={}&slippageBps=300&onlyDirectRoutes=true",
        JUPITER_QUOTE, WSOL_MINT, token_mint, amount
    );
    let buy_resp: serde_json::Value = client.get(&buy_url)
        .header("x-api-key", api_key).send()?.json()?;
    let tokens_out: u64 = buy_resp.get("outAmount")
        .and_then(|v| v.as_str()).unwrap_or("0").parse()?;
    if tokens_out == 0 { return Ok(false); }

    let sell_url = format!(
        "{}?inputMint={}&outputMint={}&amount={}&slippageBps=300&onlyDirectRoutes=true",
        JUPITER_QUOTE, token_mint, WSOL_MINT, tokens_out
    );
    let sell_resp: serde_json::Value = client.get(&sell_url)
        .header("x-api-key", api_key).send()?.json()?;
    let sol_back: u64 = sell_resp.get("outAmount")
        .and_then(|v| v.as_str()).unwrap_or("0").parse()?;

    if (sol_back as i64) <= (amount as i64) { return Ok(false); }

    // Get swap TXs in parallel
    let user_pk = kp.pubkey().to_string();
    let buy_q = buy_resp.clone();
    let sell_q = sell_resp.clone();
    let ak1 = api_key.to_string();
    let ak2 = api_key.to_string();
    let up1 = user_pk.clone();
    let up2 = user_pk.clone();

    let h1 = std::thread::spawn(move || {
        let c = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(4)).build().ok()?;
        get_jupiter_swap_tx_static(&c, &ak1, &buy_q, &up1).ok()
    });
    let h2 = std::thread::spawn(move || {
        let c = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(4)).build().ok()?;
        get_jupiter_swap_tx_static(&c, &ak2, &sell_q, &up2).ok()
    });

    let buy_swap = h1.join().ok().flatten().ok_or(anyhow!("buy swap TX failed"))?;
    let sell_swap = h2.join().ok().flatten().ok_or(anyhow!("sell swap TX failed"))?;

    let (buy_ixs, mut alts) = extract_ixs_and_alts(&buy_swap, &kp.pubkey())?;
    let (sell_ixs, sell_alts) = extract_ixs_and_alts(&sell_swap, &kp.pubkey())?;

    let mut swap_ixs = buy_ixs;
    swap_ixs.extend(sell_ixs);
    alts.extend(sell_alts);
    alts.sort_by_key(|a| a.key);
    alts.dedup_by_key(|a| a.key);

    if swap_ixs.is_empty() { return Ok(false); }

    println!("   {} swap IXs, {} ALTs", swap_ixs.len(), alts.len());
    try_marginfi_flash_with_ixs(client, kp, swap_ixs, alts, amount, stats)
}

#[derive(Default)]
struct Stats {
    sent: u64,
    mfi_sent: u64,
    direct_sent: u64,
    sim_fail: u64,
    too_big: u64,
    jup_opportunities: u64,
}

// ============================================================
// Scanner 1: Carbium circular arb (SOL→X→SOL native)
// ============================================================
fn scan_carbium(client: &reqwest::blocking::Client, api_key: &str, kp: &Keypair,
    amount: &str, taker: &str, min_profit: i64, stats: &mut Stats) -> Result<bool> {

    let url = format!(
        "{}?src_mint={}&dst_mint={}&amount_in={}&slippage_bps=500&user_account={}",
        CARBIUM_API, WSOL_MINT, WSOL_MINT, amount, taker
    );
    let resp: serde_json::Value = client.get(&url).header("X-API-KEY", api_key).send()?.json()?;

    if let Some(e) = resp.get("error").and_then(|v| v.as_str()) {
        return Err(anyhow!("{}", e));
    }

    let in_amt: i64 = resp.get("srcAmountIn").and_then(|v| v.as_str()).unwrap_or("0").parse()?;
    let out_amt: i64 = resp.get("destAmountOut").and_then(|v| v.as_str()).unwrap_or("0").parse()?;
    let raw_profit = out_amt - in_amt;
    let borrow_amount: u64 = amount.parse()?;
    let txn_b64 = match resp.get("txn").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Ok(false),
    };

    let net_profit = raw_profit - JITO_TIP_LAMPORTS;
    if net_profit < min_profit { return Ok(false); }

    println!("✅ [Carbium] +{} net @ {} SOL", net_profit, in_amt as f64 / 1e9);

    // Try MarginFi flash
    match try_marginfi_flash(client, kp, txn_b64, borrow_amount, stats) {
        Ok(true) => return Ok(true),
        _ => {}
    }
    // Fallback: direct TX
    send_carbium_direct(client, txn_b64, kp, stats)
}

// ============================================================
// Scanner 2: Jupiter 2-leg arb (SOL→token→SOL)
// Buy via one route, sell via another — detect cross-DEX spread
// ============================================================
fn scan_jupiter_2leg(client: &reqwest::blocking::Client, api_key: &str, kp: &Keypair,
    token_mint: &str, token_name: &str, amount_lamports: u64,
    min_profit: i64, stats: &mut Stats) -> Result<bool> {

    // FAST: parallel buy+sell quotes using threads
    let buy_url = format!(
        "{}?inputMint={}&outputMint={}&amount={}&slippageBps=300&onlyDirectRoutes=true",
        JUPITER_QUOTE, WSOL_MINT, token_mint, amount_lamports
    );

    // First get buy quote (need tokens_out for sell quote)
    let buy_resp: serde_json::Value = client.get(&buy_url)
        .header("x-api-key", api_key).send()?.json()?;
    if buy_resp.get("error").is_some() { return Ok(false); }

    let tokens_out: u64 = buy_resp.get("outAmount")
        .and_then(|v| v.as_str()).unwrap_or("0").parse().unwrap_or(0);
    if tokens_out == 0 { return Ok(false); }

    // Sell quote + buy swap TX in parallel (both independent once we have tokens_out)
    let sell_url = format!(
        "{}?inputMint={}&outputMint={}&amount={}&slippageBps=300&onlyDirectRoutes=true",
        JUPITER_QUOTE, token_mint, WSOL_MINT, tokens_out
    );
    let sell_resp: serde_json::Value = client.get(&sell_url)
        .header("x-api-key", api_key).send()?.json()?;
    if sell_resp.get("error").is_some() { return Ok(false); }

    let sol_back: u64 = sell_resp.get("outAmount")
        .and_then(|v| v.as_str()).unwrap_or("0").parse().unwrap_or(0);

    let raw_profit = sol_back as i64 - amount_lamports as i64;
    let net_profit = raw_profit - JITO_TIP_LAMPORTS;

    if net_profit <= 0 { return Ok(false); }

    let buy_dex = buy_resp.get("routePlan").and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.get("swapInfo")).and_then(|s| s.get("label"))
        .and_then(|l| l.as_str()).unwrap_or("?");
    let sell_dex = sell_resp.get("routePlan").and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.get("swapInfo")).and_then(|s| s.get("label"))
        .and_then(|l| l.as_str()).unwrap_or("?");

    stats.jup_opportunities += 1;

    if net_profit < min_profit {
        if raw_profit > 0 {
            let bps = (raw_profit as f64 / amount_lamports as f64) * 10_000.0;
            println!("   [Jup] {} {:.1}bps ({} lam) buy@{} sell@{} @ {} SOL",
                token_name, bps, raw_profit, buy_dex, sell_dex, amount_lamports as f64 / 1e9);
        }
        return Ok(false);
    }

    let bps = (raw_profit as f64 / amount_lamports as f64) * 10_000.0;
    println!("✅ [Jup] {} +{} net ({:.1}bps) buy@{} sell@{} @ {} SOL",
        token_name, net_profit, bps, buy_dex, sell_dex, amount_lamports as f64 / 1e9);

    // FAST: get both swap TXs in parallel using threads
    let user_pk = kp.pubkey().to_string();
    let buy_quote = buy_resp.clone();
    let sell_quote = sell_resp.clone();
    let api_key_buy = api_key.to_string();
    let api_key_sell = api_key.to_string();
    let user_pk_buy = user_pk.clone();
    let user_pk_sell = user_pk.clone();

    let buy_handle = std::thread::spawn(move || {
        let c = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(4)).build().ok()?;
        get_jupiter_swap_tx_static(&c, &api_key_buy, &buy_quote, &user_pk_buy).ok()
    });
    let sell_handle = std::thread::spawn(move || {
        let c = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(4)).build().ok()?;
        get_jupiter_swap_tx_static(&c, &api_key_sell, &sell_quote, &user_pk_sell).ok()
    });

    let buy_swap = buy_handle.join().ok().flatten().ok_or(anyhow!("buy swap TX failed"))?;
    let sell_swap = sell_handle.join().ok().flatten().ok_or(anyhow!("sell swap TX failed"))?;

    // Extract IXs from both TXs
    let (buy_ixs, mut buy_alts) = extract_ixs_and_alts(&buy_swap, &kp.pubkey())?;
    let (sell_ixs, sell_alts) = extract_ixs_and_alts(&sell_swap, &kp.pubkey())?;

    let mut swap_ixs = buy_ixs;
    swap_ixs.extend(sell_ixs);
    buy_alts.extend(sell_alts);
    buy_alts.sort_by_key(|a| a.key);
    buy_alts.dedup_by_key(|a| a.key);

    if swap_ixs.is_empty() { return Err(anyhow!("No swap IXs from Jupiter")); }

    println!("   {} swap IXs, {} ALTs", swap_ixs.len(), buy_alts.len());

    // Wrap with MarginFi flash loan + send
    match try_marginfi_flash_with_ixs(client, kp, swap_ixs, buy_alts, amount_lamports, stats) {
        Ok(true) => return Ok(true),
        Ok(false) => println!("   MFI failed for Jupiter arb"),
        Err(e) => println!("   MFI error: {}", e),
    }
    Ok(false)
}

fn get_jupiter_swap_tx(client: &reqwest::blocking::Client, api_key: &str,
    quote: &serde_json::Value, user_pubkey: &str) -> Result<String> {
    get_jupiter_swap_tx_static(client, api_key, quote, user_pubkey)
}

fn get_jupiter_swap_tx_static(client: &reqwest::blocking::Client, api_key: &str,
    quote: &serde_json::Value, user_pubkey: &str) -> Result<String> {
    let resp: serde_json::Value = client.post(JUPITER_SWAP)
        .header("Content-Type", "application/json")
        .header("x-api-key", api_key)
        .json(&serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": user_pubkey,
            "wrapAndUnwrapSol": true,
            "useSharedAccounts": true,
        }))
        .send()?.json()?;
    resp.get("swapTransaction")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no swapTransaction: {}", resp))
}

// ============================================================
// MarginFi flash loan wrapper (from Carbium TX)
// ============================================================
fn try_marginfi_flash(client: &reqwest::blocking::Client, kp: &Keypair,
    txn_b64: &str, borrow_amount: u64, stats: &mut Stats) -> Result<bool> {

    let (swap_ixs, alts) = extract_ixs_and_alts(txn_b64, &kp.pubkey())?;
    if swap_ixs.is_empty() { return Err(anyhow!("No swap IXs")); }

    try_marginfi_flash_with_ixs(client, kp, swap_ixs, alts, borrow_amount, stats)
}

// ============================================================
// MarginFi flash loan wrapper (from any swap IXs)
// Flow: [compute, compute, create_ata, start_flashloan, borrow, swaps..., repay, end_flashloan]
// ============================================================
fn try_marginfi_flash_with_ixs(client: &reqwest::blocking::Client, kp: &Keypair,
    swap_ixs: Vec<Instruction>, mut alts: Vec<AddressLookupTableAccount>,
    borrow_amount: u64, stats: &mut Stats) -> Result<bool> {

    let mfi_prog: Pubkey = MARGINFI_PROGRAM.parse()?;
    let mfi_group: Pubkey = MARGINFI_GROUP.parse()?;
    let mfi_account: Pubkey = MARGINFI_ACCOUNT.parse()?;
    let sol_bank: Pubkey = MARGINFI_SOL_BANK.parse()?;
    let sol_vault: Pubkey = MARGINFI_SOL_VAULT.parse()?;
    let sol_oracle: Pubkey = MARGINFI_SOL_ORACLE.parse()?;
    let tok_prog: Pubkey = TOKEN_PROGRAM.parse()?;
    let ata_prog: Pubkey = ATA_PROGRAM.parse()?;
    let wsol: Pubkey = WSOL_MINT.parse()?;

    let user_ata = Pubkey::find_program_address(
        &[kp.pubkey().as_ref(), tok_prog.as_ref(), wsol.as_ref()], &ata_prog).0;
    let (vault_auth, _) = Pubkey::find_program_address(
        &[b"liquidity_vault_auth", sol_bank.as_ref()], &mfi_prog);

    let create_ata = Instruction {
        program_id: ata_prog,
        accounts: vec![
            AccountMeta::new(kp.pubkey(), true),
            AccountMeta::new(user_ata, false),
            AccountMeta::new_readonly(kp.pubkey(), false),
            AccountMeta::new_readonly(wsol, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(tok_prog, false),
        ],
        data: vec![1],
    };

    let n_swap = swap_ixs.len();
    // Layout: [0:CU_limit, 1:CU_price, 2:create_ata, 3:start_flash, 4:borrow, 5..4+n:swaps, 5+n:repay, 6+n:end_flash]
    let end_flash_index = (3 + 1 + 1 + n_swap + 1) as u64; // 3(preceding) + start + borrow + swaps + repay = index of end

    let start_ix = Instruction {
        program_id: mfi_prog,
        accounts: vec![
            AccountMeta::new(mfi_account, false),
            AccountMeta::new_readonly(kp.pubkey(), true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
        ],
        data: marginfi_start_flashloan_data(end_flash_index),
    };

    let borrow_ix = Instruction {
        program_id: mfi_prog,
        accounts: vec![
            AccountMeta::new_readonly(mfi_group, false),
            AccountMeta::new(mfi_account, false),
            AccountMeta::new_readonly(kp.pubkey(), true),
            AccountMeta::new(sol_bank, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new_readonly(vault_auth, false),
            AccountMeta::new(sol_vault, false),
            AccountMeta::new_readonly(tok_prog, false),
        ],
        data: marginfi_borrow_data(borrow_amount),
    };

    let repay_ix = Instruction {
        program_id: mfi_prog,
        accounts: vec![
            AccountMeta::new_readonly(mfi_group, false),
            AccountMeta::new(mfi_account, false),
            AccountMeta::new_readonly(kp.pubkey(), true),
            AccountMeta::new(sol_bank, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(sol_vault, false),
            AccountMeta::new_readonly(tok_prog, false),
        ],
        data: marginfi_repay_data(borrow_amount),
    };

    let end_ix = Instruction {
        program_id: mfi_prog,
        accounts: vec![
            AccountMeta::new(mfi_account, false),
            AccountMeta::new_readonly(kp.pubkey(), true),
            AccountMeta::new(sol_bank, false),
            AccountMeta::new_readonly(sol_oracle, false),
        ],
        data: marginfi_end_flashloan_data(),
    };

    let mut instructions = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(1_000),
        create_ata,
        start_ix,
        borrow_ix,
    ];
    instructions.extend(swap_ixs);
    instructions.push(repay_ix);
    instructions.push(end_ix);

    // Add Jito tip (required for bundle inclusion, ~1000 lamports min)
    let jito_tip_accounts = [
        "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
        "HFqU5x63VTqvQss8hp11i4bPDSiRrGSAerMh89a1ie95",
        "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
        "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
        "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
        "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
        "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
        "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
    ];
    // Pick random tip account
    let tip_idx = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis() as usize) % jito_tip_accounts.len();
    let tip_account: Pubkey = jito_tip_accounts[tip_idx].parse().unwrap();
    let tip_lamports = 10_000u64; // p75 Jito tip floor
    let tip_ix = solana_sdk::system_instruction::transfer(&kp.pubkey(), &tip_account, tip_lamports);
    instructions.push(tip_ix);

    // Add MarginFi ALT
    let mfi_alt_key: Pubkey = MARGINFI_ALT.parse()?;
    if let Ok(addrs) = resolve_alt(&mfi_alt_key) {
        alts.push(AddressLookupTableAccount { key: mfi_alt_key, addresses: addrs });
    }

    build_sim_send(client, kp, instructions, alts, stats, "MFI")
}

// ============================================================
// Shared: build TX, simulate, send
// ============================================================
fn build_sim_send(client: &reqwest::blocking::Client, kp: &Keypair,
    instructions: Vec<Instruction>, alts: Vec<AddressLookupTableAccount>,
    stats: &mut Stats, label: &str) -> Result<bool> {

    let blockhash = get_blockhash(client)?;
    let message = v0::Message::try_compile(&kp.pubkey(), &instructions, &alts, blockhash)
        .map_err(|e| anyhow!("compile: {}", e))?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[&kp])
        .map_err(|e| anyhow!("sign: {}", e))?;
    let tx_bytes = bincode::serialize(&tx)?;

    println!("   {} TX: {} bytes ({} IXs)", label, tx_bytes.len(), instructions.len());
    if tx_bytes.len() > 1232 {
        println!("   {} TX too large ({}B), skipping", label, tx_bytes.len());
        stats.too_big += 1;
        return Ok(false);
    }

    let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

    // Skip simulation for flash loans — atomic via Jito (fail = 0 cost)
    // Simulation adds ~100ms latency which costs us the arb

    let sent = send_to_jito(client, &tx_b64);
    if sent {
        stats.sent += 1;
        if label == "MFI" { stats.mfi_sent += 1; } else { stats.direct_sent += 1; }
        return Ok(true);
    }
    Ok(false)
}

fn send_carbium_direct(client: &reqwest::blocking::Client, txn_b64: &str, kp: &Keypair,
    stats: &mut Stats) -> Result<bool> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(txn_b64)?;
    let mut tx: VersionedTransaction = bincode::deserialize(&bytes)?;
    tx.signatures[0] = kp.sign_message(&tx.message.serialize());
    let signed = base64::engine::general_purpose::STANDARD.encode(&bincode::serialize(&tx)?);

    match simulate_tx(client, &signed) {
        Ok(Some(err)) => { stats.sim_fail += 1; println!("   Direct sim FAILED: {}", err); return Ok(false); }
        _ => {}
    }

    let sent = send_to_jito(client, &signed);
    if sent { stats.sent += 1; stats.direct_sent += 1; return Ok(true); }
    Ok(false)
}

// ============================================================
// MarginFi: withdraw existing deposit (fixes BorrowOnly error)
// ============================================================
fn withdraw_marginfi_deposit(client: &reqwest::blocking::Client, kp: &Keypair) {
    println!("   Checking MarginFi deposit balance...");
    let mfi_prog: Pubkey = MARGINFI_PROGRAM.parse().unwrap();
    let mfi_group: Pubkey = MARGINFI_GROUP.parse().unwrap();
    let mfi_account: Pubkey = MARGINFI_ACCOUNT.parse().unwrap();
    let sol_bank: Pubkey = MARGINFI_SOL_BANK.parse().unwrap();
    let sol_vault: Pubkey = MARGINFI_SOL_VAULT.parse().unwrap();
    let tok_prog: Pubkey = TOKEN_PROGRAM.parse().unwrap();
    let ata_prog: Pubkey = ATA_PROGRAM.parse().unwrap();
    let wsol: Pubkey = WSOL_MINT.parse().unwrap();
    let user_ata = Pubkey::find_program_address(
        &[kp.pubkey().as_ref(), tok_prog.as_ref(), wsol.as_ref()], &ata_prog).0;
    let (vault_auth, _) = Pubkey::find_program_address(
        &[b"liquidity_vault_auth", sol_bank.as_ref()], &mfi_prog);

    // Create ATA first (idempotent)
    let create_ata = Instruction {
        program_id: ata_prog,
        accounts: vec![
            AccountMeta::new(kp.pubkey(), true),
            AccountMeta::new(user_ata, false),
            AccountMeta::new_readonly(kp.pubkey(), false),
            AccountMeta::new_readonly(wsol, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(tok_prog, false),
        ],
        data: vec![1],
    };

    // lending_account_withdraw(amount=0, withdraw_all=Some(true))
    // disc(8) + amount(8) + Option<bool>: Some(true) = [1, 1] (2 bytes)
    let mut withdraw_data = vec![36, 72, 74, 19, 210, 210, 192, 192]; // sha256("global:lending_account_withdraw")[..8]
    withdraw_data.extend_from_slice(&0u64.to_le_bytes()); // amount=0 (withdraw_all handles it)
    withdraw_data.push(1u8); // Option tag: Some
    withdraw_data.push(1u8); // bool value: true

    let sol_oracle: Pubkey = MARGINFI_SOL_ORACLE.parse().unwrap();

    // Withdraw accounts: group, marginfi_account, signer, bank, user_ata, vault_auth, vault, token_program
    // + remaining_accounts for health: [bank, oracle] pairs
    let withdraw_ix = Instruction {
        program_id: mfi_prog,
        accounts: vec![
            AccountMeta::new(mfi_group, false),  // must be mut
            AccountMeta::new(mfi_account, false),
            AccountMeta::new_readonly(kp.pubkey(), true),
            AccountMeta::new(sol_bank, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new_readonly(vault_auth, false),
            AccountMeta::new(sol_vault, false),
            AccountMeta::new_readonly(tok_prog, false),
            // Health check remaining accounts
            AccountMeta::new(sol_bank, false),
            AccountMeta::new_readonly(sol_oracle, false),
        ],
        data: withdraw_data,
    };

    // Close WSOL ATA to recover SOL (unwrap WSOL → SOL)
    let close_ata = Instruction {
        program_id: tok_prog,
        accounts: vec![
            AccountMeta::new(user_ata, false),
            AccountMeta::new(kp.pubkey(), false),
            AccountMeta::new_readonly(kp.pubkey(), true),
        ],
        data: vec![9], // CloseAccount
    };

    let instructions = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(400_000),
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(1_000),
        create_ata,
        withdraw_ix,
        close_ata,
    ];

    match get_blockhash(client) {
        Ok(blockhash) => {
            let msg = solana_sdk::message::Message::new_with_blockhash(
                &instructions, Some(&kp.pubkey()), &blockhash);
            let tx = solana_sdk::transaction::Transaction::new(&[kp], msg, blockhash);
            let tx_bytes = bincode::serialize(&tx).unwrap();
            let tx_b64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

            // Simulate first
            match simulate_tx(client, &tx_b64) {
                Ok(None) => {
                    println!("   Withdraw sim OK, sending...");
                    if send_to_jito(client, &tx_b64) {
                        println!("   ✅ MarginFi deposit withdrawn — flash loans enabled");
                        std::thread::sleep(std::time::Duration::from_secs(3)); // Wait for confirmation
                    }
                }
                Ok(Some(err)) => {
                    if err.contains("AccountNotInitialized") || err.contains("no balance") {
                        println!("   No deposit found — flash loans should work");
                    } else {
                        println!("   Withdraw sim: {} (may be OK if no deposit)", err);
                    }
                }
                Err(e) => println!("   Withdraw sim error: {}", e),
            }
        }
        Err(e) => println!("   Withdraw blockhash error: {}", e),
    }
}

// ============================================================
// MarginFi instruction data
// ============================================================
fn marginfi_start_flashloan_data(end_index: u64) -> Vec<u8> {
    let mut data = vec![14, 131, 33, 220, 81, 186, 180, 107];
    data.extend_from_slice(&end_index.to_le_bytes());
    data
}

fn marginfi_borrow_data(amount: u64) -> Vec<u8> {
    let mut data = vec![4, 126, 116, 53, 48, 5, 212, 31];
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn marginfi_repay_data(amount: u64) -> Vec<u8> {
    let mut data = vec![79, 209, 172, 177, 222, 51, 173, 151];
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn marginfi_end_flashloan_data() -> Vec<u8> {
    vec![105, 124, 201, 106, 153, 2, 8, 156]
}

// ============================================================
// Helpers
// ============================================================
fn get_blockhash(client: &reqwest::blocking::Client) -> Result<Hash> {
    // RPC blockhash (shred-derived entry hashes are NOT valid as recent_blockhash —
    // only the last tick of a completed slot is valid, which is what RPC returns)
    let rpcs = ["https://solana-rpc.publicnode.com", CARBIUM_RPC];
    for rpc in &rpcs {
        if let Ok(resp) = client.post(*rpc)
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash",
                "params":[{"commitment":"finalized"}]}))
            .send() {
            if let Ok(bh) = resp.json::<serde_json::Value>() {
                if let Some(hash_str) = bh.get("result").and_then(|r| r.get("value"))
                    .and_then(|v| v.get("blockhash")).and_then(|b| b.as_str()) {
                    if let Ok(hash) = hash_str.parse() {
                        return Ok(hash);
                    }
                }
            }
        }
    }
    Err(anyhow!("no blockhash"))
}

fn simulate_tx(client: &reqwest::blocking::Client, tx_b64: &str) -> Result<Option<String>> {
    let resp: serde_json::Value = client.post(CARBIUM_RPC)
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"simulateTransaction",
            "params":[tx_b64, {"encoding":"base64","commitment":"confirmed",
                "replaceRecentBlockhash":true}]
        }))
        .send()?.json()?;

    if let Some(result) = resp.get("result") {
        if let Some(err) = result.get("err") {
            if !err.is_null() {
                let logs = result.get("value").and_then(|v| v.get("logs"))
                    .or_else(|| result.get("logs"));
                let log_str = if let Some(logs) = logs.and_then(|l| l.as_array()) {
                    logs.iter().rev().take(3).filter_map(|l| l.as_str())
                        .collect::<Vec<_>>().join(" | ")
                } else { String::new() };
                return Ok(Some(format!("{} [{}]", err, log_str)));
            }
        }
    }
    Ok(None)
}

/// Send TX via Jito bundles ONLY (failed TX = 0 cost, atomic).
fn send_to_jito(client: &reqwest::blocking::Client, tx_b64: &str) -> bool {
    // Convert base64 → raw bytes → base58 (Jito requires base58)
    let tx_bytes = match base64::engine::general_purpose::STANDARD.decode(tx_b64) {
        Ok(b) => b,
        Err(_) => { println!("   Jito: base64 decode failed"); return false; }
    };
    let tx_b58 = bs58::encode(&tx_bytes).into_string();

    // Primary: Jito sendTransaction (no vote account restriction, still prioritized)
    let jito_tx_endpoints = [
        "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/transactions",
        "https://mainnet.block-engine.jito.wtf/api/v1/transactions",
    ];

    // Secondary: Jito bundles (atomic but has vote account restriction)
    let jito_endpoints = [
        "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles",
        "https://mainnet.block-engine.jito.wtf/api/v1/bundles",
    ];

    let bundle_body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendBundle",
        "params":[[tx_b58]]
    });

    // Also prepare sendTransaction via Jito (different endpoint, no tip required but faster)
    let send_tx_body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[tx_b64, {"encoding":"base64","skipPreflight":true}]
    });

    let mut sent = false;

    // JITO BUNDLES ONLY — atomic: failed TX = 0 cost (no fee, no tip charged)
    // sendTransaction is BANNED — failed TXs still charge fee+tip on-chain
    for endpoint in &jito_endpoints {
        match client.post(*endpoint).json(&bundle_body).send() {
            Ok(r) => {
                let status = r.status();
                match r.text() {
                    Ok(text) => {
                        if let Ok(resp) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(bundle_id) = resp.get("result").and_then(|v| v.as_str()) {
                                if !sent {
                                    println!("🎉 JITO BUNDLE: {} ({})",
                                        bundle_id, endpoint.split('/').nth(2).unwrap_or("?"));
                                    sent = true;
                                }
                            } else if !sent {
                                let region = endpoint.split('/').nth(2).unwrap_or("?");
                                println!("   Jito {} [{}]: {}", region, status,
                                    text.chars().take(120).collect::<String>());
                            }
                        } else if !sent {
                            println!("   Jito [{}]: {}", status, text.chars().take(120).collect::<String>());
                        }
                    }
                    Err(e) => println!("   Jito read error: {}", e),
                }
            }
            Err(e) => {
                if !sent { println!("   Jito conn error: {}", e); }
                continue;
            }
        }
        if sent { break; }
        std::thread::sleep(std::time::Duration::from_millis(1100)); // Jito rate limit: 1 req/s
    }

    sent
}

fn extract_ixs_and_alts(tx_b64: &str, user_pubkey: &Pubkey) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(tx_b64)?;
    let tx: VersionedTransaction = bincode::deserialize(&bytes)?;

    let static_keys = match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.clone(),
        VersionedMessage::V0(m) => m.account_keys.clone(),
    };
    let n_static = static_keys.len();
    let mut all_keys = static_keys.clone();
    let mut alts = Vec::new();

    if let VersionedMessage::V0(m) = &tx.message {
        for alt_ref in &m.address_table_lookups {
            if let Ok(addrs) = resolve_alt(&alt_ref.account_key) {
                for &idx in &alt_ref.writable_indexes {
                    if (idx as usize) < addrs.len() { all_keys.push(addrs[idx as usize]); }
                }
                for &idx in &alt_ref.readonly_indexes {
                    if (idx as usize) < addrs.len() { all_keys.push(addrs[idx as usize]); }
                }
                alts.push(AddressLookupTableAccount { key: alt_ref.account_key, addresses: addrs });
            }
        }
    }

    let compiled = match &tx.message {
        VersionedMessage::Legacy(m) => &m.instructions,
        VersionedMessage::V0(m) => &m.instructions,
    };

    let compute: Pubkey = "ComputeBudget111111111111111111111111111111".parse().unwrap();
    let ata: Pubkey = ATA_PROGRAM.parse().unwrap();
    let sys = solana_sdk::system_program::id();
    let tok: Pubkey = TOKEN_PROGRAM.parse().unwrap();

    let mut ixs = Vec::new();
    for cix in compiled {
        if (cix.program_id_index as usize) >= all_keys.len() { continue; }
        let pid = all_keys[cix.program_id_index as usize];
        if pid == compute || pid == ata { continue; }
        if pid == sys { continue; }
        if pid == tok && cix.data.len() == 1 && cix.data[0] == 9 { continue; }
        if pid == tok && cix.data.len() == 1 && cix.data[0] == 17 { continue; }
        if cix.accounts.iter().any(|&i| (i as usize) >= all_keys.len()) { continue; }

        let accounts: Vec<AccountMeta> = cix.accounts.iter().map(|&idx| {
            let pk = all_keys[idx as usize];
            let is_user = pk == *user_pubkey;
            let signer = is_user || (if (idx as usize) < n_static { tx.message.is_signer(idx as usize) } else { false });
            let writable = if (idx as usize) < n_static { tx.message.is_maybe_writable(idx as usize) } else { true };
            if writable { AccountMeta::new(pk, signer) } else { AccountMeta::new_readonly(pk, signer) }
        }).collect();

        ixs.push(Instruction { program_id: pid, accounts, data: cix.data.clone() });
    }
    Ok((ixs, alts))
}

use std::sync::Mutex;
use std::collections::HashMap;

static ALT_CACHE: std::sync::LazyLock<Mutex<HashMap<Pubkey, Vec<Pubkey>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn resolve_alt(addr: &Pubkey) -> Result<Vec<Pubkey>> {
    // Check cache first
    if let Ok(cache) = ALT_CACHE.lock() {
        if let Some(addrs) = cache.get(addr) {
            return Ok(addrs.clone());
        }
    }

    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(3)).build()?;
    let resp: serde_json::Value = client.post(CARBIUM_RPC)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getAccountInfo","params":[addr.to_string(),{"encoding":"base64"}]}))
        .send()?.json()?;
    let b64 = resp.get("result").and_then(|r| r.get("value")).and_then(|v| v.get("data"))
        .and_then(|d| d.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str()).unwrap_or("");
    let data = base64::engine::general_purpose::STANDARD.decode(b64)?;
    if data.len() < 56 { return Ok(vec![]); }
    let addrs: Vec<Pubkey> = data[56..].chunks_exact(32).filter_map(|c| Pubkey::try_from(c).ok()).collect();

    // Cache it
    if let Ok(mut cache) = ALT_CACHE.lock() {
        cache.insert(*addr, addrs.clone());
    }
    Ok(addrs)
}
