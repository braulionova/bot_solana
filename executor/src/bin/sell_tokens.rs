//! sell_tokens.rs — Sell all PumpSwap tokens back to SOL.
//!
//! Reads pool state on-chain, constructs BUY instruction (buy WSOL with token),
//! and sends via sim-server + Agave.

use std::str::FromStr;

use anyhow::{Context, Result};
use base64::Engine;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::v0::Message as MessageV0,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};

const PUMPSWAP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ASSOC_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
const PROTOCOL_FEE_RECIPIENT: &str = "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV";
const FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";

/// PumpSwap buy discriminator (buy base_mint, pay quote_mint).
const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// PumpSwap sell discriminator (sell base_mint, receive quote_mint).
const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

#[derive(Debug)]
struct TokenHolding {
    mint: Pubkey,
    amount_raw: u64,
    token_program: Pubkey,
    pool: Pubkey,
}

#[derive(Debug)]
struct PoolState {
    base_mint: Pubkey,
    quote_mint: Pubkey,
    pool_base_vault: Pubkey,
    pool_quote_vault: Pubkey,
    coin_creator: Pubkey,
}

fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let assoc: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &assoc,
    ).0
}

#[tokio::main]
async fn main() -> Result<()> {
    let wallet_path = std::env::var("WALLET_PATH")
        .unwrap_or_else(|_| "/root/solana-bot/wallet.json".into());
    let rpc_url = std::env::var("SIM_RPC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".into());
    let only_mint = std::env::var("ONLY_MINT")
        .ok()
        .map(|s| Pubkey::from_str(&s))
        .transpose()
        .context("parse ONLY_MINT")?;
    let only_pool = std::env::var("ONLY_POOL")
        .ok()
        .map(|s| Pubkey::from_str(&s))
        .transpose()
        .context("parse ONLY_POOL")?;
    let skip_preflight = std::env::var("SKIP_PREFLIGHT")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);

    let payer = read_keypair_file(&wallet_path)
        .map_err(|e| anyhow::anyhow!("read keypair: {e}"))?;
    let payer_pk = payer.pubkey();

    println!("Wallet: {payer_pk}");
    println!("RPC: {rpc_url}");
    if let Some(mint) = only_mint {
        println!("ONLY_MINT: {mint}");
    }
    if let Some(pool) = only_pool {
        println!("ONLY_POOL: {pool}");
    }
    println!("SKIP_PREFLIGHT: {skip_preflight}");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // 1. Discover all token holdings
    let holdings = if let (Some(mint), Some(pool)) = (only_mint, only_pool) {
        discover_holding_by_mint(&client, &rpc_url, &payer_pk, mint, pool).await?
    } else {
        discover_holdings(&client, &rpc_url, &payer_pk, only_mint).await?
    };
    println!("\nFound {} tokens to sell", holdings.len());

    if holdings.is_empty() {
        println!("Nothing to sell.");
        return Ok(());
    }

    // 2. Get blockhash
    let blockhash = get_blockhash(&client, &rpc_url).await?;
    println!("Blockhash: {blockhash}");

    // 3. Sell each token
    let mut sold = 0;
    let mut failed = 0;
    for holding in &holdings {
        println!("\n--- Selling {} of {} ---", holding.amount_raw, holding.mint);
        match sell_token(&client, &rpc_url, &payer, holding, &blockhash, skip_preflight).await {
            Ok(sig) => {
                println!("  TX sent: {sig}");
                sold += 1;
                // Small delay to avoid rate limits
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                eprintln!("  FAILED: {e}");
                failed += 1;
            }
        }
    }

    println!("\n=== Summary ===");
    println!("Sold: {sold}");
    println!("Failed: {failed}");

    // 4. Close empty token accounts to reclaim rent
    println!("\nWaiting 5s for TXs to confirm...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Re-check and close empty accounts
    let holdings_after = if let (Some(mint), Some(pool)) = (only_mint, only_pool) {
        discover_holding_by_mint(&client, &rpc_url, &payer_pk, mint, pool).await?
    } else {
        discover_holdings(&client, &rpc_url, &payer_pk, only_mint).await?
    };
    println!("Tokens remaining: {}", holdings_after.len());

    Ok(())
}

