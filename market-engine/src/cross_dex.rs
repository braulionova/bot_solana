//! cross_dex.rs – E6: Cross-DEX Same-Pair Arbitrage Scanner
//!
//! For each token pair that exists on ≥2 DEXes, compares prices and emits
//! a 2-hop route (buy cheap → sell expensive) when divergence > fees.
//!
//! This is more efficient than Bellman-Ford for same-pair 2-hop arbs because
//! it directly compares all pools for a pair instead of walking graph edges.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info};

use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{dex_programs, DexType, Hop, RouteParams};

/// Route tier based on robustness to Solana's ~400ms slot delay.
///
/// Lower tier = more robust to stale reserves during slot transitions.
/// Tier 99 = fragile Orca route with insufficient tick liquidity (filtered out).
pub fn route_tier(route: &RouteParams, pool_cache: &PoolStateCache) -> u8 {
    let dex_types: Vec<DexType> = route.hops.iter()
        .filter_map(|h| pool_cache.get(&h.pool).map(|p| p.dex_type))
        .collect();

    // Tier 1: All Raydium V4 (constant product, smooth slippage)
    if dex_types.iter().all(|d| *d == DexType::RaydiumAmmV4) {
        return 1;
    }

    // Tier 2: Raydium + Meteora or Meteora-only
    if dex_types.iter().all(|d| matches!(d, DexType::RaydiumAmmV4 | DexType::MeteoraDlmm)) {
        return 2;
    }

    // Tier 3: PumpSwap involved (graduation arbs — structural spreads)
    if dex_types.iter().any(|d| *d == DexType::PumpSwap) {
        return 3;
    }

    // Tier 4: Orca with sufficient active tick liquidity (>$200k equivalent)
    if dex_types.iter().any(|d| *d == DexType::OrcaWhirlpool) {
        for hop in &route.hops {
            if let Some(pool) = pool_cache.get(&hop.pool) {
                if pool.dex_type == DexType::OrcaWhirlpool {
                    if let Some(meta) = &pool.orca_meta {
                        // liquidity is the L value in the active tick range
                        // Skip if liquidity is too low (< 1B raw units ~ $200k)
                        if meta.liquidity < 1_000_000_000 {
                            return 99; // Skip — fragile tick range
                        }
                    } else {
                        return 99; // No metadata — skip
                    }
                }
            }
        }
        return 4; // Orca with sufficient liquidity
    }

    5 // Other
}

/// Minimum divergence margin above combined pool fees (bps).
/// With blind send (no simulation), flash loan protects capital — failed TX = 0 cost.
/// Lower threshold = more opportunities sent = higher chance of catching real spreads.
/// At 30bps with WS real-time reserves (~200ms), EV is positive.
const MIN_MARGIN_ABOVE_FEES_BPS: u64 = 30;

/// Tokens that flash loan providers support — routes MUST borrow one of these.
const ANCHOR_TOKENS: &[&str] = &[
    "So11111111111111111111111111111111111111112",    // WSOL
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
    // USDT excluded: no flash-loan provider configured for it
];

/// Maximum return: 5% profit.  Real cross-DEX arbs rarely exceed 1%.
/// Higher returns almost always mean stale reserves.
const MAX_RETURN_MULTIPLIER: f64 = 1.05;

/// Minimum reserves to consider a pool (filter dust).
const MIN_RESERVE: u64 = 1_000_000;

/// Maximum price impact per hop (%).
const MAX_IMPACT_PCT: f64 = 5.0;

/// TX fee estimate per hop (lamports).
const TX_FEE_PER_HOP: u64 = 5_000;

/// Canonical pair key: smaller pubkey first to normalize (A,B) == (B,A).
fn pair_key(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a < b {
        (*a, *b)
    } else {
        (*b, *a)
    }
}

/// A pool with its effective exchange rate for a specific direction.
struct PoolRate {
    pool: DexPool,
    /// How many units of token_out you get per unit of token_in (direction: canonical_a → canonical_b).
    rate_a_to_b: f64,
    /// True if the pool's token_a == canonical pair's first token.
    canonical_order: bool,
}

