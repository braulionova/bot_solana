// gossip_client.rs – Lightweight leader-schedule client for spy-node.
// We fetch leader schedule via RPC instead of running a full gossip service
// (GossipService had a use-after-free crash in Agave 1.18.26).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use solana_gossip::{
    cluster_info::{ClusterInfo, VALIDATOR_PORT_RANGE},
    legacy_contact_info::LegacyContactInfo,
};
use solana_net_utils::{
    get_cluster_shred_version, get_public_ip_addr, PortRange, MINIMUM_VALIDATOR_PORT_RANGE_WIDTH,
};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use solana_streamer::socket::SocketAddrSpace;
use tracing::{info, warn};

use crate::config::Config;

// ---------------------------------------------------------------------------
// Leader Schedule Cache
// ---------------------------------------------------------------------------

/// Maps slot → leader pubkey (populated from the leader schedule RPC call).
pub type LeaderSchedule = HashMap<u64, Pubkey>;

#[derive(Debug, Clone)]
pub struct LeaderScheduleCache {
    inner: Arc<RwLock<LeaderScheduleInner>>,
}

#[derive(Debug)]
struct LeaderScheduleInner {
    schedule: LeaderSchedule,
    /// Epoch for which the schedule is valid.
    epoch: u64,
    /// Wall-clock time of the last refresh.
    last_refresh: Instant,
    /// Fallback / default leader used when the slot isn't in the schedule.
    fallback_leader: Pubkey,
}

impl LeaderScheduleCache {
    pub fn new(fallback_leader: Pubkey) -> Self {
        Self {
            inner: Arc::new(RwLock::new(LeaderScheduleInner {
                schedule: HashMap::new(),
                epoch: 0,
                last_refresh: Instant::now(),
                fallback_leader,
            })),
        }
    }

    /// Return the leader for `slot`, falling back to the stored default.
    pub fn leader_for_slot(&self, slot: u64) -> Pubkey {
        let inner = self.inner.read();
        inner
            .schedule
            .get(&slot)
            .copied()
            .unwrap_or(inner.fallback_leader)
    }

    /// Replace the entire schedule (called after a fresh RPC fetch).
    pub fn update(&self, epoch: u64, schedule: LeaderSchedule) {
        let mut inner = self.inner.write();
        inner.epoch = epoch;
        inner.schedule = schedule;
        inner.last_refresh = Instant::now();
        info!(
            epoch,
            slots = inner.schedule.len(),
            "leader schedule updated"
        );
    }

    pub fn needs_refresh(&self, refresh_interval: Duration) -> bool {
        self.inner.read().last_refresh.elapsed() > refresh_interval
    }
}

// ---------------------------------------------------------------------------
// GossipClient
// ---------------------------------------------------------------------------

/// Minimal gossip spy that uses the Solana SDK gossip machinery.
///
/// In production this would join the gossip network as a spy node (no TVU
/// forwarding) to receive ContactInfo updates and identify current leaders.
/// For now we implement the essential pieces: identity keypair management,
/// entrypoint resolution, and periodic leader-schedule refresh via RPC.
pub struct GossipClient {
    pub identity: Arc<Keypair>,
    pub entrypoint: SocketAddr,
    pub rpc_url: String,
    pub leader_schedule_cache: LeaderScheduleCache,
    pub cluster_info: Arc<ClusterInfo>,
}