/// Discover holdings from TX history (bypasses broken getTokenAccountsByOwner).
/// For each successful PumpSwap TX, extracts the token mint, ATA, pool, and current balance.
async fn discover_holdings(
    client: &reqwest::Client,
    rpc_url: &str,
    owner: &Pubkey,
    only_mint: Option<Pubkey>,
) -> Result<Vec<TokenHolding>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getSignaturesForAddress",
        "params": [owner.to_string(), {"limit": 200}]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;
    let sigs = json["result"].as_array().cloned().unwrap_or_default();
    let ok_sigs: Vec<&str> = sigs.iter()
        .filter(|s| s["err"].is_null())
        .filter_map(|s| s["signature"].as_str())
        .collect();

    println!("  Scanning {} successful TXs...", ok_sigs.len());

    let mut seen_mints = std::collections::HashSet::new();
    let mut holdings = Vec::new();
    let owner_str = owner.to_string();

    for sig in ok_sigs {
        let body2 = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTransaction",
            "params": [sig, {"encoding": "json", "maxSupportedTransactionVersion": 0}]
        });
        let resp2 = client.post(rpc_url).json(&body2).send().await?;
        let json2: serde_json::Value = resp2.json().await?;
        let result = &json2["result"];
        if result.is_null() { continue; }

        let keys: Vec<String> = result["transaction"]["message"]["accountKeys"]
            .as_array()
            .map(|arr| arr.iter().map(|k| {
                if let Some(s) = k.as_str() { s.to_string() }
                else if let Some(s) = k["pubkey"].as_str() { s.to_string() }
                else { String::new() }
            }).collect())
            .unwrap_or_default();

        // Find PumpSwap pool from this TX
        let empty_vec = vec![];
        let mut pool = Pubkey::default();
        for inst in result["transaction"]["message"]["instructions"].as_array().unwrap_or(&empty_vec) {
            let prog_idx = inst["programIdIndex"].as_u64().unwrap_or(999) as usize;
            if prog_idx < keys.len() && keys[prog_idx] == PUMPSWAP_PROGRAM {
                let empty_arr = vec![];
                let accs = inst["accounts"].as_array().unwrap_or(&empty_arr);
                if !accs.is_empty() {
                    let pool_idx = accs[0].as_u64().unwrap_or(999) as usize;
                    if pool_idx < keys.len() {
                        pool = keys[pool_idx].parse().unwrap_or_default();
                    }
                }
            }
        }
        if pool == Pubkey::default() { continue; }

        // Find token balance from postTokenBalances
        let empty_arr2 = vec![];
        for t in result["meta"]["postTokenBalances"].as_array().unwrap_or(&empty_arr2) {
            let t_owner = t["owner"].as_str().unwrap_or("");
            let mint_str = t["mint"].as_str().unwrap_or("");
            if t_owner != owner_str || mint_str == WSOL_MINT { continue; }
            if seen_mints.contains(mint_str) { continue; }

            let idx = t["accountIndex"].as_u64().unwrap_or(999) as usize;
            if idx >= keys.len() { continue; }
            let ata_key = &keys[idx];

            // Check current balance of this ATA directly
            let body3 = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "getAccountInfo",
                "params": [ata_key, {"encoding": "jsonParsed"}]
            });
            let resp3 = client.post(rpc_url).json(&body3).send().await?;
            let json3: serde_json::Value = resp3.json().await?;
            let val = &json3["result"]["value"];
            if val.is_null() { continue; }

            let amount_raw: u64 = val["data"]["parsed"]["info"]["tokenAmount"]["amount"]
                .as_str().unwrap_or("0").parse().unwrap_or(0);
            if amount_raw == 0 { continue; }

            let tp_str = val["owner"].as_str().unwrap_or(TOKEN_PROGRAM);
            let tp: Pubkey = tp_str.parse().unwrap_or_else(|_| TOKEN_PROGRAM.parse().unwrap());
            let mint: Pubkey = mint_str.parse().unwrap_or_default();
            if only_mint.is_some_and(|target| target != mint) { continue; }

            seen_mints.insert(mint_str.to_string());
            println!("  Found: {mint_str} = {amount_raw} (pool={pool})");

            holdings.push(TokenHolding {
                mint,
                amount_raw,
                token_program: tp,
                pool,
            });
        }
    }

    Ok(holdings)
}

