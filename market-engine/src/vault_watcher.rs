// vault_watcher.rs — Real-time vault balance tracking via WebSocket accountSubscribe.
//
// Subscribes to cross-DEX vault account changes via RPC WebSocket.
// When a vault balance changes on-chain, updates pool cache in ~300ms.
// This replaces the need for Agave Geyser push.
//
// Strategy: subscribe only to ~200 cross-DEX vaults (not all 4956).
// These are the only vaults where arb opportunities exist.
// Uses multiple WS connections (40 subs each) round-robin across RPC endpoints.

use std::sync::Arc;
use std::collections::HashMap;

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn, debug};

use crate::pool_state::PoolStateCache;
use crate::types::DexType;

const MAX_SUBS_PER_CONN: usize = 35; // conservative, public RPCs limit ~40

/// Spawn vault watcher that tracks cross-DEX vaults via WebSocket.
pub fn spawn_vault_watcher(
    pool_cache: Arc<PoolStateCache>,
    ws_urls: Vec<String>,
) {
    // Load ML-prioritized vault list (scored by activity, volatility, cross-DEX paths).
    // Falls back to cross-DEX static selection if ML file doesn't exist.
    let vault_index = pool_cache.build_vault_index();
    let ml_vaults: Option<Vec<Pubkey>> = std::fs::read_to_string("/root/spy_node/deploy/ml_vault_priorities.json")
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.iter().filter_map(|s| s.parse::<Pubkey>().ok()).collect());

    let cross_vaults: Vec<(Pubkey, Pubkey, bool)> = if let Some(ref ml) = ml_vaults {
        // Use ML-scored vaults (top by activity + volatility + cross-DEX)
        let ml_set: std::collections::HashSet<Pubkey> = ml.iter().copied().collect();
        vault_index.iter()
            .filter(|(vault, _)| ml_set.contains(vault))
            .map(|(v, (p, a))| (*v, *p, *a))
            .collect()
    } else {
        // Fallback: all cross-DEX vaults
        let wsol = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
        let mut token_dexes: HashMap<Pubkey, std::collections::HashSet<DexType>> = HashMap::new();
        for pool in pool_cache.inner_iter() {
            let token = if pool.token_a == wsol { pool.token_b }
                       else if pool.token_b == wsol { pool.token_a }
                       else { continue };
            token_dexes.entry(token).or_default().insert(pool.dex_type);
        }
        let cross_tokens: std::collections::HashSet<Pubkey> = token_dexes.iter()
            .filter(|(_, d)| d.len() >= 2)
            .map(|(t, _)| *t)
            .collect();
        vault_index.iter()
            .filter(|(_, (pool_addr, _))| {
                pool_cache.get(pool_addr).map_or(false, |p|
                    cross_tokens.contains(&p.token_a) || cross_tokens.contains(&p.token_b))
            })
            .map(|(v, (p, a))| (*v, *p, *a))
            .collect()
    };

    info!(
        ml_scored = ml_vaults.is_some(),
        "vault-watcher: using {} vault selection",
        if ml_vaults.is_some() { "ML-prioritized" } else { "static cross-DEX" }
    );

    if cross_vaults.is_empty() {
        warn!("vault-watcher: 0 cross-DEX vaults to watch");
        return;
    }

    let n_conns = (cross_vaults.len() + MAX_SUBS_PER_CONN - 1) / MAX_SUBS_PER_CONN;
    info!(
        vaults = cross_vaults.len(),
        connections = n_conns,
        "vault-watcher: starting WebSocket vault tracking"
    );

    // Build vault→(pool, is_a) lookup for fast updates
    let vault_map: Arc<DashMap<Pubkey, (Pubkey, bool)>> = Arc::new(DashMap::new());
    for (v, p, a) in &cross_vaults {
        vault_map.insert(*v, (*p, *a));
    }

    // Spawn one thread per WS connection
    for (i, chunk) in cross_vaults.chunks(MAX_SUBS_PER_CONN).enumerate() {
        let cache = pool_cache.clone();
        let vmap = vault_map.clone();
        let vaults: Vec<Pubkey> = chunk.iter().map(|(v, _, _)| *v).collect();
        let ws_url = ws_urls[i % ws_urls.len()].clone();

        std::thread::Builder::new()
            .name(format!("vault-ws-{}", i))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("vault-ws tokio rt");
                loop {
                    rt.block_on(async {
                        if let Err(e) = ws_session(&cache, &vmap, &vaults, &ws_url).await {
                            warn!(error = %e, conn = i, "vault-watcher: session ended");
                        }
                    });
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            })
            .expect("spawn vault-ws");
    }
}

async fn ws_session(
    cache: &Arc<PoolStateCache>,
    vault_map: &Arc<DashMap<Pubkey, (Pubkey, bool)>>,
    vaults: &[Pubkey],
    ws_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio_tungstenite::connect_async;
    use futures_util::{SinkExt, StreamExt};

    let (mut ws, _) = connect_async(ws_url).await?;
    info!(vaults = vaults.len(), url = ws_url, "vault-watcher: WS connected");

    // Subscribe to each vault with dataSlice (only 8 bytes at offset 64 = balance)
    for (id, vault) in vaults.iter().enumerate() {
        let sub_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id + 1,
            "method": "accountSubscribe",
            "params": [
                vault.to_string(),
                {
                    "encoding": "base64",
                    "commitment": "processed",
                    "dataSlice": { "offset": 64, "length": 8 }
                }
            ]
        });
        ws.send(tokio_tungstenite::tungstenite::Message::Text(sub_msg.to_string())).await?;
    }

    info!(subscribed = vaults.len(), "vault-watcher: all subscriptions sent");

    // Process incoming messages
    let mut updates = 0u64;
    while let Some(msg) = ws.next().await {
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            // Parse notification
            if let Ok(notif) = serde_json::from_str::<serde_json::Value>(&text) {
                // accountNotification has params.result.value.data
                if let Some(data_arr) = notif
                    .get("params")
                    .and_then(|p| p.get("result"))
                    .and_then(|r| r.get("value"))
                    .and_then(|v| v.get("data"))
                    .and_then(|d| d.as_array())
                {
                    if let Some(b64) = data_arr.first().and_then(|v| v.as_str()) {
                        if let Ok(bytes) = base64_decode(b64) {
                            if bytes.len() >= 8 {
                                let amount = u64::from_le_bytes(
                                    bytes[..8].try_into().unwrap_or([0u8; 8])
                                );

                                // Find which vault this belongs to by subscription ID
                                // The subscription param includes the pubkey in the context
                                if let Some(pubkey_str) = notif
                                    .get("params")
                                    .and_then(|p| p.get("result"))
                                    .and_then(|r| r.get("value"))
                                    .and_then(|v| v.get("pubkey"))
                                    .and_then(|p| p.as_str())
                                {
                                    if let Ok(vault) = pubkey_str.parse::<Pubkey>() {
                                        if let Some(entry) = vault_map.get(&vault) {
                                            let (pool, is_a) = *entry;
                                            if cache.update_reserve_by_vault(&pool, is_a, amount) {
                                                updates += 1;
                                                if updates % 100 == 0 {
                                                    info!(updates, "vault-watcher: reserves updated from WS");
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn base64_decode(input: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.decode(input)?)
}
