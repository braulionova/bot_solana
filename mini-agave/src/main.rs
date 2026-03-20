//! mini-agave — Lightweight Solana RPC node for Helios arb bot.
//!
//! Replaces the full Agave validator (~88GB RAM) with a ~1GB polling cache
//! that stays synchronized with mainnet in real-time.
//!
//! ┌──────────────────┬──────────────────────────────────┬──────────┐
//! │ Method           │ Backend                           │ Latency  │
//! ├──────────────────┼──────────────────────────────────┼──────────┤
//! │ getLatestBH      │ polling cache (400ms refresh)     │ ~0ms     │
//! │ getMultipleAccs  │ account cache (15-30s refresh)    │ ~0ms     │
//! │ getAccountInfo   │ account cache                     │ ~0ms     │
//! │ sendTransaction  │ TPU QUIC direct → 2 leaders       │ 5-20ms   │
//! │ simulateTx       │ proxy → upstream RPC              │ 80-150ms │
//! │ getHealth        │ local check                       │ ~0ms     │
//! │ getEpochInfo     │ proxy → upstream                  │ 80-150ms │
//! │ getSignatureStts │ proxy → upstream                  │ 80-150ms │
//! │ *                │ proxy → upstream                  │ 80-150ms │
//! └──────────────────┴──────────────────────────────────┴──────────┘

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod grpc_feed;
mod pool_loader;
mod tpu_sender;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use grpc_feed::GrpcState;
use tpu_sender::TpuSender;

// ─────────────────────────────────────────────────────────────────────────────
// Shared state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    grpc: Arc<GrpcState>,
    tpu: Option<Arc<TpuSender>>,
    proxy_client: reqwest::Client,
    upstream_url: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON-RPC types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    #[allow(dead_code)]
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
    (
        StatusCode::OK,
        Json(json!(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        })),
    )
}

fn err_resp(id: Value, code: i32, msg: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(json!({ "code": code, "message": msg })),
        })),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// RPC dispatcher
// ─────────────────────────────────────────────────────────────────────────────

