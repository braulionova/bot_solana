/// Carbium Circular Arb: SOL→intermediate→SOL in 1 atomic TX
/// Carbium CQ1 engine supports circular swaps natively (31ms latency)
/// Wrap with Solend flash loan for 0-capital arb

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};

const CARBIUM_API: &str = "https://api.carbium.io/api/v2/quote";

fn main() -> Result<()> {
    let api_key = std::env::var("CARBIUM_API_KEY").unwrap_or("814f09dc-d645".into());
    let wb: Vec<u8> = serde_json::from_str(&std::fs::read_to_string("/root/solana-bot/wallet.json")?)?;
    let kp = Keypair::from_bytes(&wb).map_err(|e| anyhow!("{}", e))?;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build()?;
    let continuous = std::env::var("CONTINUOUS").map(|v| v == "true").unwrap_or(false);
    let taker = kp.pubkey().to_string();

    println!("Carbium Circular Arb | wallet: {}", taker);

    let amounts = ["200000000", "500000000", "1000000000"];
    let mut round = 0u64;

    loop {
        round += 1;
        for &amount in &amounts {
            // Circular quote: SOL → ??? → SOL
            let url = format!(
                "{}?src_mint=So11111111111111111111111111111111111111112&dst_mint=So11111111111111111111111111111111111111112&amount_in={}&slippage_bps=500&user_account={}",
                CARBIUM_API, amount, taker
            );
            let resp: serde_json::Value = match client.get(&url)
                .header("X-API-KEY", &api_key)
                .send() {
                Ok(r) => r.json().unwrap_or_default(),
                Err(_) => continue,
            };

            if resp.get("error").is_some() {
                let err = resp["error"].as_str().unwrap_or("?");
                if err.contains("Rate limit") { std::thread::sleep(std::time::Duration::from_secs(5)); }
                continue;
            }

            let in_amt: i64 = resp.get("srcAmountIn").and_then(|v| v.as_str())
                .unwrap_or("0").parse().unwrap_or(0);
            let out_amt: i64 = resp.get("destAmountOut").and_then(|v| v.as_str())
                .unwrap_or("0").parse().unwrap_or(0);
            let profit = out_amt - in_amt;
            let routes: Vec<String> = resp.get("routePlan").and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|r| r.get("swapInfo").and_then(|s| s.get("label")).and_then(|l| l.as_str()).map(String::from)).collect())
                .unwrap_or_default();

            if profit > 0 {
                println!("✅ PROFITABLE! {} SOL: +{} lam | route: {:?}", in_amt as f64 / 1e9, profit, routes);

                // Get the TX and sign+send
                if let Some(txn_b64) = resp.get("txn").and_then(|v| v.as_str()) {
                    match sign_and_send(&client, txn_b64, &kp) {
                        Ok(sig) => println!("🎉 TX SENT: {} | profit: {}", sig, profit),
                        Err(e) => println!("❌ Send failed: {}", e),
                    }
                }
            } else if profit > -10000 {
                let sol = in_amt as f64 / 1e9;
                println!("⚠️ {:.1} SOL: {} lam ({:?})", sol, profit, routes);
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        if !continuous { break; }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    Ok(())
}

fn sign_and_send(client: &reqwest::blocking::Client, txn_b64: &str, kp: &Keypair) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(txn_b64)?;
    let mut tx: VersionedTransaction = bincode::deserialize(&bytes)?;
    let msg = tx.message.serialize();
    tx.signatures[0] = kp.sign_message(&msg);
    let signed = bincode::serialize(&tx)?;
    let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed);

    // Send via Carbium RPC + publicnode
    let carbium_rpc = "https://rpc-service.carbium.io/?apiKey=578475ff-1380";
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {"encoding":"base64","skipPreflight":true}]
    });

    let resp: serde_json::Value = client.post(carbium_rpc)
        .json(&body).send()?.json()?;

    if let Some(sig) = resp.get("result").and_then(|v| v.as_str()) {
        return Ok(sig.to_string());
    }

    // Fallback publicnode
    let resp2: serde_json::Value = client.post("https://solana-rpc.publicnode.com")
        .json(&body).send()?.json()?;
    resp2.get("result").and_then(|v| v.as_str()).map(String::from)
        .ok_or(anyhow!("send failed: {}", resp2))
}
