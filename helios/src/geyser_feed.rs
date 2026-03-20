//! geyser_feed.rs – Real-time vault balance streaming via Richat gRPC (Dragon's Mouth compatible).
//!
//! Connects to richat-plugin-agave's gRPC endpoint (localhost:10100) for <1ms latency
//! vault balance updates. Uses yellowstone-grpc-client (same proto as Dragon's Mouth).
//!
//! When a vault balance changes, updates PoolStateCache reserve immediately
//! and bumps swap_gen so the route engine re-evaluates.
//!
//! Latency: <1ms (localhost gRPC) vs ~200ms (WS logsSubscribe).
//!
//! Environment variables:
//!   GEYSER_ENDPOINT  – gRPC URL (default "http://127.0.0.1:10100" for richat local)
//!   GEYSER_TOKEN     – (optional) x-token for auth

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use futures::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestAccountsDataSlice, SubscribeRequestFilterAccounts,
};

use market_engine::pool_state::PoolStateCache;
use spy_node::signal_bus::{LiquidityEventType, SpySignal};

/// SPL Token Account data layout: offset 64 = amount (u64 LE, 8 bytes).
const SPL_TOKEN_AMOUNT_OFFSET: usize = 64;
/// We request only 8 bytes from offset 64 via accounts_data_slice.
const AMOUNT_SLICE_LEN: usize = 8;
/// How often to rebuild the vault index (picks up newly hydrated pools).
const VAULT_INDEX_REFRESH_SECS: u64 = 120;

/// Spawn the Richat/Yellowstone gRPC vault subscription feed.
pub fn spawn_geyser_feed(
    geyser_endpoint: String,
    x_token: String,
    pool_cache: Arc<PoolStateCache>,
    sig_tx: Option<Sender<SpySignal>>,
) {
    if geyser_endpoint.is_empty() {
        warn!("geyser-grpc: GEYSER_ENDPOINT not set, feed disabled");
        return;
    }

    thread::Builder::new()
        .name("geyser-grpc".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt geyser-grpc");

            rt.block_on(run_loop(geyser_endpoint, x_token, pool_cache, sig_tx));
        })
        .expect("spawn geyser-grpc");
}

