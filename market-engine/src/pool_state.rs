// pool_state.rs – In-memory pool state cache.

use crate::meteora::{
    quote_exact_in, MeteoraBinArray, MeteoraStaticParameters, MeteoraVariableParameters,
};
use crate::orca::{
    quote_exact_in as quote_orca_exact_in, tick_array_start_index as orca_array_start,
    OrcaTickArray, ORCA_TICK_ARRAY_SIZE,
};
use crate::types::DexType;
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::HashMap, sync::Arc, time::Instant};

#[derive(Debug, Clone, Default)]
pub struct RaydiumMetadata {
    pub authority: Pubkey,
    pub nonce: u8,
    pub open_orders: Pubkey,
    pub target_orders: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub market_id: Pubkey,
    pub market_program: Pubkey,
    pub market_event_queue: Pubkey,
    pub market_bids: Pubkey,
    pub market_asks: Pubkey,
    pub market_base_vault: Pubkey,
    pub market_quote_vault: Pubkey,
    pub market_vault_signer: Pubkey,
}

#[derive(Debug, Clone, Default)]
pub struct OrcaMetadata {
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_spacing: u16,
    pub tick_current_index: i32,
    pub tick_arrays: Arc<HashMap<i32, OrcaTickArray>>,
}

#[derive(Debug, Clone, Default)]
pub struct MeteoraMetadata {
    pub active_id: i32,
    pub bin_step: u16,
    pub token_vault_a: Pubkey,
    pub token_vault_b: Pubkey,
    pub parameters: MeteoraStaticParameters,
    pub v_parameters: MeteoraVariableParameters,
    pub bin_array_bitmap: [u64; 16],
    pub bin_arrays: Arc<HashMap<i64, MeteoraBinArray>>,
}

#[derive(Debug, Clone)]
pub struct DexPool {
    pub pool_address: Pubkey,
    pub dex_type: DexType,
    pub token_a: Pubkey,
    pub token_b: Pubkey,
    pub decimals_a: u8,
    pub decimals_b: u8,
    pub reserve_a: u64,
    pub reserve_b: u64,
    /// Optimistic reserves from shred delta simulation (Strategy A).
    /// Used for quoting when fresher than Helius polling (<2s).
    pub reserve_a_optimistic: u64,
    pub reserve_b_optimistic: u64,
    pub last_shred_update: Instant,
    pub pool_vault_a_balance: u64,
    pub pool_vault_b_balance: u64,
    pub market_vault_a_balance: u64,
    pub market_vault_b_balance: u64,
    pub last_updated: Instant,
    pub fee_bps: u64,
    /// DEX-specific metadata (e.g. Raydium account list).
    pub raydium_meta: Option<RaydiumMetadata>,
    pub orca_meta: Option<OrcaMetadata>,
    pub meteora_meta: Option<MeteoraMetadata>,
}

impl DexPool {
    pub fn reserve_a_ui(&self) -> f64 {
        self.reserve_a as f64 / 10f64.powi(self.decimals_a as i32)
    }

    pub fn reserve_b_ui(&self) -> f64 {
        self.reserve_b as f64 / 10f64.powi(self.decimals_b as i32)
    }

