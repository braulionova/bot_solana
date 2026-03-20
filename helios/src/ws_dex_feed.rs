/// ws_dex_feed.rs — Helius WebSocket DEX trigger feed (async tokio-tungstenite).
///
/// Una sola conexión WS con 4 logsSubscribe multiplexados — evita el límite de
/// conexiones concurrentes del plan Helius free (2 conex. máx).
///
/// Latencia: ~150-300ms desde processed commitment.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message};
use tracing::{info, warn};

use spy_node::signal_bus::{LiquidityEventType, SpySignal};

const DEX_PROGRAMS: &[(&str, &str)] = &[
    ("Raydium AMM V4",  "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"),
    ("Orca Whirlpool",  "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
    ("Meteora DLMM",    "LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS"),
    ("PumpSwap AMM",    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
];

const MIN_TRIGGER_INTERVAL_MS: u64 = 100;

pub fn spawn_ws_dex_feed(rpc_url: &str, sig_tx: Sender<SpySignal>) {
    let ws_url = std::env::var("WS_RPC_URL").unwrap_or_else(|_| rpc_to_ws_url(rpc_url));
    info!(ws_url = %ws_url, programs = DEX_PROGRAMS.len(), "starting WS DEX feed (multiplexed)");

    thread::Builder::new()
        .name("ws-dex-mux".into())
        .spawn(move || {
            static LAST_TRIGGER_MS: AtomicU64 = AtomicU64::new(0);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt ws-dex-mux");
            rt.block_on(run_mux_loop(ws_url, sig_tx, &LAST_TRIGGER_MS));
        })
        .expect("spawn ws-dex-mux thread");
}

async fn run_mux_loop(
    ws_url: String,
    sig_tx: Sender<SpySignal>,
    last_trigger_ms: &'static AtomicU64,
) {
    let mut backoff = Duration::from_secs(3);
    loop {
        match connect_mux(&ws_url, &sig_tx, last_trigger_ms).await {
            Ok(()) => {
                warn!("WS DEX mux connection ended, reconnecting in 3s");
                backoff = Duration::from_secs(3);
            }
            Err(e) => {
                warn!(error = %e, ?backoff, "WS DEX mux error, reconnecting");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn connect_mux(
    ws_url: &str,
    sig_tx: &Sender<SpySignal>,
    last_trigger_ms: &AtomicU64,
) -> anyhow::Result<()> {
    let url = url::Url::parse(ws_url)
        .map_err(|e| anyhow::anyhow!("invalid WS URL {ws_url}: {e}"))?;

    let (ws_stream, _) = connect_async_tls_with_config(url, None, false, None)
        .await
        .map_err(|e| anyhow::anyhow!("WS connect to {ws_url}: {e}"))?;

    let (mut sink, mut stream) = ws_stream.split();

    // Enviar las 4 suscripciones usando IDs 1..4
    for (i, (_name, program_id)) in DEX_PROGRAMS.iter().enumerate() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": i + 1,
            "method": "logsSubscribe",
            "params": [
                { "mentions": [program_id] },
                { "commitment": "processed" }
            ]
        })
        .to_string();
        sink.send(Message::Text(msg))
            .await
            .map_err(|e| anyhow::anyhow!("WS send subscribe: {e}"))?;
    }

    // sub_id → program name (filled as confirmations arrive)
    let mut sub_map: HashMap<u64, &str> = HashMap::new();
    // request id (1-4) → program name
    let req_map: HashMap<usize, &str> = DEX_PROGRAMS
        .iter()
        .enumerate()
        .map(|(i, (name, _))| (i + 1, *name))
        .collect();

    while let Some(raw) = stream.next().await {
        let raw = raw.map_err(|e| anyhow::anyhow!("WS recv: {e}"))?;

        let text = match raw {
            Message::Text(t) => t,
            Message::Ping(payload) => {
                let _ = sink.send(Message::Pong(payload)).await;
                continue;
            }
            Message::Close(_) => return Ok(()),
            _ => continue,
        };

        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Confirmación de suscripción: {"id": N, "result": <sub_id>}
        if let (Some(id), Some(result)) = (value.get("id"), value.get("result")) {
            if let (Some(req_id), Some(sub_id)) = (id.as_u64(), result.as_u64()) {
                if let Some(&name) = req_map.get(&(req_id as usize)) {
                    info!(name, sub_id, "WS DEX subscription confirmed");
                    sub_map.insert(sub_id, name);
                }
            }
            continue;
        }

        // Notificación: {"method":"logsNotification","params":{"subscription":N,"result":{...}}}
        let slot = value["params"]["result"]["context"]["slot"]
            .as_u64()
            .unwrap_or(0);

        let now_ms = now_ms();
        let prev = last_trigger_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < MIN_TRIGGER_INTERVAL_MS {
            continue;
        }
        last_trigger_ms.store(now_ms, Ordering::Relaxed);

        let signal = SpySignal::LiquidityEvent {
            slot,
            pool: solana_sdk::pubkey::Pubkey::default(),
            event_type: LiquidityEventType::Added {
                amount_a: 0,
                amount_b: 0,
            },
        };
        let _ = sig_tx.try_send(signal);
    }

    Ok(())
}

fn rpc_to_ws_url(rpc_url: &str) -> String {
    if rpc_url.starts_with("https://") {
        format!("wss://{}", &rpc_url[8..])
    } else if rpc_url.starts_with("http://") {
        format!("ws://{}", &rpc_url[7..])
    } else {
        rpc_url.to_string()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