/// Scan the pool cache for cross-DEX same-pair arbitrage opportunities.
/// Returns routes sorted by net_profit descending.
pub fn scan_cross_dex_opportunities(
    pool_cache: &Arc<PoolStateCache>,
    borrow_amounts: &[u64],
) -> Vec<RouteParams> {
    // Group eligible pools by canonical pair.
    let mut pair_pools: HashMap<(Pubkey, Pubkey), Vec<PoolRate>> = HashMap::new();

    for entry in pool_cache.inner.iter() {
        let pool = entry.value();

        // Filter dust pools and pools without reserves.
        if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
            continue;
        }
        // Only include pools with COMPLETE metadata for TX building.
        // Without this, routes pass scanner but fail at executor ("meta missing").
        match pool.dex_type {
            DexType::RaydiumAmmV4 => {
                if pool.raydium_meta.is_none() { continue; }
                if let Some(m) = &pool.raydium_meta {
                    if m.vault_a == Pubkey::default() || m.market_id == Pubkey::default() { continue; }
                }
            }
            DexType::OrcaWhirlpool => {
                if pool.orca_meta.is_none() { continue; }
                if let Some(m) = &pool.orca_meta {
                    if m.tick_spacing == 0 || m.token_vault_a == Pubkey::default() { continue; }
                }
            }
            DexType::MeteoraDlmm => {
                if pool.meteora_meta.is_none() { continue; }
                if let Some(m) = &pool.meteora_meta {
                    if m.token_vault_a == Pubkey::default() { continue; }
                }
            }
            _ => {} // PumpSwap, etc. don't need extra metadata
        }
        // Skip pools without proper metadata for execution or unsupported by TX builder.
        if pool.dex_type == DexType::Unknown || pool.dex_type == DexType::RaydiumClmm
            || pool.dex_type == DexType::RaydiumCpmm {
            continue;
        }
        if pool.dex_type == DexType::RaydiumAmmV4 {
            if let Some(meta) = &pool.raydium_meta {
                if meta.vault_a == Pubkey::default() || meta.vault_b == Pubkey::default() {
                    continue; // Phantom pool
                }
            } else {
                continue; // No metadata
            }
        }

        let key = pair_key(&pool.token_a, &pool.token_b);
        let canonical_order = pool.token_a == key.0;

        // Compute rate in canonical direction (key.0 → key.1) using actual quote function.
        // For CLMM pools (Orca/Meteora), reserve_b/reserve_a is meaningless — we must use
        // the tick/bin-based quote engine to get the real exchange rate.
        let reference_amount = borrow_amounts.first().copied().unwrap_or(100_000_000);
        let rate_a_to_b = if canonical_order {
            let out = pool.quote_a_to_b(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        } else {
            let out = pool.quote_b_to_a(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        };

        if rate_a_to_b <= 0.0 || !rate_a_to_b.is_finite() {
            continue;
        }

        pair_pools.entry(key).or_default().push(PoolRate {
            pool: pool.clone(),
            rate_a_to_b,
            canonical_order,
        });
    }

    let anchor_set: HashSet<Pubkey> = ANCHOR_TOKENS
        .iter()
        .filter_map(|s| s.parse::<Pubkey>().ok())
        .collect();
    let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();

    let mut routes = Vec::new();

    for ((token_a, token_b), pools) in &pair_pools {
        // Need at least 2 pools for cross-DEX arb.
        if pools.len() < 2 {
            continue;
        }

        // At least one side must be a flash-loanable anchor token.
        // The borrow token (token_a in canonical pair) must be the anchor.
        let a_is_anchor = anchor_set.contains(token_a);
        let b_is_anchor = anchor_set.contains(token_b);
        if !a_is_anchor && !b_is_anchor {
            continue; // Neither side is WSOL/USDC/USDT — can't flash loan
        }

        // Skip stablecoin pairs (USDC↔USDT in either direction).
        // These have persistent ~0.01% peg differences that look like 1%+ arb in cached
        // reserves but produce <10K lamports on-chain. 1323 attempts, 0 landed.
        let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let usdt: Pubkey = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".parse().unwrap();
        let is_stablecoin_pair = (*token_a == usdc && *token_b == usdt)
            || (*token_a == usdt && *token_b == usdc);
        if is_stablecoin_pair {
            continue;
        }
        // Also skip any pair without WSOL that has both sides as known stables
        let has_wsol = *token_a == wsol || *token_b == wsol;
        if a_is_anchor && b_is_anchor && !has_wsol {
            continue;
        }

        // Rates are a_to_b (token_b per token_a).
        // highest rate = most token_b per token_a (cheap to buy token_b).
        // lowest rate  = least token_b per token_a (expensive to buy token_b = cheap to sell token_b).
        let best_high = pools.iter().max_by(|a, b| {
            a.rate_a_to_b.partial_cmp(&b.rate_a_to_b).unwrap_or(std::cmp::Ordering::Equal)
        });
        let best_low = pools.iter().min_by(|a, b| {
            a.rate_a_to_b.partial_cmp(&b.rate_a_to_b).unwrap_or(std::cmp::Ordering::Equal)
        });

        let (Some(high), Some(low)) = (best_high, best_low) else {
            continue;
        };

        if high.pool.pool_address == low.pool.pool_address {
            continue;
        }
        if high.rate_a_to_b <= low.rate_a_to_b {
            continue;
        }
        let divergence_bps =
            ((high.rate_a_to_b / low.rate_a_to_b - 1.0) * 10_000.0) as u64;
        let combined_fee_bps = dex_fee_bps(high.pool.dex_type) + dex_fee_bps(low.pool.dex_type);
        let min_divergence = combined_fee_bps + MIN_MARGIN_ABOVE_FEES_BPS;
        if divergence_bps < min_divergence {
            continue;
        }

        // Determine borrow direction: always borrow the anchor token.
        // Direction A: borrow token_a → buy token_b (high rate pool) → sell token_b (low rate pool)
        // Direction B: borrow token_b → buy token_a (low rate pool) → sell token_a (high rate pool)
        if a_is_anchor {
            // Borrow token_a, arb via: token_a →[high]→ token_b →[low]→ token_a
            // Only keep the best borrow amount (highest net_profit).
            let mut best: Option<RouteParams> = None;
            for &borrow_amount in borrow_amounts {
                if let Some(route) = build_cross_dex_route(
                    &high.pool, high.canonical_order,
                    &low.pool,  low.canonical_order,
                    *token_a, *token_b,
                    borrow_amount, divergence_bps,
                ) {
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
            // Borrow token_b, arb via: token_b →[low]→ token_a →[high]→ token_b
            let mut best: Option<RouteParams> = None;
            for &borrow_amount in borrow_amounts {
                if let Some(route) = build_cross_dex_route_reverse(
                    &low.pool,  low.canonical_order,
                    &high.pool, high.canonical_order,
                    *token_a, *token_b,
                    borrow_amount, divergence_bps,
                ) {
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

    // Assign tier and filter fragile Orca routes.
    for r in &mut routes {
        r.tier = route_tier(r, pool_cache);
    }
    routes.retain(|r| r.tier < 99);

    // Sort by net_profit descending.
    routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
    routes
}

/// Scan only pools for a specific token pair (triggered by whale swap).
/// Faster than full scan — only considers pools matching the affected pair.
pub fn scan_pair_opportunities(
    pool_cache: &Arc<PoolStateCache>,
    token_a: Pubkey,
    token_b: Pubkey,
    borrow_amounts: &[u64],
) -> Vec<RouteParams> {
    let key = pair_key(&token_a, &token_b);
    let anchor_set: HashSet<Pubkey> = ANCHOR_TOKENS
        .iter()
        .filter_map(|s| s.parse::<Pubkey>().ok())
        .collect();

    // At least one side must be flash-loanable.
    let a_is_anchor = anchor_set.contains(&key.0);
    let b_is_anchor = anchor_set.contains(&key.1);
    if !a_is_anchor && !b_is_anchor {
        return Vec::new();
    }

    // Skip stablecoin pairs — 1323 attempts, 0 landed.
    let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
    let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
    let usdt: Pubkey = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".parse().unwrap();
    let is_stablecoin_pair = (key.0 == usdc && key.1 == usdt) || (key.0 == usdt && key.1 == usdc);
    if is_stablecoin_pair { return Vec::new(); }
    if a_is_anchor && b_is_anchor && key.0 != wsol && key.1 != wsol {
        return Vec::new();
    }

    // Collect eligible pools for this pair.
    let mut pools: Vec<PoolRate> = Vec::new();
    for entry in pool_cache.inner.iter() {
        let pool = entry.value();
        if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
            continue;
        }
        if pool.dex_type == DexType::Unknown || pool.dex_type == DexType::RaydiumClmm {
            continue;
        }
        if pool.dex_type == DexType::RaydiumAmmV4 {
            if let Some(meta) = &pool.raydium_meta {
                if meta.vault_a == Pubkey::default() || meta.vault_b == Pubkey::default() {
                    continue;
                }
            } else {
                continue;
            }
        }

        let pk = pair_key(&pool.token_a, &pool.token_b);
        if pk != key {
            continue;
        }
        let canonical_order = pool.token_a == key.0;
        let reference_amount = borrow_amounts.first().copied().unwrap_or(100_000_000);
        let rate_a_to_b = if canonical_order {
            let out = pool.quote_a_to_b(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        } else {
            let out = pool.quote_b_to_a(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        };
        if rate_a_to_b <= 0.0 || !rate_a_to_b.is_finite() {
            continue;
        }
        pools.push(PoolRate { pool: pool.clone(), rate_a_to_b, canonical_order });
    }

    if pools.len() < 2 {
        return Vec::new();
    }

    let best_high = pools.iter().max_by(|a, b| {
        a.rate_a_to_b.partial_cmp(&b.rate_a_to_b).unwrap_or(std::cmp::Ordering::Equal)
    });
    let best_low = pools.iter().min_by(|a, b| {
        a.rate_a_to_b.partial_cmp(&b.rate_a_to_b).unwrap_or(std::cmp::Ordering::Equal)
    });

    let (Some(high), Some(low)) = (best_high, best_low) else {
        return Vec::new();
    };
    if high.pool.pool_address == low.pool.pool_address || high.rate_a_to_b <= low.rate_a_to_b {
        return Vec::new();
    }
    let divergence_bps = ((high.rate_a_to_b / low.rate_a_to_b - 1.0) * 10_000.0) as u64;
    let combined_fee_bps = dex_fee_bps(high.pool.dex_type) + dex_fee_bps(low.pool.dex_type);
    // Lower threshold for whale backrun: divergence is confirmed by the whale TX.
    let min_divergence = combined_fee_bps + 15; // 15bps margin (vs 30bps for general scan)
    if divergence_bps < min_divergence {
        return Vec::new();
    }

    let mut routes = Vec::new();
    if a_is_anchor {
        for &borrow_amount in borrow_amounts {
            if let Some(mut route) = build_cross_dex_route(
                &high.pool, high.canonical_order,
                &low.pool, low.canonical_order,
                key.0, key.1, borrow_amount, divergence_bps,
            ) {
                route.strategy = "whale-backrun";
                routes.push(route);
            }
        }
    }
    if b_is_anchor {
        for &borrow_amount in borrow_amounts {
            if let Some(mut route) = build_cross_dex_route_reverse(
                &low.pool, low.canonical_order,
                &high.pool, high.canonical_order,
                key.0, key.1, borrow_amount, divergence_bps,
            ) {
                route.strategy = "whale-backrun";
                routes.push(route);
            }
        }
    }

    // Assign tier and filter fragile Orca routes.
    for r in &mut routes {
        r.tier = route_tier(r, pool_cache);
    }
    routes.retain(|r| r.tier < 99);

    // Keep best by net_profit.
    routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
    if routes.len() > 3 { routes.truncate(3); }

    if !routes.is_empty() {
        info!(
            pair = %format!("{}../{}..", &key.0.to_string()[..8], &key.1.to_string()[..8]),
            count = routes.len(),
            best_profit = routes[0].net_profit,
            divergence_bps,
            "[whale-backrun] pair scan found routes"
        );
    }
    routes
}

/// Build a 2-hop route: borrow token_a → swap to token_b on buy_pool → swap back to token_a on sell_pool.
pub fn build_cross_dex_route(
    buy_pool: &DexPool,
    buy_canonical: bool,
    sell_pool: &DexPool,
    sell_canonical: bool,
    token_a: Pubkey,
    token_b: Pubkey,
    borrow_amount: u64,
    divergence_bps: u64,
) -> Option<RouteParams> {
    // Reserve must be >10x borrow to avoid catastrophic price impact.
    let buy_reserve_in = if buy_canonical { buy_pool.reserve_a } else { buy_pool.reserve_b };
    if buy_reserve_in < borrow_amount.saturating_mul(10) {
        return None;
    }

    // Hop 1: token_a → token_b on buy_pool (higher rate = more token_b output).
    let (mid_amount, impact_1) = if buy_canonical {
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

    // Hop 2: token_b → token_a on sell_pool (lower rate = cheaper token_a).
    let (final_amount, impact_2) = if sell_canonical {
        // sell_pool.token_a == token_a, so b_to_a
        let out = sell_pool.quote_b_to_a(mid_amount);
        let impact = mid_amount as f64 / sell_pool.reserve_b.max(1) as f64 * 100.0;
        (out, impact)
    } else {
        // sell_pool.token_b == token_a, so a_to_b
        let out = sell_pool.quote_a_to_b(mid_amount);
        let impact = mid_amount as f64 / sell_pool.reserve_a.max(1) as f64 * 100.0;
        (out, impact)
    };

    if final_amount == 0 || impact_2 > MAX_IMPACT_PCT {
        return None;
    }

    if final_amount <= borrow_amount {
        return None;
    }

    // Sanity: reject implausible returns (stale reserves / phantom pools).
    if final_amount as f64 > borrow_amount as f64 * MAX_RETURN_MULTIPLIER {
        return None;
    }

    let gross_profit = final_amount - borrow_amount;
    let net_profit = gross_profit as i64 - (TX_FEE_PER_HOP * 2) as i64;

    if net_profit <= 0 {
        return None;
    }

    // Risk: combination of both price impacts.
    let max_impact = impact_1.max(impact_2);
    let risk_factor = (max_impact / 100.0).min(0.95);

    // Determine correct token_in/token_out for each hop.
    let (hop1_token_in, hop1_token_out) = (token_a, token_b);
    let (hop2_token_in, hop2_token_out) = (token_b, token_a);

    debug!(
        buy_pool = %buy_pool.pool_address,
        sell_pool = %sell_pool.pool_address,
        buy_dex = ?buy_pool.dex_type,
        sell_dex = ?sell_pool.dex_type,
        divergence_bps,
        borrow_amount,
        mid_amount,
        final_amount,
        gross_profit,
        net_profit,
        "[E6] cross-DEX arb candidate"
    );

    Some(RouteParams {
        hops: vec![
            Hop {
                pool: buy_pool.pool_address,
                dex_program: dex_program_for(buy_pool.dex_type),
                token_in: hop1_token_in,
                token_out: hop1_token_out,
                amount_out: mid_amount,
                price_impact: impact_1,
            },
            Hop {
                pool: sell_pool.pool_address,
                dex_program: dex_program_for(sell_pool.dex_type),
                token_in: hop2_token_in,
                token_out: hop2_token_out,
                amount_out: final_amount,
                price_impact: impact_2,
            },
        ],
        borrow_amount,
        gross_profit,
        net_profit,
        risk_factor,
        strategy: "cross-dex",
        tier: 0,
    })
}

/// Estimated swap fee per DEX in basis points (conservative).
pub fn dex_fee_bps(dex: DexType) -> u64 {
    match dex {
        DexType::RaydiumAmmV4 => 25,    // 0.25%
        DexType::RaydiumClmm  => 25,    // variable, 0.25% typical
        DexType::OrcaWhirlpool => 30,   // variable, 0.30% typical
        DexType::MeteoraDlmm  => 30,    // variable, ~0.30%
        DexType::PumpSwap      => 100,  // 1% flat
        DexType::Fluxbeam      => 30,   // 0.30%
        DexType::Saber         => 4,    // 0.04% stable
        DexType::RaydiumCpmm   => 25,   // 0.25%
        DexType::Unknown       => 50,   // conservative
    }
}

/// Build a 2-hop route borrowing token_b (the anchor):
/// borrow token_b → swap to token_a on buy_a_pool → swap back to token_b on sell_a_pool.
pub fn build_cross_dex_route_reverse(
    buy_a_pool: &DexPool,    // pool where we buy token_a (gives most token_a per token_b)
    buy_a_canonical: bool,
    sell_a_pool: &DexPool,   // pool where we sell token_a (gives most token_b per token_a)
    sell_a_canonical: bool,
    token_a: Pubkey,         // canonical pair tokens
    token_b: Pubkey,         // this is the borrow token (anchor)
    borrow_amount: u64,
    divergence_bps: u64,
) -> Option<RouteParams> {
    // Hop 1: token_b → token_a on buy_a_pool.
    // buy_a_pool has LOW rate_a_to_b = cheap token_a = most token_a per token_b.
    let (mid_amount, impact_1) = if buy_a_canonical {
        // pool.token_a == token_a. We input token_b → output token_a = b_to_a.
        let out = buy_a_pool.quote_b_to_a(borrow_amount);
        let impact = borrow_amount as f64 / buy_a_pool.reserve_b.max(1) as f64 * 100.0;
        (out, impact)
    } else {
        // pool.token_b == token_a. We input token_b (pool's token_a) → output token_a (pool's token_b) = a_to_b.
        let out = buy_a_pool.quote_a_to_b(borrow_amount);
        let impact = borrow_amount as f64 / buy_a_pool.reserve_a.max(1) as f64 * 100.0;
        (out, impact)
    };

    if mid_amount == 0 || impact_1 > MAX_IMPACT_PCT {
        return None;
    }

    // Hop 2: token_a → token_b on sell_a_pool.
    // sell_a_pool has HIGH rate_a_to_b = most token_b per token_a.
    let (final_amount, impact_2) = if sell_a_canonical {
        // pool.token_a == token_a. We input token_a → output token_b = a_to_b.
        let out = sell_a_pool.quote_a_to_b(mid_amount);
        let impact = mid_amount as f64 / sell_a_pool.reserve_a.max(1) as f64 * 100.0;
        (out, impact)
    } else {
        // pool.token_b == token_a. We input token_a (pool's token_b) → output token_b (pool's token_a) = b_to_a.
        let out = sell_a_pool.quote_b_to_a(mid_amount);
        let impact = mid_amount as f64 / sell_a_pool.reserve_b.max(1) as f64 * 100.0;
        (out, impact)
    };

    if final_amount == 0 || impact_2 > MAX_IMPACT_PCT {
        return None;
    }

    if final_amount <= borrow_amount {
        return None;
    }

    // Sanity: reject implausible returns.
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
        buy_pool = %buy_a_pool.pool_address,
        sell_pool = %sell_a_pool.pool_address,
        buy_dex = ?buy_a_pool.dex_type,
        sell_dex = ?sell_a_pool.dex_type,
        divergence_bps,
        borrow_amount,
        mid_amount,
        final_amount,
        gross_profit,
        net_profit,
        "[E6] cross-DEX arb candidate (reverse, borrow token_b)"
    );

    Some(RouteParams {
        hops: vec![
            Hop {
                pool: buy_a_pool.pool_address,
                dex_program: dex_program_for(buy_a_pool.dex_type),
                token_in: token_b,
                token_out: token_a,
                amount_out: mid_amount,
                price_impact: impact_1,
            },
            Hop {
                pool: sell_a_pool.pool_address,
                dex_program: dex_program_for(sell_a_pool.dex_type),
                token_in: token_a,
                token_out: token_b,
                amount_out: final_amount,
                price_impact: impact_2,
            },
        ],
        borrow_amount,
        gross_profit,
        net_profit,
        risk_factor,
        strategy: "cross-dex",
        tier: 0,
    })
}

// ─── 3-Hop Cross-DEX Triangular Arbitrage ─────────────────────────────────
//
// Finds triangular arb: WSOL →[DEX1]→ TokenX →[DEX2]→ TokenY →[DEX3]→ WSOL
// where the product of exchange rates exceeds 1.0 + fees.
// This is different from cyclic_arb.rs which requires 3 DIFFERENT DEX types.
// Here we allow same DEX for different legs (e.g., Raydium→Orca→Raydium).

/// Scan for 3-hop cross-DEX triangular arbitrage opportunities.
/// Finds: anchor → tokenX → tokenY → anchor where each hop uses best-priced pool.
pub fn scan_cross_dex_3hop(
    pool_cache: &Arc<PoolStateCache>,
    borrow_amounts: &[u64],
) -> Vec<RouteParams> {
    let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
    let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
    let anchors = [wsol, usdc];

    // Build adjacency: token → Vec<(other_token, pool)>
    let mut adj: HashMap<Pubkey, Vec<(Pubkey, DexPool)>> = HashMap::new();
    for entry in pool_cache.inner.iter() {
        let pool = entry.value();
        if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
            continue;
        }
        if pool.dex_type == DexType::Unknown || pool.dex_type == DexType::RaydiumClmm
            || pool.dex_type == DexType::RaydiumCpmm {
            continue;
        }
        if pool.dex_type == DexType::RaydiumAmmV4 {
            if let Some(meta) = &pool.raydium_meta {
                if meta.vault_a == Pubkey::default() || meta.vault_b == Pubkey::default() {
                    continue;
                }
            } else {
                continue;
            }
        }
        adj.entry(pool.token_a).or_default().push((pool.token_b, pool.clone()));
        adj.entry(pool.token_b).or_default().push((pool.token_a, pool.clone()));
    }

    let mut routes = Vec::new();
    let ref_amount = borrow_amounts.first().copied().unwrap_or(100_000_000);

    // For each anchor token, find triangles: anchor→X→Y→anchor
    for &anchor in &anchors {
        let hop1_neighbors = match adj.get(&anchor) {
            Some(n) => n,
            None => continue,
        };

        // Limit to tokens with decent liquidity (top neighbors by pool count)
        let unique_x: HashSet<Pubkey> = hop1_neighbors.iter()
            .filter(|(t, _)| *t != anchor && !anchors.contains(t))
            .map(|(t, _)| *t)
            .collect();

        // Cap exploration to avoid O(n³) blowup
        if unique_x.len() > 200 { continue; }

        for &token_x in &unique_x {
            // Quote hop1: anchor → tokenX (best pool)
            let hop1_pools: Vec<&DexPool> = hop1_neighbors.iter()
                .filter(|(t, _)| *t == token_x)
                .map(|(_, p)| p)
                .collect();
            let best_hop1 = hop1_pools.iter()
                .map(|p| {
                    let out = if p.token_a == anchor { p.quote_a_to_b(ref_amount) }
                              else { p.quote_b_to_a(ref_amount) };
                    (*p, out)
                })
                .filter(|(_, out)| *out > 0)
                .max_by_key(|(_, out)| *out);
            let Some((pool1, amount_x)) = best_hop1 else { continue; };

            // Find hop2 neighbors of tokenX
            let hop2_neighbors = match adj.get(&token_x) {
                Some(n) => n,
                None => continue,
            };

            for &(token_y, ref pool2) in hop2_neighbors {
                if token_y == anchor || token_y == token_x { continue; }
                // Skip if tokenY has no path back to anchor
                let hop3_neighbors = match adj.get(&token_y) {
                    Some(n) => n,
                    None => continue,
                };
                let has_return = hop3_neighbors.iter().any(|(t, _)| *t == anchor);
                if !has_return { continue; }

                // Quote hop2: tokenX → tokenY
                let amount_y = if pool2.token_a == token_x {
                    pool2.quote_a_to_b(amount_x)
                } else {
                    pool2.quote_b_to_a(amount_x)
                };
                if amount_y == 0 { continue; }

                // Quote hop3: tokenY → anchor (best pool)
                let hop3_pools: Vec<&(Pubkey, DexPool)> = hop3_neighbors.iter()
                    .filter(|(t, _)| *t == anchor)
                    .collect();
                let best_hop3 = hop3_pools.iter()
                    .map(|(_, p)| {
                        let out = if p.token_a == token_y { p.quote_a_to_b(amount_y) }
                                  else { p.quote_b_to_a(amount_y) };
                        (p, out)
                    })
                    .filter(|(_, out)| *out > 0)
                    .max_by_key(|(_, out)| *out);
                let Some((pool3, final_amount)) = best_hop3 else { continue; };

                // Check profitability
                if final_amount <= ref_amount { continue; }
                let return_pct = final_amount as f64 / ref_amount as f64;
                if return_pct > MAX_RETURN_MULTIPLIER as f64 { continue; } // phantom
                let gross_profit = final_amount - ref_amount;
                let fees = TX_FEE_PER_HOP * 3;
                let net_profit = gross_profit as i64 - fees as i64;
                let combined_fee_bps = dex_fee_bps(pool1.dex_type) + dex_fee_bps(pool2.dex_type) + dex_fee_bps(pool3.dex_type);
                let margin_bps = ((return_pct - 1.0) * 10_000.0) as u64;
                if margin_bps < combined_fee_bps + 20 { continue; } // need 20bps above fees

                if net_profit < 1_000 { continue; }

                // Try all borrow amounts, keep best
                let mut best: Option<RouteParams> = None;
                for &borrow_amount in borrow_amounts {
                    let a1 = if pool1.token_a == anchor { pool1.quote_a_to_b(borrow_amount) }
                             else { pool1.quote_b_to_a(borrow_amount) };
                    if a1 == 0 { continue; }
                    let a2 = if pool2.token_a == token_x { pool2.quote_a_to_b(a1) }
                             else { pool2.quote_b_to_a(a1) };
                    if a2 == 0 { continue; }
                    let a3 = if pool3.token_a == token_y { pool3.quote_a_to_b(a2) }
                             else { pool3.quote_b_to_a(a2) };
                    if a3 == 0 || a3 <= borrow_amount { continue; }
                    let ret = a3 as f64 / borrow_amount as f64;
                    if ret > MAX_RETURN_MULTIPLIER as f64 { continue; }

                    let gp = a3 - borrow_amount;
                    let np = gp as i64 - (TX_FEE_PER_HOP * 3) as i64;
                    if np <= 0 { continue; }

                    let impact1 = (borrow_amount as f64 / pool1.reserve_a.max(pool1.reserve_b).max(1) as f64) * 100.0;
                    let impact2 = (a1 as f64 / pool2.reserve_a.max(pool2.reserve_b).max(1) as f64) * 100.0;
                    let impact3 = (a2 as f64 / pool3.reserve_a.max(pool3.reserve_b).max(1) as f64) * 100.0;
                    if impact1 > MAX_IMPACT_PCT || impact2 > MAX_IMPACT_PCT || impact3 > MAX_IMPACT_PCT {
                        continue;
                    }

                    let route = RouteParams {
                        hops: vec![
                            Hop {
                                pool: pool1.pool_address,
                                dex_program: dex_program_for(pool1.dex_type),
                                token_in: anchor,
                                token_out: token_x,
                                amount_out: a1,
                                price_impact: impact1,
                            },
                            Hop {
                                pool: pool2.pool_address,
                                dex_program: dex_program_for(pool2.dex_type),
                                token_in: token_x,
                                token_out: token_y,
                                amount_out: a2,
                                price_impact: impact2,
                            },
                            Hop {
                                pool: pool3.pool_address,
                                dex_program: dex_program_for(pool3.dex_type),
                                token_in: token_y,
                                token_out: anchor,
                                amount_out: a3,
                                price_impact: impact3,
                            },
                        ],
                        borrow_amount,
                        gross_profit: gp,
                        net_profit: np,
                        risk_factor: 0.3,
                        strategy: "cross_dex_3hop",
                        tier: 0,
                    };
                    if best.as_ref().map_or(true, |b| np > b.net_profit) {
                        best = Some(route);
                    }
                }
                if let Some(route) = best {
                    routes.push(route);
                }
            }
        }
    }

    // Assign tier and filter fragile Orca routes.
    for r in &mut routes {
        r.tier = route_tier(r, pool_cache);
    }
    routes.retain(|r| r.tier < 99);

    routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
    // Limit to top 20 to avoid flooding executor
    routes.truncate(20);

    if !routes.is_empty() {
        info!(
            count = routes.len(),
            best_profit = routes[0].net_profit,
            "[cross-dex-3hop] opportunities found"
        );
    }

    routes
}

// ─── Hot Routes: Priority scanning of historically profitable pairs ────────

/// A historically profitable route (pool pair) loaded from PG.
#[derive(Debug, Clone)]
pub struct HotRoute {
    pub pool_a: Pubkey,
    pub pool_b: Pubkey,
    pub token_a: Pubkey,
    pub token_b: Pubkey,
    pub freq: u64,
    pub avg_profit: i64,
    pub max_profit: i64,
}

/// Load proven profitable routes from PostgreSQL.
/// Queries opportunity_hops for the most frequent and profitable pool pairs.
pub async fn load_hot_routes(db_url: &str) -> anyhow::Result<Vec<HotRoute>> {
    let (client, connection) =
        tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move { let _ = connection.await; });

    let rows = client.query(
        "WITH hop_pairs AS (
            SELECT o.id as opp_id, o.net_profit,
                   array_agg(oh.pool ORDER BY oh.hop_index) as pools,
                   array_agg(oh.token_in ORDER BY oh.hop_index) as tokens_in,
                   array_agg(oh.token_out ORDER BY oh.hop_index) as tokens_out
            FROM opportunities o
            JOIN opportunity_hops oh ON oh.opp_db_id = o.id
            WHERE o.net_profit > 0 AND o.source_strategy IN ('cross-dex', 'whale-backrun')
            GROUP BY o.id, o.net_profit
            HAVING count(*) = 2
        )
        SELECT pools[1] as pool_a, pools[2] as pool_b,
               tokens_in[1] as borrow_token, tokens_out[1] as mid_token,
               count(*) as freq,
               avg(net_profit)::bigint as avg_profit,
               max(net_profit) as max_profit
        FROM hop_pairs
        GROUP BY pools[1], pools[2], tokens_in[1], tokens_out[1]
        HAVING count(*) >= 5
        ORDER BY count(*) * avg(net_profit) DESC
        LIMIT 50", &[],
    ).await?;

    let mut routes = Vec::new();
    for row in &rows {
        let pool_a_str: String = row.get(0);
        let pool_b_str: String = row.get(1);
        let token_a_str: String = row.get(2);
        let token_b_str: String = row.get(3);
        let freq: i64 = row.get(4);
        let avg_profit: i64 = row.get(5);
        let max_profit: i64 = row.get(6);
        if let (Ok(pa), Ok(pb), Ok(ta), Ok(tb)) = (
            pool_a_str.parse::<Pubkey>(),
            pool_b_str.parse::<Pubkey>(),
            token_a_str.parse::<Pubkey>(),
            token_b_str.parse::<Pubkey>(),
        ) {
            routes.push(HotRoute {
                pool_a: pa, pool_b: pb, token_a: ta, token_b: tb,
                freq: freq as u64, avg_profit, max_profit,
            });
        }
    }

    tracing::info!(count = routes.len(), "hot-routes loaded from PG");
    Ok(routes)
}

/// Priority scan: check only hot routes for divergence.
/// Uses a lower margin threshold (half of normal) because these pairs
/// have proven arb history — the divergence is more likely real.
pub fn scan_hot_routes(
    pool_cache: &Arc<PoolStateCache>,
    hot_routes: &[HotRoute],
    borrow_amounts: &[u64],
) -> Vec<RouteParams> {
    let anchor_set: HashSet<Pubkey> = ANCHOR_TOKENS
        .iter()
        .filter_map(|s| s.parse::<Pubkey>().ok())
        .collect();

    let mut routes = Vec::new();

    for hr in hot_routes {
        let pool_a = match pool_cache.get(&hr.pool_a) {
            Some(p) => p,
            None => continue,
        };
        let pool_b = match pool_cache.get(&hr.pool_b) {
            Some(p) => p,
            None => continue,
        };

        // Skip if either pool is dust
        if pool_a.reserve_a < MIN_RESERVE || pool_a.reserve_b < MIN_RESERVE {
            continue;
        }
        if pool_b.reserve_a < MIN_RESERVE || pool_b.reserve_b < MIN_RESERVE {
            continue;
        }

        // Determine canonical key and direction
        let key = pair_key(&hr.token_a, &hr.token_b);
        let a_is_anchor = anchor_set.contains(&key.0);
        let b_is_anchor = anchor_set.contains(&key.1);
        if !a_is_anchor && !b_is_anchor {
            continue;
        }
        // Skip stablecoin pairs (USDC↔USDT)
        let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
        let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let usdt: Pubkey = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".parse().unwrap();
        let is_stablecoin_pair = (key.0 == usdc && key.1 == usdt)
            || (key.0 == usdt && key.1 == usdc);
        if is_stablecoin_pair {
            continue;
        }
        if a_is_anchor && b_is_anchor && key.0 != wsol && key.1 != wsol {
            continue;
        }

        // Compute rates for both pools
        let reference_amount = borrow_amounts.first().copied().unwrap_or(100_000_000);

        let pool_a_canonical = pool_a.token_a == key.0;
        let rate_a = if pool_a_canonical {
            let out = pool_a.quote_a_to_b(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        } else {
            let out = pool_a.quote_b_to_a(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        };

        let pool_b_canonical = pool_b.token_a == key.0;
        let rate_b = if pool_b_canonical {
            let out = pool_b.quote_a_to_b(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        } else {
            let out = pool_b.quote_b_to_a(reference_amount);
            if out == 0 { continue; }
            out as f64 / reference_amount as f64
        };

        if rate_a <= 0.0 || rate_b <= 0.0 || !rate_a.is_finite() || !rate_b.is_finite() {
            continue;
        }

        // Determine high/low
        let (high_pool, high_canonical, low_pool, low_canonical) = if rate_a > rate_b {
            (&pool_a, pool_a_canonical, &pool_b, pool_b_canonical)
        } else if rate_b > rate_a {
            (&pool_b, pool_b_canonical, &pool_a, pool_a_canonical)
        } else {
            continue;
        };
        let high_rate = rate_a.max(rate_b);
        let low_rate = rate_a.min(rate_b);

        let divergence_bps = ((high_rate / low_rate - 1.0) * 10_000.0) as u64;
        let combined_fee_bps = dex_fee_bps(high_pool.dex_type) + dex_fee_bps(low_pool.dex_type);
        // Hot routes get HALF the normal margin requirement — proven pairs.
        let min_divergence = combined_fee_bps + MIN_MARGIN_ABOVE_FEES_BPS / 2;
        if divergence_bps < min_divergence {
            continue;
        }

        if a_is_anchor {
            let mut best: Option<RouteParams> = None;
            for &borrow_amount in borrow_amounts {
                if let Some(mut route) = build_cross_dex_route(
                    high_pool, high_canonical,
                    low_pool, low_canonical,
                    key.0, key.1, borrow_amount, divergence_bps,
                ) {
                    route.strategy = "hot-route";
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
                    low_pool, low_canonical,
                    high_pool, high_canonical,
                    key.0, key.1, borrow_amount, divergence_bps,
                ) {
                    route.strategy = "hot-route";
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

    // Assign tier and filter fragile Orca routes.
    for r in &mut routes {
        r.tier = route_tier(r, pool_cache);
    }
    routes.retain(|r| r.tier < 99);

    routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
    routes
}

/// Load ML-scored hot routes from the snapshot scanner's `hot_routes` table.
/// These are 2-hop and 3-hop routes scored by the snapshot_scanner with low competition.
/// Returns HotRoute structs for 2-hop pairs; 3-hop routes are returned as MultiHopHotRoute.
pub async fn load_snapshot_hot_routes(db_url: &str) -> anyhow::Result<(Vec<HotRoute>, Vec<MultiHopHotRoute>)> {
    let (client, connection) =
        tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move { let _ = connection.await; });

    // Load 2-hop routes
    let rows_2 = client.query(
        "SELECT pools, tokens, dex_types, borrow_mint, borrow_amount,
                net_profit_lamports, ml_score, competition_score
         FROM hot_routes
         WHERE hops = 2 AND expires_at > now() AND ml_score > 0.5
         ORDER BY ml_score DESC LIMIT 50", &[],
    ).await?;

    let mut hot_2: Vec<HotRoute> = Vec::new();
    for row in &rows_2 {
        let pools: Vec<String> = row.get(0);
        let tokens: Vec<String> = row.get(1);
        let profit: i64 = row.get(5);
        let ml_score: f64 = row.get(6);
        if pools.len() >= 2 && tokens.len() >= 2 {
            if let (Ok(pa), Ok(pb), Ok(ta), Ok(tb)) = (
                pools[0].parse::<Pubkey>(), pools[1].parse::<Pubkey>(),
                tokens[0].parse::<Pubkey>(), tokens[1].parse::<Pubkey>(),
            ) {
                hot_2.push(HotRoute {
                    pool_a: pa, pool_b: pb, token_a: ta, token_b: tb,
                    freq: (ml_score * 10.0) as u64, avg_profit: profit, max_profit: profit,
                });
            }
        }
    }

    // Load 3-hop routes
    let rows_3 = client.query(
        "SELECT pools, tokens, dex_types, borrow_mint, borrow_amount,
                net_profit_lamports, ml_score, competition_score
         FROM hot_routes
         WHERE hops = 3 AND expires_at > now() AND ml_score > 0.5
         ORDER BY ml_score DESC LIMIT 100", &[],
    ).await?;

    let mut hot_3: Vec<MultiHopHotRoute> = Vec::new();
    for row in &rows_3 {
        let pools: Vec<String> = row.get(0);
        let tokens: Vec<String> = row.get(1);
        let dex_types: Vec<String> = row.get(2);
        let borrow_mint: String = row.get(3);
        let borrow_amount: i64 = row.get(4);
        let net_profit: i64 = row.get(5);
        let ml_score: f64 = row.get(6);
        let competition: f64 = row.get(7);

        if pools.len() >= 3 && tokens.len() >= 4 {
            let pool_pks: Vec<Pubkey> = pools.iter()
                .filter_map(|s| s.parse::<Pubkey>().ok())
                .collect();
            let token_pks: Vec<Pubkey> = tokens.iter()
                .filter_map(|s| s.parse::<Pubkey>().ok())
                .collect();
            if pool_pks.len() >= 3 && token_pks.len() >= 4 {
                hot_3.push(MultiHopHotRoute {
                    pools: pool_pks,
                    tokens: token_pks,
                    dex_types,
                    borrow_mint: borrow_mint.parse().unwrap_or_default(),
                    borrow_amount: borrow_amount as u64,
                    expected_profit: net_profit,
                    ml_score,
                    competition,
                });
            }
        }
    }

    tracing::info!(
        two_hop = hot_2.len(),
        three_hop = hot_3.len(),
        "snapshot hot routes loaded from PG"
    );
    Ok((hot_2, hot_3))
}

/// A multi-hop (3+) hot route from snapshot scanner.
#[derive(Debug, Clone)]
pub struct MultiHopHotRoute {
    pub pools: Vec<Pubkey>,
    pub tokens: Vec<Pubkey>,
    pub dex_types: Vec<String>,
    pub borrow_mint: Pubkey,
    pub borrow_amount: u64,
    pub expected_profit: i64,
    pub ml_score: f64,
    pub competition: f64,
}

/// Scan multi-hop hot routes with real-time quotes from pool cache.
/// Re-quotes the route with current reserves to verify profitability.
pub fn scan_multihop_hot_routes(
    pool_cache: &Arc<PoolStateCache>,
    hot_routes: &[MultiHopHotRoute],
    borrow_amounts: &[u64],
) -> Vec<RouteParams> {
    let mut results = Vec::new();

    for hr in hot_routes {
        // Skip if any pool is missing from cache
        let pools: Vec<DexPool> = hr.pools.iter()
            .filter_map(|p| pool_cache.get(p))
            .collect();
        if pools.len() != hr.pools.len() {
            continue;
        }

        for &borrow_amount in borrow_amounts {
            let mut current = borrow_amount;
            let mut hops = Vec::new();
            let mut valid = true;

            for (i, pool) in pools.iter().enumerate() {
                if i + 1 >= hr.tokens.len() { valid = false; break; }
                let token_in = hr.tokens[i];
                let token_out = hr.tokens[i + 1];

                let amount_out = if pool.token_a == token_in {
                    pool.quote_a_to_b(current)
                } else if pool.token_b == token_in {
                    pool.quote_b_to_a(current)
                } else {
                    0
                };

                if amount_out == 0 {
                    valid = false;
                    break;
                }

                let impact = if pool.token_a == token_in {
                    if pool.reserve_a > 0 { current as f64 / pool.reserve_a as f64 * 100.0 } else { 100.0 }
                } else {
                    if pool.reserve_b > 0 { current as f64 / pool.reserve_b as f64 * 100.0 } else { 100.0 }
                };

                if impact > 10.0 {
                    valid = false;
                    break;
                }

                hops.push(crate::types::Hop {
                    pool: pool.pool_address,
                    dex_program: dex_program_for(pool.dex_type),
                    token_in,
                    token_out,
                    amount_out,
                    price_impact: impact,
                });

                current = amount_out;
            }

            if !valid || current <= borrow_amount {
                continue;
            }

            let gross_profit = current - borrow_amount;
            let tx_cost = 5_000 + 5_000 * hops.len() as i64;
            let net_profit = gross_profit as i64 - tx_cost;

            if net_profit > 5_000 {
                let max_impact = hops.iter().map(|h| h.price_impact).fold(0.0f64, f64::max);
                results.push(RouteParams {
                    hops,
                    borrow_amount,
                    gross_profit,
                    net_profit,
                    risk_factor: (max_impact / 100.0).min(1.0),
                    strategy: "hot-route",
                    tier: 0,
                });
            }
        }
    }

    // Assign tier and filter fragile Orca routes.
    for r in &mut results {
        r.tier = route_tier(r, pool_cache);
    }
    results.retain(|r| r.tier < 99);

    results.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
    results
}

pub fn dex_program_for(dex: DexType) -> Pubkey {
    match dex {
        DexType::RaydiumAmmV4 => dex_programs::raydium_amm_v4(),
        DexType::RaydiumClmm => dex_programs::raydium_clmm(),
        DexType::OrcaWhirlpool => dex_programs::orca_whirlpool(),
        DexType::MeteoraDlmm => dex_programs::meteora_dlmm(),
        DexType::PumpSwap => dex_programs::pumpswap(),
        DexType::Fluxbeam => dex_programs::fluxbeam(),
        DexType::Saber => dex_programs::saber(),
        DexType::RaydiumCpmm => dex_programs::raydium_cpmm(),
        DexType::Unknown => Pubkey::default(),
    }
}
