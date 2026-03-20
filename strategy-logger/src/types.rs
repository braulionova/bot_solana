//! Log event types — all data needed for ML training.

use chrono::{DateTime, Utc};
use std::time::Instant;

/// Convert an Instant to wall-clock UTC (approximate, ms-level accuracy).
pub fn instant_to_utc(instant: Instant) -> DateTime<Utc> {
    let elapsed = instant.elapsed();
    Utc::now() - chrono::Duration::from_std(elapsed).unwrap_or_default()
}

/// Top-level event enum sent through the channel.
pub enum LogEvent {
    Opportunity(LogOpportunity),
    Execution(LogExecution),
    ExecutionUpdate(LogExecutionUpdate),
    Sniper(LogSniperEvent),
    SniperSellUpdate(LogSniperSellUpdate),
    PoolSnapshots(Vec<LogPoolSnapshot>),
    PoolPerfUpdate(LogPoolPerfUpdate),
    RouteCandidate(LogRouteCandidate),
    PoolDiscovery(LogPoolDiscovery),
    WalletSwap(LogWalletSwap),
    WalletUpdate(LogWalletUpdate),
    PatternUpdate(LogPatternUpdate),
}

// ---------------------------------------------------------------------------
// Opportunity
// ---------------------------------------------------------------------------

pub struct LogOpportunity {
    pub opp_id: u64,
    pub slot: u64,
    pub detected_at: DateTime<Utc>,
    pub signal_type: String,
    pub source_strategy: String,
    pub token_mints: Vec<String>,
    pub borrow_amount: u64,
    pub flash_provider: String,
    pub gross_profit: u64,
    pub net_profit: i64,
    pub score: f64,
    pub risk_factor: f64,
    pub n_hops: i16,
    pub fast_arb: bool,
    pub hops: Vec<LogHop>,
}

pub struct LogHop {
    pub hop_index: i16,
    pub pool: String,
    pub dex_type: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: Option<u64>,
    pub amount_out: u64,
    pub price_impact: f64,
    pub pool_reserve_a: Option<u64>,
    pub pool_reserve_b: Option<u64>,
    pub reserve_age_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

pub struct LogExecution {
    pub opp_id: u64,
    pub tx_signature: Option<String>,
    pub send_channels: Vec<String>,
    pub fast_arb: bool,
    pub detect_to_build_us: Option<u64>,
    pub build_to_send_us: Option<u64>,
    pub total_us: Option<u64>,
    pub jito_tip: Option<u64>,
    pub tpu_leaders: Option<i16>,
}

pub struct LogExecutionUpdate {
    pub tx_signature: String,
    pub landed: bool,
    pub landed_slot: Option<u64>,
    pub actual_profit: Option<i64>,
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Sniper
// ---------------------------------------------------------------------------

pub struct LogSniperEvent {
    pub slot: u64,
    pub token_mint: String,
    pub pool: String,
    pub source: String,
    pub safety_passed: bool,
    pub safety_reason: Option<String>,
    pub pool_sol_reserve: Option<u64>,
    pub snipe_lamports: u64,
    pub tx_signature: Option<String>,
    pub build_ms: Option<u64>,
    pub send_ms: Option<u64>,
}

pub struct LogSniperSellUpdate {
    pub token_mint: String,
    pub sell_triggered: bool,
    pub sell_reason: Option<String>,
    pub sell_tx_sig: Option<String>,
    pub sell_sol_received: Option<u64>,
    pub hold_duration_ms: Option<u64>,
    pub pnl_lamports: Option<i64>,
}

// ---------------------------------------------------------------------------
// Pool Snapshots
// ---------------------------------------------------------------------------

pub struct LogPoolSnapshot {
    pub pool_address: String,
    pub dex_type: String,
    pub token_a: String,
    pub token_b: String,
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub fee_bps: i16,
}

// ---------------------------------------------------------------------------
// Pool Performance (for ML model)
// ---------------------------------------------------------------------------

pub struct LogPoolPerfUpdate {
    pub pool_address: String,
    pub dex_type: String,
    pub landed: bool,
    pub profit: i64,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Route Candidates (ALL routes for ML training — including rejected)
// ---------------------------------------------------------------------------

pub struct LogRouteCandidate {
    pub slot: u64,
    pub detected_at: DateTime<Utc>,
    pub strategy: String,
    pub borrow_amount: u64,
    pub n_hops: i16,
    pub gross_profit: u64,
    pub net_profit: i64,
    pub score: f64,
    pub ml_boost: f64,
    pub risk_factor: f64,
    pub emitted: bool,
    pub reject_reason: Option<String>,
    pub pools: Vec<String>,
    pub token_mints: Vec<String>,
    pub reserves_a: Vec<i64>,
    pub reserves_b: Vec<i64>,
}

// ---------------------------------------------------------------------------
// Pool Discovery (new coins/pools detected)
// ---------------------------------------------------------------------------

pub struct LogPoolDiscovery {
    pub pool_address: String,
    pub dex_type: String,
    pub token_a: String,
    pub token_b: String,
    pub initial_reserve_a: Option<u64>,
    pub initial_reserve_b: Option<u64>,
    pub discovery_source: String,
    pub discovery_slot: Option<u64>,
}

// ---------------------------------------------------------------------------
// Wallet Tracking (smart money analysis for ML)
// ---------------------------------------------------------------------------

pub struct LogWalletSwap {
    pub wallet_address: String,
    pub tx_signature: Option<String>,
    pub slot: u64,
    pub dex_type: String,
    pub pool: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: u64,
    pub amount_out: u64,
    pub is_buy: bool,
    pub price_impact_bps: Option<f64>,
    pub pool_reserve_a: Option<u64>,
    pub pool_reserve_b: Option<u64>,
}

pub struct LogWalletUpdate {
    pub wallet_address: String,
    pub label: String,
    pub category: String,
    pub total_swaps: u64,
    pub total_profit_lamports: i64,
    pub win_rate: f64,
    pub last_swap_slot: Option<u64>,
}

pub struct LogPatternUpdate {
    pub pattern_type: String,
    pub wallet_address: Option<String>,
    pub token_mint: Option<String>,
    pub pool: Option<String>,
    pub dex_type: Option<String>,
    pub occurrences: u64,
    pub success_count: u64,
    pub avg_profit: f64,
    pub avg_delay_ms: Option<u64>,
    pub confidence: f64,
    pub features: serde_json::Value,
}
