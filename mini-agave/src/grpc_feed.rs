//! grpc_feed.rs — Real-time state feed for mini-agave.
//!
//! Three modes (auto-detected in priority order):
//!   A) Richat local (127.0.0.1:10100) — geyser plugin on local Agave, <1ms
//!   B) External gRPC (Yellowstone/LaserStream) — push-based, ~10-50ms
//!   C) RPC polling fallback — blockhash every 400ms, accounts every 15s
//!
//! All state stored in shared DashMap/RwLock (zero-copy reads from RPC server).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use dashmap::DashMap;
use parking_lot::RwLock;
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use tracing::{debug, info, warn};

/// Cached account data from gRPC or RPC polling.
#[derive(Clone, Debug)]
pub struct CachedAccount {
    pub lamports: u64,
    pub data: Vec<u8>,
    pub owner: Pubkey,
    pub executable: bool,
    pub rent_epoch: u64,
    pub slot: u64,
}

/// Cached epoch info (refreshed every 60s or from gRPC).
#[derive(Clone, Debug)]
pub struct CachedEpochInfo {
    pub epoch: u64,
    pub slot_index: u64,
    pub slots_in_epoch: u64,
    pub absolute_slot: u64,
    pub block_height: u64,
    pub transaction_count: Option<u64>,
}

/// Cached cluster node info.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CachedClusterNode {
    pub pubkey: String,
    pub gossip: Option<String>,
    pub tpu: Option<String>,
    pub tpu_quic: Option<String>,
    pub rpc: Option<String>,
    pub version: Option<String>,
}

/// DEX programs to monitor for transaction signals.
const DEX_PROGRAMS: &[&str] = &[
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium AMM v4
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  // Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS",  // Meteora DLMM
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",   // PumpSwap AMM
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",   // PumpFun bonding
];

/// Default Richat gRPC endpoint (local Agave geyser plugin).
const RICHAT_LOCAL: &str = "http://127.0.0.1:10100";

/// Shared state updated by the feed (gRPC or polling).
pub struct GrpcState {
    /// Latest confirmed blockhash.
    pub blockhash: Arc<RwLock<Hash>>,
    /// Slot of latest blockhash.
    pub blockhash_slot: AtomicU64,
    /// Current processed slot (most recent from any source).
    pub current_slot: AtomicU64,
    /// Block height (from epoch info or block meta).
    pub block_height: AtomicU64,
    /// Account data by pubkey.
    pub accounts: Arc<DashMap<Pubkey, CachedAccount>>,
    /// Whether feed is connected/active.
    pub connected: AtomicBool,
    /// Total account updates received.
    pub account_updates: AtomicU64,
    /// Total blockhash updates received.
    pub blockhash_updates: AtomicU64,
    /// Total DEX transactions seen.
    pub dex_tx_count: AtomicU64,
    /// Feed mode description.
    pub mode: RwLock<String>,
    /// Cached epoch info (refreshed periodically).
    pub epoch_info: RwLock<Option<CachedEpochInfo>>,
    /// Cached cluster nodes (refreshed every 5min).
    pub cluster_nodes: RwLock<Vec<CachedClusterNode>>,
    /// Cached leader schedule (refreshed per epoch).
    pub leader_schedule: RwLock<Option<serde_json::Value>>,
    /// Epoch of cached leader schedule.
    pub leader_schedule_epoch: AtomicU64,
    /// Timestamp (ms) of last cluster nodes refresh.
    pub cluster_nodes_updated_ms: AtomicU64,
    /// Timestamp (ms) of last epoch info refresh.
    pub epoch_info_updated_ms: AtomicU64,
    /// Whether Richat local is available.
    pub richat_connected: AtomicBool,
}

