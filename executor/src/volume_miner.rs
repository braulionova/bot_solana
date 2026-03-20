// volume_miner.rs — PumpSwap volume mining via self-trade for PUMP token rewards.
//
// PumpSwap distributes PUMP token rewards to traders proportional to their
// volume. By self-trading (buy + sell same token on same pool), we accumulate
// volume and earn PUMP rewards. Profitable if PUMP reward value > trading fees.
//
// Cost: 50bps round-trip (25bps per swap).
// Revenue: PUMP token rewards proportional to volume share.

use solana_sdk::pubkey::Pubkey;
use tracing::info;

/// PumpSwap AMM program.
pub const PUMPSWAP_PROGRAM: Pubkey = solana_sdk::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");

/// WSOL mint.
pub const WSOL: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");

/// Configuration for volume mining.
#[derive(Clone)]
pub struct VolumeMinerConfig {
    /// Amount per round-trip in lamports (e.g., 100_000_000 = 0.1 SOL).
    pub amount_per_trade: u64,
    /// Interval between round-trips in milliseconds.
    pub interval_ms: u64,
    /// Maximum SOL to spend on fees per day.
    pub max_daily_fee_sol: f64,
    /// PumpSwap pool to self-trade on (high liquidity, low impact).
    pub target_pool: Pubkey,
    /// Whether volume mining is enabled.
    pub enabled: bool,
}

impl Default for VolumeMinerConfig {
    fn default() -> Self {
        Self {
            amount_per_trade: 100_000_000, // 0.1 SOL
            interval_ms: 10_000,           // every 10 seconds
            max_daily_fee_sol: 0.05,       // max 0.05 SOL/day in fees
            target_pool: Pubkey::default(),
            enabled: false,
        }
    }
}

/// Check if volume mining is economically viable.
pub fn check_viability(pump_token_price_sol: f64, reward_rate_per_sol_volume: f64) -> bool {
    let fee_cost_per_sol = 0.005; // 50bps round-trip
    let reward_per_sol = reward_rate_per_sol_volume * pump_token_price_sol;

    let viable = reward_per_sol > fee_cost_per_sol;

    info!(
        pump_price = pump_token_price_sol,
        reward_rate = reward_rate_per_sol_volume,
        reward_value = reward_per_sol,
        fee_cost = fee_cost_per_sol,
        viable,
        "volume-miner: viability check"
    );

    viable
}

/// Calculate daily P&L for given volume.
pub fn daily_pnl(
    daily_volume_sol: f64,
    pump_token_price_sol: f64,
    reward_rate_per_sol_volume: f64,
) -> f64 {
    let fees = daily_volume_sol * 0.005;
    let rewards = daily_volume_sol * reward_rate_per_sol_volume * pump_token_price_sol;
    rewards - fees
}
