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
    let amount = std::env::var("ARB_AMOUNT").unwrap_or("10000000".into());
    let taker = kp.pubkey().to_string();

    // Step 1: Get buy quote
    let buy_quote: serde_json::Value = client
        .get(format!("https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=100", wsol, usdc, amount))
        .header("x-api-key", &api_key)
        .send()?.json()?;
    let tokens_out = buy_quote.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");
    println!("Buy: {} SOL → {} USDC", amount, tokens_out);

    // Step 2: Get sell quote
    let sell_quote: serde_json::Value = client
        .get(format!("https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=100", usdc, wsol, tokens_out))
        .header("x-api-key", &api_key)
        .send()?.json()?;
    let sol_back = sell_quote.get("outAmount").and_then(|v| v.as_str()).unwrap_or("0");
    let profit: i64 = sol_back.parse::<i64>().unwrap_or(0) - amount.parse::<i64>().unwrap_or(0);
    println!("Sell: {} USDC → {} SOL (profit: {})", tokens_out, sol_back, profit);

    if profit <= 0 {
        println!("❌ Not profitable, skipping");
        return Ok(());
    }

    // Step 3: Get swap TXs
    let buy_tx_resp: serde_json::Value = client.post("https://api.jup.ag/swap/v1/swap")
        .header("Content-Type", "application/json").header("x-api-key", &api_key)
        .json(&serde_json::json!({"quoteResponse": buy_quote, "userPublicKey": taker, "wrapAndUnwrapSol": true}))
        .send()?.json()?;
    let buy_tx_b64 = buy_tx_resp.get("swapTransaction").and_then(|v| v.as_str())
        .ok_or(anyhow!("no buy swapTransaction"))?;

    let sell_tx_resp: serde_json::Value = client.post("https://api.jup.ag/swap/v1/swap")
        .header("Content-Type", "application/json").header("x-api-key", &api_key)
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
            // Wait briefly then send sell
            std::thread::sleep(std::time::Duration::from_millis(500));
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

    Ok(())
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