impl GrpcState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            blockhash: Arc::new(RwLock::new(Hash::default())),
            blockhash_slot: AtomicU64::new(0),
            current_slot: AtomicU64::new(0),
            block_height: AtomicU64::new(0),
            accounts: Arc::new(DashMap::new()),
            connected: AtomicBool::new(false),
            account_updates: AtomicU64::new(0),
            blockhash_updates: AtomicU64::new(0),
            dex_tx_count: AtomicU64::new(0),
            mode: RwLock::new("initializing".into()),
            epoch_info: RwLock::new(None),
            cluster_nodes: RwLock::new(Vec::new()),
            leader_schedule: RwLock::new(None),
            leader_schedule_epoch: AtomicU64::new(0),
            cluster_nodes_updated_ms: AtomicU64::new(0),
            epoch_info_updated_ms: AtomicU64::new(0),
            richat_connected: AtomicBool::new(false),
        })
    }

    pub fn get_blockhash(&self) -> Hash {
        *self.blockhash.read()
    }

    pub fn get_account(&self, key: &Pubkey) -> Option<CachedAccount> {
        self.accounts.get(key).map(|r| r.clone())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn get_mode(&self) -> String {
        self.mode.read().clone()
    }

    pub fn get_slot(&self) -> u64 {
        let s = self.current_slot.load(Ordering::Relaxed);
        if s > 0 { s } else { self.blockhash_slot.load(Ordering::Relaxed) }
    }

    pub fn get_epoch_info(&self) -> Option<CachedEpochInfo> {
        self.epoch_info.read().clone()
    }

    pub fn get_cluster_nodes(&self) -> Vec<CachedClusterNode> {
        self.cluster_nodes.read().clone()
    }

    pub fn get_leader_schedule(&self) -> Option<serde_json::Value> {
        self.leader_schedule.read().clone()
    }
}

/// DEX signal emitted when a swap transaction is detected.
#[derive(Debug)]
pub struct DexSignal {
    pub slot: u64,
}

