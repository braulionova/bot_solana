//! pump_graduation_arb.rs – PumpSwap graduation cross-DEX arbitrage.
//!
//! When a token graduates from PumpFun bonding curve to PumpSwap AMM, it often
//! has simultaneous pools on Raydium/Meteora/Orca with desynchronized prices.
//! This scanner finds cross-DEX price differences exploitable via atomic flash
//! loan arb: flash borrow → buy cheap DEX → sell expensive DEX → repay.

use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info};

use crate::cross_dex::{build_cross_dex_route, build_cross_dex_route_reverse, dex_fee_bps};
use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{DexType, RouteParams};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum spread above combined fees to attempt arb (bps).
/// Lowered from 50 → 20: flash loan arb is atomic (0 cost on failure),
/// so we can be aggressive on thin spreads — proven profitable at 4/4 TXs.
const MIN_MARGIN_ABOVE_FEES_BPS: u64 = 20;

/// Minimum pool reserve in either token (raw units) — ~5 SOL.
/// Lowered from 10 SOL: graduation pools often start with 5-10 SOL liquidity.
const MIN_RESERVE_SOL: u64 = 5_000_000_000;

/// Minimum reserve for non-SOL side (raw units).
const MIN_RESERVE: u64 = 1_000_000;

/// WSOL mint.
const WSOL: &str = "So11111111111111111111111111111111111111112";

/// USDC mint.
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Anchor tokens for flash loan borrow.
const ANCHOR_TOKENS: &[&str] = &[WSOL, USDC];

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

pub struct PumpGraduationArb {
    pool_cache: Arc<PoolStateCache>,
}

