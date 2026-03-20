//! lst_scanner.rs – LST (Liquid Staking Token) Arbitrage Scanner
//!
//! Scans for price divergences between DEXes on staking derivative pairs
//! (mSOL/SOL, jitoSOL/SOL, bSOL/SOL, etc.). These pairs have anchored prices
//! (staking rate) so divergences are low-risk and revert quickly.

use std::collections::HashSet;
use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::info;

use crate::cross_dex::{
    build_cross_dex_route, build_cross_dex_route_reverse, dex_fee_bps, dex_program_for,
};
use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{DexType, RouteParams};

/// LST mints on mainnet.
const LST_MINTS: &[&str] = &[
    "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",  // mSOL
    "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", // jitoSOL
    "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1",  // bSOL
    "7dHbWXmci3dT8UFYWYZweBLXgycu7Y3iL6trKn1Y7ARj", // stSOL
    "5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm", // INF
];

/// WSOL — the anchor token for LST pairs.
const WSOL: &str = "So11111111111111111111111111111111111111112";

/// Lower threshold for LST arb: staking rate is anchored, so divergences are reliable.
const MIN_MARGIN_ABOVE_FEES_BPS: u64 = 10; // 0.1% (vs 0.3% for general cross-dex)

/// Minimum reserves to consider a pool.
const MIN_RESERVE: u64 = 1_000_000;

/// Maximum return multiplier (LST arbs are typically tiny, <1%).
const MAX_RETURN_MULTIPLIER: f64 = 1.02;

const MAX_IMPACT_PCT: f64 = 3.0;
const TX_FEE_PER_HOP: u64 = 5_000;

/// Canonical pair key.
fn pair_key(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a < b { (*a, *b) } else { (*b, *a) }
}

struct PoolRate {
    pool: DexPool,
    rate_a_to_b: f64,
    canonical_order: bool,
}

/// Scan all LST/SOL pairs across DEXes for arbitrage opportunities.
pub fn scan_lst_opportunities(
    pool_cache: &Arc<PoolStateCache>,
    borrow_amounts: &[u64],
) -> Vec<RouteParams> {
    let wsol: Pubkey = WSOL.parse().expect("valid WSOL pubkey");
    let lst_set: HashSet<Pubkey> = LST_MINTS
        .iter()
        .filter_map(|s| s.parse::<Pubkey>().ok())
        .collect();

    // Group eligible LST/SOL pools by canonical pair.
    let mut pair_pools: std::collections::HashMap<(Pubkey, Pubkey), Vec<PoolRate>> =
        std::collections::HashMap::new();

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

        // Check if this is an LST/SOL pair.
        let is_lst_pair = (pool.token_a == wsol && lst_set.contains(&pool.token_b))
            || (pool.token_b == wsol && lst_set.contains(&pool.token_a));
        if !is_lst_pair {
            continue;
        }

        let key = pair_key(&pool.token_a, &pool.token_b);
        let canonical_order = pool.token_a == key.0;
        let rate_a_to_b = if canonical_order {
            if pool.reserve_a == 0 { continue; }
            pool.reserve_b as f64 / pool.reserve_a as f64
        } else {
            if pool.reserve_b == 0 { continue; }
            pool.reserve_a as f64 / pool.reserve_b as f64
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

    let mut routes = Vec::new();

    for ((token_a, token_b), pools) in &pair_pools {
        if pools.len() < 2 {
            continue;
        }

        let best_high = pools.iter().max_by(|a, b| {
            a.rate_a_to_b.partial_cmp(&b.rate_a_to_b).unwrap_or(std::cmp::Ordering::Equal)
        });
        let best_low = pools.iter().min_by(|a, b| {
            a.rate_a_to_b.partial_cmp(&b.rate_a_to_b).unwrap_or(std::cmp::Ordering::Equal)
        });

        let (Some(high), Some(low)) = (best_high, best_low) else { continue };

        if high.pool.pool_address == low.pool.pool_address || high.rate_a_to_b <= low.rate_a_to_b {
            continue;
        }

        let divergence_bps = ((high.rate_a_to_b / low.rate_a_to_b - 1.0) * 10_000.0) as u64;
        let combined_fee_bps = dex_fee_bps(high.pool.dex_type) + dex_fee_bps(low.pool.dex_type);
        if divergence_bps < combined_fee_bps + MIN_MARGIN_ABOVE_FEES_BPS {
            continue;
        }

        // WSOL is always the borrow token (flash loanable).
        let wsol_is_a = *token_a == wsol;

        let mut best: Option<RouteParams> = None;
        for &borrow_amount in borrow_amounts {
            let route_opt = if wsol_is_a {
                build_cross_dex_route(
                    &high.pool, high.canonical_order,
                    &low.pool, low.canonical_order,
                    *token_a, *token_b, borrow_amount, divergence_bps,
                )
            } else {
                build_cross_dex_route_reverse(
                    &low.pool, low.canonical_order,
                    &high.pool, high.canonical_order,
                    *token_a, *token_b, borrow_amount, divergence_bps,
                )
            };

            if let Some(mut route) = route_opt {
                // Reject implausible returns for LST.
                let return_ratio = (route.borrow_amount as f64 + route.gross_profit as f64)
                    / route.borrow_amount as f64;
                if return_ratio > MAX_RETURN_MULTIPLIER {
                    continue;
                }
                route.strategy = "lst-arb";
                // LST arbs are lower risk — anchored price.
                route.risk_factor = (route.risk_factor * 0.5).min(0.3);
                if best.as_ref().map_or(true, |b| route.net_profit > b.net_profit) {
                    best = Some(route);
                }
            }
        }
        if let Some(route) = best {
            routes.push(route);
        }
    }

    routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));

    if !routes.is_empty() {
        info!(
            count = routes.len(),
            best_profit = routes[0].net_profit,
            "[lst-arb] opportunities found"
        );
    }

    routes
}