async fn run_loop(
    endpoint: String,
    x_token: String,
    pool_cache: Arc<PoolStateCache>,
    sig_tx: Option<Sender<SpySignal>>,
) {
    let mut backoff = Duration::from_secs(2);

    loop {
        let vault_index = pool_cache.build_vault_index();
        if vault_index.is_empty() {
            warn!("geyser-grpc: no vaults in pool cache, waiting 10s...");
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }

        info!(
            vaults = vault_index.len(),
            endpoint = %endpoint,
            "geyser-grpc: subscribing to vault accounts"
        );

        match run_grpc_stream(&endpoint, &x_token, &pool_cache, vault_index, &sig_tx).await {
            Ok(()) => {
                warn!("geyser-grpc: stream ended — reconnecting in 2s");
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                warn!(error = %e, ?backoff, "geyser-grpc: error — reconnecting");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn run_grpc_stream(
    endpoint: &str,
    x_token: &str,
    pool_cache: &Arc<PoolStateCache>,
    mut vault_index: HashMap<Pubkey, (Pubkey, bool)>,
    sig_tx: &Option<Sender<SpySignal>>,
) -> anyhow::Result<()> {
    // Connect to Richat gRPC (Dragon's Mouth compatible).
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.to_string())?;
    if !x_token.is_empty() {
        builder = builder.x_token(Some(x_token.to_string()))?;
    }
    let mut client = builder
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("geyser-grpc connect to {endpoint}: {e}"))?;

    info!(endpoint, vaults = vault_index.len(), "geyser-grpc: connected");

    // Subscribe by explicit vault pubkeys — most precise, no wasted bandwidth.
    let pubkey_strings: Vec<String> = vault_index.keys().map(|k| k.to_string()).collect();

    let mut accounts_filter = HashMap::new();
    accounts_filter.insert(
        "vaults".to_string(),
        SubscribeRequestFilterAccounts {
            account: pubkey_strings,
            owner: vec![],
            filters: vec![],
        },
    );

    // Request only the 8-byte amount field at offset 64 to minimize bandwidth.
    let data_slice = vec![SubscribeRequestAccountsDataSlice {
        offset: SPL_TOKEN_AMOUNT_OFFSET as u64,
        length: AMOUNT_SLICE_LEN as u64,
    }];

    let request = SubscribeRequest {
        accounts: accounts_filter,
        slots: HashMap::new(),
        transactions: HashMap::new(),
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        commitment: Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: data_slice,
        ping: None,
        entry: HashMap::new(),
    };

    let (mut sub_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;

    let mut updates_total = 0u64;
    let mut reserves_changed = 0u64;
    let mut last_log = Instant::now();
    let mut last_vault_refresh = Instant::now();
    let start = Instant::now();

    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(|e| anyhow::anyhow!("geyser-grpc stream: {e}"))?;

        match msg.update_oneof {
            Some(UpdateOneof::Account(acc_update)) => {
                let slot = acc_update.slot;
                if let Some(acc) = acc_update.account {
                    if acc.pubkey.len() != 32 {
                        continue;
                    }

                    // With data_slice, acc.data is exactly 8 bytes (the amount).
                    if acc.data.len() < AMOUNT_SLICE_LEN {
                        continue;
                    }

                    let key = Pubkey::try_from(acc.pubkey.as_slice()).unwrap_or_default();

                    let Some(&(pool_address, is_vault_a)) = vault_index.get(&key) else {
                        continue;
                    };

                    let amount_bytes: [u8; 8] =
                        acc.data[..AMOUNT_SLICE_LEN].try_into().unwrap_or([0; 8]);
                    let new_balance = u64::from_le_bytes(amount_bytes);

                    updates_total += 1;

                    let changed =
                        pool_cache.update_reserve_by_vault(&pool_address, is_vault_a, new_balance);
                    if changed {
                        reserves_changed += 1;
                        debug!(
                            pool = %pool_address,
                            side = if is_vault_a { "A" } else { "B" },
                            new_balance,
                            slot,
                            "geyser-grpc: reserve updated"
                        );

                        // Emit LiquidityEvent signal to wake route engine immediately.
                        if let Some(ref tx) = sig_tx {
                            let _ = tx.try_send(SpySignal::LiquidityEvent {
                                slot,
                                pool: pool_address,
                                event_type: LiquidityEventType::Added {
                                    amount_a: 0,
                                    amount_b: 0,
                                },
                            });
                        }
                    }
                }
            }
            Some(UpdateOneof::Ping(_)) => {}
            _ => {}
        }

        // Periodically refresh vault index to pick up newly hydrated pools.
        if last_vault_refresh.elapsed() > Duration::from_secs(VAULT_INDEX_REFRESH_SECS) {
            let new_index = pool_cache.build_vault_index();
            let new_keys: Vec<String> = new_index
                .keys()
                .filter(|k| !vault_index.contains_key(k))
                .map(|k| k.to_string())
                .collect();

            if !new_keys.is_empty() {
                info!(
                    new_vaults = new_keys.len(),
                    total = new_index.len(),
                    "geyser-grpc: adding new vault subscriptions"
                );

                // Send updated subscription via the sub_tx channel.
                let all_keys: Vec<String> = new_index.keys().map(|k| k.to_string()).collect();
                let mut accounts_filter = HashMap::new();
                accounts_filter.insert(
                    "vaults".to_string(),
                    SubscribeRequestFilterAccounts {
                        account: all_keys,
                        owner: vec![],
                        filters: vec![],
                    },
                );

                let update_request = SubscribeRequest {
                    accounts: accounts_filter,
                    slots: HashMap::new(),
                    transactions: HashMap::new(),
                    transactions_status: HashMap::new(),
                    blocks: HashMap::new(),
                    blocks_meta: HashMap::new(),
                    commitment: Some(CommitmentLevel::Processed as i32),
                    accounts_data_slice: vec![SubscribeRequestAccountsDataSlice {
                        offset: SPL_TOKEN_AMOUNT_OFFSET as u64,
                        length: AMOUNT_SLICE_LEN as u64,
                    }],
                    ping: None,
                    entry: HashMap::new(),
                };

                if sub_tx.send(update_request).await.is_err() {
                    warn!("geyser-grpc: failed to update subscription (channel closed)");
                    return Err(anyhow::anyhow!("subscription channel closed"));
                }
            }

            vault_index = new_index;
            last_vault_refresh = Instant::now();
        }

        // Stats every 30s.
        if last_log.elapsed() > Duration::from_secs(30) {
            let elapsed = last_log.elapsed().as_secs_f64().max(0.001);
            info!(
                updates_total,
                reserves_changed,
                vaults = vault_index.len(),
                rate = format!("{:.1}/s", updates_total as f64 / elapsed),
                uptime_s = start.elapsed().as_secs(),
                "geyser-grpc: throughput"
            );
            updates_total = 0;
            reserves_changed = 0;
            last_log = Instant::now();
        }
    }

    Err(anyhow::anyhow!("geyser-grpc stream closed"))
}
