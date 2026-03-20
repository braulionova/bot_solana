// jito_sender.rs – Submit transaction bundles to the Jito Block Engine.
//
// Jito Block Engine exposes JSON-RPC endpoints:
//   POST /api/v1/bundles       → sendBundle (up to 5 TXs, atomic)
//   POST /api/v1/transactions  → sendTransaction (single TX, faster propagation)
//
// Multi-region fanout: send to Frankfurt + Amsterdam + NY simultaneously
// for maximum inclusion probability.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default Jito Block Engine endpoint (mainnet).
const DEFAULT_JITO_ENDPOINT: &str = "https://frankfurt.mainnet.block-engine.jito.wtf";

/// Additional endpoints for multi-region fanout.
/// Sending to multiple regions increases inclusion probability.
/// Includes Jito regions + alternative relayers (Nozomi, Temporal, NextBlock).
const FANOUT_ENDPOINTS: &[&str] = &[
    // Jito regions
    "https://amsterdam.mainnet.block-engine.jito.wtf",
    "https://ny.mainnet.block-engine.jito.wtf",
    "https://mainnet.block-engine.jito.wtf",
    // Nozomi (Temporal) — alternative MEV relay, no auth required for sendTransaction
    "https://fra.nozomi.temporal.xyz",
    // NextBlock — another MEV relay
    "https://fra.nextblock.io",
];

/// Jito tip lamports fallback (used if tip floor API unreachable).
const DEFAULT_TIP_LAMPORTS: u64 = 50_000; // 0.00005 SOL

/// Max tip we're willing to pay (cap to stay profitable).
const MAX_TIP_LAMPORTS: u64 = 500_000; // 0.0005 SOL

/// Min tip floor (never go below this even if API says lower).
const MIN_TIP_LAMPORTS: u64 = 10_000; // 0.00001 SOL

/// Tip floor refresh interval.
const TIP_FLOOR_REFRESH_SECS: u64 = 10;

/// Well-known Jito tip accounts (rotate through them).
const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonRpcRequest<T: Serialize> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: T,
}

#[derive(Deserialize, Debug)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize, Debug)]
struct BundleResult {
    bundle_id: String,
}

// ---------------------------------------------------------------------------
// JitoSender
// ---------------------------------------------------------------------------

pub struct JitoSender {
    client: Client,
    endpoint: String,
    /// Static tip override (0 = use dynamic tip floor).
    tip_lamports_override: u64,
    /// Dynamic tip from Jito tip_floor API (atomic, refreshed every 10s).
    dynamic_tip: std::sync::atomic::AtomicU64,
    request_id: std::sync::atomic::AtomicU64,
    /// Endpoint → RTT in ms (updated by periodic ping). Lower = send first.
    endpoint_latency: dashmap::DashMap<String, u64>,
}