impl PumpGraduationArb {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        Self { pool_cache }
    }

    /// Called when a graduation event is detected. Scans all DEXes for the same
    /// token mint and finds cross-DEX price differences exploitable via flash loan arb.
    /// Returns atomic flash loan arb routes (buy cheap DEX -> sell expensive DEX -> repay).
    pub fn scan_graduation_arb(
        &self,
        token_mint: &Pubkey,
        _pump_pool: &Pubkey,
        borrow_amounts: &[u64],
    ) -> Vec<RouteParams> {
        let anchor_set: std::collections::HashSet<Pubkey> = ANCHOR_TOKENS
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();

        // Find all pools containing token_mint across all DEXes.
        let all_pools = self.pool_cache.pools_for_token(token_mint);

        if all_pools.len() < 2 {
            return Vec::new();
        }

        // Filter to eligible pools with adequate reserves.
        let eligible: Vec<DexPool> = all_pools
            .into_iter()
            .filter(|pool| {
                if pool.dex_type == DexType::Unknown || pool.dex_type == DexType::RaydiumClmm {
                    return false;
                }
                if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
                    return false;
                }
                // Raydium V4: require complete metadata.
                if pool.dex_type == DexType::RaydiumAmmV4 {
                    match &pool.raydium_meta {
                        Some(meta) => {
                            if meta.vault_a == Pubkey::default()
                                || meta.vault_b == Pubkey::default()
                            {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                // At least one side must be an anchor token (for flash loan borrow).
                let has_anchor =
                    anchor_set.contains(&pool.token_a) || anchor_set.contains(&pool.token_b);
                if !has_anchor {
                    return false;
                }
                // Check minimum SOL reserve for PumpSwap graduation pools.
                let wsol: Pubkey = WSOL.parse().unwrap();
                if pool.token_a == wsol && pool.reserve_a < MIN_RESERVE_SOL {
                    return false;
                }
                if pool.token_b == wsol && pool.reserve_b < MIN_RESERVE_SOL {
                    return false;
                }
                true
            })
            .collect();

        if eligible.len() < 2 {
            return Vec::new();
        }

        // Group by canonical pair (anchor_token, graduated_token).
        let mut pair_groups: std::collections::HashMap<(Pubkey, Pubkey), Vec<&DexPool>> =
            std::collections::HashMap::new();
        for pool in &eligible {
            let key = pair_key(&pool.token_a, &pool.token_b);
            pair_groups.entry(key).or_default().push(pool);
        }

        let mut routes = Vec::new();

        for ((token_a, token_b), pools) in &pair_groups {
            if pools.len() < 2 {
                continue;
            }

            let a_is_anchor = anchor_set.contains(token_a);
            let b_is_anchor = anchor_set.contains(token_b);
            if !a_is_anchor && !b_is_anchor {
                continue;
            }

            // Compute rates for all pools in canonical direction.
            let reference_amount = borrow_amounts.first().copied().unwrap_or(100_000_000);
            let mut pool_rates: Vec<(&DexPool, f64, bool)> = Vec::new();

            for pool in pools {
                let canonical_order = pool.token_a == *token_a;
                let rate = if canonical_order {
                    let out = pool.quote_a_to_b(reference_amount);
                    if out == 0 { continue; }
                    out as f64 / reference_amount as f64
                } else {
                    let out = pool.quote_b_to_a(reference_amount);
                    if out == 0 { continue; }
                    out as f64 / reference_amount as f64
                };
                if rate <= 0.0 || !rate.is_finite() {
                    continue;
                }
                pool_rates.push((pool, rate, canonical_order));
            }

            if pool_rates.len() < 2 {
                continue;
            }

            // Compare all pairs of pools on different DEXes.
            for i in 0..pool_rates.len() {
                for j in (i + 1)..pool_rates.len() {
                    let (pool_i, rate_i, canon_i) = &pool_rates[i];
                    let (pool_j, rate_j, canon_j) = &pool_rates[j];

                    // Same pool → skip.
                    if pool_i.pool_address == pool_j.pool_address {
                        continue;
                    }
                    // Same DEX, same type → only allow if reserves look real
                    // (not estimated 79 SOL from graduation insertion).
                    if pool_i.dex_type == pool_j.dex_type {
                        // Both PumpSwap with estimated reserves → phantom divergence, skip.
                        let est = 79_000_000_000u64;
                        let i_estimated = pool_i.reserve_a == est || pool_i.reserve_b == est;
                        let j_estimated = pool_j.reserve_a == est || pool_j.reserve_b == est;
                        if i_estimated || j_estimated {
                            continue;
                        }
                    }

                    let (high_pool, high_canon, low_pool, low_canon, high_rate, low_rate) =
                        if rate_i > rate_j {
                            (*pool_i, *canon_i, *pool_j, *canon_j, *rate_i, *rate_j)
                        } else if rate_j > rate_i {
                            (*pool_j, *canon_j, *pool_i, *canon_i, *rate_j, *rate_i)
                        } else {
                            continue;
                        };

                    let divergence_bps = ((high_rate / low_rate - 1.0) * 10_000.0) as u64;
                    let combined_fee_bps =
                        dex_fee_bps(high_pool.dex_type) + dex_fee_bps(low_pool.dex_type);
                    if divergence_bps < combined_fee_bps + MIN_MARGIN_ABOVE_FEES_BPS {
                        continue;
                    }

                    info!(
                        token_mint = %token_mint,
                        high_dex = ?high_pool.dex_type,
                        low_dex = ?low_pool.dex_type,
                        divergence_bps,
                        "[pump-graduation-arb] divergence detected"
                    );

                    // Build routes in each anchor direction.
                    if a_is_anchor {
                        let mut best: Option<RouteParams> = None;
                        for &borrow_amount in borrow_amounts {
                            if let Some(mut route) = build_cross_dex_route(
                                high_pool, high_canon,
                                low_pool, low_canon,
                                *token_a, *token_b,
                                borrow_amount, divergence_bps,
                            ) {
                                route.strategy = "pump_graduation_arb";
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
                                *token_a, *token_b,
                                borrow_amount, divergence_bps,
                            ) {
                                route.strategy = "pump_graduation_arb";
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
        }

        // Sort by net_profit descending.
        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));

        if !routes.is_empty() {
            info!(
                token_mint = %token_mint,
                count = routes.len(),
                best_profit = routes[0].net_profit,
                "[pump-graduation-arb] opportunities found"
            );
        }

        routes
    }
}

/// Canonical pair key: smaller pubkey first to normalize (A,B) == (B,A).
fn pair_key(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a < b { (*a, *b) } else { (*b, *a) }
}
