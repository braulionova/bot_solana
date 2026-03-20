//! dlmm_whirlpool_arb.rs – Meteora DLMM <-> Orca Whirlpool arbitrage.
//!
//! When Meteora adjusts bins during volatility, spreads temporarily widen
//! vs Orca Whirlpool on the same token pair. This scanner detects those
//! divergences and builds atomic flash loan arb routes.

use std::collections::HashSet;
use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info};

use crate::cross_dex::{build_cross_dex_route, build_cross_dex_route_reverse, dex_fee_bps};
use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{DexType, RouteParams};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum divergence above combined fees (bps) to attempt arb.
const MIN_MARGIN_ABOVE_FEES_BPS: u64 = 20;

/// Minimum reserve in either token (raw units).
const MIN_RESERVE: u64 = 1_000_000;

/// Maximum plausible return: 3% profit for concentrated-liquidity pairs.
const MAX_RETURN_MULTIPLIER: f64 = 1.03;

/// WSOL mint.
const WSOL: &str = "So11111111111111111111111111111111111111112";
/// USDC mint.
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Anchor tokens for flash loan borrow.
const ANCHOR_TOKENS: &[&str] = &[WSOL, USDC];

/// High-volume token mints to focus on (reduce search space).
/// These tokens commonly have both DLMM and Whirlpool pools.
const FOCUS_TOKENS: &[&str] = &[
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263", // BONK
    "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm", // WIF
    "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",  // JUP
    "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL",  // JTO
    "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3", // PYTH
    "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R", // RAY
    "So11111111111111111111111111111111111111112",     // WSOL (for pairs like WSOL/USDC)
];

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

pub struct DlmmWhirlpoolArb {
    pool_cache: Arc<PoolStateCache>,
}