impl JitoSender {
    pub fn new(endpoint: Option<String>, tip_lamports: Option<u64>) -> Self {
        let sender = Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap_or_default(),
            endpoint: endpoint.unwrap_or_else(|| DEFAULT_JITO_ENDPOINT.to_string()),
            tip_lamports_override: tip_lamports.unwrap_or(0),
            dynamic_tip: std::sync::atomic::AtomicU64::new(DEFAULT_TIP_LAMPORTS),
            request_id: std::sync::atomic::AtomicU64::new(1),
            endpoint_latency: dashmap::DashMap::new(),
        };
        sender
    }

    /// Current tip in lamports: uses dynamic floor (p75) if no static override.
    pub fn current_tip(&self) -> u64 {
        if self.tip_lamports_override > 0 {
            return self.tip_lamports_override;
        }
        let dynamic = self.dynamic_tip.load(std::sync::atomic::Ordering::Relaxed);
        dynamic.max(MIN_TIP_LAMPORTS).min(MAX_TIP_LAMPORTS)
    }

    /// Get all endpoints sorted by latency (fastest first).
    fn sorted_endpoints(&self) -> Vec<String> {
        let mut eps = vec![self.endpoint.clone()];
        for ep in FANOUT_ENDPOINTS {
            let s = ep.to_string();
            if s != self.endpoint {
                eps.push(s);
            }
        }
        // Sort by measured RTT (unknown = 500ms default).
        eps.sort_by_key(|ep| {
            self.endpoint_latency.get(ep).map(|v| *v).unwrap_or(500)
        });
        eps
    }

    /// Spawn background thread that pings endpoints every 30s and ranks by RTT.
    pub fn spawn_endpoint_latency_probe(self: &std::sync::Arc<Self>) {
        let sender = std::sync::Arc::clone(self);
        std::thread::Builder::new()
            .name("relayer-ping".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("relayer-ping rt");
                let client = Client::builder()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .unwrap_or_default();

                loop {
                    let mut all_eps = vec![sender.endpoint.clone()];
                    for ep in FANOUT_ENDPOINTS {
                        all_eps.push(ep.to_string());
                    }
                    all_eps.sort();
                    all_eps.dedup();

                    for ep in &all_eps {
                        let start = std::time::Instant::now();
                        // Lightweight health check — just measure TCP connect + HTTP response.
                        let url = format!("{}/api/v1/bundles", ep);
                        let ok = rt.block_on(async {
                            client.get(&url).send().await.is_ok()
                        });
                        let rtt_ms = start.elapsed().as_millis() as u64;
                        if ok {
                            sender.endpoint_latency.insert(ep.clone(), rtt_ms);
                        } else {
                            // Penalty: 9999ms for unreachable endpoints.
                            sender.endpoint_latency.insert(ep.clone(), 9999);
                        }
                    }

                    let mut ranked: Vec<_> = sender.endpoint_latency.iter()
                        .map(|e| (e.key().clone(), *e.value()))
                        .collect();
                    ranked.sort_by_key(|(_,ms)| *ms);
                    let top3: Vec<String> = ranked.iter().take(3)
                        .map(|(ep, ms)| format!("{}={}ms", &ep[8..ep.find(".mainnet").unwrap_or(ep.len()).min(30)], ms))
                        .collect();
                    info!(endpoints = ?top3, "relayer-ping: RTT ranked");

                    std::thread::sleep(std::time::Duration::from_secs(30));
                }
            })
            .ok();
    }

    /// Spawn a background thread that refreshes the tip floor every 10s.
    pub fn spawn_tip_floor_refresh(self: &std::sync::Arc<Self>) {
        let sender = std::sync::Arc::clone(self);
        std::thread::Builder::new()
            .name("jito-tip-floor".into())
            .spawn(move || {
                let client = Client::builder()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .unwrap_or_default();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tip-floor rt");

                loop {
                    std::thread::sleep(std::time::Duration::from_secs(TIP_FLOOR_REFRESH_SECS));

                    let result: Result<u64, _> = rt.block_on(async {
                        let resp = client
                            .get("https://bundles.jito.wtf/api/v1/bundles/tip_floor")
                            .send()
                            .await?;
                        let json: serde_json::Value = resp.json().await?;
                        // Use p75 as our target — wins 75% of auctions.
                        if let Some(p75) = json[0]["landed_tips_75th_percentile"].as_f64() {
                            let lamports = (p75 * 1_000_000_000.0) as u64;
                            Ok(lamports)
                        } else {
                            anyhow::bail!("no p75 in tip_floor response")
                        }
                    });

                    match result {
                        Ok(tip) => {
                            let capped = tip.max(MIN_TIP_LAMPORTS).min(MAX_TIP_LAMPORTS);
                            let prev = sender.dynamic_tip.swap(capped, std::sync::atomic::Ordering::Relaxed);
                            if capped != prev {
                                info!(
                                    tip_lamports = capped,
                                    tip_sol = capped as f64 / 1e9,
                                    "Jito dynamic tip updated (p75)"
                                );
                            }
                        }
                        Err(e) => {
                            debug!(error = %e, "tip_floor refresh failed, keeping previous value");
                        }
                    }
                }
            })
            .expect("spawn tip-floor thread");
        info!("Jito dynamic tip floor refresh started (every {}s, p75, max {})",
            TIP_FLOOR_REFRESH_SECS, MAX_TIP_LAMPORTS);
    }

    /// Select the next tip account in round-robin fashion.
    pub fn tip_account(&self) -> Pubkey {
        let idx = self.request_id.load(std::sync::atomic::Ordering::Relaxed) as usize
            % JITO_TIP_ACCOUNTS.len();
        JITO_TIP_ACCOUNTS[idx].parse().expect("valid tip account")
    }

    /// Build a Jito tip transfer instruction (uses dynamic tip floor).
    pub fn tip_instruction(&self, payer: &Pubkey) -> Instruction {
        system_instruction::transfer(payer, &self.tip_account(), self.current_tip())
    }

    /// Serialise a transaction to base58.
    fn tx_to_base58(tx: &VersionedTransaction) -> Result<String> {
        let bytes = bincode::serialize(tx).context("serialise tx")?;
        Ok(bs58::encode(&bytes).into_string())
    }

    /// Send a bundle to the primary endpoint.
    /// Returns the bundle UUID on success.
    pub async fn send_bundle(&self, txs: &[VersionedTransaction]) -> Result<String> {
        self.send_bundle_to_endpoint(txs, &self.endpoint).await
    }

    /// Send a bundle to ALL regions in parallel (primary + fanout endpoints).
    /// Returns the first successful bundle_id. Fire-and-forget to other regions.
    pub async fn send_bundle_multi_region(&self, txs: &[VersionedTransaction]) -> Result<String> {
        if txs.is_empty() {
            bail!("empty bundle");
        }
        if txs.len() > 5 {
            bail!("Jito bundles are limited to 5 transactions");
        }

        let encoded: Result<Vec<String>> = txs.iter().map(Self::tx_to_base58).collect();
        let encoded = encoded?;

        let id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Endpoints sorted by latency (fastest first).
        let endpoints = self.sorted_endpoints();

        // Fire to all regions concurrently
        let mut handles = Vec::new();
        for ep in &endpoints {
            let client = self.client.clone();
            let url = format!("{}/api/v1/bundles", ep);
            let payload = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "sendBundle",
                "params": [&encoded]
            });
            let ep_name = ep.clone();
            handles.push(tokio::spawn(async move {
                let result = client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await;
                (ep_name, result)
            }));
        }

        let mut first_bundle_id: Option<String> = None;
        let mut regions_ok = 0u32;
        let mut regions_err = 0u32;

        for handle in handles {
            match handle.await {
                Ok((ep, Ok(response))) => {
                    if response.status().is_success() {
                        if let Ok(rpc_resp) = response.json::<JsonRpcResponse<String>>().await {
                            if rpc_resp.error.is_none() {
                                regions_ok += 1;
                                if first_bundle_id.is_none() {
                                    first_bundle_id = rpc_resp.result;
                                }
                                continue;
                            }
                        }
                    }
                    regions_err += 1;
                    debug!(endpoint = %ep, "Jito fanout: region rejected bundle");
                }
                Ok((ep, Err(e))) => {
                    regions_err += 1;
                    debug!(endpoint = %ep, error = %e, "Jito fanout: region request failed");
                }
                Err(_) => {
                    regions_err += 1;
                }
            }
        }

        if let Some(bundle_id) = first_bundle_id {
            info!(
                bundle_id = %bundle_id,
                regions_ok,
                regions_err,
                total = endpoints.len(),
                "Jito multi-region bundle accepted"
            );
            Ok(bundle_id)
        } else {
            bail!("Jito bundle rejected by all {} regions", endpoints.len())
        }
    }

    /// Send a single transaction via Jito's /api/v1/transactions endpoint.
    /// This uses sendTransaction (not sendBundle) — faster propagation, no atomicity.
    /// Sends to all regions in parallel for maximum coverage.
    pub async fn send_transaction_multi_region(&self, tx: &VersionedTransaction) -> Result<String> {
        let encoded = Self::tx_to_base58(tx)?;

        let id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let endpoints = self.sorted_endpoints();

        let mut handles = Vec::new();
        for ep in &endpoints {
            let client = self.client.clone();
            let url = format!("{}/api/v1/transactions", ep);
            let payload = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "sendTransaction",
                "params": [&encoded]
            });
            let ep_name = ep.clone();
            handles.push(tokio::spawn(async move {
                let result = client
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await;
                (ep_name, result)
            }));
        }

        let mut first_sig: Option<String> = None;
        let mut ok = 0u32;

        for handle in handles {
            if let Ok((_, Ok(response))) = handle.await {
                if response.status().is_success() {
                    if let Ok(rpc_resp) = response.json::<JsonRpcResponse<String>>().await {
                        if rpc_resp.error.is_none() {
                            ok += 1;
                            if first_sig.is_none() {
                                first_sig = rpc_resp.result;
                            }
                        }
                    }
                }
            }
        }

        if let Some(sig) = first_sig {
            debug!(sig = %sig, regions = ok, "Jito sendTransaction accepted");
            Ok(sig)
        } else {
            bail!("Jito sendTransaction rejected by all regions")
        }
    }

    /// Internal: send bundle to a specific endpoint.
    async fn send_bundle_to_endpoint(
        &self,
        txs: &[VersionedTransaction],
        endpoint: &str,
    ) -> Result<String> {
        if txs.is_empty() {
            bail!("empty bundle");
        }
        if txs.len() > 5 {
            bail!("Jito bundles are limited to 5 transactions");
        }

        let encoded: Result<Vec<String>> = txs.iter().map(Self::tx_to_base58).collect();
        let encoded = encoded?;

        let id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let payload = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: "sendBundle",
            params: [encoded],
        };

        let url = format!("{}/api/v1/bundles", endpoint);
        debug!(url = %url, txs = txs.len(), "sending Jito bundle");

        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("HTTP POST to Jito")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("Jito returned HTTP {}: {}", status, body);
        }

        let rpc_resp: JsonRpcResponse<String> =
            response.json().await.context("parse Jito response")?;

        if let Some(err) = rpc_resp.error {
            bail!("Jito RPC error {}: {}", err.code, err.message);
        }

        let bundle_id = rpc_resp.result.unwrap_or_else(|| "unknown".to_string());
        info!(bundle_id = %bundle_id, "Jito bundle accepted");
        Ok(bundle_id)
    }

    /// Query Jito for bundle status.
    pub async fn get_bundle_status(&self, bundle_id: &str) -> Result<BundleStatus> {
        #[derive(Deserialize, Debug)]
        struct StatusResult {
            bundle_id: String,
            status: String,
            landed_slot: Option<u64>,
        }

        let id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let payload = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: "getBundleStatuses",
            params: [[bundle_id]],
        };

        let url = format!("{}/api/v1/bundles", self.endpoint);

        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("HTTP POST getBundle status")?;

        #[derive(Deserialize)]
        struct Outer {
            value: Vec<StatusResult>,
        }

        let rpc_resp: JsonRpcResponse<Outer> =
            response.json().await.context("parse bundle status")?;

        let outer = rpc_resp.result.context("missing result in bundle status")?;
        let sr = outer
            .value
            .into_iter()
            .next()
            .context("empty bundle status")?;

        Ok(BundleStatus {
            bundle_id: sr.bundle_id,
            status: sr.status,
            landed_slot: sr.landed_slot,
        })
    }
}

// ---------------------------------------------------------------------------
// BundleStatus
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct BundleStatus {
    pub bundle_id: String,
    pub status: String,
    pub landed_slot: Option<u64>,
}

impl BundleStatus {
    pub fn is_landed(&self) -> bool {
        self.status == "Landed"
    }

    pub fn is_failed(&self) -> bool {
        self.status == "Failed" || self.status == "Invalid"
    }
}