impl GossipClient {
    pub fn new(identity: Keypair, cfg: &Config) -> Result<Self> {
        let entrypoint = cfg
            .gossip_entrypoint
            .to_socket_addrs()
            .context("resolve gossip entrypoint")?
            .next()
            .context("empty DNS result for gossip entrypoint")?;
        let public_ip = resolve_public_ip(cfg.gossip_host.as_deref(), &entrypoint)?;
        let shred_version = cfg.gossip_shred_version.unwrap_or_else(|| {
            match get_cluster_shred_version(&entrypoint) {
                Ok(version) => version,
                Err(err) => {
                    warn!(error = %err, "failed to query cluster shred version; falling back to 0");
                    0
                }
            }
        });
        let advertised_gossip_addr = SocketAddr::new(public_ip, cfg.gossip_port);
        let identity = Arc::new(identity);
        let (mut contact_info, _gossip_socket, _ip_echo_listener) =
            ClusterInfo::gossip_node(identity.pubkey(), &advertised_gossip_addr, shred_version);
        contact_info.set_shred_version(shred_version);
        contact_info
            .set_tvu(cfg.advertised_tvu_addr(public_ip))
            .context("set advertised TVU socket")?;

        let leader_schedule_cache = LeaderScheduleCache::new(Pubkey::default());
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            identity.clone(),
            SocketAddrSpace::Unspecified,
        ));
        cluster_info.set_entrypoint(LegacyContactInfo::new_gossip_entry_point(&entrypoint));

        info!(
            identity = %identity.pubkey(),
            entrypoint = %entrypoint,
            shred_version,
            "gossip client initialized (RPC-only mode, no GossipService)"
        );

        Ok(Self {
            identity,
            entrypoint,
            rpc_url: cfg.rpc_url.clone(),
            leader_schedule_cache,
            cluster_info,
        })
    }

    /// Fetch the current epoch's leader schedule from the RPC and populate
    /// the cache.  This is a blocking HTTP call — call it from a dedicated
    /// background thread.
    pub fn refresh_leader_schedule(&self) -> Result<()> {
        use solana_rpc_client::rpc_client::RpcClient;

        let client = RpcClient::new(self.rpc_url.clone());

        // Get current epoch info.
        let epoch_info = client.get_epoch_info().context("get_epoch_info RPC")?;

        // Fetch leader schedule for current epoch.
        let schedule_map = client
            .get_leader_schedule(Some(epoch_info.absolute_slot))
            .context("get_leader_schedule RPC")?
            .unwrap_or_default();

        // Convert from identity-string → Vec<usize> to slot → Pubkey.
        let epoch_start_slot = epoch_info
            .absolute_slot
            .saturating_sub(epoch_info.slot_index);

        let mut slot_leaders: LeaderSchedule = HashMap::new();
        for (identity_str, slot_indices) in &schedule_map {
            let pk: Pubkey = identity_str.parse().context("parse leader pubkey")?;
            for &si in slot_indices {
                let slot = epoch_start_slot + si as u64;
                slot_leaders.insert(slot, pk);
            }
        }

        self.leader_schedule_cache
            .update(epoch_info.epoch, slot_leaders);
        Ok(())
    }

    /// Background thread: continuously refresh the leader schedule.
    pub fn run_refresh_loop(self: Arc<Self>, refresh_interval: Duration) {
        loop {
            if self.leader_schedule_cache.needs_refresh(refresh_interval) {
                if let Err(e) = self.refresh_leader_schedule() {
                    warn!(error = %e, "leader schedule refresh failed");
                } else {
                    let gossip_peers = self.cluster_info.gossip_peers().len();
                    let tvu_peers = self.cluster_info.all_tvu_peers().len();
                    info!(gossip_peers, tvu_peers, "gossip peer snapshot");
                }
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    /// Return the best-known leader pubkey for `slot`.
    pub fn leader_for_slot(&self, slot: u64) -> Pubkey {
        self.leader_schedule_cache.leader_for_slot(slot)
    }

    pub fn tvu_peers(&self) -> Vec<LegacyContactInfo> {
        self.cluster_info.tvu_peers()
    }
}

fn resolve_public_ip(gossip_host: Option<&str>, entrypoint: &SocketAddr) -> Result<IpAddr> {
    if let Some(gossip_host) = gossip_host {
        if let Ok(ip) = gossip_host.parse::<IpAddr>() {
            return Ok(ip);
        }
        return format!("{gossip_host}:0")
            .to_socket_addrs()
            .context("resolve GOSSIP_HOST")?
            .next()
            .map(|addr| addr.ip())
            .context("empty DNS result for GOSSIP_HOST");
    }
    get_public_ip_addr(entrypoint)
        .map_err(|err| anyhow!("discover public IP from gossip entrypoint: {err}"))
}

fn gossip_node_port_range(tvu_port: u16) -> PortRange {
    let (start, end) = VALIDATOR_PORT_RANGE;
    let min_width = MINIMUM_VALIDATOR_PORT_RANGE_WIDTH;
    if (start..=end).contains(&tvu_port) {
        let above_start = tvu_port.saturating_add(1);
        if end.saturating_sub(above_start) >= min_width {
            return (above_start, end);
        }
        let below_end = tvu_port.saturating_sub(1);
        if below_end.saturating_sub(start) >= min_width {
            return (start, below_end);
        }
        return (10_001, 12_000);
    }
    VALIDATOR_PORT_RANGE
}
