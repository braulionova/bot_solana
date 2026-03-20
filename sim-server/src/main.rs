//! sim-server – Helios Gold V2 ultra-low latency RPC hub.
//!
//! Acts as a transparent JSON-RPC proxy with hot-path optimizations:
//!
//! ┌────────────────┬────────────────────────────────────────────┬──────────┐
//! │ Method         │ Backend                                    │ Latency  │
//! ├────────────────┼────────────────────────────────────────────┼──────────┤
//! │ simulateTx     │ BanksClient (if BANKS_SERVER_ADDR set)     │ 30-60ms  │
//! │                │ → HTTP/2 pool to Helius (fallback)         │ 80-150ms │
//! │ getLatestBH    │ Local cache (400ms refresh)                │ ~0ms     │
//! │ sendTransaction│ TpuClient → QUIC/UDP to current leader(s)  │ 5-20ms   │
//! │ *              │ HTTP/2 transparent proxy to Helius          │ 80-150ms │
//! └────────────────┴────────────────────────────────────────────┴──────────┘
//!
//! Environment variables:
//!   SIM_SERVER_PORT    – port to listen on (default: 8081)
//!   UPSTREAM_RPC_URL   – Helius HTTPS endpoint (proxy + TpuClient leader schedule)
//!   UPSTREAM_WS_URL    – Helius WSS endpoint  (TpuClient slot subscription)
//!   BANKS_SERVER_ADDR  – host:port of BanksServer (enables BanksClient simulate)

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tracing::{debug, info, warn};

mod banks_backend;
mod blockhash_cache;
mod geyser_cache;
mod http_backend;
mod tpu_sender;

use blockhash_cache::BlockhashCache;
use geyser_cache::GeyserCache;
use http_backend::HttpBackend;
use tpu_sender::TpuSender;

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait SimulationBackend: Send + Sync {
    async fn simulate(
        &self,
        tx_bytes: Vec<u8>,
        replace_blockhash: bool,
        recent_blockhash: solana_sdk::hash::Hash,
    ) -> anyhow::Result<SimulateResponse>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SimulateResponse {
    pub err: Option<String>,
    pub logs: Vec<String>,
    pub units_consumed: Option<u64>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    blockhash_cache: Arc<BlockhashCache>,
    geyser: Option<Arc<GeyserCache>>,
    sim_backend: Arc<dyn SimulationBackend + Send + Sync>,
    tpu_sender: Option<Arc<TpuSender>>,
    proxy: Arc<HttpBackend>, // fallback proxy for unknown methods
}

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

fn ok_resp(id: Value, result: Value) -> (StatusCode, Json<Value>) {
    let r = JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(result),
        error: None,
    };
    (StatusCode::OK, Json(json!(r)))
}

fn err_resp(id: Value, code: i32, msg: &str) -> (StatusCode, Json<Value>) {
    let r = JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(json!({ "code": code, "message": msg })),
    };
    (StatusCode::OK, Json(json!(r)))
}

// ---------------------------------------------------------------------------
// Main dispatcher
// ---------------------------------------------------------------------------

