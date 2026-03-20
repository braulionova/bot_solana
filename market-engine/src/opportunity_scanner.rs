// opportunity_scanner.rs — Continuous on-chain opportunity scanner.
//
// Runs on EVERY vault update from shreds (1,300+/s). When a vault balance
// changes, instantly computes the new pool price and compares against ALL
// counterpart pools for the same token pair. If spread > fees → emit route.
//
// This is the core arb detection engine. Speed: <1μs per check (DashMap lookup).
// Advantage: 100-200ms ahead of Geyser bots, 300-1300ms ahead of RPC bots.

use std::sync::Arc;
use std::collections::HashMap;
use std::time::Instant;

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info};

use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{DexType, Hop, RouteParams};

const WSOL: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
const USDC: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// Minimum spread in bps to consider profitable (covers 2x25bps fees + gas).
const MIN_SPREAD_BPS: f64 = 55.0;
/// Maximum spread — above this is likely stale reserves.
const MAX_SPREAD_BPS: f64 = 2000.0;
/// Borrow amounts to try.
const BORROWS: &[u64] = &[50_000_000, 100_000_000, 500_000_000];
/// Minimum reserves to consider a pool tradeable.
const MIN_RESERVE: u64 = 1_000_000; // 0.001 SOL — low to catch newly graduated pools

/// Pre-computed index: for each pool, which other pools share the same token pair?
/// This allows O(1) lookup of counterpart pools when a vault update arrives.
pub struct OpportunityScanner {
    pool_cache: Arc<PoolStateCache>,
    /// pool_address → vec of counterpart pool addresses (same token pair, different pool).
    counterpart_index: DashMap<Pubkey, Vec<Pubkey>>,
    /// Stats
    checks: std::sync::atomic::AtomicU64,
    opportunities: std::sync::atomic::AtomicU64,
    last_log: parking_lot::Mutex<Instant>,
}

