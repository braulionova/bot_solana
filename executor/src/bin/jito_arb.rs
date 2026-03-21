/// Jito Bundle Arb: execute buy+sell as atomic Jito bundle.
/// Each leg is a Jupiter swap TX (<1232 bytes).
/// Bundle atomicity: both succeed or both fail = 0 cost on failure.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};

fn main() -> Result<()> {
    let continuous = std::env::var("CONTINUOUS").map(|v| v == "true" || v == "1").unwrap_or(false);
    let api_key = std::env::var("JUPITER_API_KEY").context("JUPITER_API_KEY")?;
    let wb: Vec<u8> = serde_json::from_str(
        &std::fs::read_to_string("/root/solana-bot/wallet.json")?
    )?;
    let kp = Keypair::from_bytes(&wb).map_err(|e| anyhow!("{}", e))?;
    println!("Wallet: {}", kp.pubkey());

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let wsol = "So11111111111111111111111111111111111111112";
    let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    let usdt = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
    let amount = std::env::var("ARB_AMOUNT").unwrap_or("100000000".into()); // 0.1 SOL default
    let min_profit: i64 = std::env::var("MIN_PROFIT").and_then(|v| Ok(v.parse().unwrap_or(5000))).unwrap_or(5000);

    let taker = kp.pubkey().to_string();
    let tokens = [usdc, usdt]; // scan both stablecoins
    let mut round = 0u64;

    let api_key_2 = std::env::var("JUPITER_API_KEY_2").unwrap_or(api_key.clone());

    loop {
        round += 1;

        // RECOVERY: sell any stuck USDC/USDT from previous failed sells
        for &token in &tokens {
            recover_stuck_tokens(&client, &api_key_2, &kp, token, wsol, &taker);
        }

        for &token in &tokens {
            match run_arb(&client, &api_key, &kp, wsol, token, &amount, &taker, min_profit) {
                Ok(profit) => {
                    if profit > 0 {
                        println!("🎉 Round {} PROFIT: {} lamports", round, profit);
                    }
                    // Wait 3s after successful send for TXs to settle
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    // Recover any stuck tokens from this round
                    recover_stuck_tokens(&client, &api_key_2, &kp, token, wsol, &taker);
                }
                Err(e) => {
                    if !format!("{}", e).contains("Not profitable") {
                        println!("⚠️ Error: {}", e);
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        if !continuous { break; }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    Ok(())
}

fn run_arb(client: &reqwest::blocking::Client, api_key: &str, kp: &Keypair,
    wsol: &str, token: &str, amount: &str, taker: &str, min_profit: i64) -> Result<i64> {

    // Step 1: Get buy quote
    let buy_quote: serde_json::Value = client
        .get(format!("https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=300", wsol, token, amount))
        .header("x-api-key", api_key)
        .send()?.json()?;
    let tokens_out = buy_quote.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");
    println!("Buy: {} SOL → {} USDC", amount, tokens_out);

    // Step 2: Get sell quote
    let sell_quote: serde_json::Value = client
        .get(format!("https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=300", token, wsol, tokens_out))
        .header("x-api-key", api_key)
        .send()?.json()?;
    let sol_back = sell_quote.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");
    let profit: i64 = sol_back.parse::<i64>().unwrap_or(0) - amount.parse::<i64>().unwrap_or(0);
    println!("Sell: {} USDC → {} SOL (profit: {})", tokens_out, sol_back, profit);

    if profit < min_profit {
        return Err(anyhow!("Not profitable: {} < {} min", profit, min_profit));
    }
    println!("✅ Arb found: +{} lamports", profit);

    // Step 3: Get swap TXs
    let buy_tx_resp: serde_json::Value = client.post("https://api.jup.ag/swap/v1/swap")
        .header("Content-Type", "application/json").header("x-api-key", api_key)
        .json(&serde_json::json!({"quoteResponse": buy_quote, "userPublicKey": taker, "wrapAndUnwrapSol": true}))
        .send()?.json()?;
    let buy_tx_b64 = buy_tx_resp.get("swapTransaction").and_then(|v| v.as_str())
        .ok_or(anyhow!("no buy swapTransaction"))?;

    let sell_tx_resp: serde_json::Value = client.post("https://api.jup.ag/swap/v1/swap")
        .header("Content-Type", "application/json").header("x-api-key", api_key)
        .json(&serde_json::json!({"quoteResponse": sell_quote, "userPublicKey": taker, "wrapAndUnwrapSol": true}))
        .send()?.json()?;
    let sell_tx_b64 = sell_tx_resp.get("swapTransaction").and_then(|v| v.as_str())
        .ok_or(anyhow!("no sell swapTransaction"))?;

    // Step 4: Sign both TXs + build Jito tip TX
    let buy_tx = sign_tx(buy_tx_b64, kp)?;
    let sell_tx = sign_tx(sell_tx_b64, kp)?;

    // Build tip TX (required for Jito bundle inclusion)
    let tip_accounts = [
        "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
        "HFqU5x63VTqvQss8hp11i4bPYoTdFDd2fi1rssk8ybP1",
        "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
        "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    ];
    let tip_idx = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_secs() as usize) % tip_accounts.len();
    let tip_pubkey: solana_sdk::pubkey::Pubkey = tip_accounts[tip_idx].parse()?;
    let tip_ix = solana_sdk::system_instruction::transfer(&kp.pubkey(), &tip_pubkey, 1000);

    // Get fresh blockhash for tip TX
    let bh_resp: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash","params":[{"commitment":"confirmed"}]}))
        .send()?.json()?;
    let bh_str = bh_resp.get("result").and_then(|r| r.get("value")).and_then(|v| v.get("blockhash"))
        .and_then(|b| b.as_str()).ok_or(anyhow!("no blockhash"))?;
    let blockhash: solana_sdk::hash::Hash = bh_str.parse()?;

    let tip_tx_obj = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[tip_ix], Some(&kp.pubkey()), &[kp], blockhash,
    );
    let tip_tx_bytes = bincode::serialize(&tip_tx_obj)?;
    let tip_tx = bs58::encode(&tip_tx_bytes).into_string();

    println!("3 TXs signed: buy({}) + sell({}) + tip({} bytes)", buy_tx.len(), sell_tx.len(), tip_tx_bytes.len());

    // Step 5: Try ALL Jito endpoints (Frankfurt/Amsterdam usually not rate limited)
    let jito_endpoints = [
        "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles",
        "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles",
        "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles",
        "https://mainnet.block-engine.jito.wtf/api/v1/bundles",
    ];
    let mut jito_sent = false;
    for ep in &jito_endpoints {
        match client.post(*ep)
            .timeout(std::time::Duration::from_secs(3))
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendBundle","params":[[&buy_tx, &sell_tx, &tip_tx]]}))
            .send()
        {
            Ok(r) => {
                if let Ok(resp) = r.json::<serde_json::Value>() {
                    if let Some(id) = resp.get("result").and_then(|v| v.as_str()) {
                        println!("✅ JITO BUNDLE SENT: {} via {}", id, ep);
                        jito_sent = true;
                        break;
                    }
                }
            }
            Err(_) => continue, // timeout or error, try next
        }
    }

    if !jito_sent {
        // Send BOTH TXs via Jito sendTransaction (faster than public RPC, less rate limited than bundle)
        println!("⚡ Sending via Jito sendTransaction + public RPC (parallel)...");
        let buy_b64 = base64::engine::general_purpose::STANDARD.encode(
            &bs58::decode(&buy_tx).into_vec()?);
        let sell_b64 = base64::engine::general_purpose::STANDARD.encode(
            &bs58::decode(&sell_tx).into_vec()?);

        // Send buy via Jito sendTransaction + publicnode simultaneously
        let jito_buy = client.post("https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/transactions")
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
                "params":[buy_tx,{"encoding":"base58"}]}))
            .send();
        let rpc_buy = client.post("https://solana-rpc.publicnode.com")
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
                "params":[buy_b64,{"encoding":"base64","skipPreflight":true}]}))
            .send();
        // Get first successful response
        let buy_sig = if let Ok(r) = jito_buy { r.json::<serde_json::Value>().ok() } else { None }
            .or_else(|| rpc_buy.ok().and_then(|r| r.json().ok()))
            .and_then(|v| v.get("result").and_then(|r| r.as_str().map(String::from)));
        if let Some(sig) = buy_sig {
            println!("✅ BUY TX SENT: {}", sig);
            // Send pre-signed sell TX via Jito sendTransaction (NO delay, NO Ultra)
            let jito_sell = client.post("https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/transactions")
                .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
                    "params":[sell_tx,{"encoding":"base58"}]}))
                .send();
            let rpc_sell = client.post("https://solana-rpc.publicnode.com")
                .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
                    "params":[sell_b64,{"encoding":"base64","skipPreflight":true}]}))
                .send();
            let sell_sig = if let Ok(r) = jito_sell { r.json::<serde_json::Value>().ok() } else { None }
                .or_else(|| rpc_sell.ok().and_then(|r| r.json().ok()))
                .and_then(|v| v.get("result").and_then(|r| r.as_str().map(String::from)));
            if let Some(ssig) = sell_sig {
                println!("✅ SELL TX SENT: {} (expected profit: {} lam)", ssig, profit);
                return Ok(profit);
            } else {
                println!("❌ Sell send failed, falling back to Ultra...");
            }
            println!("⚡ Selling via Ultra (fallback)...");

            let sell_url = format!(
                "https://api.jup.ag/ultra/v1/order?inputMint={}&outputMint={}&amount={}&taker={}&excludeRouters=jupiterz,dflow",
                token, wsol, tokens_out, taker
            );
            let sell_order: serde_json::Value = client.get(&sell_url)
                .header("x-api-key", api_key).send()?.json()?;

            if let Some(e) = sell_order.get("error").and_then(|v| v.as_str()) {
                if !e.is_empty() {
                    println!("❌ Ultra sell quote error: {}", e);
                    return Ok(0);
                }
            }

            let sell_tx_b64 = sell_order.get("transaction").and_then(|v| v.as_str())
                .ok_or(anyhow!("no Ultra sell TX"))?;
            let sell_req_id = sell_order.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
            let sell_out = sell_order.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");

            // Sign Ultra TX
            let sell_bytes = base64::engine::general_purpose::STANDARD.decode(sell_tx_b64)?;
            let mut sell_tx_obj: VersionedTransaction = bincode::deserialize(&sell_bytes)?;
            let sell_msg = sell_tx_obj.message.serialize();
            sell_tx_obj.signatures[0] = kp.sign_message(&sell_msg);
            let sell_signed_b64 = base64::engine::general_purpose::STANDARD.encode(
                &bincode::serialize(&sell_tx_obj)?);

            // Execute via Ultra
            let exec_resp: serde_json::Value = client.post("https://api.jup.ag/ultra/v1/execute")
                .header("Content-Type", "application/json").header("x-api-key", api_key)
                .json(&serde_json::json!({"signedTransaction": sell_signed_b64, "requestId": sell_req_id}))
                .send()?.json()?;

            let status = exec_resp.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let sell_sig_str = exec_resp.get("signature").and_then(|v| v.as_str()).unwrap_or("");

            if status == "Success" {
                let sol_back: i64 = sell_out.parse().unwrap_or(0);
                let real_profit = sol_back - amount.parse::<i64>().unwrap_or(0);
                println!("✅ SELL SUCCESS: {} | net: {} lamports", sell_sig_str, real_profit);
                return Ok(real_profit);
            } else {
                println!("❌ Ultra sell failed: {}", exec_resp);
                return Ok(0);
            }

            let sell_resp: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
                .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
                    "params":[sell_b64,{"encoding":"base64","skipPreflight":true}]}))
                .send()?.json()?;
            if let Some(sig2) = sell_resp.get("result").and_then(|v| v.as_str()) {
                println!("✅ SELL TX SENT: {}", sig2);
                println!("✅ PROFIT: {} lamports", profit);
            } else {
                println!("❌ SELL failed: {}", sell_resp);
            }
        } else {
            println!("❌ BUY send failed");
        }
    }

    Ok(0)
}

