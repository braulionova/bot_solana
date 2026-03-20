// backrun_calculator.rs — Calculate exact backrun profit ceiling from swap parameters.
//
// When a user submits a swap with amount_in and minimum_amount_out, the
// difference between the actual output and minimum_output is the "slippage
// tolerance". This defines the MAXIMUM profit a backrunner can extract.
//
// profit_ceiling = actual_output - minimum_output
//
// If the user sets tight slippage (0.5%), backrun profit is capped at 0.5%.
// If the user sets loose slippage (5%), backrun profit can be up to 5%.

use crate::pool_state::DexPool;
use crate::swap_decoder::DetectedSwap;
use crate::fee_tiers::pool_fee_bps;

/// Result of backrun profitability analysis.
#[derive(Debug, Clone)]
pub struct BackrunAnalysis {
    /// Maximum extractable profit in output token units.
    pub profit_ceiling: u64,
    /// The user's actual slippage tolerance in bps.
    pub slippage_bps: u64,
    /// Post-swap reserves (after the user's swap executes).
    pub post_reserve_a: u64,
    pub post_reserve_b: u64,
    /// Recommended backrun amount (to extract ~50% of available profit).
    pub recommended_amount: u64,
    /// Whether this swap is worth backrunning.
    pub is_profitable: bool,
}

/// Analyze a detected swap for backrun profitability.
pub fn analyze_backrun(swap: &DetectedSwap, pool: &DexPool) -> BackrunAnalysis {
    let (ra, rb) = pool.effective_reserves();
    if ra == 0 || rb == 0 || swap.amount_in == 0 {
        return BackrunAnalysis {
            profit_ceiling: 0,
            slippage_bps: 0,
            post_reserve_a: ra,
            post_reserve_b: rb,
            recommended_amount: 0,
            is_profitable: false,
        };
    }

    let fee_bps = pool_fee_bps(pool);
    let fee_mult = (10_000 - fee_bps) as u128;

    // Calculate actual output using XY=K.
    let (actual_out, post_ra, post_rb) = if swap.a_to_b {
        let amount_after_fee = swap.amount_in as u128 * fee_mult / 10_000;
        let out = (rb as u128 * amount_after_fee) / (ra as u128 + amount_after_fee);
        let new_ra = ra + swap.amount_in;
        let new_rb = rb.saturating_sub(out as u64);
        (out as u64, new_ra, new_rb)
    } else {
        let amount_after_fee = swap.amount_in as u128 * fee_mult / 10_000;
        let out = (ra as u128 * amount_after_fee) / (rb as u128 + amount_after_fee);
        let new_ra = ra.saturating_sub(out as u64);
        let new_rb = rb + swap.amount_in;
        (out as u64, new_ra, new_rb)
    };

    // Profit ceiling = actual_output - minimum_output.
    let profit_ceiling = if swap.amount_out_min > 0 && actual_out > swap.amount_out_min {
        actual_out - swap.amount_out_min
    } else {
        0
    };

    // Slippage tolerance in bps.
    let slippage_bps = if swap.amount_out_min > 0 {
        ((actual_out as f64 / swap.amount_out_min as f64 - 1.0) * 10_000.0) as u64
    } else {
        0
    };

    // Recommended backrun amount: extract ~50% of available profit.
    // Use sqrt(profit_ceiling * reserve) as optimal amount (geometric mean).
    let recommended = if profit_ceiling > 0 {
        let reserve = if swap.a_to_b { post_rb } else { post_ra };
        ((profit_ceiling as f64 * reserve as f64).sqrt() * 0.5) as u64
    } else {
        0
    };

    // Profitable if: profit > 2x combined fees (our fee + gas + tip).
    let min_profit = 20_000u64; // 0.00002 SOL minimum
    let is_profitable = profit_ceiling > min_profit && slippage_bps > 10;

    BackrunAnalysis {
        profit_ceiling,
        slippage_bps,
        post_reserve_a: post_ra,
        post_reserve_b: post_rb,
        recommended_amount: recommended.min(profit_ceiling),
        is_profitable,
    }
}