impl OpportunityScanner {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        Self {
            pool_cache,
            counterpart_index: DashMap::new(),
            checks: std::sync::atomic::AtomicU64::new(0),
            opportunities: std::sync::atomic::AtomicU64::new(0),
            last_log: parking_lot::Mutex::new(Instant::now()),
        }
    }

    /// Build counterpart index from current pool cache.
    /// Call once at startup and periodically (every 5 min) to capture new pools.
    pub fn rebuild_index(&self) {
        self.counterpart_index.clear();

        // Group pools by canonical token pair (min_mint, max_mint).
        let mut pair_pools: HashMap<(Pubkey, Pubkey), Vec<Pubkey>> = HashMap::new();
        for pool in self.pool_cache.inner_iter() {
            if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
                continue;
            }
            let pair = canonical_pair(&pool.token_a, &pool.token_b);
            pair_pools.entry(pair).or_default().push(pool.pool_address);
        }

        // For each pool, store its counterparts (same pair, different pool).
        let mut indexed = 0u32;
        for (_, pools) in &pair_pools {
            if pools.len() < 2 { continue; }
            for &pool_addr in pools {
                let counterparts: Vec<Pubkey> = pools.iter()
                    .filter(|&&p| p != pool_addr)
                    .copied()
                    .collect();
                if !counterparts.is_empty() {
                    self.counterpart_index.insert(pool_addr, counterparts);
                    indexed += 1;
                }
            }
        }

        info!(
            indexed_pools = indexed,
            pairs = pair_pools.values().filter(|v| v.len() >= 2).count(),
            "opportunity-scanner: index rebuilt"
        );
    }

    /// Called on EVERY vault update from the delta tracker.
    /// Checks if the reserve change creates a cross-DEX arb opportunity.
    /// Returns arb routes if profitable.
    ///
    /// This runs ~1,300 times/second. Must be ULTRA fast (<1μs per call).
    #[inline]
    pub fn on_vault_update(&self, pool_addr: &Pubkey) -> Vec<RouteParams> {
        self.checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // O(1) lookup: does this pool have counterparts?
        let counterparts = match self.counterpart_index.get(pool_addr) {
            Some(c) => c.clone(),
            None => return vec![], // No counterpart = no arb possible
        };

        // Get the updated pool.
        let pool = match self.pool_cache.get(pool_addr) {
            Some(p) => p,
            None => return vec![],
        };

        if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
            return vec![];
        }

        // Determine anchor token.
        let (anchor, meme) = if pool.token_a == WSOL {
            (WSOL, pool.token_b)
        } else if pool.token_b == WSOL {
            (WSOL, pool.token_a)
        } else if pool.token_a == USDC {
            (USDC, pool.token_b)
        } else if pool.token_b == USDC {
            (USDC, pool.token_a)
        } else {
            return vec![]; // No borrowable anchor
        };

        let mut routes = Vec::new();

        for cp_addr in &counterparts {
            let cp = match self.pool_cache.get(cp_addr) {
                Some(p) if p.reserve_a >= MIN_RESERVE && p.reserve_b >= MIN_RESERVE => p,
                _ => continue,
            };

            // Compare prices: how much meme_token per unit of anchor?
            for &borrow in BORROWS {
                let out_this = quote_buy(&pool, anchor, borrow);
                let out_cp = quote_buy(&cp, anchor, borrow);

                if out_this == 0 || out_cp == 0 { continue; }

                // Determine direction: buy on cheaper, sell on expensive.
                let (buy_pool, sell_pool, buy_out) = if out_this > out_cp {
                    // This pool gives more meme_token → buy here, sell on counterpart.
                    (&pool, &cp, out_this)
                } else if out_cp > out_this {
                    (&cp, &pool, out_cp)
                } else {
                    continue; // Same price
                };

                // Sell the meme_token on the expensive pool to get anchor back.
                let sell_out = quote_sell(sell_pool, anchor, buy_out);
                if sell_out <= borrow { continue; }

                let gross = sell_out - borrow;
                let net = gross as i64 - 15_000;
                if net < 5_000 { continue; }

                let spread = (sell_out as f64 / borrow as f64 - 1.0) * 10_000.0;
                // Use actual pool fees instead of hardcoded 55bps.
                let combined_fees = crate::fee_tiers::pool_fee_bps(buy_pool)
                    + crate::fee_tiers::pool_fee_bps(sell_pool);
                let min_spread = (combined_fees as f64) + 10.0; // fees + 10bps min profit
                if spread < min_spread || spread > MAX_SPREAD_BPS { continue; }

                self.opportunities.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                routes.push(RouteParams {
                    hops: vec![
                        Hop {
                            pool: buy_pool.pool_address,
                            token_in: anchor,
                            token_out: meme,
                            dex_program: dex_program(buy_pool.dex_type),
                            amount_out: buy_out,
                            price_impact: (borrow as f64 / buy_pool.reserve_a.max(1) as f64) * 100.0,
                        },
                        Hop {
                            pool: sell_pool.pool_address,
                            token_in: meme,
                            token_out: anchor,
                            dex_program: dex_program(sell_pool.dex_type),
                            amount_out: sell_out,
                            price_impact: 0.0,
                        },
                    ],
                    borrow_amount: borrow,
                    gross_profit: gross,
                    net_profit: net,
                    risk_factor: 0.15,
                    strategy: "shred_opp_scan",
                    tier: 1,
                });
                break; // Best borrow for this pair
            }
        }

        // Periodic stats + index rebuild (every 60s to capture new pools).
        {
            let mut last = self.last_log.lock();
            if last.elapsed().as_secs() >= 60 {
                let checks = self.checks.swap(0, std::sync::atomic::Ordering::Relaxed);
                let opps = self.opportunities.swap(0, std::sync::atomic::Ordering::Relaxed);

                // Rebuild index to capture newly graduated/created pools.
                self.rebuild_index();

                info!(
                    checks_60s = checks,
                    opportunities_60s = opps,
                    indexed = self.counterpart_index.len(),
                    "opportunity-scanner: stats + index rebuilt"
                );
                *last = Instant::now();
            }
        }

        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
        routes.truncate(3);
        routes
    }
}

#[inline]
fn quote_buy(pool: &DexPool, anchor: Pubkey, amount: u64) -> u64 {
    if pool.token_a == anchor {
        pool.quote_a_to_b(amount)
    } else {
        pool.quote_b_to_a(amount)
    }
}

#[inline]
fn quote_sell(pool: &DexPool, anchor: Pubkey, amount: u64) -> u64 {
    if pool.token_a == anchor {
        pool.quote_b_to_a(amount)
    } else {
        pool.quote_a_to_b(amount)
    }
}

fn canonical_pair(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a < b { (*a, *b) } else { (*b, *a) }
}

fn dex_program(dex: DexType) -> Pubkey {
    match dex {
        DexType::RaydiumAmmV4 => solana_sdk::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"),
        DexType::PumpSwap => solana_sdk::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
        DexType::OrcaWhirlpool => solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
        DexType::MeteoraDlmm => solana_sdk::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS"),
        DexType::RaydiumClmm => solana_sdk::pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
        _ => Pubkey::default(),
    }
}
