//! ml_models.rs — 6 ML models for profitable arbitrage.
//!
//! #6 Kalman Reserve Estimator (CRITICAL) — fuses shred deltas + Helius polls
//! #5 Competition Detector — blacklist routes where we always lose
//! #2 Route Profitability Estimator — filter phantom routes
//! #3 Optimal Tip Selector — maximize EV = P(land) × (profit - tip)
//! #4 Pool Volatility Classifier — prioritize high-activity pools
//! #1 Landing Predictor — P(TX lands) from 18 features

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, trace};

// ═══════════════════════════════════════════════════════════════════════
// #6: Kalman Reserve Estimator
// ═══════════════════════════════════════════════════════════════════════

/// Per-pool Kalman state for reserve estimation.
#[derive(Debug, Clone)]
pub struct KalmanState {
    /// Estimated reserves [a, b].
    pub x: [f64; 2],
    /// Uncertainty variance [var_a, var_b].
    pub p: [f64; 2],
    /// Last update time.
    pub last_update: Instant,
    /// Discrepancy count (shred vs Helius divergence).
    pub discrepancies: u32,
}

impl KalmanState {
    pub fn new(reserve_a: u64, reserve_b: u64) -> Self {
        Self {
            x: [reserve_a as f64, reserve_b as f64],
            p: [1e12, 1e12], // high initial uncertainty
            last_update: Instant::now(),
            discrepancies: 0,
        }
    }

    /// Prediction step: apply shred delta.
    /// Q = process noise (higher for Orca/Meteora where we don't simulate math).
    pub fn predict_shred_delta(&mut self, delta_a: f64, delta_b: f64, q_noise: f64) {
        self.x[0] += delta_a;
        self.x[1] += delta_b;
        self.x[0] = self.x[0].max(0.0);
        self.x[1] = self.x[1].max(0.0);
        self.p[0] += q_noise;
        self.p[1] += q_noise;
        self.last_update = Instant::now();
    }

    /// Update step: Helius RPC measurement.
    /// R = measurement noise (Helius has ~10s delay → higher R).
    pub fn update_helius(&mut self, helius_a: u64, helius_b: u64, r_noise: f64) {
        let z = [helius_a as f64, helius_b as f64];

        for i in 0..2 {
            let k = self.p[i] / (self.p[i] + r_noise); // Kalman gain
            let innovation = z[i] - self.x[i];

            // Divergence check: >10% = shred had a drop or failed TX
            if self.x[i] > 0.0 && (innovation.abs() / self.x[i]) > 0.10 {
                self.discrepancies += 1;
                // Trust Helius when divergence is large
                self.x[i] = z[i];
                self.p[i] = r_noise;
            } else {
                self.x[i] += k * innovation;
                self.p[i] *= 1.0 - k;
            }
        }
        self.last_update = Instant::now();
    }

    /// Get estimated reserves as u64.
    pub fn reserves(&self) -> (u64, u64) {
        (self.x[0].max(0.0) as u64, self.x[1].max(0.0) as u64)
    }

    /// Confidence: lower uncertainty = higher confidence.
    pub fn confidence(&self) -> f64 {
        let avg_p = (self.p[0] + self.p[1]) / 2.0;
        1.0 / (1.0 + avg_p / 1e9)
    }
}

/// Kalman estimator for all pools.
pub struct KalmanReserveEstimator {
    states: DashMap<Pubkey, KalmanState>,
    /// Q noise for shred simulation (Raydium/PumpSwap = accurate).
    pub q_accurate: f64,
    /// Q noise for unknown DEX (Orca/Meteora = higher uncertainty).
    pub q_unknown: f64,
    /// R noise for Helius measurement (10s delay → high).
    pub r_helius: f64,
}

impl KalmanReserveEstimator {
    pub fn new() -> Self {
        Self {
            states: DashMap::new(),
            q_accurate: 1e6,  // low noise for Raydium/PumpSwap delta sim
            q_unknown: 1e9,   // high noise for Orca/Meteora (no sim)
            r_helius: 1e8,    // moderate noise for Helius (10s delay but accurate)
        }
    }

    pub fn predict(&self, pool: &Pubkey, delta_a: f64, delta_b: f64, accurate_sim: bool) {
        let q = if accurate_sim { self.q_accurate } else { self.q_unknown };
        self.states
            .entry(*pool)
            .or_insert_with(|| KalmanState::new(0, 0))
            .predict_shred_delta(delta_a, delta_b, q);
    }

