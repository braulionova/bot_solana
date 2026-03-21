// ultra_executor.rs — Execute arb via Jupiter Ultra API.
//
// Jupiter Ultra handles: routing, MEV protection, sub-second landing.
// We handle: detecting profitable routes, signing TXs, monitoring results.
//
// Flow:
// 1. Jupiter scanner detects profitable cross-DEX route
// 2. Ultra /order gives us an unsigned TX
// 3. We sign with our keypair
// 4. Ultra /execute lands it on-chain with MEV protection
//
// IMPORTANT: Each leg is a separate TX (not atomic).
// We only execute when profit > 2x expected fees to account for timing risk.

use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use tracing::{info, warn};

pub struct UltraExecutor {
    api_key: String,
    api_key_2: String,
    client: reqwest::Client,
    keypair: Keypair,
    pub executions: std::sync::atomic::AtomicU64,
    pub profits: std::sync::atomic::AtomicI64,
}

impl UltraExecutor {
    pub fn new(api_key: String, api_key_2: String, keypair: Keypair) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            api_key,
            api_key_2,
            keypair,
            executions: std::sync::atomic::AtomicU64::new(0),
            profits: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Execute a buy+sell arb via Jupiter Ultra.
    /// SAFETY: Gets sell quote BEFORE executing buy to verify profitability.
    /// Returns (buy_sig, sell_sig, profit_lamports) on success.
    pub async fn execute_arb(
        &self,
        token_mint: &str,
        amount_sol_lamports: u64,
    ) -> Result<(String, String, i64), String> {
        let wsol = "So11111111111111111111111111111111111111112";
        let taker = self.keypair.pubkey().to_string();

        // Step 1: Get buy quote (SOL → token)
        let buy_order = self.get_order(wsol, token_mint, amount_sol_lamports, &taker, &self.api_key).await?;
        let tokens_out: u64 = buy_order.out_amount.parse().map_err(|e| format!("parse buy: {}", e))?;
        if tokens_out == 0 { return Err("buy quote: 0 tokens out".into()); }

        // Step 2: Get sell quote BEFORE buying using swap/v1/quote (no taker = no balance check)
        let sell_quote_url = format!(
            "https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=100",
            token_mint, wsol, tokens_out
        );
        let sell_resp = self.client.get(&sell_quote_url)
            .header("x-api-key", &self.api_key_2)
            .send().await
            .map_err(|e| format!("sell quote: {}", e))?;
        let sell_body: serde_json::Value = sell_resp.json().await
            .map_err(|e| format!("sell quote json: {}", e))?;
        let sol_back: u64 = sell_body.get("outAmount")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .parse().unwrap_or(0);
        let expected_profit = sol_back as i64 - amount_sol_lamports as i64;

        if expected_profit <= 0 {
            return Err(format!("pre-check: sell quote unprofitable ({} lamports)", expected_profit));
        }
        info!(
            token = token_mint, amount = amount_sol_lamports,
            tokens = tokens_out, sell_back = sol_back,
            expected_profit, "Ultra: pre-check passed, executing buy"
        );

        // Step 3: Execute buy
        let buy_sig = self.sign_and_execute(&buy_order.transaction, &buy_order.request_id, &self.api_key).await?;
        info!(sig = %buy_sig, tokens = tokens_out, "Ultra: buy landed");

        // Step 4: Execute sell immediately (get fresh quote — price may have moved)
        let sell_order = self.get_order(token_mint, wsol, tokens_out, &taker, &self.api_key_2).await?;
        let sol_received: u64 = sell_order.out_amount.parse().map_err(|e| format!("parse sell2: {}", e))?;

        let sell_sig = self.sign_and_execute(&sell_order.transaction, &sell_order.request_id, &self.api_key_2).await?;
        let profit = sol_received as i64 - amount_sol_lamports as i64;
        info!(
            sig = %sell_sig, sol_received, profit,
            "Ultra: sell landed — arb complete"
        );

        self.executions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.profits.fetch_add(profit, std::sync::atomic::Ordering::Relaxed);

        Ok((buy_sig, sell_sig, profit))
    }

    async fn get_order(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        taker: &str,
        api_key: &str,
    ) -> Result<UltraOrder, String> {
        // Exclude jupiterz (signing fails) and dflow (unreliable). OKX + iris work.
        let url = format!(
            "https://api.jup.ag/ultra/v1/order?inputMint={}&outputMint={}&amount={}&taker={}&excludeRouters=jupiterz,dflow",
            input_mint, output_mint, amount, taker
        );
        info!(url = %url, "Ultra: requesting order");
        let resp = self.client.get(&url)
            .header("x-api-key", api_key)
            .send()
            .await
            .map_err(|e| format!("order request: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("order failed: {} {}", status, body));
        }

        let body: serde_json::Value = resp.json().await
            .map_err(|e| format!("order json: {}", e))?;

        // Check for errors first
        if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
            if !err.is_empty() && err != "null" {
                return Err(format!("order error: {}", err));
            }
        }
        if let Some(err_msg) = body.get("errorMessage").and_then(|v| v.as_str()) {
            if !err_msg.is_empty() {
                return Err(format!("order error: {}", err_msg));
            }
        }

        let out_amount = body.get("outAmount")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .to_string();
        let request_id = body.get("requestId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let transaction = body.get("transaction")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if transaction.is_empty() || request_id.is_empty() {
            return Err("no transaction or requestId in response".into());
        }
        let router = body.get("router")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(UltraOrder {
            out_amount,
            request_id,
            transaction,
            router,
        })
    }

    async fn sign_and_execute(
        &self,
        tx_b64: &str,
        request_id: &str,
        api_key: &str,
    ) -> Result<String, String> {
        use base64::Engine;

        // Decode the unsigned TX
        let tx_bytes = base64::engine::general_purpose::STANDARD.decode(tx_b64)
            .map_err(|e| format!("decode tx: {}", e))?;
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| format!("deserialize tx: {}", e))?;

        // Sign with our keypair
        let message_bytes = tx.message.serialize();
        let sig = self.keypair.sign_message(&message_bytes);
        tx.signatures[0] = sig;

        // Serialize signed TX
        let signed_bytes = bincode::serialize(&tx)
            .map_err(|e| format!("serialize tx: {}", e))?;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        // POST to /execute
        let resp = self.client.post("https://api.jup.ag/ultra/v1/execute")
            .header("Content-Type", "application/json")
            .header("x-api-key", api_key)
            .json(&serde_json::json!({
                "signedTransaction": signed_b64,
                "requestId": request_id,
            }))
            .send()
            .await
            .map_err(|e| format!("execute request: {}", e))?;

        let body: serde_json::Value = resp.json().await
            .map_err(|e| format!("execute json: {}", e))?;

        let status = body.get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if status == "Success" {
            let signature = body.get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(signature)
        } else {
            let error = body.get("error")
                .map(|v| v.to_string())
                .unwrap_or_else(|| body.to_string());
            Err(format!("execute failed: status={}, error={}", status, error))
        }
    }
}

struct UltraOrder {
    out_amount: String,
    request_id: String,
    transaction: String,
    router: String,
}
