// tpu_sender.rs – Direct TPU fanout sender.
//
// After simulation succeeds, sends the raw serialized transaction via UDP
// directly to the TPU sockets of the next N upcoming leaders (default 4).
// This runs in parallel with the Jito bundle submission, giving ~1.6 seconds
// of coverage across leader slots without waiting for confirmations.
//
// Architecture:
//   shred detected → TX built + simulated → tokio::join!(tpu_fanout, jito_bundle)
//
// Validator TPU addresses are resolved via `getClusterNodes` RPC and cached.
// Leader schedule is fetched via `getLeaderSchedule` and cached.
// Both caches refresh every 30 seconds in a background thread.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bincode::serialize;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of upcoming leaders to target (covers ~1.6 s of slot time at 400ms/slot).
const DEFAULT_FANOUT: usize = 4;

/// How often to refresh the cluster node / leader schedule cache.
const CACHE_TTL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// TpuAddrCache
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TpuAddrCacheInner {
    /// validator pubkey → TPU socket addr
    node_tpu: HashMap<Pubkey, SocketAddr>,
    /// slot → validator pubkey
    leader_schedule: HashMap<u64, Pubkey>,
    refreshed_at: Option<Instant>,
}

impl TpuAddrCacheInner {
    fn stale(&self) -> bool {
        self.refreshed_at
            .map(|t| t.elapsed() > CACHE_TTL)
            .unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------
// TpuSender
// ---------------------------------------------------------------------------

pub struct TpuSender {
    socket: UdpSocket,
    rpc_url: String,
    fanout: usize,
    cache: Arc<RwLock<TpuAddrCacheInner>>,
}

impl TpuSender {
    /// Create a new TpuSender. Binds a local UDP socket for outbound packets.
    pub fn new(rpc_url: &str, fanout: Option<usize>) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").context("bind TpuSender UDP socket")?;
        socket.set_nonblocking(false).ok();
        Ok(Self {
            socket,
            rpc_url: rpc_url.to_string(),
            fanout: fanout.unwrap_or(DEFAULT_FANOUT),
            cache: Arc::new(RwLock::new(TpuAddrCacheInner::default())),
        })
    }

    /// Refresh cluster nodes + leader schedule if the cache is stale.
    fn maybe_refresh(&self) {
        {
            let r = self.cache.read().expect("cache read");
            if !r.stale() {
                return;
            }
        }
        match self.fetch_fresh() {
            Ok((nodes, schedule)) => {
                let mut w = self.cache.write().expect("cache write");
                w.node_tpu = nodes;
                w.leader_schedule = schedule;
                w.refreshed_at = Some(Instant::now());
                info!(
                    cluster_nodes = w.node_tpu.len(),
                    schedule_slots = w.leader_schedule.len(),
                    "TpuSender cache refreshed"
                );
            }
            Err(e) => {
                warn!(error = %e, "TpuSender: failed to refresh cluster info");
            }
        }
    }

    /// Fetch cluster nodes and current leader schedule from RPC.
    fn fetch_fresh(&self) -> Result<(HashMap<Pubkey, SocketAddr>, HashMap<u64, Pubkey>)> {
        let rpc = RpcClient::new(self.rpc_url.clone());

        // getClusterNodes puede ser bloqueado por Helius; caer a endpoint público si falla.
        let nodes = rpc.get_cluster_nodes().or_else(|_| {
            let fallback = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());
            fallback.get_cluster_nodes().context("getClusterNodes (fallback)")
        }).context("getClusterNodes")?;
        let node_tpu: HashMap<Pubkey, SocketAddr> = nodes
            .into_iter()
            .filter_map(|n| {
                let pk: Pubkey = n.pubkey.parse().ok()?;
                let addr: SocketAddr = n.tpu?;
                Some((pk, addr))
            })
            .collect();

        // Leader schedule for current epoch
        let epoch_info = rpc.get_epoch_info().context("getEpochInfo")?;
        let raw = rpc
            .get_leader_schedule(Some(epoch_info.absolute_slot))
            .context("getLeaderSchedule")?
            .unwrap_or_default();

        // Convert slot-index map → absolute-slot map
        let epoch_start = epoch_info.absolute_slot - epoch_info.slot_index;
        let mut schedule: HashMap<u64, Pubkey> = HashMap::with_capacity(raw.len() * 4);
        for (pk_str, slot_indices) in &raw {
            if let Ok(pk) = pk_str.parse::<Pubkey>() {
                for &idx in slot_indices {
                    schedule.insert(epoch_start + idx as u64, pk);
                }
            }
        }

        Ok((node_tpu, schedule))
    }

    /// Resolve the TPU addresses of the next `fanout` leaders from `current_slot`.
    fn next_tpu_addrs(&self, current_slot: u64) -> Vec<(Pubkey, SocketAddr)> {
        let cache = self.cache.read().expect("cache read");
        let mut found = Vec::with_capacity(self.fanout);
        let mut slot = current_slot;
        while found.len() < self.fanout && slot < current_slot + 200 {
            if let Some(leader) = cache.leader_schedule.get(&slot) {
                if let Some(addr) = cache.node_tpu.get(leader) {
                    // Deduplicate: same leader might hold multiple consecutive slots
                    if !found.iter().any(|(pk, _)| pk == leader) {
                        found.push((*leader, *addr));
                    }
                }
            }
            slot += 1;
        }
        found
    }

    /// Serialize and send `tx` via UDP to the next `fanout` leader TPU sockets.
    ///
    /// This is fire-and-forget: we don't wait for confirmations. Returns the
    /// number of endpoints successfully sent to.
    pub fn send(&self, tx: &VersionedTransaction, current_slot: u64) -> usize {
        self.maybe_refresh();

        let bytes = match serialize(tx) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "TpuSender: serialize tx failed");
                return 0;
            }
        };

        let targets = self.next_tpu_addrs(current_slot);
        if targets.is_empty() {
            warn!(
                slot = current_slot,
                "TpuSender: no leader TPU addresses found"
            );
            return 0;
        }

        let mut sent = 0usize;
        for (leader, addr) in &targets {
            match self.socket.send_to(&bytes, addr) {
                Ok(_) => {
                    debug!(leader = %leader, addr = %addr, "TPU direct send OK");
                    sent += 1;
                }
                Err(e) => {
                    warn!(leader = %leader, addr = %addr, error = %e, "TPU direct send failed");
                }
            }
        }

        info!(
            slot = current_slot,
            sent,
            total_targets = targets.len(),
            tx_bytes = bytes.len(),
            "TPU fanout complete"
        );
        sent
    }

    /// Spawn a background thread that refreshes the cache proactively every
    /// CACHE_TTL / 2 seconds, so the hot path never blocks on a stale fetch.
    pub fn spawn_refresh_loop(this: Arc<Self>) {
        let interval = CACHE_TTL / 2;
        std::thread::Builder::new()
            .name("tpu-cache-refresh".into())
            .spawn(move || loop {
                std::thread::sleep(interval);
                this.maybe_refresh();
            })
            .expect("spawn tpu-cache-refresh");
    }
}