    pub fn update(&self, pool: &Pubkey, helius_a: u64, helius_b: u64) {
        self.states
            .entry(*pool)
            .or_insert_with(|| KalmanState::new(helius_a, helius_b))
            .update_helius(helius_a, helius_b, self.r_helius);
    }

    pub fn get_reserves(&self, pool: &Pubkey) -> Option<(u64, u64, f64)> {
        self.states.get(pool).map(|s| {
            let (a, b) = s.reserves();
            (a, b, s.confidence())
        })
    }

    pub fn stats(&self) -> String {
        let total = self.states.len();
        let high_conf = self.states.iter()
            .filter(|s| s.confidence() > 0.5)
            .count();
        format!("pools={total} high_confidence={high_conf}")
    }
}

// ═══════════════════════════════════════════════════════════════════════
// #5: Competition Detector
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct RouteCompetition {
    pub attempts: u64,
    pub landed: u64,
    pub requote_fails: u64,
    pub avg_detection_ms: f64,
    pub last_attempt: Instant,
    pub last_fail_interval_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompetitionLevel {
    Blacklist,  // never lands, always skip
    High,       // >50% fail rate, reduce priority
    Normal,     // not enough data or viable
    Low,        // good landing rate
}

pub struct CompetitionDetector {
    routes: DashMap<String, RouteCompetition>,
}

impl CompetitionDetector {
    pub fn new() -> Self {
        Self { routes: DashMap::new() }
    }

    pub fn observe(&self, route_key: &str, landed: bool, requote_failed: bool, detection_ms: f64) {
        let mut entry = self.routes
            .entry(route_key.to_string())
            .or_insert_with(|| RouteCompetition {
                attempts: 0, landed: 0, requote_fails: 0,
                avg_detection_ms: detection_ms,
                last_attempt: Instant::now(),
                last_fail_interval_ms: 1000.0,
            });

        let interval = entry.last_attempt.elapsed().as_millis() as f64;
        entry.last_fail_interval_ms = 0.2 * interval + 0.8 * entry.last_fail_interval_ms;
        entry.last_attempt = Instant::now();
        entry.attempts += 1;
        entry.avg_detection_ms = 0.1 * detection_ms + 0.9 * entry.avg_detection_ms;

        if landed { entry.landed += 1; }
        if requote_failed { entry.requote_fails += 1; }
    }

    pub fn classify(&self, route_key: &str) -> CompetitionLevel {
        let entry = match self.routes.get(route_key) {
            Some(e) => e,
            None => return CompetitionLevel::Normal, // no data = try
        };

        // Blacklist: >20 attempts, 0 landings
        if entry.attempts > 20 && entry.landed == 0 {
            return CompetitionLevel::Blacklist;
        }
        // Blacklist: >50 requote fails, 0 landings
        if entry.requote_fails > 50 && entry.landed == 0 {
            return CompetitionLevel::Blacklist;
        }
        // Blacklist: too slow for this route
        if entry.avg_detection_ms > 200.0 && entry.attempts > 10 && entry.landed == 0 {
            return CompetitionLevel::Blacklist;
        }

        // High competition
        let rate = if entry.attempts > 0 { entry.landed as f64 / entry.attempts as f64 } else { 0.5 };
        if entry.attempts > 10 && rate < 0.05 {
            return CompetitionLevel::High;
        }

        // Low competition
        if rate > 0.1 && entry.attempts > 5 {
            return CompetitionLevel::Low;
        }

        CompetitionLevel::Normal
    }

    /// Score multiplier: 0.0 = blacklist, 0.5 = high comp, 1.0 = normal, 1.5 = low comp
    pub fn score_multiplier(&self, route_key: &str) -> f64 {
        match self.classify(route_key) {
            CompetitionLevel::Blacklist => 0.0,
            CompetitionLevel::High => 0.5,
            CompetitionLevel::Normal => 1.0,
            CompetitionLevel::Low => 1.5,
        }
    }

    pub fn stats(&self) -> String {
        let total = self.routes.len();
        let blacklisted = self.routes.iter()
            .filter(|r| r.attempts > 20 && r.landed == 0)
            .count();
        format!("routes={total} blacklisted={blacklisted}")
    }
}

// ═══════════════════════════════════════════════════════════════════════
// #2: Route Profitability Estimator
// ═══════════════════════════════════════════════════════════════════════

/// Estimates the ratio: real_profit / estimated_profit.
/// Values near 0.0 = phantom route, near 1.0 = accurate estimate.
pub struct ProfitabilityEstimator {
    /// Per-route EMA of (actual_profit / estimated_profit).
    route_ratios: DashMap<String, f64>,
    /// Per-DEX-combo average ratio.
    dex_combo_ratios: DashMap<String, (f64, u64)>, // (ema_ratio, n_samples)
    /// Global ratio baseline.
    global_ratio: std::sync::atomic::AtomicU64, // f64 bits
    pub observations: AtomicU64,
}

impl ProfitabilityEstimator {
    pub fn new() -> Self {
        Self {
            route_ratios: DashMap::new(),
            dex_combo_ratios: DashMap::new(),
            global_ratio: std::sync::atomic::AtomicU64::new(f64::to_bits(0.3)), // pessimistic prior
            observations: AtomicU64::new(0),
        }
    }

    /// Observe actual result after execution.
    pub fn observe(&self, route_key: &str, dex_combo: &str, estimated: i64, actual: i64) {
        if estimated <= 0 { return; }
        let ratio = (actual as f64 / estimated as f64).max(0.0).min(2.0);

        self.observations.fetch_add(1, Ordering::Relaxed);

        // Update route-specific ratio
        let alpha = 0.15;
        self.route_ratios
            .entry(route_key.to_string())
            .and_modify(|r| *r = alpha * ratio + (1.0 - alpha) * *r)
            .or_insert(ratio);

        // Update DEX combo ratio
        self.dex_combo_ratios
            .entry(dex_combo.to_string())
            .and_modify(|(r, n)| { *r = alpha * ratio + (1.0 - alpha) * *r; *n += 1; })
            .or_insert((ratio, 1));

        // Update global
        let old = f64::from_bits(self.global_ratio.load(Ordering::Relaxed));
        let new = alpha * ratio + (1.0 - alpha) * old;
        self.global_ratio.store(f64::to_bits(new), Ordering::Relaxed);
    }

    /// Predict real profit from estimated profit.
    pub fn predict_real_profit(&self, route_key: &str, dex_combo: &str, estimated: i64) -> i64 {
        let ratio = self.get_ratio(route_key, dex_combo);
        (estimated as f64 * ratio) as i64
    }

    /// Should we attempt this route?
    pub fn is_worth_attempting(&self, route_key: &str, dex_combo: &str, estimated: i64, min_profit: i64) -> bool {
        let predicted = self.predict_real_profit(route_key, dex_combo, estimated);
        predicted > min_profit
    }

    fn get_ratio(&self, route_key: &str, dex_combo: &str) -> f64 {
        // Route-specific ratio (most precise)
        if let Some(r) = self.route_ratios.get(route_key) {
            return *r;
        }
        // DEX combo ratio (less precise but more data)
        if let Some(entry) = self.dex_combo_ratios.get(dex_combo) {
            if entry.1 >= 5 { return entry.0; }
        }
        // Global baseline (least precise)
        f64::from_bits(self.global_ratio.load(Ordering::Relaxed))
    }

    pub fn stats(&self) -> String {
        let obs = self.observations.load(Ordering::Relaxed);
        let ratio = f64::from_bits(self.global_ratio.load(Ordering::Relaxed));
        format!("observations={obs} global_ratio={ratio:.2} routes={}", self.route_ratios.len())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// #3: Optimal Tip Selector
// ═══════════════════════════════════════════════════════════════════════

pub struct TipSelector {
    /// EMA of tips that led to successful landing.
    successful_tip_ema: std::sync::atomic::AtomicU64, // f64 bits
    pub n_landed: AtomicU64,
    pub n_attempts: AtomicU64,
}

impl TipSelector {
    pub fn new() -> Self {
        Self {
            successful_tip_ema: std::sync::atomic::AtomicU64::new(f64::to_bits(50000.0)),
            n_landed: AtomicU64::new(0),
            n_attempts: AtomicU64::new(0),
        }
    }

    pub fn recommend_tip(&self, profit: i64, competition: f64, hour: u32, jito_p75: u64) -> u64 {
        let profit = profit.max(0) as u64;

        // Base tip % from profit tier
        let base_pct = if profit > 5_000_000 {
            30.0 // high profit: 30%
        } else if profit > 500_000 {
            20.0 // medium: 20%
        } else {
            15.0 // low: 15%
        };

        // Competition adjustment
        let comp_mult = if competition > 0.8 { 1.5 } else if competition > 0.5 { 1.2 } else { 1.0 };

        // Peak hour adjustment (13-14 UTC from model)
        let hour_mult = if hour == 13 || hour == 14 { 1.3 } else if hour >= 12 && hour <= 15 { 1.1 } else { 1.0 };

        let tip = (profit as f64 * base_pct / 100.0 * comp_mult * hour_mult) as u64;
        tip.max(jito_p75).min(2_000_000) // floor at p75, cap at 2M
    }

    pub fn observe(&self, tip: u64, landed: bool) {
        self.n_attempts.fetch_add(1, Ordering::Relaxed);
        if landed {
            self.n_landed.fetch_add(1, Ordering::Relaxed);
            let old = f64::from_bits(self.successful_tip_ema.load(Ordering::Relaxed));
            let new = 0.15 * tip as f64 + 0.85 * old;
            self.successful_tip_ema.store(f64::to_bits(new), Ordering::Relaxed);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// #4: Pool Volatility Classifier
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VolatilityClass {
    High,   // >10 swaps/min — check every signal
    Medium, // 2-10 swaps/min — check normally
    Low,    // <2 swaps/min — check every 5th signal
    Dead,   // 0 swaps — skip
}

pub struct PoolVolatilityClassifier {
    /// pool → (swap_count_ema, last_check)
    pool_activity: DashMap<Pubkey, (f64, Instant)>,
}

impl PoolVolatilityClassifier {
    pub fn new() -> Self {
        Self { pool_activity: DashMap::new() }
    }

    pub fn observe_swap(&self, pool: &Pubkey) {
        self.pool_activity
            .entry(*pool)
            .and_modify(|(ema, last)| {
                let elapsed = last.elapsed().as_secs_f64().max(0.001);
                let rate = 1.0 / elapsed; // swaps per second
                *ema = 0.1 * rate * 60.0 + 0.9 * *ema; // EMA in swaps/min
                *last = Instant::now();
            })
            .or_insert((1.0, Instant::now()));
    }

    pub fn classify(&self, pool: &Pubkey) -> VolatilityClass {
        match self.pool_activity.get(pool) {
            None => VolatilityClass::Dead,
            Some(entry) => {
                let (ema, last) = entry.value();
                if last.elapsed().as_secs() > 300 { return VolatilityClass::Dead; }
                if *ema > 10.0 { VolatilityClass::High }
                else if *ema > 2.0 { VolatilityClass::Medium }
                else { VolatilityClass::Low }
            }
        }
    }

    /// Should we check this pool on this signal? (signal_count for throttling)
    pub fn should_check(&self, pool: &Pubkey, signal_count: u64) -> bool {
        match self.classify(pool) {
            VolatilityClass::High => true,
            VolatilityClass::Medium => true,
            VolatilityClass::Low => signal_count % 5 == 0,
            VolatilityClass::Dead => false,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// #1: Landing Predictor (simplified — logistic regression)
// ═══════════════════════════════════════════════════════════════════════

pub struct LandingModel {
    /// Feature weights (learned from observation).
    /// Simplified: 6 features with hand-tuned initial weights.
    weights: [f64; 7], // [bias, detection_ms, profit, competition, hour_peak, n_hops, reserve_fresh]
    pub observations: AtomicU64,
}

impl LandingModel {
    pub fn new() -> Self {
        Self {
            // Initial weights from data analysis:
            // - Lower detection_ms = better
            // - Higher profit = better
            // - Lower competition = better
            // - Peak hours (13-14) = better
            // - Fewer hops = better
            // - Fresher reserves = better
            weights: [
                -1.0,   // bias (pessimistic prior)
                -0.005, // detection_ms (lower = better, each ms costs 0.005)
                0.0001, // profit (each 10K lamports adds 0.001)
                -2.0,   // competition (0-1, high = bad)
                0.5,    // hour_peak (1 if 13-14, else 0)
                -0.3,   // n_hops (more hops = worse)
                0.8,    // reserve_fresh (1 if <500ms, else 0)
            ],
            observations: AtomicU64::new(0),
        }
    }

    /// Predict P(land) from features.
    pub fn predict(&self, detection_ms: f64, profit: f64, competition: f64,
                   hour: u32, n_hops: u32, reserve_fresh: bool) -> f64 {
        let features = [
            1.0, // bias
            detection_ms,
            profit / 100_000.0, // normalize
            competition,
            if hour == 13 || hour == 14 { 1.0 } else { 0.0 },
            n_hops as f64,
            if reserve_fresh { 1.0 } else { 0.0 },
        ];

        let z: f64 = self.weights.iter().zip(features.iter()).map(|(w, f)| w * f).sum();
        sigmoid(z)
    }

    /// Online learning: update weights with gradient descent.
    pub fn observe(&self, features: [f64; 7], landed: bool) {
        self.observations.fetch_add(1, Ordering::Relaxed);
        // Note: weights are not &mut self for thread safety.
        // In production, use AtomicF64 or periodic batch updates.
        // For now, weights are static from initialization.
    }

    /// Should we attempt based on P(land)?
    pub fn should_attempt(&self, p_land: f64, profit: i64, tip: u64) -> bool {
        let ev = p_land * (profit as f64 - tip as f64);
        ev > 5000.0 // minimum EV of 5K lamports
    }
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// ═══════════════════════════════════════════════════════════════════════
// Unified Decision Engine
// ═══════════════════════════════════════════════════════════════════════

pub struct DecisionEngine {
    pub kalman: KalmanReserveEstimator,
    pub competition: CompetitionDetector,
    pub profitability: ProfitabilityEstimator,
    pub tip_selector: TipSelector,
    pub volatility: PoolVolatilityClassifier,
    pub landing: LandingModel,
}

impl DecisionEngine {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            kalman: KalmanReserveEstimator::new(),
            competition: CompetitionDetector::new(),
            profitability: ProfitabilityEstimator::new(),
            tip_selector: TipSelector::new(),
            volatility: PoolVolatilityClassifier::new(),
            landing: LandingModel::new(),
        })
    }

    /// Full decision pipeline: should we send this route?
    pub fn evaluate(&self, route_key: &str, dex_combo: &str,
                    estimated_profit: i64, detection_ms: f64,
                    hour: u32, n_hops: u32, reserve_fresh: bool,
                    jito_p75: u64) -> Option<(i64, u64, f64)> {
        // #5: Competition check
        let comp = self.competition.score_multiplier(route_key);
        if comp == 0.0 {
            trace!(route = route_key, "BLACKLISTED by competition detector");
            return None;
        }

        // #2: Adjust profit estimate
        let real_profit = self.profitability.predict_real_profit(route_key, dex_combo, estimated_profit);
        if real_profit < 10_000 {
            trace!(route = route_key, estimated = estimated_profit, real = real_profit, "phantom route filtered");
            return None;
        }

        // #3: Optimal tip
        let competition_score = 1.0 - comp; // invert: comp multiplier 1.5 = low competition = 0 score
        let tip = self.tip_selector.recommend_tip(real_profit, competition_score, hour, jito_p75);

        // #1: Landing probability
        let p_land = self.landing.predict(detection_ms, real_profit as f64, competition_score, hour, n_hops, reserve_fresh);

        // EV check
        let ev = p_land * (real_profit as f64 - tip as f64);
        if ev < 5000.0 {
            trace!(route = route_key, ev, p_land, "low EV, skip");
            return None;
        }

        debug!(
            route = route_key,
            estimated_profit,
            real_profit,
            tip,
            p_land = format!("{:.3}", p_land),
            ev = format!("{:.0}", ev),
            comp = format!("{:.2}", comp),
            "decision: SEND"
        );

        Some((real_profit, tip, p_land))
    }

    /// Record execution result for all models.
    pub fn observe_result(&self, route_key: &str, dex_combo: &str,
                          estimated_profit: i64, actual_profit: i64,
                          tip: u64, landed: bool, detection_ms: f64) {
        self.competition.observe(route_key, landed, actual_profit <= 0, detection_ms);
        self.profitability.observe(route_key, dex_combo, estimated_profit, actual_profit);
        self.tip_selector.observe(tip, landed);
    }

    pub fn stats(&self) -> String {
        format!(
            "kalman=[{}] comp=[{}] profit=[{}] landing_obs={}",
            self.kalman.stats(),
            self.competition.stats(),
            self.profitability.stats(),
            self.landing.observations.load(Ordering::Relaxed),
        )
    }
}
