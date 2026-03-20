// scoring.rs – Opportunity scoring.
//
// Formula (from spec):
//   score = (expected_profit / cost) * confidence * urgency * (1 - risk_factor)
//
// Where:
//   expected_profit = route.net_profit (lamports)
//   cost            = flash loan fee + estimated tx fee
//   confidence      = function of data freshness and route hop count
//   urgency         = exponential decay from detected_at timestamp
//   risk_factor     = route.risk_factor (0 = low, 1 = high)

use std::time::{Duration, Instant};
use chrono::Timelike;

use crate::types::{FlashProvider, OpportunityBundle, RouteParams};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Signals within this window get maximum urgency (no penalty).
const URGENCY_BOOST_WINDOW_MS: f64 = 200.0;

/// After the boost window, urgency decays to ~37% (1/e) per this half-life.
const URGENCY_DECAY_HALF_LIFE_MS: f64 = 1000.0;

/// Maximum flash loan fee to consider viable (lamports).
const MAX_FLASH_FEE_LAMPORTS: u64 = 1_000_000;

/// Base transaction fee (lamports, used when no CU price is set).
const BASE_TX_FEE: u64 = 5_000;

// ---------------------------------------------------------------------------
// Scorer
// ---------------------------------------------------------------------------

pub struct Scorer;

impl Scorer {
    pub fn new() -> Self {
        Self
    }

    /// Score an opportunity bundle.  Higher = more attractive.
    pub fn score(&self, bundle: &OpportunityBundle) -> f64 {
        let route = &bundle.route;
        let flash_fee = flash_loan_fee(bundle.flash_provider, route.borrow_amount);
        let total_cost = flash_fee + BASE_TX_FEE * route.hops.len() as u64;

        if total_cost == 0 || route.net_profit <= 0 {
            return 0.0;
        }

        let expected_profit = route.net_profit as f64;
        let cost = total_cost as f64;
        let confidence = compute_confidence(route);
        let urgency = compute_urgency(bundle.detected_at);
        let risk = route.risk_factor.clamp(0.0, 1.0);

        let time_boost = time_of_day_boost();
        let score = (expected_profit / cost) * confidence * urgency * (1.0 - risk) * time_boost;
        score.max(0.0)
    }

    /// Return `true` if the opportunity meets the minimum bar to send.
    pub fn is_viable(&self, bundle: &OpportunityBundle) -> bool {
        bundle.route.net_profit > 0 && self.score(bundle) > 0.1
    }
}

impl Default for Scorer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Compute the flash loan fee for `borrow_amount` lamports.
fn flash_loan_fee(provider: FlashProvider, borrow_amount: u64) -> u64 {
    let fee_bps = provider.fee_bps();
    (borrow_amount as u128 * fee_bps as u128 / 10_000) as u64
}

/// Confidence decreases with more hops and high price impact, but gets a
/// bonus when multiple distinct DEXes are involved (more robust arb).
fn compute_confidence(route: &RouteParams) -> f64 {
    let hop_penalty = match route.hops.len() {
        0 | 1 => 1.0,
        2 => 0.98,
        3 => 0.90,
        _ => 0.70,
    };

    // DEX diversity bonus: routes across different DEXes are more robust.
    let unique_dexes: std::collections::HashSet<_> = route.hops.iter()
        .map(|h| h.dex_program)
        .collect();
    let diversity = match unique_dexes.len() {
        0 | 1 => 0.95,  // single DEX → concentration risk
        2     => 1.0,    // two DEXes → healthy
        _     => 1.05,   // 3+ DEXes → robustness bonus
    };

    let max_impact = route
        .hops
        .iter()
        .map(|h| h.price_impact)
        .fold(0.0_f64, f64::max);

    // Confidence drops with price impact (capped at 30% degradation).
    let impact_factor = 1.0 - (max_impact / 100.0).min(0.3);

    // Strategy-specific confidence boost.
    let strategy_boost = match route.strategy {
        "hot-route"     => 2.50, // ML-validated low-competition route from PG
        "whale-backrun" => 1.10, // whale TX confirms the price dislocation
        "lst-arb"       => 1.15, // anchored staking rate = reliable spread
        _               => 1.0,
    };

    // Multi-hop bonus: 3-hop Meteora routes have lowest competition (model trained: score 1.64, comp 0.62)
    let hop_bonus = if route.hops.len() >= 3 { 1.3 } else { 1.0 };

    hop_penalty * diversity * impact_factor * strategy_boost * hop_bonus
}

/// Time-of-day multiplier based on ML model (trained from 274K samples).
/// Model data: 4 confirmed TXs all at hours 13-14 UTC.
/// Peak MEV: 13-14 UTC. Secondary: 12, 15-16. Off-peak: mostly phantom.
fn time_of_day_boost() -> f64 {
    let hour = chrono::Utc::now().hour();
    match hour {
        13..=14 => 2.0,  // ML confirmed: all 4 landed TXs were at 13-14 UTC
        12 | 15 => 1.5,
        10..=11 | 16..=18 => 1.2,
        8..=9 | 19..=21 => 1.0,
        _ => 0.5,         // off-peak: reduce to avoid phantom routes
    }
}

/// Urgency: full boost during the first 200ms, then exponential decay.
fn compute_urgency(detected_at: Instant) -> f64 {
    let elapsed_ms = detected_at.elapsed().as_millis() as f64;
    if elapsed_ms < URGENCY_BOOST_WINDOW_MS {
        1.0
    } else {
        let t = (elapsed_ms - URGENCY_BOOST_WINDOW_MS) / URGENCY_DECAY_HALF_LIFE_MS;
        (-t).exp()
    }
}
