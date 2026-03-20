//! Shred Delta Tracker: real-time vault balance updates from decoded transactions.
//!
//! Watches SPL Token Transfer/TransferChecked instructions in shred-decoded
//! transactions. Updates pool reserves in PoolStateCache in-place.
//! Zero RPC calls — all state derived from Turbine shreds.

use crate::pool_state::PoolStateCache;
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::VersionedTransaction;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::sync::Arc;
use tracing::trace;

// DEX program IDs for swap detection
const RAYDIUM_V4: Pubkey = solana_sdk::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
const ORCA: Pubkey = solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
const METEORA: Pubkey = solana_sdk::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const PUMPSWAP: Pubkey = solana_sdk::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");

const SPL_TOKEN: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const TRANSFER_DISC: u8 = 3;
const TRANSFER_CHECKED_DISC: u8 = 12;

pub struct ShredDeltaTracker {
    pool_cache: Arc<PoolStateCache>,
    /// vault_pubkey → (pool_address, is_vault_a)
    vault_to_pool: DashMap<Pubkey, (Pubkey, bool)>,
    /// pool → swap count (for hot route detection from shreds)
    pool_swap_counts: DashMap<Pubkey, AtomicU64>,
    /// pool → (dex_label, first_seen_slot) for pools not in our cache
    unknown_pools: DashMap<Pubkey, (String, u64)>,
    /// Vaults touched by shred swaps — pending batch RPC refresh (Strategy C)
    pub dirty_vaults: DashMap<Pubkey, Instant>,
    pub txs_scanned: AtomicU64,
    pub transfers_found: AtomicU64,
    pub vault_updates: AtomicU64,
    /// Opportunity scanner: checks cross-DEX arb on every vault update.
    opp_scanner: Option<Arc<crate::opportunity_scanner::OpportunityScanner>>,
    /// Channel to emit arb routes directly to executor.
    opp_route_tx: Option<crossbeam_channel::Sender<crate::types::RouteParams>>,
}

impl ShredDeltaTracker {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        // Use existing vault index builder.
        let index = pool_cache.build_vault_index();
        let vault_to_pool = DashMap::with_capacity(index.len());
        for (vault, (pool, is_a)) in &index {
            vault_to_pool.insert(*vault, (*pool, *is_a));
        }

        tracing::info!(
            "ShredDeltaTracker: {} vault→pool mappings indexed",
            vault_to_pool.len()
        );

