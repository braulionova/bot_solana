//! clmm_tick_arb.rs – Detect Raydium CLMM tick-boundary crossings and arb vs AMM V4.
//!
//! When a CLMM swap crosses a tick boundary, the pool's effective price jumps
//! discretely. If an AMM V4 pool for the same pair still reflects the old
//! continuous-curve price, a temporary spread opens. This module detects that
//! spread and builds a 2-hop route to capture it.

use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, trace};

use crate::cross_dex::{dex_fee_bps, dex_program_for};
use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{DexType, Hop, RouteParams};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum spread between post-swap CLMM price and AMM V4 price (bps).
const MIN_SPREAD_BPS: u64 = 50;

/// Maximum price impact per hop (%).
const MAX_IMPACT_PCT: f64 = 10.0;

/// TX fee estimate per hop (lamports).
const TX_FEE_PER_HOP: u64 = 5_000;

/// Maximum plausible return multiplier (reject stale/phantom).
const MAX_RETURN_MULTIPLIER: f64 = 1.05;

/// Minimum reserves to consider a pool (filter dust).
const MIN_RESERVE: u64 = 1_000_000;

/// WSOL mint.
const WSOL: &str = "So11111111111111111111111111111111111111112";
/// USDC mint.
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

pub struct ClmmTickArbScanner {
    pool_cache: Arc<PoolStateCache>,
}

