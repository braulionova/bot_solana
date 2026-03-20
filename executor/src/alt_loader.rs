// alt_loader.rs – Address Lookup Table (ALT) cache for Helios executor.
//
// Strategy:
//   1. At startup, fetch a configurable list of well-known protocol ALTs.
//   2. Every `refresh_secs` seconds, re-fetch to pick up any extensions.
//   3. During TX building, pass ALL cached ALTs to v0::Message::try_compile —
//      the SDK automatically selects only the accounts that appear in the TX.
//
// Known mainnet-beta protocol ALTs (seeded by default):
//   • GrDMoeqMLFjeXQ24H56S1RLgT4R76jsuWCd6SvXyGPQ5  — Solana sysvar/program ALT
//
// Add extra ALTs via the `ALT_ADDRESSES` env var (comma-separated pubkeys).

use std::{
    collections::HashSet,
    fs,
    hash::{Hash, Hasher},
    str::FromStr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context};
use dashmap::DashMap;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::{
        instruction::{create_lookup_table_signed, extend_lookup_table},
        state::AddressLookupTable,
        AddressLookupTableAccount,
    },
    clock::Slot,
    commitment_config::CommitmentConfig,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    slot_hashes::SlotHashes,
    sysvar::slot_hashes,
    transaction::Transaction,
};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Well-known mainnet-beta protocol ALTs
// ---------------------------------------------------------------------------

const KNOWN_ALTS: &[&str] = &[
    "GrDMoeqMLFjeXQ24H56S1RLgT4R76jsuWCd6SvXyGPQ5", // Solana sysvars / common programs
];
const ALT_ADDRESS_FILES: &[&str] = &["/root/solana-bot/alt_address.txt"];
const DYNAMIC_ALT_CHUNK_SIZE: usize = 30; // max per extend TX
const ALT_READY_TIMEOUT: Duration = Duration::from_secs(20);
const ALT_READY_POLL_INTERVAL: Duration = Duration::from_millis(400);
/// Path where the route→ALT mapping is persisted across restarts.
const ROUTE_ALT_PERSIST_FILE: &str = "/root/spy_node/route_alt_cache.json";

// ---------------------------------------------------------------------------
// AltCache
// ---------------------------------------------------------------------------

pub struct AltCache {
    rpc: Arc<RpcClient>,
    cache: DashMap<Pubkey, AddressLookupTableAccount>,
    refresh_secs: u64,
    last_refresh: std::sync::Mutex<Instant>,
    /// Maps hash(sorted addresses) → ALT pubkey for previously created dynamic ALTs.
    /// Avoids recreating (and the 3–15 s propagation wait) for identical route account sets.
    route_alt_cache: DashMap<u64, Pubkey>,
    /// Track address-set hashes currently being created in background threads.
    pending_alt_creations: DashMap<u64, Instant>,
}

impl AltCache {
    /// Create a new cache and immediately load all ALTs.
    pub fn new(rpc_url: &str, extra_alts: &[Pubkey], refresh_secs: u64) -> Arc<Self> {
        let rpc = Arc::new(RpcClient::new_with_commitment(
            rpc_url.to_owned(),
            CommitmentConfig::confirmed(),
        ));

        let cache = DashMap::new();

        // Collect all pubkeys: built-in + extra.
        let mut keys: Vec<Pubkey> = KNOWN_ALTS
            .iter()
            .filter_map(|s| Pubkey::from_str(s).ok())
            .collect();
        for path in ALT_ADDRESS_FILES {
            if let Ok(raw) = fs::read_to_string(path) {
                keys.extend(
                    raw.lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .filter_map(|line| Pubkey::from_str(line).ok()),
                );
            }
        }
        keys.extend_from_slice(extra_alts);
        keys.dedup();

        // Restore persisted route→ALT mapping from disk.
        let route_alt_cache: DashMap<u64, Pubkey> = DashMap::new();
        let mut persisted_alts: Vec<Pubkey> = Vec::new();
        if let Ok(raw) = fs::read_to_string(ROUTE_ALT_PERSIST_FILE) {
            if let Ok(map) = serde_json::from_str::<std::collections::HashMap<String, String>>(&raw) {
                for (k, v) in &map {
                    if let (Ok(hash), Ok(pk)) = (k.parse::<u64>(), v.parse::<Pubkey>()) {
                        route_alt_cache.insert(hash, pk);
                        persisted_alts.push(pk);
                    }
                }
                info!(count = route_alt_cache.len(), "route ALT cache restored from disk");
            }
        }
        // Include persisted route ALTs in the initial fetch.
        keys.extend_from_slice(&persisted_alts);
        keys.dedup();

        let this = Arc::new(Self {
            rpc,
            cache,
            refresh_secs,
            last_refresh: std::sync::Mutex::new(Instant::now()),
            route_alt_cache,
            pending_alt_creations: DashMap::new(),
        });

        this.fetch_and_store(&keys);
        info!(loaded = this.cache.len(), "ALT cache initialised");

        this
    }

