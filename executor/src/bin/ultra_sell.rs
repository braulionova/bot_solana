use base64::Engine;
use solana_sdk::{signature::{Keypair, Signer}, transaction::VersionedTransaction};

fn main() {
    let wb: Vec<u8> = serde_json::from_str(&std::fs::read_to_string("/root/solana-bot/wallet.json").unwrap()).unwrap();
    let kp = Keypair::from_bytes(&wb).unwrap();
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap();
    let key = std::env::var("JUPITER_API_KEY").unwrap();
    let wsol = "So11111111111111111111111111111111111111112";
    let taker = kp.pubkey().to_string();

    let args: Vec<String> = std::env::args().collect();
    let mint = args.get(1).expect("mint");
    let amount = args.get(2).expect("amount");

    println!("Selling {} amount={}", &mint[..8], amount);
    // Allow okx + iris (jupiterz fails signing, dflow unreliable)
    let url = format!("https://api.jup.ag/ultra/v1/order?inputMint={}&outputMint={}&amount={}&taker={}&excludeRouters=jupiterz,dflow", mint, wsol, amount, taker);
    let resp: serde_json::Value = client.get(&url).header("x-api-key", &key).send().unwrap().json().unwrap();
    
    if let Some(e) = resp.get("error").and_then(|v| v.as_str()) {
        if !e.is_empty() { println!("Error: {}", e); return; }
    }
    let tx_b64 = resp["transaction"].as_str().unwrap_or("");
    let req_id = resp["requestId"].as_str().unwrap_or("");
    let out = resp["outAmount"].as_str().unwrap_or("0");
    let router = resp["router"].as_str().unwrap_or("?");
    println!("Router: {} Quote: {} ({:.4} SOL)", router, out, out.parse::<u64>().unwrap_or(0) as f64 / 1e9);
    
    if tx_b64.is_empty() { println!("No TX"); return; }

    let bytes = base64::engine::general_purpose::STANDARD.decode(tx_b64).unwrap();
    let mut tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
    let msg = tx.message.serialize();
    tx.signatures[0] = kp.sign_message(&msg);
    let signed = base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx).unwrap());

    let exec: serde_json::Value = client.post("https://api.jup.ag/ultra/v1/execute")
        .header("Content-Type", "application/json").header("x-api-key", &key)
        .json(&serde_json::json!({"signedTransaction": signed, "requestId": req_id}))
        .send().unwrap().json().unwrap();
    
    let status = exec["status"].as_str().unwrap_or("?");
    let sig = exec["signature"].as_str().unwrap_or("");
    println!("Status: {} Sig: {}", status, sig);
    if status != "Success" { println!("Full: {}", exec); }
}
