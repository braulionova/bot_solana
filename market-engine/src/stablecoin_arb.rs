//! stablecoin_arb.rs – Monitor USDC/USDT micro-depegs across multiple pools.
//!
//! When SOL is priced differently across pools denominated in the same
//! stablecoin, a 2-hop arb exists: buy SOL where it's cheap → sell where
//! it's expensive.  Triggers at >30bps divergence after fees.

use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, trace};

use crate::cross_dex::{dex_fee_bps, dex_program_for};
use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{DexType, Hop, RouteParams};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// USDC mint.
const USDC_STR: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// USDT mint.
const USDT_STR: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
/// WSOL mint.
const WSOL_STR: &str = "So11111111111111111111111111111111111111112";

/// SOL decimals.
const SOL_DECIMALS: u8 = 9;
/// Stablecoin decimals (USDC, USDT).
const STABLE_DECIMALS: u8 = 6;

/// Minimum spread between max and min SOL price (bps of average).
const MIN_SPREAD_BPS: u64 = 30;

/// Minimum reserve in either token (raw units) — 1 SOL equivalent.
const MIN_POOL_LIQUIDITY: u64 = 1_000_000_000;

/// Maximum price impact per hop (%).
const MAX_IMPACT_PCT: f64 = 5.0;

/// TX fee estimate per hop (lamports).
const TX_FEE_PER_HOP: u64 = 5_000;

/// Maximum plausible return multiplier (reject stale/phantom).
const MAX_RETURN_MULTIPLIER: f64 = 1.02;

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

pub struct StablecoinArbScanner {
    pool_cache: Arc<PoolStateCache>,
}

