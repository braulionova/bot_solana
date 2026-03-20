//! cyclic_arb.rs – 3-leg cyclic arbitrage through niche venue combinations.
//!
//! Finds profitable cycles A -> B -> C -> A where the output exceeds input,
//! specifically targeting venue combinations with lower competition.
//! Every cycle is a flash loan arb: borrow token -> hop1 -> hop2 -> hop3 -> repay.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info};

use crate::cross_dex::{dex_fee_bps, dex_program_for};
use crate::pool_state::{DexPool, PoolStateCache};
use crate::types::{DexType, Hop, RouteParams};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum net margin above fees for 3-leg cycles (bps).
/// Lower than 2-hop because 3-hop faces less competition.
const MIN_MARGIN_ABOVE_FEES_BPS: u64 = 30;

/// Maximum trade size: 2 SOL (conservative for low-liquidity venues).
const MAX_BORROW_LAMPORTS: u64 = 2_000_000_000;

/// Maximum plausible return: 5% profit. Higher = likely stale reserves.
const MAX_RETURN_MULTIPLIER: f64 = 1.05;

/// Maximum per-hop price impact (%).
const MAX_IMPACT_PCT: f64 = 10.0;

/// Minimum reserve in either token (raw units).
const MIN_RESERVE: u64 = 1_000_000;

/// TX fee estimate per hop (lamports).
const TX_FEE_PER_HOP: u64 = 5_000;

/// WSOL mint.
const WSOL: &str = "So11111111111111111111111111111111111111112";
/// USDC mint.
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// Anchor tokens: cycles must start and end at one of these (flash loanable).
const ANCHOR_TOKENS: &[&str] = &[WSOL, USDC];

/// Venue combinations to scan for 3-leg cycles.
/// Each tuple defines three DEX types that frequently have overlapping but
/// differently-priced pools for niche tokens.
const VENUE_COMBOS: &[(DexType, DexType, DexType)] = &[
    // Active combos — use DEXes with pools in mapped_pools.json
    (DexType::RaydiumAmmV4, DexType::OrcaWhirlpool, DexType::MeteoraDlmm),
    (DexType::RaydiumAmmV4, DexType::MeteoraDlmm, DexType::OrcaWhirlpool),
    (DexType::OrcaWhirlpool, DexType::RaydiumAmmV4, DexType::MeteoraDlmm),
    (DexType::OrcaWhirlpool, DexType::MeteoraDlmm, DexType::RaydiumAmmV4),
    (DexType::MeteoraDlmm, DexType::RaydiumAmmV4, DexType::OrcaWhirlpool),
    // PumpSwap combos — activate when PumpSwap pools are discovered
    (DexType::PumpSwap, DexType::RaydiumAmmV4, DexType::OrcaWhirlpool),
    (DexType::PumpSwap, DexType::MeteoraDlmm, DexType::RaydiumAmmV4),
    // Niche venue combos — activate when Fluxbeam/Saber/CPMM pools are added
    (DexType::Fluxbeam, DexType::MeteoraDlmm, DexType::OrcaWhirlpool),
    (DexType::Saber, DexType::OrcaWhirlpool, DexType::RaydiumAmmV4),
    (DexType::RaydiumCpmm, DexType::RaydiumAmmV4, DexType::OrcaWhirlpool),
];

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

pub struct CyclicArbScanner {
    pool_cache: Arc<PoolStateCache>,
}