impl DlmmWhirlpoolArb {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        Self { pool_cache }
    }

    /// Scan for DLMM/Whirlpool price divergences on the same token pair.
    /// Returns flash loan arb routes exploiting temporary bin imbalances.
    pub fn scan_bin_divergence(
        &self,
        borrow_amounts: &[u64],
    ) -> Vec<RouteParams> {
        let anchor_set: HashSet<Pubkey> = ANCHOR_TOKENS
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();

        let focus_set: HashSet<Pubkey> = FOCUS_TOKENS
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();

        // Collect Meteora DLMM and Orca Whirlpool pools separately.
        let mut dlmm_pools: Vec<DexPool> = Vec::new();
        let mut whirlpool_pools: Vec<DexPool> = Vec::new();

        for entry in self.pool_cache.inner.iter() {
            let pool = entry.value();
            if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
                continue;
            }

            match pool.dex_type {
                DexType::MeteoraDlmm => {
                    // Require meteora metadata for accurate quoting.
                    if pool.meteora_meta.is_some() {
                        dlmm_pools.push(pool.clone());
                    }
                }
                DexType::OrcaWhirlpool => {
                    // Require orca metadata with tick arrays for accurate quoting.
                    if let Some(meta) = &pool.orca_meta {
                        if !meta.tick_arrays.is_empty()
                            && meta.liquidity > 0
                            && meta.sqrt_price > 0
                        {
                            whirlpool_pools.push(pool.clone());
                        }
                    }
                }
                _ => continue,
            }
        }

        // Index DLMM pools by canonical pair.
        let mut dlmm_by_pair: std::collections::HashMap<(Pubkey, Pubkey), Vec<DexPool>> =
            std::collections::HashMap::new();
        for pool in &dlmm_pools {
            let key = pair_key(&pool.token_a, &pool.token_b);
            dlmm_by_pair.entry(key).or_default().push(pool.clone());
        }

        let mut routes = Vec::new();
        let reference_amount = borrow_amounts.first().copied().unwrap_or(100_000_000);

        // For each Whirlpool, check if there is a DLMM pool for the same pair.
        for wp in &whirlpool_pools {
            let key = pair_key(&wp.token_a, &wp.token_b);

            // Optional focus filter: skip pairs where neither token is in the focus set.
            let has_focus = focus_set.contains(&wp.token_a) || focus_set.contains(&wp.token_b);
            if !has_focus {
                continue;
            }

            // At least one side must be an anchor token.
            let a_is_anchor = anchor_set.contains(&key.0);
            let b_is_anchor = anchor_set.contains(&key.1);
            if !a_is_anchor && !b_is_anchor {
                continue;
            }

            let dlmm_list = match dlmm_by_pair.get(&key) {
                Some(list) if !list.is_empty() => list,
                _ => continue,
            };

            // For each DLMM pool on the same pair, compare rates.
            for dlmm in dlmm_list {
                let wp_canonical = wp.token_a == key.0;
                let dlmm_canonical = dlmm.token_a == key.0;

                // Compute rates in canonical direction.
                let wp_rate = {
                    let out = if wp_canonical {
                        wp.quote_a_to_b(reference_amount)
                    } else {
                        wp.quote_b_to_a(reference_amount)
                    };
                    if out == 0 { continue; }
                    out as f64 / reference_amount as f64
                };

                let dlmm_rate = {
                    let out = if dlmm_canonical {
                        dlmm.quote_a_to_b(reference_amount)
                    } else {
                        dlmm.quote_b_to_a(reference_amount)
                    };
                    if out == 0 { continue; }
                    out as f64 / reference_amount as f64
                };

                if wp_rate <= 0.0 || dlmm_rate <= 0.0
                    || !wp_rate.is_finite() || !dlmm_rate.is_finite()
                {
                    continue;
                }

                // Determine which is cheap (high rate) and which is expensive (low rate).
                let (high_pool, high_canon, low_pool, low_canon, high_rate, low_rate) =
                    if wp_rate > dlmm_rate {
                        (wp, wp_canonical, dlmm, dlmm_canonical, wp_rate, dlmm_rate)
                    } else if dlmm_rate > wp_rate {
                        (dlmm, dlmm_canonical, wp, wp_canonical, dlmm_rate, wp_rate)
                    } else {
                        continue;
                    };

                let divergence_bps = ((high_rate / low_rate - 1.0) * 10_000.0) as u64;
                let combined_fee_bps =
                    dex_fee_bps(high_pool.dex_type) + dex_fee_bps(low_pool.dex_type);
                if divergence_bps < combined_fee_bps + MIN_MARGIN_ABOVE_FEES_BPS {
                    continue;
                }

                debug!(
                    whirlpool = %wp.pool_address,
                    dlmm = %dlmm.pool_address,
                    divergence_bps,
                    wp_rate,
                    dlmm_rate,
                    "[dlmm-whirlpool-arb] divergence detected"
                );

                // Build routes.
                if a_is_anchor {
                    let mut best: Option<RouteParams> = None;
                    for &borrow_amount in borrow_amounts {
                        if let Some(mut route) = build_cross_dex_route(
                            high_pool, high_canon,
                            low_pool, low_canon,
                            key.0, key.1,
                            borrow_amount, divergence_bps,
                        ) {
                            // Apply return cap specific to concentrated liquidity.
                            let return_ratio = (route.borrow_amount as f64
                                + route.gross_profit as f64)
                                / route.borrow_amount as f64;
                            if return_ratio > MAX_RETURN_MULTIPLIER {
                                continue;
                            }
                            route.strategy = "dlmm_whirlpool_arb";
                            // Concentrated liquidity arb is lower risk — tight spreads.
                            route.risk_factor = (route.risk_factor * 0.7).min(0.5);
                            if best.as_ref().map_or(true, |b| route.net_profit > b.net_profit) {
                                best = Some(route);
                            }
                        }
                    }
                    if let Some(route) = best {
                        routes.push(route);
                    }
                }

                if b_is_anchor {
                    let mut best: Option<RouteParams> = None;
                    for &borrow_amount in borrow_amounts {
                        if let Some(mut route) = build_cross_dex_route_reverse(
                            low_pool, low_canon,
                            high_pool, high_canon,
                            key.0, key.1,
                            borrow_amount, divergence_bps,
                        ) {
                            let return_ratio = (route.borrow_amount as f64
                                + route.gross_profit as f64)
                                / route.borrow_amount as f64;
                            if return_ratio > MAX_RETURN_MULTIPLIER {
                                continue;
                            }
                            route.strategy = "dlmm_whirlpool_arb";
                            route.risk_factor = (route.risk_factor * 0.7).min(0.5);
                            if best.as_ref().map_or(true, |b| route.net_profit > b.net_profit) {
                                best = Some(route);
                            }
                        }
                    }
                    if let Some(route) = best {
                        routes.push(route);
                    }
                }
            }
        }

        // Sort by net_profit descending.
        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));

        if !routes.is_empty() {
            info!(
                count = routes.len(),
                best_profit = routes[0].net_profit,
                "[dlmm-whirlpool-arb] opportunities found"
            );
        }

        routes
    }
}

/// Canonical pair key: smaller pubkey first.
fn pair_key(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a < b { (*a, *b) } else { (*b, *a) }
}
