//! http_backend.rs – HTTP/2 persistent connection pool to upstream Solana RPC.
//!
//! Uses a single reqwest::Client with:
//!   - HTTP/2 multiplexing (multiple requests on one TCP connection)
//!   - keep-alive (no TCP handshake per request)
//!   - 10s timeout
//!
//! Implements SimulationBackend by forwarding simulateTransaction to upstream.

use crate::{SimulateResponse, SimulationBackend};
use anyhow::Context;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use solana_sdk::hash::Hash;
use tracing::debug;

pub struct HttpBackend {
    client: Client,
    url: String,
}

impl HttpBackend {
    /// Forward an arbitrary JSON-RPC body to the upstream and return the raw response.
    pub async fn forward(&self, body: Value) -> anyhow::Result<Value> {
        let resp: Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .context("upstream proxy request")?
            .json()
            .await
            .context("parse upstream proxy response")?;
        Ok(resp)
    }

    pub fn new(url: &str) -> Self {
        // For HTTPS endpoints, use standard TLS with ALPN h2 negotiation.
        // http2_prior_knowledge() only works for plain HTTP (h2c) — NOT for HTTPS.
        let mut builder = Client::builder()
            .http2_adaptive_window(true)
            .connection_verbose(false)
            .tcp_nodelay(true)
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(std::time::Duration::from_secs(120))
            .tcp_keepalive(std::time::Duration::from_secs(15));

        if url.starts_with("http://") {
            builder = builder.http2_prior_knowledge();
        }

        let client = builder.build().expect("reqwest client build");

        Self {
            client,
            url: url.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl SimulationBackend for HttpBackend {
    async fn simulate(
        &self,
        tx_bytes: Vec<u8>,
        replace_blockhash: bool,
        _recent_blockhash: Hash,
    ) -> anyhow::Result<SimulateResponse> {
        let tx_b64 = B64.encode(&tx_bytes);

        // Build JSON-RPC simulateTransaction request.
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateTransaction",
            "params": [
                tx_b64,
                {
                    "encoding": "base64",
                    "replaceRecentBlockhash": replace_blockhash,
                    "sigVerify": false,
                    "commitment": "processed",
                }
            ]
        });

        let resp: serde_json::Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .context("HTTP simulate request")?
            .json()
            .await
            .context("parse simulate response")?;

        if let Some(err) = resp.get("error") {
            anyhow::bail!("RPC error: {}", err);
        }

        let value = resp
            .get("result")
            .and_then(|r| r.get("value"))
            .ok_or_else(|| anyhow::anyhow!("missing result.value in response"))?;

        let err_str = value
            .get("err")
            .filter(|e| !e.is_null())
            .map(|e| e.to_string());

        let logs: Vec<String> = value
            .get("logs")
            .and_then(|l| l.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let units_consumed = value.get("unitsConsumed").and_then(|u| u.as_u64());

        debug!(
            err = ?err_str,
            logs = logs.len(),
            units = ?units_consumed,
            "simulation response received"
        );

        Ok(SimulateResponse {
            err: err_str,
            logs,
            units_consumed,
        })
    }
}