async fn discover_holding_by_mint(
    client: &reqwest::Client,
    rpc_url: &str,
    owner: &Pubkey,
    mint: Pubkey,
    pool: Pubkey,
) -> Result<Vec<TokenHolding>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getTokenAccountsByOwner",
        "params": [
            owner.to_string(),
            {"mint": mint.to_string()},
            {"encoding": "jsonParsed"}
        ]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;
    let accounts = json["result"]["value"].as_array().cloned().unwrap_or_default();

    let mut amount_raw = 0u64;
    let mut token_program: Option<Pubkey> = None;
    for acc in accounts {
        let val = &acc["account"];
        let token_amount = val["data"]["parsed"]["info"]["tokenAmount"]["amount"]
            .as_str().unwrap_or("0").parse::<u64>().unwrap_or(0);
        amount_raw = amount_raw.saturating_add(token_amount);

        if token_program.is_none() {
            token_program = val["owner"]
                .as_str()
                .and_then(|s| s.parse::<Pubkey>().ok());
        }
    }

    if amount_raw == 0 {
        return Ok(Vec::new());
    }

    let token_program = token_program.unwrap_or_else(|| TOKEN_PROGRAM.parse().unwrap());
    println!("  Direct holding: {mint} = {amount_raw} (pool={pool})");

    Ok(vec![TokenHolding {
        mint,
        amount_raw,
        token_program,
        pool,
    }])
}

async fn get_blockhash(client: &reqwest::Client, rpc_url: &str) -> Result<solana_sdk::hash::Hash> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getLatestBlockhash",
        "params": [{"commitment": "confirmed"}]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;
    let bh_str = json["result"]["value"]["blockhash"].as_str()
        .context("no blockhash")?;
    bh_str.parse().context("parse blockhash")
}

async fn read_pool_state(client: &reqwest::Client, rpc_url: &str, pool: &Pubkey) -> Result<PoolState> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getAccountInfo",
        "params": [pool.to_string(), {"encoding": "base64"}]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;

    let data_b64 = json["result"]["value"]["data"][0].as_str()
        .context("no pool data")?;
    let data = base64::engine::general_purpose::STANDARD.decode(data_b64)?;

    if data.len() < 243 {
        anyhow::bail!("pool data too short: {}", data.len());
    }

    Ok(PoolState {
        base_mint: Pubkey::from(<[u8; 32]>::try_from(&data[43..75])?),
        quote_mint: Pubkey::from(<[u8; 32]>::try_from(&data[75..107])?),
        pool_base_vault: Pubkey::from(<[u8; 32]>::try_from(&data[139..171])?),
        pool_quote_vault: Pubkey::from(<[u8; 32]>::try_from(&data[171..203])?),
        coin_creator: Pubkey::from(<[u8; 32]>::try_from(&data[211..243])?),
    })
}