impl StablecoinArbScanner {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        Self { pool_cache }
    }

    /// Scan for stablecoin price deviations across pools.
    /// Returns arb routes when USDC or USDT deviates >30bps from peg.
    pub fn scan(&self, borrow_amounts: &[u64]) -> Vec<RouteParams> {
        let wsol: Pubkey = WSOL_STR.parse().unwrap();
        let usdc: Pubkey = USDC_STR.parse().unwrap();
        let usdt: Pubkey = USDT_STR.parse().unwrap();

        let mut routes = Vec::new();

        // Scan SOL/USDC pools.
        let usdc_routes = self.scan_stablecoin_pair(&wsol, &usdc, borrow_amounts);
        routes.extend(usdc_routes);

        // Scan SOL/USDT pools.
        let usdt_routes = self.scan_stablecoin_pair(&wsol, &usdt, borrow_amounts);
        routes.extend(usdt_routes);

        // Sort by net_profit descending.
        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
        routes
    }

    /// Scan pools for a specific SOL/stablecoin pair.
    fn scan_stablecoin_pair(
        &self,
        wsol: &Pubkey,
        stable: &Pubkey,
        borrow_amounts: &[u64],
    ) -> Vec<RouteParams> {
        let all_pools = self.pool_cache.pools_for_pair(wsol, stable);

        // Filter to eligible pools with sufficient liquidity and metadata.
        let eligible: Vec<&DexPool> = all_pools
            .iter()
            .filter(|p| is_eligible_pool(p))
            .collect();

        if eligible.len() < 2 {
            return Vec::new();
        }

        // Compute implied SOL price on each pool: price = reserve_stable / reserve_sol
        // adjusted for decimals (SOL=9, stable=6).
        let mut priced_pools: Vec<(&DexPool, f64)> = Vec::new();
        for pool in &eligible {
            let price = implied_sol_price(pool, wsol);
            if price > 0.0 && price.is_finite() {
                priced_pools.push((pool, price));
            }
        }

        if priced_pools.len() < 2 {
            return Vec::new();
        }

        // Find max and min price pools.
        let (max_idx, _) = priced_pools
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap();
        let (min_idx, _) = priced_pools
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap();

        if max_idx == min_idx {
            return Vec::new();
        }

        let (expensive_pool, max_price) = priced_pools[max_idx];
        let (cheap_pool, min_price) = priced_pools[min_idx];

        let avg_price = (max_price + min_price) / 2.0;
        let spread_bps = ((max_price - min_price) / avg_price * 10_000.0) as u64;

        let combined_fee_bps =
            dex_fee_bps(cheap_pool.dex_type) + dex_fee_bps(expensive_pool.dex_type);

        if spread_bps < combined_fee_bps + MIN_SPREAD_BPS {
            trace!(
                stable = %stable,
                spread_bps,
                combined_fee_bps,
                "stablecoin spread below threshold"
            );
            return Vec::new();
        }

        debug!(
            stable = %stable,
            cheap_pool = %cheap_pool.pool_address,
            expensive_pool = %expensive_pool.pool_address,
            min_price,
            max_price,
            spread_bps,
            "stablecoin arb spread detected"
        );

        // Build routes: buy SOL where cheap (high USDC/SOL = expensive SOL in stable terms,
        // but that means SOL is "expensive" there... let's be precise:
        //   high price = more stablecoins per SOL = SOL is expensive
        //   low price  = fewer stablecoins per SOL = SOL is cheap
        // Arb: buy SOL on cheap_pool (low USDC/SOL), sell SOL on expensive_pool (high USDC/SOL).
        // We borrow stablecoin (USDC/USDT), buy SOL on cheap pool, sell SOL on expensive pool.
        //
        // Hop 1: stable → SOL on cheap_pool (SOL is cheap here)
        // Hop 2: SOL → stable on expensive_pool (SOL is expensive here = more stable back)

        let mut routes = Vec::new();
        for &borrow_amount in borrow_amounts {
            if let Some(route) = build_stablecoin_route(
                cheap_pool,
                expensive_pool,
                *stable,
                *wsol,
                borrow_amount,
                spread_bps,
            ) {
                routes.push(route);
            }
        }

        // Also try WSOL borrow direction:
        // Hop 1: SOL → stable on expensive_pool (get more stable)
        // Hop 2: stable → SOL on cheap_pool (get more SOL back)
        for &borrow_amount in borrow_amounts {
            if let Some(route) = build_stablecoin_route(
                expensive_pool,
                cheap_pool,
                *wsol,
                *stable,
                borrow_amount,
                spread_bps,
            ) {
                routes.push(route);
            }
        }

        // Keep best by net_profit.
        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
        routes.truncate(3);
        routes
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if pool is eligible for stablecoin arb routing.
fn is_eligible_pool(pool: &DexPool) -> bool {
    // Must have minimum liquidity.
    if pool.reserve_a < MIN_POOL_LIQUIDITY || pool.reserve_b < MIN_POOL_LIQUIDITY {
        return false;
    }

    // Reject unsupported DEX types.
    if pool.dex_type == DexType::Unknown || pool.dex_type == DexType::RaydiumCpmm {
        return false;
    }

    // Raydium AMM V4: need complete metadata.
    if pool.dex_type == DexType::RaydiumAmmV4 {
        match &pool.raydium_meta {
            Some(meta) => {
                let default = Pubkey::default();
                if meta.vault_a == default
                    || meta.vault_b == default
                    || meta.market_id == default
                    || meta.market_bids == default
                    || meta.market_asks == default
                {
                    return false;
                }
            }
            None => return false,
        }
    }

    // Orca: need orca_meta with valid tick_spacing.
    if pool.dex_type == DexType::OrcaWhirlpool {
        match &pool.orca_meta {
            Some(m) => {
                if m.tick_spacing == 0 || m.token_vault_a == Pubkey::default() {
                    return false;
                }
            }
            None => return false,
        }
    }

    // Meteora: need meteora_meta.
    if pool.dex_type == DexType::MeteoraDlmm {
        match &pool.meteora_meta {
            Some(m) => {
                if m.token_vault_a == Pubkey::default() {
                    return false;
                }
            }
            None => return false,
        }
    }

    true
}

/// Compute implied SOL price in stablecoin units (e.g., USDC per SOL).
/// Adjusts for decimal differences: SOL=9 decimals, stable=6 decimals.
fn implied_sol_price(pool: &DexPool, wsol: &Pubkey) -> f64 {
    let (ra, rb) = pool.effective_reserves();
    if ra == 0 || rb == 0 {
        return 0.0;
    }

    // Determine which reserve is SOL and which is the stablecoin.
    let (sol_reserve, sol_decimals, stable_reserve, stable_decimals) = if pool.token_a == *wsol {
        (ra, pool.decimals_a, rb, pool.decimals_b)
    } else if pool.token_b == *wsol {
        (rb, pool.decimals_b, ra, pool.decimals_a)
    } else {
        return 0.0;
    };

    // Use actual decimals from pool metadata (should be 9 for SOL, 6 for USDC/USDT).
    let sol_ui = sol_reserve as f64 / 10f64.powi(sol_decimals as i32);
    let stable_ui = stable_reserve as f64 / 10f64.powi(stable_decimals as i32);

    if sol_ui <= 0.0 {
        return 0.0;
    }

    stable_ui / sol_ui
}

/// Build a 2-hop stablecoin arb route.
/// Borrow `borrow_token` → swap to `mid_token` on buy_pool → swap back on sell_pool.
fn build_stablecoin_route(
    buy_pool: &DexPool,
    sell_pool: &DexPool,
    borrow_token: Pubkey,
    mid_token: Pubkey,
    borrow_amount: u64,
    spread_bps: u64,
) -> Option<RouteParams> {
    // Hop 1: borrow_token → mid_token on buy_pool.
    let buy_a_is_borrow = buy_pool.token_a == borrow_token;
    let buy_reserve_in = if buy_a_is_borrow {
        buy_pool.reserve_a
    } else {
        buy_pool.reserve_b
    };

    // Reserve must be >5x borrow to limit price impact (stablecoins are more predictable).
    if buy_reserve_in < borrow_amount.saturating_mul(5) {
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

    // Reject implausible returns (stablecoins should have tight spreads).
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
        "[stablecoin_arb] route candidate"
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
        strategy: "stablecoin_arb",
        tier: 0, // Assigned by caller via route_tier()
    })
}