    /// Return all cached ALTs as a Vec (used in TX building).
    pub fn all(&self) -> Vec<AddressLookupTableAccount> {
        self.maybe_refresh();
        self.cache
            .iter()
            .map(
                |e: dashmap::mapref::multiple::RefMulti<Pubkey, AddressLookupTableAccount>| {
                    e.value().clone()
                },
            )
            .collect()
    }

    /// Add pool-specific ALT pubkeys discovered from route data.
    pub fn prefetch(&self, keys: &[Pubkey]) {
        let missing: Vec<Pubkey> = keys
            .iter()
            .filter(|k| !self.cache.contains_key(*k))
            .copied()
            .collect();

        if !missing.is_empty() {
            debug!(count = missing.len(), "prefetching pool-specific ALTs");
            self.fetch_and_store(&missing);
        }
    }

    /// Like `create_and_extend_alt`, but caches the result by address-set hash.
    /// Subsequent calls with the same accounts return the cached ALT pubkey instantly
    /// without touching the chain, eliminating the 3–15 s propagation delay.
    pub fn get_or_create_route_alt(
        &self,
        payer: &Keypair,
        addresses: Vec<Pubkey>,
    ) -> anyhow::Result<Pubkey> {
        let key = hash_address_set(&addresses);
        if let Some(cached) = self.route_alt_cache.get(&key) {
            let alt_pubkey = *cached;
            // Ensure it's still loaded in the local cache (might have been evicted).
            if !self.cache.contains_key(&alt_pubkey) {
                self.fetch_and_store(&[alt_pubkey]);
            }
            if self.cache.contains_key(&alt_pubkey) {
                debug!(%alt_pubkey, "reusing cached dynamic ALT");
                return Ok(alt_pubkey);
            }
        }
        let alt_pubkey = self.create_and_extend_alt(payer, addresses)?;
        self.route_alt_cache.insert(key, alt_pubkey);
        // Persist to disk so this ALT survives restarts.
        self.persist_route_alt_cache();
        Ok(alt_pubkey)
    }

    /// Non-blocking ALT lookup: returns Ok(pubkey) on cache hit, Err on cache miss.
    /// On cache miss, spawns a background thread to create the ALT.
    /// The caller should skip this opportunity; the ALT will be ready next time.
    pub fn try_get_or_spawn_create(
        self: &Arc<Self>,
        payer: &Keypair,
        addresses: Vec<Pubkey>,
    ) -> Result<Pubkey, anyhow::Error> {
        let key = hash_address_set(&addresses);

        // Cache hit → instant return.
        if let Some(cached) = self.route_alt_cache.get(&key) {
            let alt_pubkey = *cached;
            if !self.cache.contains_key(&alt_pubkey) {
                self.fetch_and_store(&[alt_pubkey]);
            }
            if self.cache.contains_key(&alt_pubkey) {
                debug!(%alt_pubkey, "reusing cached dynamic ALT");
                return Ok(alt_pubkey);
            }
        }

        // Already being created in background?
        if let Some(started) = self.pending_alt_creations.get(&key) {
            if started.elapsed() < Duration::from_secs(60) {
                return Err(anyhow!("ALT creation in progress (started {}s ago)", started.elapsed().as_secs()));
            }
            // Stale entry — remove and retry.
            drop(started);
            self.pending_alt_creations.remove(&key);
        }

        // Spawn background creation.
        self.pending_alt_creations.insert(key, Instant::now());
        let this = Arc::clone(self);
        // Clone the keypair bytes for the background thread.
        let payer_bytes = payer.to_bytes();
        thread::Builder::new()
            .name("alt-create-bg".into())
            .spawn(move || {
                let payer = Keypair::from_bytes(&payer_bytes).expect("payer keypair");
                match this.create_and_extend_alt(&payer, addresses) {
                    Ok(alt_pubkey) => {
                        this.route_alt_cache.insert(key, alt_pubkey);
                        this.persist_route_alt_cache();
                        info!(%alt_pubkey, "background ALT creation complete");
                    }
                    Err(e) => {
                        warn!(error = %e, "background ALT creation failed");
                    }
                }
                this.pending_alt_creations.remove(&key);
            })
            .ok();

        Err(anyhow!("ALT not cached — creation spawned in background"))
    }