async fn sell_token(
    client: &reqwest::Client,
    rpc_url: &str,
    payer: &Keypair,
    holding: &TokenHolding,
    blockhash: &solana_sdk::hash::Hash,
    skip_preflight: bool,
) -> Result<String> {
    let pumpswap: Pubkey = PUMPSWAP_PROGRAM.parse().unwrap();
    let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
    let spl_tp: Pubkey = TOKEN_PROGRAM.parse().unwrap();
    let assoc: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
    let system: Pubkey = SYSTEM_PROGRAM.parse().unwrap();
    let fee_recipient: Pubkey = PROTOCOL_FEE_RECIPIENT.parse().unwrap();
    let fee_program: Pubkey = FEE_PROGRAM.parse().unwrap();
    let payer_pk = payer.pubkey();

    // Read pool state
    let ps = read_pool_state(client, rpc_url, &holding.pool).await?;
    let wsol_is_base = ps.base_mint == wsol_mint;

    let (base_tp, quote_tp) = if wsol_is_base {
        (spl_tp, holding.token_program)
    } else {
        (holding.token_program, spl_tp)
    };

    // Derive addresses
    let global_config = Pubkey::find_program_address(&[b"global_config"], &pumpswap).0;
    let event_authority = Pubkey::find_program_address(&[b"__event_authority"], &pumpswap).0;
    let global_volume_acc = Pubkey::find_program_address(&[b"global_volume_accumulator"], &pumpswap).0;
    let user_volume_acc = Pubkey::find_program_address(&[b"user_volume_accumulator", payer_pk.as_ref()], &pumpswap).0;
    let fee_config = Pubkey::find_program_address(&[b"fee_config", pumpswap.as_ref()], &fee_program).0;

    let creator_vault_authority = Pubkey::find_program_address(
        &[b"creator_vault", ps.coin_creator.as_ref()], &pumpswap,
    ).0;
    let creator_vault_ata = ata(&creator_vault_authority, &ps.quote_mint, &quote_tp);

    let user_base = ata(&payer_pk, &ps.base_mint, &base_tp);
    let user_quote = ata(&payer_pk, &ps.quote_mint, &quote_tp);
    let fee_recipient_quote_ata = ata(&fee_recipient, &ps.quote_mint, &quote_tp);

    // To SELL tokens for SOL:
    // If WSOL is base → BUY instruction (buy WSOL base, pay token quote)
    //   data: [buy_disc][base_amount_out (WSOL)][max_quote_amount_in (tokens)]
    // If WSOL is quote → SELL instruction (sell token base, receive WSOL quote)
    //   data: [sell_disc][base_amount_in (tokens)][min_quote_amount_out (WSOL)]
    let token_amount = holding.amount_raw;

    // Estimate SOL output using vault balances
    let sol_out = estimate_sol_output(client, rpc_url, &ps, token_amount, wsol_is_base).await;
    // Skip worthless tokens (not worth the TX fee)
    if sol_out < 5000 {
        anyhow::bail!("est SOL out too low ({sol_out} lamports), skipping");
    }
    let min_sol_out = sol_out * 98 / 100; // 2% slippage — consume nearly all tokens

    println!("  Pool: {} wsol_is_base={}", holding.pool, wsol_is_base);
    println!("  Selling {} tokens, est SOL out: {} lamports (min: {})", token_amount, sol_out, min_sol_out);

    let (data, need_volume_accs) = if wsol_is_base {
        // BUY: buy WSOL (base) with token (quote)
        let mut d = BUY_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&min_sol_out.to_le_bytes()); // base_amount_out (min WSOL)
        d.extend_from_slice(&token_amount.to_le_bytes()); // max_quote_amount_in (all our tokens)
        (d, true)
    } else {
        // SELL: sell token (base) for WSOL (quote)
        let mut d = SELL_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&token_amount.to_le_bytes()); // base_amount_in (all our tokens)
        d.extend_from_slice(&min_sol_out.to_le_bytes()); // min_quote_amount_out (min WSOL)
        (d, false)
    };

    let mut accounts = vec![
        AccountMeta::new(holding.pool, false),
        AccountMeta::new(payer_pk, true),
        AccountMeta::new_readonly(global_config, false),
        AccountMeta::new_readonly(ps.base_mint, false),
        AccountMeta::new_readonly(ps.quote_mint, false),
        AccountMeta::new(user_base, false),
        AccountMeta::new(user_quote, false),
        AccountMeta::new(ps.pool_base_vault, false),
        AccountMeta::new(ps.pool_quote_vault, false),
        AccountMeta::new_readonly(fee_recipient, false),
        AccountMeta::new(fee_recipient_quote_ata, false),
        AccountMeta::new_readonly(base_tp, false),
        AccountMeta::new_readonly(quote_tp, false),
        AccountMeta::new_readonly(system, false),
        AccountMeta::new_readonly(assoc, false),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(pumpswap, false),
        AccountMeta::new(creator_vault_ata, false),
        AccountMeta::new_readonly(creator_vault_authority, false),
    ];
    if need_volume_accs {
        accounts.push(AccountMeta::new_readonly(global_volume_acc, false));
        accounts.push(AccountMeta::new(user_volume_acc, false));
    }
    accounts.push(AccountMeta::new_readonly(fee_config, false));
    accounts.push(AccountMeta::new_readonly(fee_program, false));

    let swap_ix = Instruction {
        program_id: pumpswap,
        accounts,
        data,
    };

    // Build IX list:
    // 1. Create WSOL ATA (to receive SOL)
    let wsol_ata = if wsol_is_base { user_base } else { user_quote };
    let create_wsol_ata_ix = Instruction {
        program_id: assoc,
        accounts: vec![
            AccountMeta::new(payer_pk, true),
            AccountMeta::new(wsol_ata, false),
            AccountMeta::new_readonly(payer_pk, false),
            AccountMeta::new_readonly(wsol_mint, false),
            AccountMeta::new_readonly(system, false),
            AccountMeta::new_readonly(spl_tp, false),
        ],
        data: vec![1u8],
    };

    // 2. Swap
    // 3. Close WSOL ATA to get SOL back
    let close_wsol_ix = Instruction {
        program_id: spl_tp,
        accounts: vec![
            AccountMeta::new(wsol_ata, false),
            AccountMeta::new(payer_pk, false),
            AccountMeta::new_readonly(payer_pk, true),
        ],
        data: vec![9u8],
    };

    // 4. Close token ATA to reclaim rent
    let token_ata = if wsol_is_base { user_quote } else { user_base };
    let close_token_ix = Instruction {
        program_id: holding.token_program,
        accounts: vec![
            AccountMeta::new(token_ata, false),
            AccountMeta::new(payer_pk, false),
            AccountMeta::new_readonly(payer_pk, true),
        ],
        data: vec![9u8],
    };

    // Don't close token ATA — BUY may not consume ALL tokens (rounding),
    // and CloseAccount fails on non-zero balance (Custom error 11).
    let ixs = vec![create_wsol_ata_ix, swap_ix, close_wsol_ix];

    let msg = MessageV0::try_compile(&payer_pk, &ixs, &[], *blockhash)
        .context("compile sell message")?;
    let tx = VersionedTransaction::try_new(
        solana_sdk::message::VersionedMessage::V0(msg),
        &[payer],
    ).context("sign sell tx")?;

    // Send via sim-server
    let encoded = bincode::serialize(&tx)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&encoded);
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "sendTransaction",
        "params": [b64, {"encoding": "base64", "skipPreflight": skip_preflight, "maxRetries": 3}]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;

    if let Some(err) = json.get("error") {
        anyhow::bail!("RPC error: {}", err);
    }

    Ok(json["result"].as_str().unwrap_or("?").to_string())
}

