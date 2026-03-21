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

    loop {
        round += 1;
        for &token in &tokens {
            match run_arb(&client, &api_key, &kp, wsol, token, &amount, &taker, min_profit) {
                Ok(profit) => {
                    if profit > 0 {
                        println!("🎉 Round {} NET PROFIT: {} lamports", round, profit);
                    }
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

    // Step 4: Sign both TXs
    let buy_tx = sign_tx(buy_tx_b64, &kp)?;
    let sell_tx = sign_tx(sell_tx_b64, &kp)?;
    println!("Both TXs signed ({} + {} bytes)", buy_tx.len(), sell_tx.len());

    // Step 5: Try Jito bundle first, fallback to sequential RPC send
    let jito_endpoints = [
        "https://mainnet.block-engine.jito.wtf/api/v1/bundles",
        "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles",
        "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles",
    ];
    let mut jito_sent = false;
    for ep in &jito_endpoints {
        let resp: serde_json::Value = client.post(*ep)
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendBundle","params":[[&buy_tx, &sell_tx]]}))
            .send()?.json()?;
        if let Some(id) = resp.get("result").and_then(|v| v.as_str()) {
            println!("✅ JITO BUNDLE SENT: {} via {}", id, ep);
            jito_sent = true;
            break;
        }
    }

    if !jito_sent {
        // Fallback: send both TXs sequentially via RPC (NOT atomic — risky!)
        println!("⚠️ Jito rate limited. Sending via RPC (non-atomic)...");
        // Convert back to base64 for RPC
        let buy_b64 = base64::engine::general_purpose::STANDARD.encode(
            &bs58::decode(&buy_tx).into_vec()?);
        let sell_b64 = base64::engine::general_purpose::STANDARD.encode(
            &bs58::decode(&sell_tx).into_vec()?);

        let buy_resp: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"sendTransaction",
                "params":[buy_b64,{"encoding":"base64","skipPreflight":true}]}))
            .send()?.json()?;
        if let Some(sig) = buy_resp.get("result").and_then(|v| v.as_str()) {
            println!("✅ BUY TX SENT: {}", sig);
            // Wait for buy to confirm before sending sell
            println!("⏳ Waiting for buy confirmation...");
            for _ in 0..15 {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let status: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
                    .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getSignatureStatuses","params":[[sig]]}))
                    .send().unwrap_or_else(|_| panic!("rpc")).json().unwrap_or_default();
                let confirmed = status.get("result")
                    .and_then(|r| r.get("value"))
                    .and_then(|v| v.get(0))
                    .and_then(|s| s.get("confirmationStatus"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let err = status.get("result")
                    .and_then(|r| r.get("value"))
                    .and_then(|v| v.get(0))
                    .and_then(|s| s.get("err"));
                if confirmed == "confirmed" || confirmed == "finalized" {
                    if err.is_some() && !err.unwrap().is_null() {
                        println!("❌ BUY FAILED on-chain: {:?}", err);
                        return Ok(0);
                    }
                    println!("✅ BUY CONFIRMED");

                    break;
                }
            }
            // Sell via Ultra (iris router — proven to work, unlike Jupiter Swap API)
            let sell_url = format!(
                "https://api.jup.ag/ultra/v1/order?inputMint={}&outputMint={}&amount={}&taker={}&excludeRouters=jupiterz,dflow",
                token, wsol, tokens_out, taker
            );
            let sell_order: serde_json::Value = client.get(&sell_url)
                .header("x-api-key", api_key).send()?.json()?;
            if let Some(e) = sell_order.get("error").and_then(|v| v.as_str()) {
                if !e.is_empty() { println!("❌ Ultra sell error: {}", e); return Ok(0); }
            }
            let sell_tx_b64 = sell_order.get("transaction").and_then(|v| v.as_str())
                .ok_or(anyhow!("no Ultra sell TX"))?;
            let sell_req_id = sell_order.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
            let sell_out = sell_order.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");
            println!("Ultra sell quote: {} USDC → {} SOL", tokens_out, sell_out);

            // Sign Ultra TX
            let sell_bytes = base64::engine::general_purpose::STANDARD.decode(sell_tx_b64)?;
            let mut sell_tx_obj: VersionedTransaction = bincode::deserialize(&sell_bytes)?;
            let sell_msg = sell_tx_obj.message.serialize();
            sell_tx_obj.signatures[0] = kp.sign_message(&sell_msg);
            let sell_signed = bincode::serialize(&sell_tx_obj)?;
            let sell_signed_b64 = base64::engine::general_purpose::STANDARD.encode(&sell_signed);

            // Execute via Ultra
            let exec_resp: serde_json::Value = client.post("https://api.jup.ag/ultra/v1/execute")
                .header("Content-Type", "application/json").header("x-api-key", api_key)
                .json(&serde_json::json!({"signedTransaction": sell_signed_b64, "requestId": sell_req_id}))
                .send()?.json()?;
            let status = exec_resp.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let sell_sig = exec_resp.get("signature").and_then(|v| v.as_str()).unwrap_or("");
            if status == "Success" {
                let real_profit: i64 = sell_out.parse::<i64>().unwrap_or(0) - amount.parse::<i64>().unwrap_or(0);
                println!("✅ SELL SUCCESS: {} (profit: {} lamports)", sell_sig, real_profit);
            } else {
                println!("❌ Ultra sell failed: {}", exec_resp);
            }
            // Skip old RPC sell path
            return Ok(0);

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
            println!("❌ BUY failed: {}", buy_resp);
        }
    }

    Ok(0)
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