async fn handle_rpc(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> (StatusCode, Json<Value>) {
    let id = req.id.clone().unwrap_or(Value::Null);

    match req.method.as_str() {
        "getLatestBlockhash" => handle_blockhash(state, id),
        "getMultipleAccounts" => handle_get_multiple_accounts(state, id, req.params).await,
        "getAccountInfo" => handle_get_account_info(state, id, req.params).await,
        "sendTransaction" => handle_send_transaction(state, id, req.params).await,
        "getHealth" => handle_health(state, id),
        "getSlot" => handle_get_slot(state, id),
        "getBlockHeight" => handle_get_block_height(state, id),
        "getEpochInfo" => handle_get_epoch_info(state, id),
        "getClusterNodes" => handle_get_cluster_nodes(state, id),
        "getLeaderSchedule" => handle_get_leader_schedule(state, id),
        "getVersion" => ok_resp(
            id,
            json!({
                "solana-core": "mini-agave-0.2.0",
                "feature-set": 0u64,
            }),
        ),
        // simulateTransaction, getSignatureStatuses, etc → proxy
        _ => proxy(state, req).await,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// getLatestBlockhash — from polling cache
// ─────────────────────────────────────────────────────────────────────────────

fn handle_blockhash(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    let slot = state.grpc.blockhash_slot.load(Ordering::Relaxed);
    if slot == 0 {
        return err_resp(id, -32005, "blockhash not yet available (warming up)");
    }
    let h = state.grpc.get_blockhash();
    ok_resp(
        id,
        json!({
            "context": { "slot": slot },
            "value": {
                "blockhash": h.to_string(),
                "lastValidBlockHeight": slot + 150,
            }
        }),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// getMultipleAccounts — serve from cache, fallback to upstream
// ─────────────────────────────────────────────────────────────────────────────

async fn handle_get_multiple_accounts(
    state: AppState,
    id: Value,
    params: Option<Value>,
) -> (StatusCode, Json<Value>) {
    let params = params.unwrap_or(Value::Null);
    let keys = match params.get(0).and_then(Value::as_array) {
        Some(a) => a,
        None => return err_resp(id, -32602, "missing pubkeys array"),
    };

    if !state.grpc.is_connected() || state.grpc.accounts.is_empty() {
        return proxy_raw(state, "getMultipleAccounts", params, id).await;
    }

    let slot = state.grpc.blockhash_slot.load(Ordering::Relaxed);
    let mut results = Vec::with_capacity(keys.len());
    let mut miss_count = 0usize;

    for kv in keys {
        if let Some(ks) = kv.as_str() {
            if let Ok(pk) = ks.parse::<solana_sdk::pubkey::Pubkey>() {
                if let Some(acc) = state.grpc.get_account(&pk) {
                    let data_b64 = B64.encode(&acc.data);
                    results.push(json!({
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
        miss_count += 1;
        results.push(Value::Null);
    }

    // Always serve from cache — nulls for missing accounts.
    // Pool hydrator handles nulls gracefully. Never block on upstream proxy.
    if miss_count > 0 {
        debug!(hits = keys.len() - miss_count, misses = miss_count, "getMultipleAccounts partial cache");
    }
    ok_resp(
        id,
        json!({ "context": { "slot": slot }, "value": results }),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// getAccountInfo — same pattern
// ─────────────────────────────────────────────────────────────────────────────

async fn handle_get_account_info(
    state: AppState,
    id: Value,
    params: Option<Value>,
) -> (StatusCode, Json<Value>) {
    let params = params.unwrap_or(Value::Null);

    if let Some(key_str) = params.get(0).and_then(Value::as_str) {
        if let Ok(pk) = key_str.parse::<solana_sdk::pubkey::Pubkey>() {
            if let Some(acc) = state.grpc.get_account(&pk) {
                let data_b64 = B64.encode(&acc.data);
                return ok_resp(
                    id,
                    json!({
                        "context": { "slot": acc.slot },
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

    // Not in cache — proxy to upstream (for accounts not in pool set)
    proxy_raw(state, "getAccountInfo", params, id).await
}

// ─────────────────────────────────────────────────────────────────────────────
// sendTransaction — TPU QUIC direct (5-20ms) → fallback RPC proxy
// ─────────────────────────────────────────────────────────────────────────────

async fn handle_send_transaction(
    state: AppState,
    id: Value,
    params: Option<Value>,
) -> (StatusCode, Json<Value>) {
    let params = params.unwrap_or(Value::Null);

    // Extract raw tx bytes
    let tx_str = match params.get(0).and_then(Value::as_str) {
        Some(s) => s,
        None => return err_resp(id, -32602, "missing transaction data"),
    };

    // Determine encoding (default base58, can be base64)
    let encoding = params
        .get(1)
        .and_then(|o| o.get("encoding"))
        .and_then(Value::as_str)
        .unwrap_or("base58");

    let tx_bytes = match encoding {
        "base64" => match B64.decode(tx_str) {
            Ok(b) => b,
            Err(e) => return err_resp(id, -32602, &format!("base64 decode: {e}")),
        },
        _ => match bs58::decode(tx_str).into_vec() {
            Ok(b) => b,
            Err(e) => return err_resp(id, -32602, &format!("base58 decode: {e}")),
        },
    };

    // Try TPU QUIC first (5-20ms direct to leaders)
    if let Some(ref tpu) = state.tpu {
        match tpu.send_wire(tx_bytes.clone()).await {
            Ok(sig) => {
                debug!(sig = %sig, "tx sent via TPU QUIC");
                return ok_resp(id, Value::String(sig));
            }
            Err(e) => {
                warn!(error = %e, "TPU send failed — falling back to RPC proxy");
            }
        }
    }

    // Fallback to upstream RPC proxy
    proxy_raw(state, "sendTransaction", params, id).await
}

// ─────────────────────────────────────────────────────────────────────────────
// getHealth
// ─────────────────────────────────────────────────────────────────────────────

fn handle_health(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    if state.grpc.is_connected() && state.grpc.blockhash_slot.load(Ordering::Relaxed) > 0 {
        ok_resp(id, json!("ok"))
    } else {
        err_resp(id, -32005, "not ready")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Proxy — forward to upstream RPC
// ─────────────────────────────────────────────────────────────────────────────

async fn proxy(state: AppState, req: JsonRpcRequest) -> (StatusCode, Json<Value>) {
    let id = req.id.clone().unwrap_or(Value::Null);
    let params = req.params.unwrap_or(Value::Null);
    proxy_raw(state, &req.method, params, id).await
}

async fn proxy_raw(
    state: AppState,
    method: &str,
    params: Value,
    id: Value,
) -> (StatusCode, Json<Value>) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    match state
        .proxy_client
        .post(&state.upstream_url)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => err_resp(id, -32603, &format!("upstream parse: {e}")),
        },
        Err(e) => {
            warn!(method, error = %e, "proxy error");
            err_resp(id, -32603, &format!("upstream: {e}"))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// getSlot — from current_slot atomic (updated by gRPC/polling)
// ─────────────────────────────────────────────────────────────────────────────

fn handle_get_slot(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    let slot = state.grpc.get_slot();
    if slot == 0 {
        return err_resp(id, -32005, "slot not yet available (warming up)");
    }
    ok_resp(id, json!(slot))
}

// ─────────────────────────────────────────────────────────────────────────────
// getBlockHeight — from block_height atomic
// ─────────────────────────────────────────────────────────────────────────────

fn handle_get_block_height(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    let height = state.grpc.block_height.load(Ordering::Relaxed);
    if height == 0 {
        return err_resp(id, -32005, "block height not yet available (warming up)");
    }
    ok_resp(id, json!(height))
}

// ─────────────────────────────────────────────────────────────────────────────
// getEpochInfo — from cached epoch info (refreshed every 60s by metadata poller)
// ─────────────────────────────────────────────────────────────────────────────

fn handle_get_epoch_info(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    if let Some(ei) = state.grpc.get_epoch_info() {
        // Use live slot/block_height from atomics (more current than cached snapshot)
        let live_slot = state.grpc.get_slot();
        let live_height = state.grpc.block_height.load(Ordering::Relaxed);
        let slot = if live_slot > ei.absolute_slot { live_slot } else { ei.absolute_slot };
        let height = if live_height > ei.block_height { live_height } else { ei.block_height };
        // Recompute slot_index from live slot
        let epoch_start = slot - (slot % ei.slots_in_epoch);
        let slot_index = slot - epoch_start;
        ok_resp(
            id,
            json!({
                "epoch": ei.epoch,
                "slotIndex": slot_index,
                "slotsInEpoch": ei.slots_in_epoch,
                "absoluteSlot": slot,
                "blockHeight": height,
                "transactionCount": ei.transaction_count,
            }),
        )
    } else {
        err_resp(id, -32005, "epoch info not yet available (warming up)")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// getClusterNodes — from cached cluster nodes (refreshed every 5min)
// ─────────────────────────────────────────────────────────────────────────────

fn handle_get_cluster_nodes(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    let nodes = state.grpc.get_cluster_nodes();
    if nodes.is_empty() {
        return err_resp(id, -32005, "cluster nodes not yet available (warming up)");
    }
    let nodes_json: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "pubkey": n.pubkey,
                "gossip": n.gossip,
                "tpu": n.tpu,
                "tpuQuic": n.tpu_quic,
                "rpc": n.rpc,
                "version": n.version,
            })
        })
        .collect();
    ok_resp(id, json!(nodes_json))
}

// ─────────────────────────────────────────────────────────────────────────────
// getLeaderSchedule — from cached leader schedule (refreshed per epoch)
// ─────────────────────────────────────────────────────────────────────────────

fn handle_get_leader_schedule(state: AppState, id: Value) -> (StatusCode, Json<Value>) {
    match state.grpc.get_leader_schedule() {
        Some(schedule) => ok_resp(id, schedule),
        None => err_resp(id, -32005, "leader schedule not yet available (warming up)"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Health endpoint (plain HTTP)
// ─────────────────────────────────────────────────────────────────────────────

async fn health_check(State(state): State<AppState>) -> &'static str {
    if state.grpc.is_connected() && state.grpc.blockhash_slot.load(Ordering::Relaxed) > 0 {
        "ok"
    } else {
        "warming up"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Metrics endpoint
// ─────────────────────────────────────────────────────────────────────────────

async fn metrics(State(state): State<AppState>) -> String {
    let g = &state.grpc;
    format!(
        "connected={}\n\
         mode={}\n\
         tpu_enabled={}\n\
         blockhash_slot={}\n\
         accounts_cached={}\n\
         account_updates={}\n\
         blockhash_updates={}\n\
         dex_tx_count={}\n",
        g.is_connected(),
        g.get_mode(),
        state.tpu.is_some(),
        g.blockhash_slot.load(Ordering::Relaxed),
        g.accounts.len(),
        g.account_updates.load(Ordering::Relaxed),
        g.blockhash_updates.load(Ordering::Relaxed),
        g.dex_tx_count.load(Ordering::Relaxed),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_thread_names(true)
        .init();

    let port: u16 = std::env::var("MINI_AGAVE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8082);

    let grpc_endpoint = std::env::var("GRPC_ENDPOINT").unwrap_or_default();
    let grpc_token = std::env::var("GRPC_X_TOKEN").unwrap_or_default();

    let upstream_rpc = std::env::var("UPSTREAM_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    let poll_rpc = std::env::var("POLL_RPC_URL")
        .unwrap_or_else(|_| upstream_rpc.clone());

    let pools_file = std::env::var("POOLS_FILE")
        .unwrap_or_else(|_| "/root/solana-bot/mapped_pools.json".to_string());

    let enable_dex = std::env::var("ENABLE_DEX_SIGNALS")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "FALSE"))
        .unwrap_or(false);

    // TPU config — needs RPC + WS for leader schedule discovery
    let tpu_rpc_url = std::env::var("TPU_RPC_URL")
        .unwrap_or_else(|_| upstream_rpc.clone());
    let tpu_ws_url = std::env::var("TPU_WS_URL")
        .unwrap_or_else(|_| {
            // Convert http(s) to ws(s) for TPU leader schedule
            tpu_rpc_url
                .replace("https://", "wss://")
                .replace("http://", "ws://")
        });
    let enable_tpu = std::env::var("ENABLE_TPU")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "FALSE"))
        .unwrap_or(true);

    info!(port, "mini-agave starting");
    info!(upstream = %upstream_rpc, "upstream RPC (proxy + simulation)");
    info!(poll_rpc = %poll_rpc, "polling RPC (blockhash)");
    if !grpc_endpoint.is_empty() {
        info!(grpc_endpoint = %grpc_endpoint, "gRPC endpoint (push mode)");
    } else {
        info!("no GRPC_ENDPOINT — polling-only mode");
    }
    info!(pools_file = %pools_file, "pool accounts source");

    // ── Phase 1: Load pool vault accounts ────────────────────────────────────
    let bootstrap_rpc = std::env::var("BOOTSTRAP_RPC_URL")
        .unwrap_or_else(|_| upstream_rpc.clone());

    let account_keys = match pool_loader::load_pool_vaults(&pools_file, &bootstrap_rpc) {
        Ok(keys) => {
            info!(accounts = keys.len(), "pool vault discovery complete");
            keys
        }
        Err(e) => {
            warn!(error = %e, "pool vault discovery failed — no account cache");
            vec![]
        }
    };

    // ── Phase 2: Start state feed (polling + optional gRPC) ──────────────────
    let grpc_state = GrpcState::new();

    let (dex_tx, _dex_rx) = if enable_dex {
        let (tx, rx) = crossbeam_channel::bounded::<grpc_feed::DexSignal>(256);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    grpc_feed::spawn_feed(
        Arc::clone(&grpc_state),
        grpc_endpoint,
        grpc_token,
        poll_rpc,
        upstream_rpc.clone(),
        account_keys,
        dex_tx,
    );

    // ── Phase 3: Initialize TPU sender ───────────────────────────────────────
    let tpu = if enable_tpu {
        match TpuSender::new(&tpu_rpc_url, &tpu_ws_url).await {
            Ok(sender) => {
                info!("TPU QUIC sender ready — sendTransaction via direct QUIC");
                Some(Arc::new(sender))
            }
            Err(e) => {
                warn!(error = %e, "TPU sender init failed — sendTransaction will proxy");
                None
            }
        }
    } else {
        info!("TPU sender disabled (ENABLE_TPU=false)");
        None
    };

    // ── Phase 4: Start RPC server ────────────────────────────────────────────
    let proxy_client = reqwest::Client::builder()
        .http2_adaptive_window(true)
        .tcp_nodelay(true)
        .timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(std::time::Duration::from_secs(120))
        .tcp_keepalive(std::time::Duration::from_secs(15))
        .build()?;

    let state = AppState {
        grpc: grpc_state,
        tpu,
        proxy_client,
        upstream_url: upstream_rpc,
    };

    // Spawn metrics logger
    let metrics_state_grpc = state.grpc.clone();
    let metrics_tpu = state.tpu.is_some();
    std::thread::Builder::new()
        .name("metrics-log".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let g = &metrics_state_grpc;
            info!(
                connected = g.is_connected(),
                mode = %g.get_mode(),
                tpu = metrics_tpu,
                bh_slot = g.blockhash_slot.load(Ordering::Relaxed),
                cached = g.accounts.len(),
                acc_updates = g.account_updates.load(Ordering::Relaxed),
                bh_updates = g.blockhash_updates.load(Ordering::Relaxed),
                dex_txs = g.dex_tx_count.load(Ordering::Relaxed),
                "mini-agave throughput"
            );
        })
        .ok();

    let app = Router::new()
        .route("/", post(handle_rpc))
        .route("/health", get(health_check))
        .route("/metrics", get(metrics))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!(port, "mini-agave RPC server listening — ready to serve");

    axum::serve(listener, app).tcp_nodelay(true).await?;

    Ok(())
}
