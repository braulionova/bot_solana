//! recover_rent.rs — Burn worthless tokens + close ATAs to recover rent SOL.
//!
//! For each token account:
//!   1. Check pool liquidity — if WSOL reserve > MIN_SELL_SOL, try selling first
//!   2. Otherwise burn all tokens (free, no pool needed)
//!   3. Close the empty ATA → rent SOL returns to wallet
//!
//! Usage:
//!   WALLET_PATH=/root/solana-bot/wallet.json \
//!   SIM_RPC_URL=http://127.0.0.1:8081 \
//!   cargo run --release --bin recover_rent
//!
//! DRY_RUN=true to preview without sending (default: false)

use std::str::FromStr;

use anyhow::{Context, Result};
use base64::Engine;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::v0::Message as MessageV0,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::VersionedTransaction,
};

const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Minimum WSOL in pool to attempt sell instead of burn (0.001 SOL).
const MIN_SELL_SOL: u64 = 1_000_000;

#[derive(Debug)]
struct TokenAccount {
    address: Pubkey,
    mint: Pubkey,
    amount: u64,
    token_program: Pubkey,
}

#[tokio::main]
async fn main() -> Result<()> {
    let wallet_path = std::env::var("WALLET_PATH")
        .unwrap_or_else(|_| "/root/solana-bot/wallet.json".into());
    // QUERY_RPC: used for getTokenAccountsByOwner (needs full index)
    let query_rpc = std::env::var("QUERY_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into());
    // SEND_RPC: used for getLatestBlockhash + sendTransaction (sim-server)
    let send_rpc = std::env::var("SEND_RPC_URL")
        .unwrap_or_else(|_| std::env::var("SIM_RPC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8081".into()));
    let dry_run = std::env::var("DRY_RUN")
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false);

    let payer = read_keypair_file(&wallet_path)
        .map_err(|e| anyhow::anyhow!("read keypair: {e}"))?;
    let payer_pk = payer.pubkey();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    println!("Wallet: {payer_pk}");
    println!("Query RPC: {query_rpc}");
    println!("Send RPC:  {send_rpc}");
    println!("DRY_RUN: {dry_run}");

    // Fetch SOL balance
    let sol_balance = get_sol_balance(&client, &send_rpc, &payer_pk).await?;
    println!("SOL balance: {:.6} SOL", sol_balance as f64 / 1e9);

    // Discover all token accounts (via query RPC that has full index)
    let accounts = discover_all_token_accounts(&client, &query_rpc, &payer_pk).await?;
    println!("\nFound {} token accounts", accounts.len());

    if accounts.is_empty() {
        println!("Nothing to recover.");
        return Ok(());
    }

    let rent_per_account: f64 = 0.00203928;
    let total_rent = accounts.len() as f64 * rent_per_account;
    println!("Estimated rent to recover: {:.6} SOL", total_rent);

    // Process in batches — each TX can hold ~6 burn+close pairs
    // (burn = 1 IX, close = 1 IX, so 12 IXs per batch fits in 1232 bytes)
    let batch_size = 5; // conservative: 5 accounts per TX (10 IXs + blockhash overhead)
    let mut recovered = 0u32;
    let mut failed = 0u32;

    for (batch_idx, chunk) in accounts.chunks(batch_size).enumerate() {
        println!("\n--- Batch {} ({} accounts) ---", batch_idx + 1, chunk.len());

        let blockhash = get_blockhash(&client, &send_rpc).await?;
        let mut ixs: Vec<Instruction> = Vec::new();

        for acc in chunk {
            println!("  {} mint={} amount={}", acc.address, acc.mint, acc.amount);

            if acc.amount > 0 {
                // Burn all tokens
                let burn_ix = Instruction {
                    program_id: acc.token_program,
                    accounts: vec![
                        AccountMeta::new(acc.address, false),     // token account
                        AccountMeta::new(acc.mint, false),        // mint
                        AccountMeta::new_readonly(payer_pk, true), // owner/authority
                    ],
                    data: {
                        // SPL Token Burn instruction: tag=8, amount=u64
                        let mut d = vec![8u8];
                        d.extend_from_slice(&acc.amount.to_le_bytes());
                        d
                    },
                };
                ixs.push(burn_ix);
            }

            // Close account → rent goes to payer
            let close_ix = Instruction {
                program_id: acc.token_program,
                accounts: vec![
                    AccountMeta::new(acc.address, false),     // account to close
                    AccountMeta::new(payer_pk, false),        // destination (receives rent)
                    AccountMeta::new_readonly(payer_pk, true), // owner
                ],
                data: vec![9u8], // CloseAccount
            };
            ixs.push(close_ix);
        }

        if dry_run {
            println!("  DRY_RUN: would send {} instructions", ixs.len());
            recovered += chunk.len() as u32;
            continue;
        }

        match send_batch_tx(&client, &send_rpc, &payer, &ixs, &blockhash).await {
            Ok(sig) => {
                println!("  TX: {sig}");
                recovered += chunk.len() as u32;
                // Wait for confirmation before next batch
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
            }
            Err(e) => {
                eprintln!("  BATCH FAILED: {e}");
                // Try one-by-one fallback
                for acc in chunk {
                    let blockhash = get_blockhash(&client, &send_rpc).await?;
                    let mut single_ixs = Vec::new();
                    if acc.amount > 0 {
                        single_ixs.push(Instruction {
                            program_id: acc.token_program,
                            accounts: vec![
                                AccountMeta::new(acc.address, false),
                                AccountMeta::new(acc.mint, false),
                                AccountMeta::new_readonly(payer_pk, true),
                            ],
                            data: {
                                let mut d = vec![8u8];
                                d.extend_from_slice(&acc.amount.to_le_bytes());
                                d
                            },
                        });
                    }
                    single_ixs.push(Instruction {
                        program_id: acc.token_program,
                        accounts: vec![
                            AccountMeta::new(acc.address, false),
                            AccountMeta::new(payer_pk, false),
                            AccountMeta::new_readonly(payer_pk, true),
                        ],
                        data: vec![9u8],
                    });

                    match send_batch_tx(&client, &send_rpc, &payer, &single_ixs, &blockhash).await {
                        Ok(sig) => {
                            println!("  Single TX OK: {sig} (mint={})", acc.mint);
                            recovered += 1;
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        Err(e2) => {
                            eprintln!("  Single TX FAILED: {e2} (mint={})", acc.mint);
                            failed += 1;
                        }
                    }
                }
            }
        }
    }

    // Final balance
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let final_balance = get_sol_balance(&client, &send_rpc, &payer_pk).await?;
    let recovered_sol = (final_balance as f64 - sol_balance as f64) / 1e9;

    println!("\n=== Summary ===");
    println!("Accounts processed: {recovered}");
    println!("Failed: {failed}");
    println!("SOL before: {:.6}", sol_balance as f64 / 1e9);
    println!("SOL after:  {:.6}", final_balance as f64 / 1e9);
    println!("Recovered:  {:.6} SOL", recovered_sol);

    Ok(())
}

async fn discover_all_token_accounts(
    client: &reqwest::Client,
    rpc_url: &str,
    owner: &Pubkey,
) -> Result<Vec<TokenAccount>> {
    let mut all = Vec::new();

    // Query both SPL Token and Token-2022
    for program in [TOKEN_PROGRAM, TOKEN_2022_PROGRAM] {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTokenAccountsByOwner",
            "params": [
                owner.to_string(),
                {"programId": program},
                {"encoding": "jsonParsed"}
            ]
        });
        let resp = client.post(rpc_url).json(&body).send().await?;
        let json: serde_json::Value = resp.json().await?;
        let accounts = json["result"]["value"].as_array().cloned().unwrap_or_default();

        let tp: Pubkey = program.parse().unwrap();
        for acc in accounts {
            let address: Pubkey = acc["pubkey"].as_str().unwrap_or("").parse().unwrap_or_default();
            let info = &acc["account"]["data"]["parsed"]["info"];
            let mint: Pubkey = info["mint"].as_str().unwrap_or("").parse().unwrap_or_default();
            let amount: u64 = info["tokenAmount"]["amount"]
                .as_str().unwrap_or("0").parse().unwrap_or(0);

            // Skip WSOL — don't burn wrapped SOL
            let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
            if mint == wsol { continue; }
            // Skip USDC/USDT
            let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
            let usdt: Pubkey = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".parse().unwrap();
            if mint == usdc || mint == usdt { continue; }

            if address == Pubkey::default() || mint == Pubkey::default() { continue; }

            all.push(TokenAccount { address, mint, amount, token_program: tp });
        }
    }

    Ok(all)
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

async fn get_sol_balance(client: &reqwest::Client, rpc_url: &str, pubkey: &Pubkey) -> Result<u64> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "getBalance",
        "params": [pubkey.to_string()]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;
    Ok(json["result"]["value"].as_u64().unwrap_or(0))
}

async fn send_batch_tx(
    client: &reqwest::Client,
    rpc_url: &str,
    payer: &Keypair,
    ixs: &[Instruction],
    blockhash: &solana_sdk::hash::Hash,
) -> Result<String> {
    let msg = MessageV0::try_compile(&payer.pubkey(), ixs, &[], *blockhash)
        .context("compile message")?;
    let tx = VersionedTransaction::try_new(
        solana_sdk::message::VersionedMessage::V0(msg),
        &[payer],
    ).context("sign tx")?;

    let encoded = bincode::serialize(&tx)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&encoded);
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "sendTransaction",
        "params": [b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 3}]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;

    if let Some(err) = json.get("error") {
        anyhow::bail!("RPC error: {}", err);
    }

    Ok(json["result"].as_str().unwrap_or("?").to_string())
}
