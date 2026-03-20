// fee_tiers.rs — Accurate per-pool fee calculation.
//
// Different DEXes and pools charge different fees:
// - Raydium AMM V4: 25bps (0.25%) — fixed
// - Raydium CLMM: varies (1, 4, 25, 100 bps) — from pool config
// - Orca Whirlpool: varies (1, 4, 8, 30, 65, 100, 200 bps)
// - Meteora DLMM: dynamic (base_fee + variable_fee based on volatility)
// - PumpSwap: 25bps — fixed
//
// Using incorrect fees leads to phantom profit calculations.

use crate::pool_state::DexPool;
use crate::types::DexType;

/// Get the actual fee in bps for a pool.
/// Uses pool metadata when available, falls back to DEX defaults.
#[inline]
pub fn pool_fee_bps(pool: &DexPool) -> u64 {
    // If pool has a non-default fee_bps set, use it.
    if pool.fee_bps > 0 && pool.fee_bps != 25 {
        return pool.fee_bps;
    }

    match pool.dex_type {
        DexType::RaydiumAmmV4 => 25,
        DexType::RaydiumClmm => {
            // CLMM fee depends on tick_spacing:
            // tick_spacing=1 → 1bps, tick_spacing=10 → 4bps,
            // tick_spacing=60 → 25bps, tick_spacing=120 → 100bps
            if let Some(meta) = &pool.orca_meta {
                match meta.tick_spacing {
                    1 => 1,
                    2 => 2,
                    4 | 8 | 10 => 4,
                    20 | 60 => 25,
                    120 | 200 => 100,
                    _ => 25, // default
                }
            } else {
                25
            }
        }
        DexType::OrcaWhirlpool => {
            // Orca fee from tick_spacing (standard mapping)
            if let Some(meta) = &pool.orca_meta {
                match meta.tick_spacing {
                    1 => 1,
                    2 => 2,
                    4 => 4,
                    8 => 8,
                    16 | 32 => 30,
                    64 => 65,
                    96 | 128 => 100,
                    256 => 200,
                    _ => pool.fee_bps.max(30), // conservative default
                }
            } else {
                30 // Orca default
            }
        }
        DexType::MeteoraDlmm => {
            // Meteora has dynamic fees. Use bin_step as approximation:
            // bin_step=1 → ~1bps, bin_step=5 → ~5bps, bin_step=25 → ~25bps
            if let Some(meta) = &pool.meteora_meta {
                (meta.bin_step as u64).max(1)
            } else {
                25
            }
        }
        DexType::PumpSwap => 25,
        DexType::RaydiumCpmm => 25,
        _ => 25,
    }
}

/// Combined fee for a multi-hop route (sum of all hop fees).
pub fn route_total_fee_bps(pools: &[&DexPool]) -> u64 {
    pools.iter().map(|p| pool_fee_bps(p)).sum()
}