async fn handle_rpc(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> (StatusCode, Json<Value>) {
    let id = req.id.clone().unwrap_or(Value::Null);

    match req.method.as_str() {
        "simulateTransaction" => simulate_handler(state, id, req.params).await,
        "getLatestBlockhash" => blockhash_handler(state, id).await,
        "sendTransaction" => send_handler(state, id, req.params).await,
        "getMultipleAccounts" => get_multiple_accounts_handler(state, id, req.params).await,
        "getAccountInfo" => get_account_info_handler(state, id, req.params).await,
        // Return hardcoded version to avoid proxying to Helius (rate limited).
        // RpcClient calls getVersion on init; this prevents the 429 cascade.
        "getVersion" => ok_resp(id, serde_json::json!({
            "solana-core": "1.18.26",
            "feature-set": 4215500110u64,
        })),
        // getEpochInfo is called by TpuSender refresh; serve from proxy but
        // don't fail the executor if it's rate-limited.
        _ => proxy_handler(state, req).await,
    }
}

// ---------------------------------------------------------------------------
// simulateTransaction
// ---------------------------------------------------------------------------

async fn simulate_handler(
    state: AppState,
    id: Value,
    params: Option<Value>,
) -> (StatusCode, Json<Value>) {
    let params = params.unwrap_or(Value::Null);

    let tx_b64 = match params.get(0).and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return err_resp(id, -32602, "missing transaction"),
    };

    let replace_blockhash = params
        .get(1)
        .and_then(|o| o.get("replaceRecentBlockhash"))
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let tx_bytes = match B64.decode(&tx_b64) {
        Ok(b) => b,
        Err(_) => return err_resp(id, -32602, "invalid base64"),
    };

    let blockhash = state.blockhash_cache.get();

    // Try primary backend first; fall back to HTTP proxy on error.
    let sim_result = match state
        .sim_backend
        .simulate(tx_bytes.clone(), replace_blockhash, blockhash)
        .await
    {
        Ok(sim) => Ok(sim),
        Err(e) => {
            warn!("simulate primary error: {}, falling back to HTTP proxy", e);
            state
                .proxy
                .simulate(tx_bytes, replace_blockhash, blockhash)
                .await
        }
    };

    match sim_result {
        Ok(sim) => {
            // Parse err as JSON Value so the Solana RPC client can deserialize it
            // as a proper TransactionError enum (not a plain string).
            let err_value: Value = match &sim.err {
                Some(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
                None => Value::Null,
            };
            ok_resp(
                id,
                json!({
                    "context": { "slot": 0 },
                    "value": {
                        "err":          err_value,
                        "logs":         sim.logs,
                        "accounts":     null,
                        "unitsConsumed": sim.units_consumed,
                        "returnData":   null,
                    }
                }),
            )
        }
        Err(e) => {
            warn!("simulate error (both backends): {}", e);
            err_resp(id, -32603, &format!("simulation backend error: {}", e))
        }
    }
}

// ---------------------------------------------------------------------------
// sendTransaction  (TpuClient → QUIC/UDP to leaders, ~5-20ms)
// ---------------------------------------------------------------------------

async fn send_handler(
    state: AppState,
    id: Value,
    params: Option<Value>,
) -> (StatusCode, Json<Value>) {
    let params = params.unwrap_or(Value::Null);
    let encoding = params
        .get(1)
        .and_then(|o| o.get("encoding"))
        .and_then(Value::as_str)
        .unwrap_or("base64");

    let tx_encoded = match params.get(0).and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return err_resp(id, -32602, "missing transaction"),
    };

    let tx_bytes = if encoding == "base58" {
        match bs58::decode(&tx_encoded).into_vec() {
            Ok(b) => b,
            Err(_) => return err_resp(id, -32602, "invalid base58"),
        }
    } else {
        match B64.decode(&tx_encoded) {
            Ok(b) => b,
            Err(_) => return err_resp(id, -32602, "invalid base64"),
        }
    };

    // If TpuSender is available, send directly to validators via QUIC.
    if let Some(tpu) = &state.tpu_sender {
        match tpu.send_wire(tx_bytes).await {
            Ok(sig) => {
                debug!(sig = %sig, "sendTransaction via TpuClient (QUIC)");
                return ok_resp(id, Value::String(sig));
            }
            Err(e) => {
                warn!(error = %e, "TpuClient send failed, falling back to RPC proxy");
            }
        }
    }

    // Fallback: forward to upstream RPC.
    proxy_handler(
        state,
        JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(id.clone()),
            method: "sendTransaction".into(),
            params: Some(params),
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Transparent proxy  (all other methods → upstream Helius HTTP/2)
// ---------------------------------------------------------------------------

async fn proxy_handler(state: AppState, req: JsonRpcRequest) -> (StatusCode, Json<Value>) {
    let id = req.id.clone().unwrap_or(Value::Null);
    let body = json!({
        "jsonrpc": req.jsonrpc.unwrap_or_else(|| "2.0".into()),
        "id": id,
        "method": req.method,
        "params": req.params.unwrap_or(Value::Null),
    });

    match state.proxy.forward(body).await {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => {
            warn!("proxy error: {}", e);
            err_resp(id, -32603, &format!("upstream error: {}", e))
        }
    }
}

// ---------------------------------------------------------------------------
// getLatestBlockhash — prefer Geyser real-time hash, fall back to polling cache
// ---------------------------------------------------------------------------

async fn blockhash_handler(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    let h = if let Some(g) = &state.geyser {
        g.get_blockhash()
    } else {
        state.blockhash_cache.get()
    };
    ok_resp(
        id,
        json!({
            "context": { "slot": 0 },
            "value": {
                "blockhash": h.to_string(),
                "lastValidBlockHeight": 0,
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// getMultipleAccounts — serve from Geyser RAM cache; fallback to upstream
// ---------------------------------------------------------------------------

async fn get_multiple_accounts_handler(
    state: AppState,
    id: Value,
    params: Option<Value>,
) -> (StatusCode, Json<Value>) {
    let params = params.unwrap_or(Value::Null);
    let keys_raw = match params.get(0).and_then(Value::as_array) {
        Some(a) => a.clone(),
        None => return err_resp(id, -32602, "missing pubkeys array"),
    };

    // If Geyser cache is active, try to serve all accounts from RAM.
    if let Some(geyser) = &state.geyser {
        if geyser.is_connected() {
            let mut all_found = true;
            let mut result_accounts = Vec::new();

            for kv in &keys_raw {
                if let Some(ks) = kv.as_str() {
                    if let Ok(pk) = ks.parse::<solana_sdk::pubkey::Pubkey>() {
                        if let Some(acc) = geyser.get_account(&pk) {
                            let data_b64 = base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD,
                                &acc.data,
                            );
                            result_accounts.push(json!({
                                "lamports": acc.lamports,
                                "owner": acc.owner.to_string(),
                                "data": [data_b64, "base64"],
                                "executable": acc.executable,
                                "rentEpoch": acc.rent_epoch,
                            }));
                            continue;
                        }
                    }
                }
                all_found = false;
                break;
            }

            if all_found {
                debug!(accounts = result_accounts.len(), "getMultipleAccounts served from Geyser cache");
                return ok_resp(
                    id,
                    json!({ "context": { "slot": 0 }, "value": result_accounts }),
                );
            }
        }
    }

    // Fallback to upstream proxy.
    proxy_handler(
        state,
        JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(id),
            method: "getMultipleAccounts".into(),
            params: Some(params),
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// getAccountInfo — same pattern: Geyser cache → upstream proxy
// ---------------------------------------------------------------------------

async fn get_account_info_handler(
    state: AppState,
    id: Value,
    params: Option<Value>,
) -> (StatusCode, Json<Value>) {
    let params = params.unwrap_or(Value::Null);

    if let Some(geyser) = &state.geyser {
        if geyser.is_connected() {
            if let Some(key_str) = params.get(0).and_then(Value::as_str) {
                if let Ok(pk) = key_str.parse::<solana_sdk::pubkey::Pubkey>() {
                    if let Some(acc) = geyser.get_account(&pk) {
                        let data_b64 = base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &acc.data,
                        );
                        return ok_resp(
                            id,
                            json!({
                                "context": { "slot": 0 },
                                "value": {
                                    "lamports": acc.lamports,
                                    "owner": acc.owner.to_string(),
                                    "data": [data_b64, "base64"],
                                    "executable": acc.executable,
                                    "rentEpoch": acc.rent_epoch,
                                }
                            }),
                        );
                    }
                }
            }
        }
    }

    proxy_handler(
        state,
        JsonRpcRequest {
            jsonrpc: Some("2.0".into()),
            id: Some(id),
            method: "getAccountInfo".into(),
            params: Some(params),
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn health() -> &'static str {
    "ok"
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Telegram helpers (inline – no external crate dependency beyond reqwest)
// ---------------------------------------------------------------------------

struct Telegram {
    client: reqwest::Client,
    token: String,
    chat_id: String,
}

impl Telegram {
    fn from_env() -> Option<Self> {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").ok()?;
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok()?;
        if token.is_empty() || chat_id.is_empty() {
            return None;
        }
        Some(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            token,
            chat_id,
        })
    }

    async fn send(&self, text: &str) {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "Markdown",
            "disable_web_page_preview": true,
        });
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            warn!(error = %e, "[telegram] send failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_thread_names(true)
        .init();

    let port: u16 = std::env::var("SIM_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8081);

    let tg = Telegram::from_env();
    if let Some(ref t) = tg {
        t.send(&format!(
            "🔄 *sim-server arrancando*\nPuerto: `{}`\nCargando snapshot y backends...",
            port
        ))
        .await;
    }

    let upstream_rpc = std::env::var("UPSTREAM_RPC_URL")
        .or_else(|_| std::env::var("RPC_URL"))
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    let upstream_ws = std::env::var("UPSTREAM_WS_URL").unwrap_or_else(|_| {
        upstream_rpc
            .replace("https://", "wss://")
            .replace("http://", "ws://")
    });

    let banks_addr = std::env::var("BANKS_SERVER_ADDR").ok();

    // ── Simulation backend ───────────────────────────────────────────────────
    let sim_backend: Arc<dyn SimulationBackend + Send + Sync> = if let Some(ref addr) = banks_addr {
        info!(addr = %addr, "trying BanksClient mode");
        if let Some(ref t) = tg {
            t.send(&format!(
                "📦 *BanksClient (snapshot)*\nConectando a `{}`...",
                addr
            ))
            .await;
        }
        match banks_backend::BanksBackend::new(addr).await {
            Ok(b) => {
                info!("BanksClient connected – ultra-low-latency simulation active");
                if let Some(ref t) = tg {
                    t.send("✅ *BanksClient conectado*\nSimulación ultra-baja latencia activa (30-60ms)")
                        .await;
                }
                Arc::new(b)
            }
            Err(e) => {
                warn!(error = %e, "BanksClient unavailable, using HTTP/2 proxy");
                if let Some(ref t) = tg {
                    t.send(&format!(
                        "⚠️ *BanksClient fallido*\n`{}`\nUsando proxy HTTP/2 → Helius (80-150ms)",
                        e
                    ))
                    .await;
                }
                Arc::new(HttpBackend::new(&upstream_rpc))
            }
        }
    } else {
        info!(upstream = %upstream_rpc, "HTTP/2 proxy simulation mode");
        if let Some(ref t) = tg {
            t.send("ℹ️ *Modo proxy HTTP/2*\nBANKS\\_SERVER\\_ADDR no configurado → Helius (80-150ms)")
                .await;
        }
        Arc::new(HttpBackend::new(&upstream_rpc))
    };

    // ── TPU sender (direct to validators) ────────────────────────────────────
    let tpu_sender: Option<Arc<TpuSender>> = {
        info!(rpc = %upstream_rpc, ws = %upstream_ws, "starting TpuClient");
        match TpuSender::new(&upstream_rpc, &upstream_ws).await {
            Ok(s) => {
                info!("TpuClient ready – sendTransaction will bypass JSON-RPC");
                if let Some(ref t) = tg {
                    t.send("✅ *TpuClient listo*\nsendTransaction directo a validadores (QUIC, 5-20ms)")
                        .await;
                }
                Some(Arc::new(s))
            }
            Err(e) => {
                warn!(error = %e, "TpuClient init failed, sendTransaction will be proxied");
                if let Some(ref t) = tg {
                    t.send(&format!(
                        "⚠️ *TpuClient fallido*\n`{}`\nsendTransaction via proxy RPC",
                        e
                    ))
                    .await;
                }
                None
            }
        }
    };

    // ── Blockhash polling cache (400ms fallback) ──────────────────────────────
    // Use BLOCKHASH_RPC_URL if set (e.g. public mainnet) to avoid spending
    // Helius credits on the 2.5 req/s blockhash poll.
    let blockhash_rpc = std::env::var("BLOCKHASH_RPC_URL").unwrap_or_else(|_| upstream_rpc.clone());
    let blockhash_cache = BlockhashCache::new(&blockhash_rpc, Duration::from_millis(400));

    // ── Geyser real-time cache (optional, enabled via GEYSER_ENDPOINT) ────────
    // Shares the same Arc<RwLock<Hash>> with blockhash_cache so Geyser writes
    // update the same slot that the polling-based code reads.
    let geyser = GeyserCache::try_start(
        // Pool vault accounts to subscribe to can be injected via env.
        // Format: GEYSER_ACCOUNTS=<pubkey1>,<pubkey2>,...
        std::env::var("GEYSER_ACCOUNTS")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.trim().parse::<solana_sdk::pubkey::Pubkey>().ok())
            .collect(),
        blockhash_cache.inner_arc(),
    );
    if geyser.is_some() {
        info!("Geyser cache active — blockhash and account data served from RAM");
        if let Some(ref t) = tg {
            t.send("✅ *Geyser cache activo*\nblockhash y cuentas servidos desde RAM")
                .await;
        }
    } else {
        info!("Geyser cache inactive — set GEYSER_ENDPOINT to enable");
    }

    // ── Transparent proxy (catch-all) ─────────────────────────────────────────
    let proxy = Arc::new(HttpBackend::new(&upstream_rpc));

    let state = AppState {
        blockhash_cache,
        geyser,
        sim_backend,
        tpu_sender,
        proxy,
    };

    let app = Router::new()
        .route("/", post(handle_rpc))
        .route("/health", get(health))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let listener = tokio::net::TcpListener::bind(addr).await?;

    if let Some(ref t) = tg {
        t.send(&format!(
            "🟢 *sim-server listo*\nEscuchando en `localhost:{}`\nRPC disponible — helios-bot puede conectar",
            port
        ))
        .await;
    }

    info!(port, "sim-server listening on localhost");

    // tcp_nodelay: disable Nagle's algorithm – each response is flushed immediately
    // without buffering (critical for sub-millisecond loopback latency).
    axum::serve(listener, app).tcp_nodelay(true).await?;

    Ok(())
}