    /// Conservative TVL estimate using available oracle prices.
    /// If only one side has an oracle price, double that side to avoid
    /// discarding otherwise-healthy pools with partial oracle coverage.
    pub fn known_tvl_usd_lower_bound(
        &self,
        oracle_prices: &std::collections::HashMap<Pubkey, f64>,
    ) -> Option<f64> {
        let side_a_usd = oracle_prices
            .get(&self.token_a)
            .map(|price| self.reserve_a_ui() * price);
        let side_b_usd = oracle_prices
            .get(&self.token_b)
            .map(|price| self.reserve_b_ui() * price);

        match (side_a_usd, side_b_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a * 2.0),
            (None, Some(b)) => Some(b * 2.0),
            (None, None) => None,
        }
    }

    /// Get best available reserves: optimistic from shreds (<10s) or Helius polled.
    /// Window is 10s (not 2s) to bridge between hydration cycles when Agave is down.
    /// Shred-decoded TX deltas keep optimistic reserves accurate even without RPC polling.
    #[inline]
    pub fn effective_reserves(&self) -> (u64, u64) {
        if self.reserve_a_optimistic > 0
            && self.reserve_b_optimistic > 0
            && self.last_shred_update.elapsed().as_secs() < 10
        {
            (self.reserve_a_optimistic, self.reserve_b_optimistic)
        } else {
            (self.reserve_a, self.reserve_b)
        }
    }

    /// Apply a shred-derived delta to optimistic reserves.
    pub fn apply_shred_delta(&mut self, delta_a: i64, delta_b: i64) {
        let (base_a, base_b) = if self.reserve_a_optimistic > 0 {
            (self.reserve_a_optimistic, self.reserve_b_optimistic)
        } else {
            (self.reserve_a, self.reserve_b)
        };
        self.reserve_a_optimistic = (base_a as i64 + delta_a).max(0) as u64;
        self.reserve_b_optimistic = (base_b as i64 + delta_b).max(0) as u64;
        self.last_shred_update = Instant::now();
    }

    pub fn quote_a_to_b(&self, amount_in: u64) -> u64 {
        let (ra, rb) = self.effective_reserves();
        if ra == 0 || rb == 0 {
            return 0;
        }

        if let Some(orca) = &self.orca_meta {
            if orca.liquidity == 0 || orca.sqrt_price == 0 || orca.tick_spacing == 0 {
                return 0;
            }
            if !orca.tick_arrays.is_empty() {
                return quote_orca_exact_in(
                    amount_in,
                    true,
                    orca.sqrt_price,
                    orca.liquidity,
                    orca.tick_current_index,
                    orca.tick_spacing,
                    self.fee_bps,
                    &orca.tick_arrays,
                )
                .amount_out;
            }

            let fee_bps = self.fee_bps as f64;
            let amount_in_after_fee = amount_in as f64 * (10_000.0 - fee_bps) / 10_000.0;
            let sqrt_p = orca.sqrt_price as f64 / (1u128 << 64) as f64;
            let l = orca.liquidity as f64;
            let sqrt_p_next = (sqrt_p * l) / (amount_in_after_fee * sqrt_p + l);
            return (l * (sqrt_p - sqrt_p_next)) as u64;
        }

        if let Some(meteora) = &self.meteora_meta {
            return quote_exact_in(
                amount_in,
                true,
                meteora.active_id,
                meteora.bin_step,
                &meteora.parameters,
                &meteora.v_parameters,
                &meteora.bin_arrays,
            );
        }

        // XY=K with optimistic reserves from shreds
        let fee_numerator = 10_000u128 - self.fee_bps as u128;
        let amount_in_with_fee = amount_in as u128 * fee_numerator;
        let numerator = amount_in_with_fee * rb as u128;
        let denominator = ra as u128 * 10_000 + amount_in_with_fee;
        (numerator / denominator) as u64
    }

    pub fn quote_b_to_a(&self, amount_in: u64) -> u64 {
        let (ra, rb) = self.effective_reserves();
        if ra == 0 || rb == 0 {
            return 0;
        }

        if let Some(orca) = &self.orca_meta {
            if orca.liquidity == 0 || orca.sqrt_price == 0 || orca.tick_spacing == 0 {
                return 0;
            }
            if !orca.tick_arrays.is_empty() {
                return quote_orca_exact_in(
                    amount_in,
                    false,
                    orca.sqrt_price,
                    orca.liquidity,
                    orca.tick_current_index,
                    orca.tick_spacing,
                    self.fee_bps,
                    &orca.tick_arrays,
                )
                .amount_out;
            }

            let fee_bps = self.fee_bps as f64;
            let amount_in_after_fee = amount_in as f64 * (10_000.0 - fee_bps) / 10_000.0;
            let sqrt_p = orca.sqrt_price as f64 / (1u128 << 64) as f64;
            let l = orca.liquidity as f64;
            let sqrt_p_next = sqrt_p + (amount_in_after_fee / l);
            return (l * ((1.0 / sqrt_p) - (1.0 / sqrt_p_next))) as u64;
        }

        if let Some(meteora) = &self.meteora_meta {
            return quote_exact_in(
                amount_in,
                false,
                meteora.active_id,
                meteora.bin_step,
                &meteora.parameters,
                &meteora.v_parameters,
                &meteora.bin_arrays,
            );
        }

        // XY=K with optimistic reserves
        let fee_numerator = 10_000u128 - self.fee_bps as u128;
        let amount_in_with_fee = amount_in as u128 * fee_numerator;
        let numerator = amount_in_with_fee * ra as u128;
        let denominator = rb as u128 * 10_000 + amount_in_with_fee;
        (numerator / denominator) as u64
    }
}