impl ClmmTickArbScanner {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        Self { pool_cache }
    }

    /// Called when a CLMM swap is detected in shreds.
    /// Checks if the swap crossed a tick boundary and if there's an arb vs AMM V4.
    pub fn on_clmm_swap(
        &self,
        clmm_pool: &Pubkey,
        amount_in: u64,
        a_to_b: bool,
        borrow_amounts: &[u64],
    ) -> Vec<RouteParams> {
        let pool = match self.pool_cache.get(clmm_pool) {
            Some(p) => p,
            None => return Vec::new(),
        };

        // Must be a CLMM pool with valid orca_meta.
        let orca_meta = match &pool.orca_meta {
            Some(m) if m.tick_spacing > 0 && m.sqrt_price > 0 && m.liquidity > 0 => m.clone(),
            _ => return Vec::new(),
        };

        let old_tick = orca_meta.tick_current_index;
        let old_sqrt_price = orca_meta.sqrt_price;

        // Simulate the swap to get new sqrt_price.
        let new_sqrt_price =
            simulate_clmm_swap(old_sqrt_price, orca_meta.liquidity, amount_in, a_to_b, pool.fee_bps);

        if new_sqrt_price == 0 {
            return Vec::new();
        }

        // Calculate new tick from sqrt_price.
        // tick = floor(log(sqrt_price^2 / 2^128) / log(1.0001))
        //      = floor(2 * log(sqrt_price / 2^64) / log(1.0001))
        let new_tick = sqrt_price_to_tick(new_sqrt_price);

        // Check if tick boundary was crossed.
        if new_tick == old_tick {
            trace!(
                pool = %clmm_pool,
                old_tick,
                new_tick,
                "CLMM swap did not cross tick boundary"
            );
            return Vec::new();
        }

        debug!(
            pool = %clmm_pool,
            old_tick,
            new_tick,
            amount_in,
            a_to_b,
            "CLMM tick boundary crossed"
        );

        // Compute post-swap CLMM price (token_b per token_a).
        let clmm_price = sqrt_price_to_price(new_sqrt_price, pool.decimals_a, pool.decimals_b);
        if clmm_price <= 0.0 || !clmm_price.is_finite() {
            return Vec::new();
        }

        // Find AMM V4 pools for the same token pair.
        let amm_pools = self.pool_cache.pools_for_pair(&pool.token_a, &pool.token_b);

        let anchor_wsol: Pubkey = WSOL.parse().unwrap();
        let anchor_usdc: Pubkey = USDC.parse().unwrap();

        let mut routes = Vec::new();

        for amm in &amm_pools {
            if amm.dex_type != DexType::RaydiumAmmV4 {
                continue;
            }
            if amm.reserve_a < MIN_RESERVE || amm.reserve_b < MIN_RESERVE {
                continue;
            }
            // Must have complete Raydium metadata for TX building.
            match &amm.raydium_meta {
                Some(meta) => {
                    let default = Pubkey::default();
                    if meta.vault_a == default
                        || meta.vault_b == default
                        || meta.market_id == default
                    {
                        continue;
                    }
                }
                None => continue,
            }

            // AMM V4 price: token_b per token_a using XY=K.
            let amm_price = compute_amm_price(amm);
            if amm_price <= 0.0 || !amm_price.is_finite() {
                continue;
            }

            // Calculate spread in bps.
            let spread_bps = ((clmm_price - amm_price).abs() / amm_price * 10_000.0) as u64;

            let combined_fee_bps = dex_fee_bps(DexType::RaydiumClmm) + dex_fee_bps(DexType::RaydiumAmmV4);
            if spread_bps < combined_fee_bps + MIN_SPREAD_BPS {
                continue;
            }

            // Determine direction: buy on cheaper, sell on expensive.
            // clmm_price > amm_price means CLMM gives more token_b per token_a.
            //   → buy token_b cheap on AMM (less token_b per token_a = cheap token_a)
            //   → sell token_b on CLMM (more token_b per token_a = expensive token_a)
            // Actually: if CLMM price is higher, token_a is more expensive on CLMM.
            //   → buy token_a on AMM (cheaper) → sell token_a on CLMM (expensive)
            //   Borrow token_b → buy token_a on AMM → sell token_a on CLMM → repay token_b
            // If AMM price is higher:
            //   → buy token_a on CLMM (cheaper) → sell token_a on AMM (expensive)
            //   Borrow token_b → buy token_a on CLMM → sell token_a on AMM → repay token_b

            let (buy_pool, sell_pool, buy_is_clmm) = if clmm_price > amm_price {
                // CLMM has higher price for token_a → buy on AMM, sell on CLMM
                (amm, &pool, false)
            } else {
                // AMM has higher price for token_a → buy on CLMM, sell on AMM
                (&pool, amm, true)
            };

            // Route must borrow an anchor token (WSOL or USDC).
            let borrow_token_a = pool.token_a == anchor_wsol || pool.token_a == anchor_usdc;
            let borrow_token_b = pool.token_b == anchor_wsol || pool.token_b == anchor_usdc;

            if !borrow_token_a && !borrow_token_b {
                continue;
            }

            for &borrow_amount in borrow_amounts {
                // Try borrowing token_b route: token_b → token_a (buy) → token_b (sell)
                if borrow_token_b {
                    if let Some(route) = build_tick_arb_route(
                        buy_pool,
                        sell_pool,
                        pool.token_b, // borrow token
                        pool.token_a, // intermediate token
                        borrow_amount,
                        spread_bps,
                        buy_is_clmm,
                    ) {
                        routes.push(route);
                    }
                }

                // Try borrowing token_a route: token_a → token_b (buy) → token_a (sell)
                if borrow_token_a {
                    if let Some(route) = build_tick_arb_route(
                        // Swap directions: buy token_b, sell token_b
                        sell_pool, // acts as buy for token_b direction
                        buy_pool,  // acts as sell for token_b direction
                        pool.token_a, // borrow token
                        pool.token_b, // intermediate token
                        borrow_amount,
                        spread_bps,
                        !buy_is_clmm,
                    ) {
                        routes.push(route);
                    }
                }
            }
        }

        // Sort by net_profit descending, keep best.
        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
        routes.truncate(5);
        routes
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simulate a CLMM swap to estimate the new sqrt_price.
/// Uses the concentrated liquidity formula for a single tick range.
/// For a_to_b: sqrt_price decreases. For b_to_a: sqrt_price increases.
fn simulate_clmm_swap(
    sqrt_price: u128,
    liquidity: u128,
    amount_in: u64,
    a_to_b: bool,
    fee_bps: u64,
) -> u128 {
    if liquidity == 0 || sqrt_price == 0 {
        return 0;
    }

    let amount_in_after_fee =
        (amount_in as u128) * (10_000u128 - fee_bps as u128) / 10_000u128;

    if amount_in_after_fee == 0 {
        return 0;
    }

    if a_to_b {
        // Selling token_a for token_b.
        // delta_sqrt_price = amount_in * sqrt_price / (liquidity + amount_in * sqrt_price / 2^64)
        // new_sqrt_price = sqrt_price - delta
        // Simplified: new_sqrt_price = sqrt_price * L / (L + amount_in * sqrt_price / 2^64)
        // Using Q64.64 format where sqrt_price is already scaled by 2^64.
        let sqrt_p_f64 = sqrt_price as f64;
        let l_f64 = liquidity as f64;
        let q64 = (1u128 << 64) as f64;
        let amt = amount_in_after_fee as f64;

        let denominator = l_f64 + amt * sqrt_p_f64 / q64;
        if denominator <= 0.0 {
            return 0;
        }
        let new_sqrt = sqrt_p_f64 * l_f64 / denominator;
        if new_sqrt <= 0.0 || !new_sqrt.is_finite() {
            return 0;
        }
        new_sqrt as u128
    } else {
        // Selling token_b for token_a.
        // new_sqrt_price = sqrt_price + amount_in * 2^64 / liquidity
        let delta = (amount_in_after_fee as u128)
            .checked_mul(1u128 << 64)
            .and_then(|n| n.checked_div(liquidity));
        match delta {
            Some(d) => sqrt_price.saturating_add(d),
            None => 0,
        }
    }
}

/// Convert sqrt_price (Q64.64) to tick index.
/// tick = floor(log(sqrt_price^2 / 2^128) / log(1.0001))
///      = floor(2 * log(sqrt_price / 2^64) / log(1.0001))
fn sqrt_price_to_tick(sqrt_price: u128) -> i32 {
    if sqrt_price == 0 {
        return i32::MIN;
    }
    let q64 = (1u128 << 64) as f64;
    let ratio = sqrt_price as f64 / q64;
    let log_ratio = ratio.ln();
    let log_base = (1.0001_f64).ln();
    // tick = floor(2 * log(ratio) / log(1.0001))
    let tick_f = 2.0 * log_ratio / log_base;
    tick_f.floor() as i32
}

/// Convert sqrt_price (Q64.64) to human-readable price (token_b per token_a),
/// adjusting for decimal differences.
fn sqrt_price_to_price(sqrt_price: u128, decimals_a: u8, decimals_b: u8) -> f64 {
    let q64 = (1u128 << 64) as f64;
    let ratio = sqrt_price as f64 / q64;
    let raw_price = ratio * ratio; // price = (sqrt_price / 2^64)^2
    // Adjust for decimal difference: price_human = raw_price * 10^(decimals_a - decimals_b)
    let decimal_adj = 10f64.powi(decimals_a as i32 - decimals_b as i32);
    raw_price * decimal_adj
}

/// Compute AMM V4 price (token_b per token_a) from reserves, adjusting for decimals.
fn compute_amm_price(pool: &DexPool) -> f64 {
    let (ra, rb) = pool.effective_reserves();
    if ra == 0 || rb == 0 {
        return 0.0;
    }
    let a_ui = ra as f64 / 10f64.powi(pool.decimals_a as i32);
    let b_ui = rb as f64 / 10f64.powi(pool.decimals_b as i32);
    b_ui / a_ui
}

/// Build a 2-hop tick arb route.
/// Borrow `borrow_token` → swap to `mid_token` on buy_pool → swap back to `borrow_token` on sell_pool.
fn build_tick_arb_route(
    buy_pool: &DexPool,
    sell_pool: &DexPool,
    borrow_token: Pubkey,
    mid_token: Pubkey,
    borrow_amount: u64,
    spread_bps: u64,
    _buy_is_clmm: bool,
) -> Option<RouteParams> {
    // Hop 1: borrow_token → mid_token on buy_pool.
    let buy_a_is_borrow = buy_pool.token_a == borrow_token;
    let buy_reserve_in = if buy_a_is_borrow {
        buy_pool.reserve_a
    } else {
        buy_pool.reserve_b
    };

    // Reserve must be >10x borrow to avoid catastrophic price impact.
    if buy_reserve_in < borrow_amount.saturating_mul(10) {
        return None;
    }

    let (mid_amount, impact_1) = if buy_a_is_borrow {
        let out = buy_pool.quote_a_to_b(borrow_amount);
        let impact = borrow_amount as f64 / buy_pool.reserve_a.max(1) as f64 * 100.0;
        (out, impact)
    } else {
        let out = buy_pool.quote_b_to_a(borrow_amount);
        let impact = borrow_amount as f64 / buy_pool.reserve_b.max(1) as f64 * 100.0;
        (out, impact)
    };

    if mid_amount == 0 || impact_1 > MAX_IMPACT_PCT {
        return None;
    }

    // Hop 2: mid_token → borrow_token on sell_pool.
    let sell_a_is_mid = sell_pool.token_a == mid_token;
    let (final_amount, impact_2) = if sell_a_is_mid {
        let out = sell_pool.quote_a_to_b(mid_amount);
        let impact = mid_amount as f64 / sell_pool.reserve_a.max(1) as f64 * 100.0;
        (out, impact)
    } else {
        let out = sell_pool.quote_b_to_a(mid_amount);
        let impact = mid_amount as f64 / sell_pool.reserve_b.max(1) as f64 * 100.0;
        (out, impact)
    };

    if final_amount == 0 || impact_2 > MAX_IMPACT_PCT {
        return None;
    }

    if final_amount <= borrow_amount {
        return None;
    }

    // Reject implausible returns.
    if final_amount as f64 > borrow_amount as f64 * MAX_RETURN_MULTIPLIER {
        return None;
    }

    let gross_profit = final_amount - borrow_amount;
    let net_profit = gross_profit as i64 - (TX_FEE_PER_HOP * 2) as i64;

    if net_profit <= 0 {
        return None;
    }

    let max_impact = impact_1.max(impact_2);
    let risk_factor = (max_impact / 100.0).min(0.95);

    debug!(
        buy_pool = %buy_pool.pool_address,
        sell_pool = %sell_pool.pool_address,
        buy_dex = ?buy_pool.dex_type,
        sell_dex = ?sell_pool.dex_type,
        spread_bps,
        borrow_amount,
        mid_amount,
        final_amount,
        net_profit,
        "[clmm_tick_arb] route candidate"
    );

    Some(RouteParams {
        hops: vec![
            Hop {
                pool: buy_pool.pool_address,
                dex_program: dex_program_for(buy_pool.dex_type),
                token_in: borrow_token,
                token_out: mid_token,
                amount_out: mid_amount,
                price_impact: impact_1,
            },
            Hop {
                pool: sell_pool.pool_address,
                dex_program: dex_program_for(sell_pool.dex_type),
                token_in: mid_token,
                token_out: borrow_token,
                amount_out: final_amount,
                price_impact: impact_2,
            },
        ],
        borrow_amount,
        gross_profit,
        net_profit,
        risk_factor,
        strategy: "clmm_tick_arb",
        tier: 0, // Assigned by caller via route_tier()
    })
}