impl CyclicArbScanner {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        Self { pool_cache }
    }

    /// Scan for profitable 3-leg cycles through specified venue combinations.
    /// Each cycle is a flash loan arb: borrow token -> hop1 -> hop2 -> hop3 -> repay.
    pub fn scan_cyclic_opportunities(
        &self,
        borrow_amounts: &[u64],
    ) -> Vec<RouteParams> {
        let anchor_set: HashSet<Pubkey> = ANCHOR_TOKENS
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();

        // Build per-DEX index: dex_type -> Vec<(token_a, token_b, pool)>.
        let mut dex_pools: HashMap<DexType, Vec<DexPool>> = HashMap::new();
        for entry in self.pool_cache.inner.iter() {
            let pool = entry.value();
            if pool.dex_type == DexType::Unknown || pool.dex_type == DexType::RaydiumClmm
                || pool.dex_type == DexType::RaydiumCpmm {
                continue;
            }
            if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
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
            dex_pools.entry(pool.dex_type).or_default().push(pool.clone());
        }

        // Build token-pair index per DEX for O(1) lookup.
        // Map: (dex_type, canonical_pair) -> Vec<DexPool>
        let mut dex_pair_index: HashMap<(DexType, Pubkey, Pubkey), Vec<DexPool>> = HashMap::new();
        for (dex, pools) in &dex_pools {
            for pool in pools {
                let (a, b) = canonical_pair(&pool.token_a, &pool.token_b);
                dex_pair_index
                    .entry((*dex, a, b))
                    .or_default()
                    .push(pool.clone());
            }
        }

        // Also build a per-dex token adjacency: dex -> token -> set of paired tokens.
        let mut dex_token_pairs: HashMap<DexType, HashMap<Pubkey, HashSet<Pubkey>>> = HashMap::new();
        for (dex, pools) in &dex_pools {
            let adj = dex_token_pairs.entry(*dex).or_default();
            for pool in pools {
                adj.entry(pool.token_a).or_default().insert(pool.token_b);
                adj.entry(pool.token_b).or_default().insert(pool.token_a);
            }
        }

        let mut routes = Vec::new();

        for &(dex1, dex2, dex3) in VENUE_COMBOS {
            let adj1 = match dex_token_pairs.get(&dex1) {
                Some(a) => a,
                None => continue,
            };
            let adj2 = match dex_token_pairs.get(&dex2) {
                Some(a) => a,
                None => continue,
            };
            let adj3 = match dex_token_pairs.get(&dex3) {
                Some(a) => a,
                None => continue,
            };

            // For each anchor token A, find cycles: A -[dex1]-> B -[dex2]-> C -[dex3]-> A
            for &anchor in &anchor_set {
                // Tokens reachable from anchor on dex1.
                let neighbors_1 = match adj1.get(&anchor) {
                    Some(n) => n,
                    None => continue,
                };

                for &token_b in neighbors_1 {
                    if anchor_set.contains(&token_b) && token_b != anchor {
                        // Skip: arb between anchors tends to be stablecoin noise.
                        continue;
                    }

                    // Tokens reachable from token_b on dex2.
                    let neighbors_2 = match adj2.get(&token_b) {
                        Some(n) => n,
                        None => continue,
                    };

                    for &token_c in neighbors_2 {
                        if token_c == anchor || token_c == token_b {
                            continue;
                        }

                        // Check token_c -> anchor exists on dex3.
                        let has_closing = adj3
                            .get(&token_c)
                            .map_or(false, |n| n.contains(&anchor));
                        if !has_closing {
                            continue;
                        }

                        // Found candidate cycle: anchor -[dex1]-> B -[dex2]-> C -[dex3]-> anchor
                        // Pick best pool for each leg and quote.
                        let pool1 = best_pool_for_pair(
                            &dex_pair_index, dex1, &anchor, &token_b,
                        );
                        let pool2 = best_pool_for_pair(
                            &dex_pair_index, dex2, &token_b, &token_c,
                        );
                        let pool3 = best_pool_for_pair(
                            &dex_pair_index, dex3, &token_c, &anchor,
                        );

                        let (Some(p1), Some(p2), Some(p3)) = (pool1, pool2, pool3) else {
                            continue;
                        };

                        // Try each borrow amount, keep the best.
                        let mut best: Option<RouteParams> = None;
                        for &borrow_amount in borrow_amounts {
                            // Cap at MAX_BORROW_LAMPORTS for low-liquidity venues.
                            let capped = borrow_amount.min(MAX_BORROW_LAMPORTS);
                            if let Some(route) = self.quote_3leg_cycle(
                                &p1, anchor, token_b,
                                &p2, token_b, token_c,
                                &p3, token_c, anchor,
                                capped,
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
            }
        }

        // Sort by net_profit descending.
        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));

        // Count pools per active DEX for diagnostics.
        let dex_counts: Vec<String> = dex_pools.iter()
            .map(|(dex, pools)| format!("{:?}={}", dex, pools.len()))
            .collect();
        let combo_count = VENUE_COMBOS.len();
        info!(
            combos = combo_count,
            dex_counts = ?dex_counts,
            routes = routes.len(),
            "[cyclic-3leg] scan complete"
        );

        if !routes.is_empty() {
            info!(
                count = routes.len(),
                best_profit = routes[0].net_profit,
                "[cyclic-3leg] opportunities found"
            );
        }

        routes
    }

    /// Quote a 3-leg cycle and return a RouteParams if profitable.
    fn quote_3leg_cycle(
        &self,
        pool1: &DexPool, token_in_1: Pubkey, token_out_1: Pubkey,
        pool2: &DexPool, token_in_2: Pubkey, token_out_2: Pubkey,
        pool3: &DexPool, token_in_3: Pubkey, token_out_3: Pubkey,
        borrow_amount: u64,
    ) -> Option<RouteParams> {
        // Hop 1.
        let (out1, impact1) = quote_hop(pool1, token_in_1, borrow_amount)?;
        if impact1 > MAX_IMPACT_PCT {
            return None;
        }

        // Hop 2.
        let (out2, impact2) = quote_hop(pool2, token_in_2, out1)?;
        if impact2 > MAX_IMPACT_PCT {
            return None;
        }

        // Hop 3.
        let (out3, impact3) = quote_hop(pool3, token_in_3, out2)?;
        if impact3 > MAX_IMPACT_PCT {
            return None;
        }

        if out3 <= borrow_amount {
            return None;
        }

        // Sanity: reject implausible returns.
        if out3 as f64 > borrow_amount as f64 * MAX_RETURN_MULTIPLIER {
            return None;
        }

        let gross_profit = out3 - borrow_amount;
        let net_profit = gross_profit as i64 - (TX_FEE_PER_HOP * 3) as i64;

        if net_profit <= 0 {
            return None;
        }

        // Check combined fee margin.
        let total_fee_bps =
            dex_fee_bps(pool1.dex_type) + dex_fee_bps(pool2.dex_type) + dex_fee_bps(pool3.dex_type);
        let profit_bps = (gross_profit as f64 / borrow_amount as f64 * 10_000.0) as u64;
        if profit_bps < total_fee_bps + MIN_MARGIN_ABOVE_FEES_BPS {
            // Profit is too close to fee floor — likely noise.
            return None;
        }

        let max_impact = impact1.max(impact2).max(impact3);
        let risk_factor = (max_impact / 100.0).min(0.95);

        debug!(
            pool1 = %pool1.pool_address,
            pool2 = %pool2.pool_address,
            pool3 = %pool3.pool_address,
            dex1 = ?pool1.dex_type,
            dex2 = ?pool2.dex_type,
            dex3 = ?pool3.dex_type,
            borrow_amount,
            out1, out2, out3,
            gross_profit,
            net_profit,
            "[cyclic-3leg] arb candidate"
        );

        Some(RouteParams {
            hops: vec![
                Hop {
                    pool: pool1.pool_address,
                    dex_program: dex_program_for(pool1.dex_type),
                    token_in: token_in_1,
                    token_out: token_out_1,
                    amount_out: out1,
                    price_impact: impact1,
                },
                Hop {
                    pool: pool2.pool_address,
                    dex_program: dex_program_for(pool2.dex_type),
                    token_in: token_in_2,
                    token_out: token_out_2,
                    amount_out: out2,
                    price_impact: impact2,
                },
                Hop {
                    pool: pool3.pool_address,
                    dex_program: dex_program_for(pool3.dex_type),
                    token_in: token_in_3,
                    token_out: token_out_3,
                    amount_out: out3,
                    price_impact: impact3,
                },
            ],
            borrow_amount,
            gross_profit,
            net_profit,
            risk_factor,
            strategy: "cyclic_3leg",
            tier: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Canonical pair key: smaller pubkey first.
fn canonical_pair(a: &Pubkey, b: &Pubkey) -> (Pubkey, Pubkey) {
    if a < b { (*a, *b) } else { (*b, *a) }
}

/// Find the best (highest liquidity) pool for a token pair on a specific DEX.
fn best_pool_for_pair(
    index: &HashMap<(DexType, Pubkey, Pubkey), Vec<DexPool>>,
    dex: DexType,
    token_in: &Pubkey,
    token_out: &Pubkey,
) -> Option<DexPool> {
    let (a, b) = canonical_pair(token_in, token_out);
    index.get(&(dex, a, b))?.iter()
        .max_by_key(|p| p.reserve_a.saturating_add(p.reserve_b))
        .cloned()
}

/// Quote a single hop: returns (amount_out, price_impact_pct).
/// Returns None if the quote is zero.
fn quote_hop(pool: &DexPool, token_in: Pubkey, amount_in: u64) -> Option<(u64, f64)> {
    let (amount_out, reserve_in) = if pool.token_a == token_in {
        (pool.quote_a_to_b(amount_in), pool.reserve_a)
    } else if pool.token_b == token_in {
        (pool.quote_b_to_a(amount_in), pool.reserve_b)
    } else {
        return None;
    };

    if amount_out == 0 {
        return None;
    }

    // Reserve must be >2x input for sane quoting.
    if reserve_in < amount_in.saturating_mul(2) {
        return None;
    }

    let impact = amount_in as f64 / reserve_in.max(1) as f64 * 100.0;
    Some((amount_out, impact))
}