pub struct PoolStateCache {
    pub(crate) inner: DashMap<Pubkey, DexPool>,
    /// Incremented only by apply_swap_delta (live market moves), not by periodic hydration.
    /// Used by RouteEngine to skip Bellman-Ford when pool state hasn't changed.
    swap_gen: AtomicU64,
    /// Secondary index: (min(token_a,token_b), max(token_a,token_b)) → pool addresses.
    /// Accelerates cross-dex pair lookup from O(N) to O(1).
    pair_index: DashMap<(Pubkey, Pubkey), Vec<Pubkey>>,
}

impl PoolStateCache {
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
            swap_gen: AtomicU64::new(0),
            pair_index: DashMap::new(),
        }
    }

    /// Generation counter — only incremented on live swap deltas, not periodic refreshes.
    pub fn swap_generation(&self) -> u64 {
        self.swap_gen.load(Ordering::Relaxed)
    }

    /// Bump the swap generation (called by shred delta tracker on vault balance changes).
    pub fn bump_swap_generation(&self) {
        self.swap_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn upsert(&self, pool: DexPool) {
        let pair = canonical_pair(&pool.token_a, &pool.token_b);
        let addr = pool.pool_address;
        self.inner.insert(pool.pool_address, pool);
        // Maintain pair index
        self.pair_index.entry(pair).or_default().retain(|a| *a != addr);
        self.pair_index.entry(pair).or_default().push(addr);
    }

    /// Get all pools for a token pair (O(1) lookup).
    pub fn pools_for_pair(&self, token_a: &Pubkey, token_b: &Pubkey) -> Vec<DexPool> {
        let pair = canonical_pair(token_a, token_b);
        match self.pair_index.get(&pair) {
            Some(addrs) => addrs
                .iter()
                .filter_map(|a| self.inner.get(a).map(|p| p.clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn get(&self, address: &Pubkey) -> Option<DexPool> {
        self.inner.get(address).map(|r| r.clone())
    }

    /// Remove a pool from cache (e.g., pool account closed on-chain).
    pub fn remove(&self, address: &Pubkey) {
        self.inner.remove(address);
    }

    pub fn update_reserve_a(&self, address: &Pubkey, amount: u64) {
        if let Some(mut e) = self.inner.get_mut(address) {
            e.reserve_a = amount;
            e.last_updated = Instant::now();
        }
    }

    pub fn update_reserve_b(&self, address: &Pubkey, amount: u64) {
        if let Some(mut e) = self.inner.get_mut(address) {
            e.reserve_b = amount;
            e.last_updated = Instant::now();
        }
    }

    pub fn update_raydium_meta(&self, address: &Pubkey, meta: RaydiumMetadata) {
        if let Some(mut e) = self.inner.get_mut(address) {
            e.raydium_meta = Some(meta);
        }
    }

    pub fn apply_swap_delta(
        &self,
        pool_addr: &Pubkey,
        amount_in: u64,
        amount_out: u64,
        a_to_b: bool,
    ) {
        if let Some(mut e) = self.inner.get_mut(pool_addr) {
            if a_to_b {
                e.reserve_a = e.reserve_a.saturating_add(amount_in);
                e.reserve_b = e.reserve_b.saturating_sub(amount_out);
            } else {
                e.reserve_b = e.reserve_b.saturating_add(amount_in);
                e.reserve_a = e.reserve_a.saturating_sub(amount_out);
            }
            // For Orca CLMM: estimate new tick from reserve ratio change.
            // This keeps tick_current_index roughly in sync between hydrations
            // so the TX builder derives the correct tick array sequence.
            if e.dex_type == DexType::OrcaWhirlpool {
                let fee_bps = e.fee_bps;
                if let Some(ref mut meta) = e.orca_meta {
                    if amount_in > 0 && meta.liquidity > 0 && meta.sqrt_price > 0 && meta.tick_spacing > 0 {
                        let quote = quote_orca_exact_in(
                            amount_in,
                            a_to_b,
                            meta.sqrt_price,
                            meta.liquidity,
                            meta.tick_current_index,
                            meta.tick_spacing,
                            fee_bps,
                            &meta.tick_arrays,
                        );
                        // Update tick_current_index based on traversal result.
                        // The quote engine tracks tick movement through initialized ticks.
                        if !quote.traversed_arrays.is_empty() {
                            // Estimate: after the swap the tick is near the last traversed array.
                            let last_arr = *quote.traversed_arrays.last().unwrap();
                            let arr_offset = meta.tick_spacing as i32
                                * ORCA_TICK_ARRAY_SIZE;
                            // Place tick at beginning of last array (conservative estimate).
                            let estimated_tick = if a_to_b {
                                last_arr + arr_offset - 1
                            } else {
                                last_arr
                            };
                            // Only update if tick actually moved to a different array.
                            let old_arr = orca_array_start(
                                meta.tick_current_index,
                                meta.tick_spacing,
                            );
                            if last_arr != old_arr {
                                meta.tick_current_index = estimated_tick;
                            }
                        }
                    }
                }
            }
            e.last_updated = Instant::now();
        }
        // Signal to RouteEngine that the graph topology changed.
        self.swap_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn mark_market_activity(&self) {
        self.swap_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn pools_for_token(&self, mint: &Pubkey) -> Vec<DexPool> {
        self.inner
            .iter()
            .filter(|e| &e.token_a == mint || &e.token_b == mint)
            .map(|e| e.clone())
            .collect()
    }

    pub fn inner_iter(&self) -> impl Iterator<Item = DexPool> + '_ {
        self.inner.iter().map(|e| e.clone())
    }

    /// Build a vault→(pool_address, is_vault_a) reverse index.
    /// Returns vaults sorted by pool liquidity (highest reserves first).
    pub fn build_vault_index(&self) -> HashMap<Pubkey, (Pubkey, bool)> {
        // Sort pools by total reserves descending so highest-liquidity pools
        // end up first when the caller truncates with .take(N).
        let mut pools: Vec<_> = self.inner.iter().map(|e| e.value().clone()).collect();
        pools.sort_by(|a, b| {
            let liq_a = a.reserve_a.saturating_add(a.reserve_b);
            let liq_b = b.reserve_a.saturating_add(b.reserve_b);
            liq_b.cmp(&liq_a)
        });

        let mut map = HashMap::new();
        for pool in &pools {
            // Raydium: pool-level vaults + market-level vaults
            if let Some(ref meta) = pool.raydium_meta {
                if meta.vault_a != Pubkey::default() {
                    map.insert(meta.vault_a, (pool.pool_address, true));
                }
                if meta.vault_b != Pubkey::default() {
                    map.insert(meta.vault_b, (pool.pool_address, false));
                }
            }
            // Orca: token_vault_a/b
            if let Some(ref meta) = pool.orca_meta {
                if meta.token_vault_a != Pubkey::default() {
                    map.insert(meta.token_vault_a, (pool.pool_address, true));
                }
                if meta.token_vault_b != Pubkey::default() {
                    map.insert(meta.token_vault_b, (pool.pool_address, false));
                }
            }
            // Meteora: token_vault_a/b
            if let Some(ref meta) = pool.meteora_meta {
                if meta.token_vault_a != Pubkey::default() {
                    map.insert(meta.token_vault_a, (pool.pool_address, true));
                }
                if meta.token_vault_b != Pubkey::default() {
                    map.insert(meta.token_vault_b, (pool.pool_address, false));
                }
            }
        }
        map
    }

    /// Update a pool's reserve via its vault address + bump swap_gen.
    /// Returns true if the reserve actually changed.
    pub fn update_reserve_by_vault(
        &self,
        pool_address: &Pubkey,
        is_vault_a: bool,
        new_balance: u64,
    ) -> bool {
        if let Some(mut e) = self.inner.get_mut(pool_address) {
            let old = if is_vault_a { e.reserve_a } else { e.reserve_b };
            if old == new_balance {
                return false;
            }
            if is_vault_a {
                e.reserve_a = new_balance;
            } else {
                e.reserve_b = new_balance;
            }
            e.last_updated = Instant::now();
            self.swap_gen.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

impl Default for PoolStateCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical pair key: smaller pubkey first.
fn canonical_pair(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a < b { (*a, *b) } else { (*b, *a) }
}