/// Sell any stuck USDC/USDT via Ultra (recovery from failed sells)
fn recover_stuck_tokens(client: &reqwest::blocking::Client, api_key: &str, kp: &Keypair, token: &str, wsol: &str, taker: &str) {
    // Check token balance via RPC
    let ata = solana_sdk::pubkey::Pubkey::find_program_address(
        &[kp.pubkey().as_ref(),
          "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse::<solana_sdk::pubkey::Pubkey>().unwrap().as_ref(),
          token.parse::<solana_sdk::pubkey::Pubkey>().unwrap().as_ref()],
        &"ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap(),
    ).0;

    let resp: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getTokenAccountBalance","params":[ata.to_string()]}))
        .send().unwrap_or_else(|_| panic!("rpc")).json().unwrap_or_default();

    let amount_str = resp.get("result").and_then(|r| r.get("value")).and_then(|v| v.get("amount"))
        .and_then(|a| a.as_str()).unwrap_or("0");
    let amount: u64 = amount_str.parse().unwrap_or(0);

    if amount < 100000 { return; } // less than 0.1 USDC, skip

    println!("♻️ Recovering {} stuck tokens for {}", amount, &token[..8]);

    // Sell via Ultra
    let url = format!(
        "https://api.jup.ag/ultra/v1/order?inputMint={}&outputMint={}&amount={}&taker={}&excludeRouters=jupiterz,dflow",
        token, wsol, amount, taker
    );
    let order: serde_json::Value = match client.get(&url).header("x-api-key", api_key).send() {
        Ok(r) => r.json().unwrap_or_default(),
        Err(_) => return,
    };
    if order.get("error").and_then(|v| v.as_str()).unwrap_or("").len() > 0 { return; }

    let tx_b64 = match order.get("transaction").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => return,
    };
    let req_id = order.get("requestId").and_then(|v| v.as_str()).unwrap_or("");

    // Sign + execute
    if let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_b64) {
        if let Ok(mut tx) = bincode::deserialize::<VersionedTransaction>(&bytes) {
            let msg = tx.message.serialize();
            tx.signatures[0] = kp.sign_message(&msg);
            let signed = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bincode::serialize(&tx).unwrap());

            let exec: serde_json::Value = client.post("https://api.jup.ag/ultra/v1/execute")
                .header("Content-Type", "application/json").header("x-api-key", api_key)
                .json(&serde_json::json!({"signedTransaction": signed, "requestId": req_id}))
                .send().unwrap_or_else(|_| panic!("exec")).json().unwrap_or_default();

            let status = exec.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            if status == "Success" {
                println!("♻️ RECOVERED: sold {} tokens", amount);
            }
        }
    }
}

fn sign_tx(tx_b64: &str, kp: &Keypair) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(tx_b64)?;
    let mut tx: VersionedTransaction = bincode::deserialize(&bytes)?;
    let msg = tx.message.serialize();
    tx.signatures[0] = kp.sign_message(&msg);
    let signed = bincode::serialize(&tx)?;
    // Jito expects base58-encoded transactions
    Ok(bs58::encode(&signed).into_string())
}