        Self {
            pool_cache,
            vault_to_pool,
            pool_swap_counts: DashMap::new(),
            unknown_pools: DashMap::new(),
            dirty_vaults: DashMap::new(),
            txs_scanned: AtomicU64::new(0),
            transfers_found: AtomicU64::new(0),
            vault_updates: AtomicU64::new(0),
            opp_scanner: None,
            opp_route_tx: None,
        }
    }

    /// Attach opportunity scanner for continuous arb detection.
    pub fn with_opportunity_scanner(
        mut self,
        scanner: Arc<crate::opportunity_scanner::OpportunityScanner>,
        route_tx: crossbeam_channel::Sender<crate::types::RouteParams>,
    ) -> Self {
        self.opp_scanner = Some(scanner);
        self.opp_route_tx = Some(route_tx);
        self
    }

    /// Register a single vault→pool mapping.
    pub fn register_vault(&self, vault: Pubkey, pool: Pubkey, is_vault_a: bool) {
        self.vault_to_pool.insert(vault, (pool, is_vault_a));
    }

    /// Re-index vaults (call after hydration cycle adds new pools).
    pub fn reindex(&self) {
        let index = self.pool_cache.build_vault_index();
        for (vault, (pool, is_a)) in &index {
            self.vault_to_pool.insert(*vault, (*pool, *is_a));
        }
    }

    /// Process a decoded transaction from Turbine shreds.
    /// Two strategies:
    /// 1. SPL Token Transfer detection (outer instructions only)
    /// 2. DEX program detection → flag vaults as "dirty" → trigger re-quote
    #[inline]
    pub fn process_transaction(&self, slot: u64, tx: &VersionedTransaction) {
        self.txs_scanned.fetch_add(1, Ordering::Relaxed);

        let msg = &tx.message;
        let keys = msg.static_account_keys();

        // Strategy A: DEX swap simulation — extract amount_in, compute delta, update optimistic reserves.
        for ix in msg.instructions() {
            let pid = match keys.get(ix.program_id_index as usize) {
                Some(pk) => *pk,
                None => continue,
            };

            if pid == RAYDIUM_V4 || pid == PUMPSWAP {
                // Raydium SwapBaseIn (disc=9): [disc:1, amount_in:8, min_out:8]
                // PumpSwap sell (disc=0x33...): [disc:8, amount_in:8, min_out:8]
                let amount_in = if pid == RAYDIUM_V4 && ix.data.len() >= 9 && ix.data[0] == 9 {
                    Some(u64::from_le_bytes(ix.data[1..9].try_into().unwrap_or([0;8])))
                } else if pid == PUMPSWAP && ix.data.len() >= 16 {
                    Some(u64::from_le_bytes(ix.data[8..16].try_into().unwrap_or([0;8])))
                } else {
                    None
                };

                if let Some(amount_in) = amount_in {
                    if amount_in == 0 { continue; }
                    // Find which pool this touches via vault accounts
                    for &acct_idx in &ix.accounts {
                        if let Some(key) = keys.get(acct_idx as usize) {
                            if let Some(entry) = self.vault_to_pool.get(key) {
                                let (pool_addr, is_vault_a) = *entry;
                                // Simulate delta: if vault_a is source → reserve_a += amount_in, reserve_b -= amount_out
                                if let Some(mut pool) = self.pool_cache.inner.get_mut(&pool_addr) {
                                    let (ra, rb) = pool.effective_reserves();
                                    if ra == 0 || rb == 0 { continue; }

                                    let fee = amount_in * pool.fee_bps / 10_000;
                                    let net_in = amount_in.saturating_sub(fee);

                                    if is_vault_a {
                                        // Swap A→B: reserve_a increases, reserve_b decreases
                                        let amount_out = (rb as u128 * net_in as u128 / (ra as u128 + net_in as u128)) as u64;
                                        pool.apply_shred_delta(amount_in as i64, -(amount_out as i64));
                                    } else {
                                        // Swap B→A: reserve_b increases, reserve_a decreases
                                        let amount_out = (ra as u128 * net_in as u128 / (rb as u128 + net_in as u128)) as u64;
                                        pool.apply_shred_delta(-(amount_out as i64), amount_in as i64);
                                    }

                                    self.vault_updates.fetch_add(1, Ordering::Relaxed);
                                    self.pool_cache.bump_swap_generation();
                                    // Strategy C: mark vault as dirty for batch RPC refresh
                                    self.dirty_vaults.insert(*key, Instant::now());
                                }
                                break;
                            }
                        }
                    }
                }
            } else if pid == ORCA || pid == METEORA {
                // For Orca/Meteora: complex math, just bump swap_gen
                for &acct_idx in &ix.accounts {
                    if let Some(key) = keys.get(acct_idx as usize) {
                        if self.vault_to_pool.contains_key(key) {
                            self.vault_updates.fetch_add(1, Ordering::Relaxed);
                            self.pool_cache.bump_swap_generation();
                            break;
                        }
                    }
                }
            }
        }

        // Strategy 1: SPL Token Transfer (original — catches direct transfers)
        for ix in msg.instructions() {
            let pid = match keys.get(ix.program_id_index as usize) {
                Some(pk) => *pk,
                None => continue,
            };

            if pid != SPL_TOKEN && pid != TOKEN_2022 {
                continue;
            }

            if ix.data.is_empty() {
                continue;
            }

            let disc = ix.data[0];
            let amount = match disc {
                TRANSFER_DISC if ix.data.len() >= 9 && ix.accounts.len() >= 3 => {
                    u64::from_le_bytes(ix.data[1..9].try_into().unwrap_or([0; 8]))
                }
                TRANSFER_CHECKED_DISC if ix.data.len() >= 9 && ix.accounts.len() >= 4 => {
                    u64::from_le_bytes(ix.data[1..9].try_into().unwrap_or([0; 8]))
                }
                _ => continue,
            };

            if amount == 0 {
                continue;
            }

            self.transfers_found.fetch_add(1, Ordering::Relaxed);

            let (src_idx, dst_idx) = match disc {
                TRANSFER_DISC => (0usize, 1usize),
                TRANSFER_CHECKED_DISC => (0, 2),
                _ => continue,
            };

            let source = ix.accounts.get(src_idx)
                .and_then(|&i| keys.get(i as usize)).copied();
            let dest = ix.accounts.get(dst_idx)
                .and_then(|&i| keys.get(i as usize)).copied();

            if let Some(src) = source {
                if let Some(e) = self.vault_to_pool.get(&src) {
                    let (pool, is_a) = *e;
                    self.apply_delta(&pool, is_a, amount, false, slot);
                }
            }
            if let Some(dst) = dest {
                if let Some(e) = self.vault_to_pool.get(&dst) {
                    let (pool, is_a) = *e;
                    self.apply_delta(&pool, is_a, amount, true, slot);
                }
            }
        }
    }

    fn apply_delta(&self, pool: &Pubkey, is_a: bool, amount: u64, credit: bool, slot: u64) {
        if let Some(mut entry) = self.pool_cache.inner.get_mut(pool) {
            let reserve = if is_a { &mut entry.reserve_a } else { &mut entry.reserve_b };
            let old = *reserve;
            *reserve = if credit {
                old.saturating_add(amount)
            } else {
                old.saturating_sub(amount)
            };
            self.pool_cache.bump_swap_generation();
            self.vault_updates.fetch_add(1, Ordering::Relaxed);

            trace!(
                %pool, vault = if is_a { "A" } else { "B" },
                old, new = *reserve,
                delta = if credit { amount as i64 } else { -(amount as i64) },
                slot, "shred delta"
            );

            // Opportunity scanner: check cross-DEX arb on this vault update.
            drop(entry); // Release DashMap lock before scanner.
            if let Some(ref scanner) = self.opp_scanner {
                let routes = scanner.on_vault_update(pool);
                if let Some(ref tx) = self.opp_route_tx {
                    for route in routes {
                        let _ = tx.try_send(route);
                    }
                }
            }
        } else {
            self.vault_updates.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn tracked_vaults(&self) -> usize {
        self.vault_to_pool.len()
    }

    /// Detect DEX swap instructions in shred-decoded TXs.
    /// Tracks which pools are actively swapped → feeds hot route detection.
    /// Also detects unknown pools (accounts touching DEX programs not in our cache).
    pub fn detect_dex_activity(&self, slot: u64, tx: &VersionedTransaction) {
        let msg = &tx.message;
        let keys = msg.static_account_keys();

        for ix in msg.instructions() {
            let pid = match keys.get(ix.program_id_index as usize) {
                Some(pk) => *pk,
                None => continue,
            };

            // Check if this is a DEX program instruction
            let dex = match identify_dex(&pid) {
                Some(d) => d,
                None => continue,
            };

            // The first account in DEX swap instructions is typically the pool
            let pool_key = ix.accounts.first()
                .and_then(|&idx| keys.get(idx as usize))
                .copied();

            if let Some(pool) = pool_key {
                // Track swap activity per pool
                self.pool_swap_counts
                    .entry(pool)
                    .or_insert_with(|| std::sync::atomic::AtomicU64::new(0))
                    .fetch_add(1, Ordering::Relaxed);

                // If pool not in our cache → new pool discovered from shreds
                if self.pool_cache.get(&pool).is_none() {
                    self.unknown_pools
                        .entry(pool)
                        .or_insert_with(|| (dex.to_string(), slot));
                }
            }
        }
    }

    /// Get the most active pools by swap count (hot routes from shred data).
    pub fn hot_pools_by_activity(&self, top_n: usize) -> Vec<(Pubkey, u64)> {
        let mut pools: Vec<(Pubkey, u64)> = self.pool_swap_counts.iter()
            .map(|e| (*e.key(), e.value().load(Ordering::Relaxed)))
            .collect();
        pools.sort_by(|a, b| b.1.cmp(&a.1));
        pools.truncate(top_n);
        pools
    }

    /// Get pools discovered from shreds that aren't in our cache.
    pub fn unknown_pools_discovered(&self) -> Vec<(Pubkey, String, u64)> {
        self.unknown_pools.iter()
            .map(|e| (*e.key(), e.value().0.clone(), e.value().1))
            .collect()
    }

    /// Drain dirty vaults for batch RPC refresh (Strategy C).
    /// Returns vault pubkeys that were touched by shred swaps since last drain.
    pub fn drain_dirty_vaults(&self, max: usize) -> Vec<Pubkey> {
        let mut vaults = Vec::with_capacity(max);
        let mut to_remove = Vec::new();
        for entry in self.dirty_vaults.iter() {
            if vaults.len() >= max { break; }
            vaults.push(*entry.key());
            to_remove.push(*entry.key());
        }
        for key in to_remove {
            self.dirty_vaults.remove(&key);
        }
        vaults
    }

    /// Update pool reserves from fetched vault balances (post-RPC).
    pub fn apply_vault_balances(&self, balances: &[(Pubkey, u64)]) {
        for (vault_key, balance) in balances {
            if let Some(entry) = self.vault_to_pool.get(vault_key) {
                let (pool_addr, is_vault_a) = *entry;
                if let Some(mut pool) = self.pool_cache.inner.get_mut(&pool_addr) {
                    if is_vault_a {
                        pool.reserve_a = *balance;
                        pool.reserve_a_optimistic = *balance;
                    } else {
                        pool.reserve_b = *balance;
                        pool.reserve_b_optimistic = *balance;
                    }
                    pool.last_shred_update = Instant::now();
                    pool.last_updated = Instant::now();
                }
            }
        }
        if !balances.is_empty() {
            self.pool_cache.bump_swap_generation();
        }
    }

    /// Reset swap counters (call periodically to track recent activity only).
    pub fn reset_activity_counters(&self) {
        self.pool_swap_counts.clear();
    }
}

fn identify_dex(program_id: &Pubkey) -> Option<&'static str> {
    if *program_id == RAYDIUM_V4 { return Some("raydium_v4"); }
    if *program_id == ORCA { return Some("orca"); }
    if *program_id == METEORA { return Some("meteora"); }
    if *program_id == PUMPSWAP { return Some("pumpswap"); }
    None
}
