//! strategies.rs – E4 (Liquidity Event Arb) and E5 (Stale Oracle Exploit).
//!
//! These strategies complement E1/E2/E3 with event-driven edge detection.

use std::sync::Arc;
use std::time::Instant;

use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use tracing::{debug, info};

use crate::pool_state::{DexPool, PoolStateCache};
use crate::swap_decoder::{decode_swaps, detect_graduation, DetectedSwap, PUMPSWAP_AMM};
use crate::types::{DexType, Hop, RouteParams};

// ---------------------------------------------------------------------------
// E4: Liquidity Event Arbitrage
// ---------------------------------------------------------------------------

/// LP add/remove discriminators (Raydium V4).
const RAYDIUM_ADD_LIQUIDITY_DISC: u8 = 3;
const RAYDIUM_REMOVE_LIQUIDITY_DISC: u8 = 4;

/// Orca Whirlpool increase/decrease liquidity discriminators.
const ORCA_INCREASE_LIQ_DISC: [u8; 8] = [46, 156, 243, 118, 13, 205, 251, 178];
const ORCA_DECREASE_LIQ_DISC: [u8; 8] = [160, 38, 208, 111, 104, 91, 202, 187];

/// Minimum price divergence (bps) to attempt E4 arb.
const E4_MIN_DIVERGENCE_BPS: i64 = 50; // 0.5 %

#[derive(Debug, Clone)]
pub enum LiquidityEventKind {
    Add,
    Remove,
    ClmmRangeCross,
}

#[derive(Debug, Clone)]
pub struct LiquidityEvent {
    pub pool: Pubkey,
    pub kind: LiquidityEventKind,
    pub slot: u64,
}

/// Detect liquidity events in a transaction.
pub fn detect_liquidity_event(tx: &VersionedTransaction, slot: u64) -> Option<LiquidityEvent> {
    use solana_sdk::message::VersionedMessage;

    let accounts = match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.clone(),
        VersionedMessage::V0(m) => m.account_keys.clone(),
    };

    let instructions: Vec<(usize, Vec<u8>, Vec<u8>)> = match &tx.message {
        VersionedMessage::Legacy(m) => m
            .instructions
            .iter()
            .map(|ix| {
                (
                    ix.program_id_index as usize,
                    ix.data.clone(),
                    ix.accounts.clone(),
                )
            })
            .collect(),
        VersionedMessage::V0(m) => m
            .instructions
            .iter()
            .map(|ix| {
                (
                    ix.program_id_index as usize,
                    ix.data.clone(),
                    ix.accounts.clone(),
                )
            })
            .collect(),
    };

    for (prog_idx, data, accs) in &instructions {
        let Some(&prog) = accounts.get(*prog_idx) else {
            continue;
        };
        let prog_str = prog.to_string();
        let pool_pk = accounts.get(*accs.first()? as usize).copied()?;

        match prog_str.as_str() {
            // Raydium V4 add/remove liquidity
            "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8" if !data.is_empty() => {
                let kind = match data[0] {
                    RAYDIUM_ADD_LIQUIDITY_DISC => LiquidityEventKind::Add,
                    RAYDIUM_REMOVE_LIQUIDITY_DISC => LiquidityEventKind::Remove,
                    _ => continue,
                };
                return Some(LiquidityEvent {
                    pool: pool_pk,
                    kind,
                    slot,
                });
            }
            // Orca Whirlpool increase/decrease liquidity
            "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc" if data.len() >= 8 => {
                let kind = if data[..8] == ORCA_INCREASE_LIQ_DISC {
                    LiquidityEventKind::Add
                } else if data[..8] == ORCA_DECREASE_LIQ_DISC {
                    LiquidityEventKind::Remove
                } else {
                    continue;
                };
                return Some(LiquidityEvent {
                    pool: pool_pk,
                    kind,
                    slot,
                });
            }
            _ => continue,
        }
    }
    None
}