async fn estimate_sol_output(
    client: &reqwest::Client,
    rpc_url: &str,
    ps: &PoolState,
    token_amount: u64,
    wsol_is_base: bool,
) -> u64 {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getMultipleAccounts",
        "params": [[
            ps.pool_base_vault.to_string(),
            ps.pool_quote_vault.to_string()
        ], {"encoding": "jsonParsed"}]
    });

    if let Ok(resp) = client.post(rpc_url).json(&body).send().await {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            let vals = &json["result"]["value"];
            let base_bal = extract_balance(&vals[0]).unwrap_or(0);
            let quote_bal = extract_balance(&vals[1]).unwrap_or(0);

            if base_bal > 0 && quote_bal > 0 {
                let (wsol_reserve, token_reserve) = if wsol_is_base {
                    (base_bal, quote_bal)
                } else {
                    (quote_bal, base_bal)
                };
                // XY=K: sol_out = wsol_reserve * token_in / (token_reserve + token_in)
                let sol_out = (wsol_reserve as u128) * (token_amount as u128)
                    / (token_reserve as u128 + token_amount as u128);
                return sol_out as u64;
            }
        }
    }
    0 // Unknown — use 0 as minimum
}

fn extract_balance(val: &serde_json::Value) -> Option<u64> {
    val["data"]["parsed"]["info"]["tokenAmount"]["amount"]
        .as_str()?
        .parse()
        .ok()
}