/// Spawn the state feed. Auto-detects Richat local, then tries external gRPC,
/// falls back to RPC polling.
pub fn spawn_feed(
    state: Arc<GrpcState>,
    grpc_endpoint: String,
    grpc_token: String,
    poll_rpc: String,
    upstream_rpc: String,
    account_keys: Vec<Pubkey>,
    dex_signal_tx: Option<Sender<DexSignal>>,
) {
    // Always start polling threads (they run as baseline)
    spawn_blockhash_poller(Arc::clone(&state), poll_rpc);
    spawn_account_poller(Arc::clone(&state), upstream_rpc.clone(), account_keys.clone());
    spawn_metadata_poller(Arc::clone(&state), upstream_rpc);

    // Determine gRPC endpoint: explicit > Richat local > none
    let effective_grpc = if !grpc_endpoint.is_empty() {
        info!(endpoint = %grpc_endpoint, "using explicit GRPC_ENDPOINT");
        Some((grpc_endpoint, grpc_token))
    } else {
        // Auto-detect Richat local
        info!("no GRPC_ENDPOINT — probing Richat local at {RICHAT_LOCAL}");
        None
    };

    if let Some((ep, token)) = effective_grpc {
        spawn_grpc_feed(state, ep, token, account_keys, dex_signal_tx);
    } else {
        // Try Richat local, fallback to polling-only
        spawn_richat_auto_detect(state, account_keys, dex_signal_tx);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Blockhash poller — runs every 400ms, always active
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_blockhash_poller(state: Arc<GrpcState>, rpc_url: String) {
    std::thread::Builder::new()
        .name("bh-poller".into())
        .spawn(move || {
            let rpc = solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
                rpc_url,
                solana_sdk::commitment_config::CommitmentConfig::confirmed(),
            );
            let mut consecutive_errors = 0u32;

            loop {
                match rpc.get_latest_blockhash_with_commitment(
                    solana_sdk::commitment_config::CommitmentConfig::confirmed(),
                ) {
                    Ok((bh, slot)) => {
                        let old_slot = state.blockhash_slot.load(Ordering::Relaxed);
                        if slot > old_slot {
                            *state.blockhash.write() = bh;
                            state.blockhash_slot.store(slot, Ordering::Relaxed);
                            state.blockhash_updates.fetch_add(1, Ordering::Relaxed);
                            state.connected.store(true, Ordering::Relaxed);
                            // Update current_slot if this is newer
                            let cur = state.current_slot.load(Ordering::Relaxed);
                            if slot > cur {
                                state.current_slot.store(slot, Ordering::Relaxed);
                            }
                            debug!(slot, blockhash = %bh, "blockhash updated (poll)");
                        }
                        consecutive_errors = 0;
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        if consecutive_errors <= 3 || consecutive_errors % 10 == 0 {
                            warn!(error = %e, n = consecutive_errors, "blockhash poll error");
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        })
        .expect("spawn bh-poller");
}

// ─────────────────────────────────────────────────────────────────────────────
// Account poller — refreshes pool vaults every N seconds
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_account_poller(state: Arc<GrpcState>, rpc_url: String, account_keys: Vec<Pubkey>) {
    if account_keys.is_empty() {
        info!("no accounts to poll — skipping account poller");
        return;
    }

    let interval_secs: u64 = std::env::var("ACCOUNT_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let batch_size: usize = std::env::var("ACCOUNT_POLL_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    let batch_sleep_ms: u64 = std::env::var("ACCOUNT_POLL_BATCH_SLEEP_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    info!(
        accounts = account_keys.len(),
        interval_secs,
        batch_size,
        "account poller starting"
    );

    std::thread::Builder::new()
        .name("acc-poller".into())
        .spawn(move || {
            let rpc = solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
                rpc_url,
                solana_sdk::commitment_config::CommitmentConfig::confirmed(),
            );

            // Initial delay to let blockhash poller connect first
            std::thread::sleep(Duration::from_secs(2));

            loop {
                let start = std::time::Instant::now();
                let mut updated = 0u64;
                let mut errors = 0u64;

                for chunk in account_keys.chunks(batch_size) {
                    match rpc.get_multiple_accounts(chunk) {
                        Ok(accounts) => {
                            for (pk, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                                if let Some(acc) = maybe_acc {
                                    state.accounts.insert(
                                        *pk,
                                        CachedAccount {
                                            lamports: acc.lamports,
                                            data: acc.data.clone(),
                                            owner: acc.owner,
                                            executable: acc.executable,
                                            rent_epoch: acc.rent_epoch,
                                            slot: state.blockhash_slot.load(Ordering::Relaxed),
                                        },
                                    );
                                    updated += 1;
                                }
                            }
                            state.account_updates.fetch_add(updated, Ordering::Relaxed);
                        }
                        Err(e) => {
                            errors += 1;
                            if errors <= 3 {
                                warn!(error = %e, "account poll batch error");
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(batch_sleep_ms));
                }

                let elapsed = start.elapsed();
                info!(
                    updated,
                    errors,
                    elapsed_ms = elapsed.as_millis(),
                    cached = state.accounts.len(),
                    "account poll cycle complete"
                );

                // Sleep remaining time to complete the interval
                let remaining = Duration::from_secs(interval_secs).saturating_sub(elapsed);
                if !remaining.is_zero() {
                    std::thread::sleep(remaining);
                }
            }
        })
        .expect("spawn acc-poller");
}

// ─────────────────────────────────────────────────────────────────────────────
// Metadata poller — epochInfo (60s), clusterNodes (5min), leaderSchedule (epoch)
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_metadata_poller(state: Arc<GrpcState>, rpc_url: String) {
    std::thread::Builder::new()
        .name("meta-poller".into())
        .spawn(move || {
            let rpc = solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
                rpc_url,
                solana_sdk::commitment_config::CommitmentConfig::confirmed(),
            );

            // Initial delay
            std::thread::sleep(Duration::from_secs(3));

            loop {
                let now = now_ms();

                // ── Epoch info (every 60s) ──
                let last_epoch = state.epoch_info_updated_ms.load(Ordering::Relaxed);
                if now.saturating_sub(last_epoch) >= 60_000 {
                    match rpc.get_epoch_info() {
                        Ok(ei) => {
                            state.block_height.store(ei.block_height, Ordering::Relaxed);
                            let epoch = ei.epoch;
                            *state.epoch_info.write() = Some(CachedEpochInfo {
                                epoch: ei.epoch,
                                slot_index: ei.slot_index,
                                slots_in_epoch: ei.slots_in_epoch,
                                absolute_slot: ei.absolute_slot,
                                block_height: ei.block_height,
                                transaction_count: ei.transaction_count,
                            });
                            state.epoch_info_updated_ms.store(now, Ordering::Relaxed);
                            // Update current_slot from epoch info
                            let cur = state.current_slot.load(Ordering::Relaxed);
                            if ei.absolute_slot > cur {
                                state.current_slot.store(ei.absolute_slot, Ordering::Relaxed);
                            }
                            debug!(epoch, slot = ei.absolute_slot, height = ei.block_height, "epoch info updated");

                            // ── Leader schedule (once per epoch) ──
                            let cached_epoch = state.leader_schedule_epoch.load(Ordering::Relaxed);
                            if epoch != cached_epoch {
                                match rpc.get_leader_schedule(None) {
                                    Ok(Some(schedule)) => {
                                        let schedule_json = serde_json::to_value(&schedule).unwrap_or_default();
                                        *state.leader_schedule.write() = Some(schedule_json);
                                        state.leader_schedule_epoch.store(epoch, Ordering::Relaxed);
                                        info!(epoch, leaders = schedule.len(), "leader schedule updated");
                                    }
                                    Ok(None) => {
                                        debug!(epoch, "leader schedule not yet available");
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "leader schedule fetch error");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "epoch info poll error");
                        }
                    }
                }

                // ── Cluster nodes (every 5min) ──
                let last_cn = state.cluster_nodes_updated_ms.load(Ordering::Relaxed);
                if now.saturating_sub(last_cn) >= 300_000 {
                    match rpc.get_cluster_nodes() {
                        Ok(nodes) => {
                            let cached: Vec<CachedClusterNode> = nodes
                                .iter()
                                .map(|n| CachedClusterNode {
                                    pubkey: n.pubkey.clone(),
                                    gossip: n.gossip.map(|a| a.to_string()),
                                    tpu: n.tpu.map(|a| a.to_string()),
                                    tpu_quic: n.tpu_quic.map(|a| a.to_string()),
                                    rpc: n.rpc.map(|a| a.to_string()),
                                    version: n.version.clone(),
                                })
                                .collect();
                            info!(nodes = cached.len(), "cluster nodes updated");
                            *state.cluster_nodes.write() = cached;
                            state.cluster_nodes_updated_ms.store(now, Ordering::Relaxed);
                        }
                        Err(e) => {
                            warn!(error = %e, "cluster nodes fetch error");
                        }
                    }
                }

                std::thread::sleep(Duration::from_secs(10));
            }
        })
        .expect("spawn meta-poller");
}

// ─────────────────────────────────────────────────────────────────────────────
// Richat auto-detect — try local geyser, fallback to polling-only
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_richat_auto_detect(
    state: Arc<GrpcState>,
    account_keys: Vec<Pubkey>,
    dex_signal_tx: Option<Sender<DexSignal>>,
) {
    std::thread::Builder::new()
        .name("richat-detect".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt richat-detect");

            rt.block_on(async {
                loop {
                    // Try Richat local
                    info!("probing Richat at {RICHAT_LOCAL}...");
                    match probe_grpc(RICHAT_LOCAL).await {
                        Ok(()) => {
                            info!("Richat local AVAILABLE — switching to gRPC push mode");
                            state.richat_connected.store(true, Ordering::Relaxed);
                            // Run the gRPC feed (blocks until disconnect)
                            run_grpc_loop(
                                &state,
                                RICHAT_LOCAL,
                                "",
                                &account_keys,
                                &dex_signal_tx,
                            )
                            .await;
                            state.richat_connected.store(false, Ordering::Relaxed);
                            warn!("Richat local disconnected — falling back to polling, will retry in 30s");
                        }
                        Err(e) => {
                            debug!(error = %e, "Richat local not available — polling-only mode");
                            if !state.is_connected() {
                                state.connected.store(true, Ordering::Relaxed);
                                *state.mode.write() = "polling (Richat unavailable)".into();
                            }
                        }
                    }
                    // Retry every 30s
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        })
        .expect("spawn richat-detect");
}

/// Quick gRPC connectivity probe.
async fn probe_grpc(endpoint: &str) -> anyhow::Result<()> {
    use yellowstone_grpc_client::GeyserGrpcClient;

    let client = GeyserGrpcClient::build_from_shared(endpoint.to_string())?
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(3))
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("probe connect: {e}"))?;

    drop(client);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// gRPC feed — Yellowstone/Richat push-based
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_grpc_feed(
    state: Arc<GrpcState>,
    endpoint: String,
    x_token: String,
    account_keys: Vec<Pubkey>,
    dex_signal_tx: Option<Sender<DexSignal>>,
) {
    std::thread::Builder::new()
        .name("grpc-feed".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt grpc-feed");

            rt.block_on(async {
                run_grpc_loop(&state, &endpoint, &x_token, &account_keys, &dex_signal_tx).await;
            });
        })
        .expect("spawn grpc-feed");
}

async fn run_grpc_loop(
    state: &Arc<GrpcState>,
    endpoint: &str,
    x_token: &str,
    account_keys: &[Pubkey],
    dex_signal_tx: &Option<Sender<DexSignal>>,
) {
    let mut backoff = Duration::from_secs(2);
    let mut grpc_failed_count = 0u32;

    loop {
        match run_grpc_once(state, endpoint, x_token, account_keys, dex_signal_tx).await {
            Ok(()) => {
                warn!("grpc-feed stream ended — reconnecting in 2s");
                backoff = Duration::from_secs(2);
                grpc_failed_count = 0;
            }
            Err(e) => {
                grpc_failed_count += 1;
                let msg = e.to_string();

                if msg.contains("PermissionDenied")
                    || msg.contains("Unsupported plan")
                    || msg.contains("Unauthorized")
                {
                    warn!(
                        error = %e,
                        "gRPC not available — running in polling-only mode"
                    );
                    *state.mode.write() = "polling (gRPC unavailable)".into();
                    return;
                }

                if grpc_failed_count >= 10 {
                    warn!(
                        failures = grpc_failed_count,
                        "gRPC failed too many times — falling back to polling-only"
                    );
                    *state.mode.write() = "polling (gRPC failed)".into();
                    return;
                }

                warn!(error = %e, ?backoff, "grpc-feed error — reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn run_grpc_once(
    state: &Arc<GrpcState>,
    endpoint: &str,
    x_token: &str,
    account_keys: &[Pubkey],
    dex_signal_tx: &Option<Sender<DexSignal>>,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    use yellowstone_grpc_client::GeyserGrpcClient;
    use yellowstone_grpc_proto::geyser::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterAccounts, SubscribeRequestFilterBlocksMeta,
        SubscribeRequestFilterSlots, SubscribeRequestFilterTransactions,
    };

    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.to_string())?;
    if !x_token.is_empty() {
        builder = builder.x_token(Some(x_token.to_string()))?;
    }
    let mut client = builder
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("grpc connect: {e}"))?;

    info!(
        endpoint,
        accounts = account_keys.len(),
        "grpc-feed connected"
    );

    // ── Build subscription request ──────────────────────────────────────────

    // 1. Blockhash from blocks_meta
    let mut blocks_meta = HashMap::new();
    blocks_meta.insert("bh".to_string(), SubscribeRequestFilterBlocksMeta {});

    // 2. Slot updates (for real-time slot tracking)
    let mut slots = HashMap::new();
    slots.insert(
        "slots".to_string(),
        SubscribeRequestFilterSlots {
            filter_by_commitment: Some(true),
        },
    );

    // 3. Account subscriptions (pool vaults)
    let mut accounts_filter = HashMap::new();
    if !account_keys.is_empty() {
        let pubkey_strings: Vec<String> = account_keys.iter().map(|k| k.to_string()).collect();
        accounts_filter.insert(
            "vaults".to_string(),
            SubscribeRequestFilterAccounts {
                account: pubkey_strings,
                owner: vec![],
                filters: vec![],
            },
        );
    }

    // 4. DEX transaction subscriptions
    let mut tx_filter = HashMap::new();
    if dex_signal_tx.is_some() {
        tx_filter.insert(
            "dex_txs".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: DEX_PROGRAMS.iter().map(|s| s.to_string()).collect(),
                account_exclude: vec![],
                account_required: vec![],
            },
        );
    }

    let request = SubscribeRequest {
        accounts: accounts_filter,
        slots,
        transactions: tx_filter,
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta,
        commitment: Some(CommitmentLevel::Confirmed as i32),
        accounts_data_slice: vec![],
        ping: None,
        entry: HashMap::new(),
    };

    let (_sub_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;

    let is_richat = endpoint.contains("127.0.0.1:10100") || endpoint.contains("localhost:10100");
    let mode_str = if is_richat { "richat (local geyser)" } else { "grpc (push)" };
    *state.mode.write() = mode_str.into();
    state.connected.store(true, Ordering::Relaxed);
    info!(mode = mode_str, "grpc-feed subscription active (slots + accounts + blockhash + dex_txs)");

    // Debounce for DEX signals: max 1 per 50ms
    let mut last_dex_emit_ms: u64 = 0;

    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(|e| anyhow::anyhow!("stream: {e}"))?;

        match msg.update_oneof {
            // ── Slot update ───────────────────────────────────────────
            Some(UpdateOneof::Slot(slot_update)) => {
                let slot = slot_update.slot;
                let cur = state.current_slot.load(Ordering::Relaxed);
                if slot > cur {
                    state.current_slot.store(slot, Ordering::Relaxed);
                }
            }

            // ── Blockhash update ────────────────────────────────────────
            Some(UpdateOneof::BlockMeta(bm)) => {
                if let Ok(bh) = bm.blockhash.parse::<Hash>() {
                    *state.blockhash.write() = bh;
                    state.blockhash_slot.store(bm.slot, Ordering::Relaxed);
                    state.blockhash_updates.fetch_add(1, Ordering::Relaxed);
                    // Update block height from block meta
                    if bm.block_height.is_some() {
                        let bh_val = bm.block_height.unwrap().block_height;
                        state.block_height.store(bh_val, Ordering::Relaxed);
                    }
                    // Update current_slot
                    let cur = state.current_slot.load(Ordering::Relaxed);
                    if bm.slot > cur {
                        state.current_slot.store(bm.slot, Ordering::Relaxed);
                    }
                    debug!(slot = bm.slot, blockhash = %bh, "blockhash updated (grpc)");
                }
            }

            // ── Account update ──────────────────────────────────────────
            Some(UpdateOneof::Account(acc_update)) => {
                let slot = acc_update.slot;
                if let Some(acc) = acc_update.account {
                    if acc.pubkey.len() == 32 {
                        let key = Pubkey::try_from(acc.pubkey.as_slice()).unwrap_or_default();
                        let owner = if acc.owner.len() == 32 {
                            Pubkey::try_from(acc.owner.as_slice()).unwrap_or_default()
                        } else {
                            Pubkey::default()
                        };
                        state.accounts.insert(
                            key,
                            CachedAccount {
                                lamports: acc.lamports,
                                data: acc.data,
                                owner,
                                executable: acc.executable,
                                rent_epoch: acc.rent_epoch,
                                slot,
                            },
                        );
                        state.account_updates.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            // ── DEX transaction ─────────────────────────────────────────
            Some(UpdateOneof::Transaction(tx_update)) => {
                state.dex_tx_count.fetch_add(1, Ordering::Relaxed);

                if let Some(ref tx) = dex_signal_tx {
                    let now = now_ms();
                    if now.saturating_sub(last_dex_emit_ms) >= 50 {
                        last_dex_emit_ms = now;
                        let _ = tx.try_send(DexSignal {
                            slot: tx_update.slot,
                        });
                    }
                }
            }

            Some(UpdateOneof::Ping(_)) => {}
            _ => {}
        }
    }

    Err(anyhow::anyhow!("grpc stream closed"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