/// Given a liquidity event on pool A, find the best cross-DEX route that
/// exploits the temporary price divergence.
pub fn e4_opportunity(
    event: &LiquidityEvent,
    pool_cache: &Arc<PoolStateCache>,
    borrow_amount: u64,
) -> Option<RouteParams> {
    let affected_pool = pool_cache.get(&event.pool)?;

    // Find a parallel pool with the same token pair on a different DEX.
    let same_pair_pools: Vec<DexPool> = pool_cache
        .inner
        .iter()
        .filter(|e| {
            e.pool_address != affected_pool.pool_address
                && e.dex_type != affected_pool.dex_type
                && ((e.token_a == affected_pool.token_a && e.token_b == affected_pool.token_b)
                    || (e.token_a == affected_pool.token_b && e.token_b == affected_pool.token_a))
        })
        .map(|e| e.clone())
        .collect();

    for alt_pool in same_pair_pools {
        let price_a = rate(&affected_pool, true);
        let price_b = rate(&alt_pool, true);

        if price_a == 0.0 || price_b == 0.0 {
            continue;
        }

        let divergence_bps = ((price_b / price_a - 1.0).abs() * 10_000.0) as i64;
        if divergence_bps < E4_MIN_DIVERGENCE_BPS {
            continue;
        }

        // Buy cheap, sell expensive.
        let (buy_pool, sell_pool) = if price_a < price_b {
            (&affected_pool, &alt_pool)
        } else {
            (&alt_pool, &affected_pool)
        };

        let mid = buy_pool.quote_a_to_b(borrow_amount);
        let out = sell_pool.quote_b_to_a(mid);

        if out <= borrow_amount {
            continue;
        }
        let gross_profit = out - borrow_amount;
        let net_profit = gross_profit as i64 - 15_000; // approx fees

        if net_profit <= 0 {
            continue;
        }

        info!(
            event_pool    = %event.pool,
            alt_pool      = %alt_pool.pool_address,
            divergence_bps,
            gross_profit,
            "[E4] liquidity event arb found"
        );

        return Some(RouteParams {
            hops: vec![
                Hop {
                    pool: buy_pool.pool_address,
                    dex_program: dex_program_id(&buy_pool.dex_type),
                    token_in: buy_pool.token_a,
                    token_out: buy_pool.token_b,
                    amount_out: mid,
                    price_impact: (borrow_amount as f64 / buy_pool.reserve_a.max(1) as f64) * 100.0,
                },
                Hop {
                    pool: sell_pool.pool_address,
                    dex_program: dex_program_id(&sell_pool.dex_type),
                    token_in: sell_pool.token_b,
                    token_out: sell_pool.token_a,
                    amount_out: out,
                    price_impact: (mid as f64 / sell_pool.reserve_b.max(1) as f64) * 100.0,
                },
            ],
            borrow_amount,
            gross_profit,
            net_profit,
            risk_factor: 0.3,
            strategy: "liquidity-event",
            tier: 0,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// E5: Stale Oracle Exploitation
// ---------------------------------------------------------------------------

/// Oracle lag threshold: spot must diverge from oracle by at least this many bps.
const E5_MIN_ORACLE_LAG_BPS: i64 = 30; // 0.3 %

/// Known Pyth price accounts for major pairs.
/// These are the Pyth price feed accounts (not oracle programs).
mod pyth_feeds {
    pub const SOL_USD: &str = "H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG";
    pub const BTC_USD: &str = "GVXRSBjFk6e909Wjy7ChVanTjuaS75yfCatZ2CL41ukM";
    pub const ETH_USD: &str = "JBu1AL4obBcCMqKBBxhpWCNUt136ijcuMZLFvTP7iWdB";
    pub const BONK_USD: &str = "8ihFLu5FimgTQ1Unh4dVyEHUGodJ738bWMDuGDMMMMnt";
}

#[derive(Debug, Clone)]
pub struct OracleLagOpportunity {
    pub pool: Pubkey,
    pub spot_price: f64,
    pub oracle_price: f64,
    pub lag_bps: i64,
    pub direction: bool, // true = buy (spot < oracle), false = sell
}

/// Check if the spot price of a pool diverges from a cached oracle price.
/// `oracle_prices` maps token_mint → USD price (from last oracle fetch).
pub fn detect_oracle_lag(
    pool: &DexPool,
    oracle_prices: &std::collections::HashMap<Pubkey, f64>,
) -> Option<OracleLagOpportunity> {
    if pool.reserve_a == 0 || pool.reserve_b == 0 {
        return None;
    }

    // Compute spot price: token_b per token_a.
    let spot = pool.reserve_b as f64 / pool.reserve_a as f64;

    // Look up oracle price for token_a in terms of token_b (both in USD ideally).
    let oracle_a = oracle_prices.get(&pool.token_a)?;
    let oracle_b = oracle_prices.get(&pool.token_b)?;
    if *oracle_b == 0.0 {
        return None;
    }
    let oracle_price = oracle_a / oracle_b;

    let lag_bps = ((spot / oracle_price - 1.0) * 10_000.0) as i64;
    if lag_bps.abs() < E5_MIN_ORACLE_LAG_BPS {
        return None;
    }

    debug!(
        pool = %pool.pool_address,
        spot,
        oracle_price,
        lag_bps,
        "[E5] oracle lag detected"
    );

    Some(OracleLagOpportunity {
        pool: pool.pool_address,
        spot_price: spot,
        oracle_price,
        lag_bps,
        direction: spot < oracle_price, // true=buy cheap spot vs oracle
    })
}

/// Build a two-hop route exploiting oracle lag: buy on lagging pool, sell on spot-accurate pool.
pub fn e5_opportunity(
    lag: &OracleLagOpportunity,
    pool_cache: &Arc<PoolStateCache>,
    borrow_amount: u64,
) -> Option<RouteParams> {
    let lagging_pool = pool_cache.get(&lag.pool)?;

    // Find a reference pool with the same pair (preferably higher liquidity).
    let ref_pool = pool_cache
        .inner
        .iter()
        .filter(|e| {
            e.pool_address != lagging_pool.pool_address
                && e.token_a == lagging_pool.token_a
                && e.token_b == lagging_pool.token_b
                && e.reserve_a > lagging_pool.reserve_a // higher liquidity = more accurate
        })
        .map(|e| e.clone())
        .max_by_key(|p| p.reserve_a)?;

    let (buy_pool, sell_pool) = if lag.direction {
        (&lagging_pool, &ref_pool)
    } else {
        (&ref_pool, &lagging_pool)
    };

    let mid = buy_pool.quote_a_to_b(borrow_amount);
    let out = sell_pool.quote_b_to_a(mid);
    if out <= borrow_amount {
        return None;
    }

    let gross_profit = out - borrow_amount;
    let net_profit = gross_profit as i64 - 10_000;
    if net_profit <= 0 {
        return None;
    }

    info!(
        lagging_pool  = %lag.pool,
        lag_bps       = lag.lag_bps,
        gross_profit,
        "[E5] stale oracle arb found"
    );

    Some(RouteParams {
        hops: vec![
            Hop {
                pool: buy_pool.pool_address,
                dex_program: dex_program_id(&buy_pool.dex_type),
                token_in: buy_pool.token_a,
                token_out: buy_pool.token_b,
                amount_out: mid,
                price_impact: (borrow_amount as f64 / buy_pool.reserve_a.max(1) as f64) * 100.0,
            },
            Hop {
                pool: sell_pool.pool_address,
                dex_program: dex_program_id(&sell_pool.dex_type),
                token_in: sell_pool.token_b,
                token_out: sell_pool.token_a,
                amount_out: out,
                price_impact: (mid as f64 / sell_pool.reserve_b.max(1) as f64) * 100.0,
            },
        ],
        borrow_amount,
        gross_profit,
        net_profit,
        risk_factor: 0.2,
        strategy: "stale-oracle",
        tier: 0,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rate(pool: &DexPool, a_to_b: bool) -> f64 {
    if pool.reserve_a == 0 || pool.reserve_b == 0 {
        return 0.0;
    }
    if a_to_b {
        pool.reserve_b as f64 / pool.reserve_a as f64
    } else {
        pool.reserve_a as f64 / pool.reserve_b as f64
    }
}

fn dex_program_id(dex: &DexType) -> Pubkey {
    use crate::types::dex_programs;
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