    /// Dynamically create an on-chain ALT with the given accounts.
    /// This is a blocking call that waits for confirmation.
    pub fn create_and_extend_alt(
        &self,
        payer: &Keypair,
        mut addresses: Vec<Pubkey>,
    ) -> anyhow::Result<Pubkey> {
        addresses.dedup();
        if addresses.is_empty() {
            anyhow::bail!("No addresses provided to create ALT");
        }

        let recent_slot = self
            .recent_lookup_table_slot()
            .context("select recent slot for dynamic ALT")?;
        let (create_ix, alt_pubkey) =
            create_lookup_table_signed(payer.pubkey(), payer.pubkey(), recent_slot);

        info!(alt_pubkey = %alt_pubkey, addresses = addresses.len(), "creating dynamic ALT on-chain");
        let (recent_blockhash, _) = self
            .rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?;
        let create_msg = Message::new(&[create_ix], Some(&payer.pubkey()));
        let create_tx = Transaction::new(&[payer], create_msg, recent_blockhash);
        let create_sig = self
            .rpc
            .send_and_confirm_transaction(&create_tx)
            .map_err(|e| {
                warn!(
                    error = %e,
                    alt_pubkey = %alt_pubkey,
                    recent_slot,
                    payer = %payer.pubkey(),
                    "ALT create TX failed — full error"
                );
                anyhow::anyhow!("create dynamic ALT {} using recent_slot {}: {}", alt_pubkey, recent_slot, e)
            })?;
        info!(sig = %create_sig, "dynamic ALT account created");

        // Each extend is sent in its own transaction to stay under the packet size limit.
        for chunk in addresses.chunks(DYNAMIC_ALT_CHUNK_SIZE) {
            let extend_ix = extend_lookup_table(
                alt_pubkey,
                payer.pubkey(),
                Some(payer.pubkey()),
                chunk.to_vec(),
            );
            let (recent_blockhash, _) = self
                .rpc
                .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?;
            let extend_msg = Message::new(&[extend_ix], Some(&payer.pubkey()));
            let extend_tx = Transaction::new(&[payer], extend_msg, recent_blockhash);
            let extend_sig = self
                .rpc
                .send_and_confirm_transaction(&extend_tx)
                .with_context(|| {
                    format!(
                        "extend dynamic ALT {} with {} addresses",
                        alt_pubkey,
                        chunk.len()
                    )
                })?;
            info!(sig = %extend_sig, chunk = chunk.len(), "dynamic ALT extended");
        }

        self.wait_until_alt_ready(alt_pubkey, addresses.len())
            .with_context(|| format!("wait for dynamic ALT {} to become usable", alt_pubkey))?;
        self.fetch_and_store(&[alt_pubkey]);

        Ok(alt_pubkey)
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    fn maybe_refresh(&self) {
        let last = self.last_refresh.lock().unwrap();
        if last.elapsed() < Duration::from_secs(self.refresh_secs) {
            return;
        }

        let keys: Vec<Pubkey> = self
            .cache
            .iter()
            .map(
                |e: dashmap::mapref::multiple::RefMulti<Pubkey, AddressLookupTableAccount>| {
                    *e.key()
                },
            )
            .collect();
        drop(last); // release lock before blocking RPC call
        self.fetch_and_store(&keys);

        *self.last_refresh.lock().unwrap() = Instant::now();
    }

    fn persist_route_alt_cache(&self) {
        let map: std::collections::HashMap<String, String> = self
            .route_alt_cache
            .iter()
            .map(|e| (e.key().to_string(), e.value().to_string()))
            .collect();
        if let Ok(json) = serde_json::to_string(&map) {
            if let Err(e) = fs::write(ROUTE_ALT_PERSIST_FILE, json) {
                warn!(error = %e, "failed to persist route ALT cache");
            }
        }
    }

    fn fetch_and_store(&self, keys: &[Pubkey]) {
        if keys.is_empty() {
            return;
        }

        // getMultipleAccounts in batches of 100 (RPC limit).
        for chunk in keys.chunks(100) {
            match self.rpc.get_multiple_accounts(chunk) {
                Err(e) => {
                    warn!(error = %e, "getMultipleAccounts failed for ALTs");
                }
                Ok(accounts) => {
                    for (pubkey, maybe_account) in chunk.iter().zip(accounts.iter()) {
                        let Some(account) = maybe_account else {
                            continue;
                        };
                        match AddressLookupTable::deserialize(&account.data) {
                            Ok(alt) => {
                                self.cache.insert(
                                    *pubkey,
                                    AddressLookupTableAccount {
                                        key: *pubkey,
                                        addresses: alt.addresses.to_vec(),
                                    },
                                );
                                debug!(%pubkey, addresses = alt.addresses.len(), "ALT cached");
                            }
                            Err(e) => {
                                warn!(%pubkey, error = %e, "failed to deserialise ALT");
                            }
                        }
                    }
                }
            }
        }
    }

    fn recent_lookup_table_slot(&self) -> anyhow::Result<Slot> {
        // Try slot_hashes sysvar first (canonical), fall back to getSlot (works with sim-server).
        match self
            .rpc
            .get_account_with_commitment(&slot_hashes::id(), CommitmentConfig::confirmed())
        {
            Ok(resp) => {
                if let Some(account) = resp.value {
                    match extract_recent_alt_slot(&account.data) {
                        Ok(slot) => return Ok(slot),
                        Err(e) => {
                            warn!(error = %e, "slot_hashes sysvar deserialization failed, falling back to getSlot");
                        }
                    }
                } else {
                    warn!("slot_hashes sysvar account not found, falling back to getSlot");
                }
            }
            Err(e) => {
                warn!(error = %e, "slot_hashes sysvar fetch failed, falling back to getSlot");
            }
        }
        // Fallback: use getSlot RPC call — simpler, always works.
        let slot = self
            .rpc
            .get_slot_with_commitment(CommitmentConfig::confirmed())?;
        Ok(slot)
    }

    fn wait_until_alt_ready(&self, alt_pubkey: Pubkey, expected_len: usize) -> anyhow::Result<()> {
        let started = Instant::now();

        loop {
            let current_slot = self
                .rpc
                .get_slot_with_commitment(CommitmentConfig::confirmed())?;
            let maybe_account = self
                .rpc
                .get_account_with_commitment(&alt_pubkey, CommitmentConfig::confirmed())?
                .value;

            if let Some(account) = maybe_account {
                let table = AddressLookupTable::deserialize(&account.data)
                    .with_context(|| format!("deserialize dynamic ALT account {}", alt_pubkey))?;
                if table.addresses.len() >= expected_len
                    && current_slot > table.meta.last_extended_slot
                {
                    info!(
                        alt_pubkey = %alt_pubkey,
                        current_slot,
                        last_extended_slot = table.meta.last_extended_slot,
                        addresses = table.addresses.len(),
                        "dynamic ALT is ready"
                    );
                    return Ok(());
                }
                debug!(
                    alt_pubkey = %alt_pubkey,
                    current_slot,
                    last_extended_slot = table.meta.last_extended_slot,
                    addresses = table.addresses.len(),
                    expected_len,
                    "waiting for dynamic ALT to become usable"
                );
            }

            if started.elapsed() >= ALT_READY_TIMEOUT {
                anyhow::bail!(
                    "dynamic ALT {} not ready after {:?}",
                    alt_pubkey,
                    ALT_READY_TIMEOUT
                );
            }

            thread::sleep(ALT_READY_POLL_INTERVAL);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: parse ALT_ADDRESSES env var
// ---------------------------------------------------------------------------

pub fn parse_alt_env() -> Vec<Pubkey> {
    std::env::var("ALT_ADDRESSES")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            s.trim().parse::<Pubkey>().ok().or_else(|| {
                warn!(addr = s, "invalid pubkey in ALT_ADDRESSES, skipping");
                None
            })
        })
        .collect()
}

/// Stable hash of a (sorted) set of pubkeys — used as key for route_alt_cache.
fn hash_address_set(addresses: &[Pubkey]) -> u64 {
    let mut sorted: Vec<&Pubkey> = addresses.iter().collect();
    sorted.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sorted.hash(&mut h);
    h.finish()
}

fn extract_recent_alt_slot(data: &[u8]) -> anyhow::Result<Slot> {
    let slot_hashes: SlotHashes =
        bincode::deserialize(data).context("deserialize SlotHashes sysvar account")?;
    slot_hashes
        .first()
        .map(|(slot, _)| *slot)
        .ok_or_else(|| anyhow!("slot hashes sysvar is empty"))
}

#[cfg(test)]
mod tests {
    use super::extract_recent_alt_slot;
    use solana_sdk::{hash::Hash, slot_hashes::SlotHashes};

    #[test]
    fn extract_recent_alt_slot_uses_most_recent_entry() {
        let slot_hashes = SlotHashes::new(&[(42, Hash::default()), (41, Hash::default())]);
        let data = bincode::serialize(&slot_hashes).unwrap();
        assert_eq!(extract_recent_alt_slot(&data).unwrap(), 42);
    }

    #[test]
    fn extract_recent_alt_slot_rejects_empty_sysvar() {
        let data = bincode::serialize(&SlotHashes::new(&[])).unwrap();
        assert!(extract_recent_alt_slot(&data).is_err());
    }
}
