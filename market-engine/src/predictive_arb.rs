// predictive_arb.rs — ML-powered predictive arbitrage from shred patterns.
//
// Instead of being FASTER than 5ms bots, we PREDICT where arb opportunities
// will appear BEFORE they happen — using patterns invisible to speed bots.
//
// Key insight from 2.5M swap analysis:
// - Pools with burst activity (10+ swaps in 10s) create price dislocations
// - Directional imbalance (90% buys → sell pressure incoming) predicts reversals
// - Hour 14 UTC has 100x larger average swaps than other hours
// - Cross-pool lag: pool A gets hit, pool B (same token) lags 1-3 slots
//
// The model scores each pool in real-time. When score > threshold,
// we PRE-COMPUTE the arb route so it's ready to fire instantly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::RwLock;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info};

use crate::pool_state::PoolStateCache;
use crate::types::RouteParams;

/// Rolling window of swap events per pool.
struct PoolActivity {
    /// Timestamps of recent swaps (last 30 seconds).
    swap_times: Vec<Instant>,
    /// Recent swap amounts (last 30 seconds).
    swap_amounts: Vec<u64>,
    /// Buy count vs sell count (a_to_b vs b_to_a) in window.
    buys: u32,
    sells: u32,
    /// Max impact seen in window.
    max_impact: f64,
    /// Last update time.
    last_update: Instant,
}

impl PoolActivity {
    fn new() -> Self {
        Self {
            swap_times: Vec::with_capacity(64),
            swap_amounts: Vec::with_capacity(64),
            buys: 0,
            sells: 0,
            max_impact: 0.0,
            last_update: Instant::now(),
        }
    }

    /// Prune events older than 30 seconds.
    fn prune(&mut self) {
        let cutoff = Instant::now() - std::time::Duration::from_secs(30);
        let keep_from = self.swap_times.iter().position(|t| *t > cutoff).unwrap_or(self.swap_times.len());
        if keep_from > 0 {
            self.swap_times.drain(..keep_from);
            self.swap_amounts.drain(..keep_from.min(self.swap_amounts.len()));
        }
    }

    /// Swap velocity: swaps per second in last 10s.
    fn velocity_10s(&self) -> f64 {
        let cutoff = Instant::now() - std::time::Duration::from_secs(10);
        let count = self.swap_times.iter().filter(|t| **t > cutoff).count();
        count as f64 / 10.0
    }

    /// Burst count: max swaps in any 3-second window in last 30s.
    fn burst_count_3s(&self) -> u32 {
        if self.swap_times.len() < 3 { return self.swap_times.len() as u32; }
        let mut max_burst = 0u32;
        for (i, t) in self.swap_times.iter().enumerate() {
            let window_end = *t + std::time::Duration::from_secs(3);
            let count = self.swap_times[i..].iter().take_while(|t2| **t2 <= window_end).count() as u32;
            max_burst = max_burst.max(count);
        }
        max_burst
    }

    /// Directional imbalance: (buys - sells) / total. Range: -1.0 to 1.0.
    fn directional_imbalance(&self) -> f64 {
        let total = self.buys + self.sells;
        if total == 0 { return 0.0; }
        (self.buys as f64 - self.sells as f64) / total as f64
    }

    /// Volume spike: current 10s volume / average 30s volume.
    fn volume_spike(&self) -> f64 {
        if self.swap_amounts.is_empty() { return 0.0; }
        let total_30s: u64 = self.swap_amounts.iter().sum();
        let avg_per_10s = total_30s as f64 / 3.0;
        if avg_per_10s == 0.0 { return 0.0; }
        let cutoff = Instant::now() - std::time::Duration::from_secs(10);
        let recent_vol: u64 = self.swap_times.iter().zip(self.swap_amounts.iter())
            .filter(|(t, _)| **t > cutoff)
            .map(|(_, a)| *a)
            .sum();
        recent_vol as f64 / avg_per_10s
    }
}

/// Predictive arb model: scores each pool for imminent arb opportunity.
pub struct PredictiveArbModel {
    pool_cache: Arc<PoolStateCache>,
    /// Per-pool rolling activity windows.
    activity: DashMap<Pubkey, PoolActivity>,
    /// Model weights (logistic regression).
    weights: RwLock<[f64; 8]>,
    bias: RwLock<f64>,
    /// Stats
    predictions: std::sync::atomic::AtomicU64,
    alerts: std::sync::atomic::AtomicU64,
}

/// Prediction result.
pub struct ArbPrediction {
    pub pool: Pubkey,
    pub score: f64,
    pub features: [f64; 8],
    pub alert: bool,
}

impl PredictiveArbModel {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        // Initial weights from domain knowledge (will be refined with data):
        // Higher weight = more predictive of arb opportunity.
        let weights = [
            2.0,   // swap_velocity_10s: fast activity = price moving
            1.5,   // burst_count_3s: concentrated swaps = dislocation
            1.0,   // directional_imbalance: one-sided = reversal coming
            1.5,   // volume_spike: abnormal volume = whale activity
            0.5,   // hour_feature: sin(hour * pi/12) — 14 UTC peak
            1.0,   // max_impact: high impact = big price move
            2.0,   // n_cross_dex: more venues = more arb paths
            -0.5,  // time_since_last_swap: stale = no opportunity
        ];

        Self {
            pool_cache,
            activity: DashMap::new(),
            weights: RwLock::new(weights),
            bias: RwLock::new(-3.0), // conservative — require strong signal
            predictions: std::sync::atomic::AtomicU64::new(0),
            alerts: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Called on EVERY decoded swap from shreds. Updates rolling window.
    pub fn observe_swap(&self, pool: &Pubkey, amount_in: u64, a_to_b: bool, impact_pct: f64) {
        let mut entry = self.activity.entry(*pool).or_insert_with(PoolActivity::new);
        let now = Instant::now();
        entry.swap_times.push(now);
        entry.swap_amounts.push(amount_in);
        if a_to_b { entry.buys += 1; } else { entry.sells += 1; }
        if impact_pct > entry.max_impact { entry.max_impact = impact_pct; }
        entry.last_update = now;

        // Prune old data every 100 swaps
        if entry.swap_times.len() > 200 {
            entry.prune();
        }
    }

    /// Score a pool for imminent arb opportunity.
    /// Returns prediction with score 0.0-1.0.
    pub fn predict(&self, pool: &Pubkey) -> ArbPrediction {
        self.predictions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let entry = self.activity.get(pool);
        let (velocity, burst, imbalance, vol_spike, max_impact, time_since) = match entry {
            Some(ref e) => (
                e.velocity_10s(),
                e.burst_count_3s() as f64,
                e.directional_imbalance(),
                e.volume_spike(),
                e.max_impact,
                e.last_update.elapsed().as_secs_f64(),
            ),
            None => return ArbPrediction { pool: *pool, score: 0.0, features: [0.0; 8], alert: false },
        };

        // Cross-DEX pool count
        let n_cross = if let Some(p) = self.pool_cache.get(pool) {
            let wsol = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
            let token = if p.token_a == wsol { p.token_b } else { p.token_a };
            self.pool_cache.pools_for_token(&token).len() as f64 - 1.0
        } else {
            0.0
        };

        // Hour feature: sin wave peaking at 14 UTC
        let hour = chrono::Utc::now().hour() as f64;
        let hour_feature = (hour * std::f64::consts::PI / 12.0).sin();

        let features = [
            velocity,
            burst,
            imbalance.abs(),
            vol_spike,
            hour_feature,
            max_impact / 100.0, // normalize to 0-1 range
            n_cross,
            -(time_since.min(30.0) / 30.0), // negative: older = worse
        ];

        // Logistic regression
        let weights = self.weights.read();
        let bias = *self.bias.read();
        let z: f64 = bias + features.iter().zip(weights.iter()).map(|(f, w)| f * w).sum::<f64>();
        let score = 1.0 / (1.0 + (-z).exp());

        let alert = score > 0.7 && n_cross > 0.0;
        if alert {
            self.alerts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            info!(
                pool = %pool,
                score = format!("{:.3}", score),
                velocity,
                burst = burst as u32,
                imbalance = format!("{:.2}", imbalance),
                vol_spike = format!("{:.1}", vol_spike),
                cross_dex = n_cross as u32,
                "🧠 ML PREDICTION: high arb probability"
            );
        }

        ArbPrediction { pool: *pool, score, features, alert }
    }

    /// Online learning: update weights when we observe an arb outcome.
    pub fn learn(&self, features: &[f64; 8], landed: bool) {
        let mut weights = self.weights.write();
        let mut bias = self.bias.write();
        let target = if landed { 1.0 } else { 0.0 };
        let z: f64 = *bias + features.iter().zip(weights.iter()).map(|(f, w)| f * w).sum::<f64>();
        let prediction = 1.0 / (1.0 + (-z).exp());
        let error = target - prediction;
        let lr = 0.01; // learning rate

        for (w, f) in weights.iter_mut().zip(features.iter()) {
            *w += lr * error * f;
        }
        *bias += lr * error;
    }

    pub fn stats(&self) -> (u64, u64) {
        (
            self.predictions.load(std::sync::atomic::Ordering::Relaxed),
            self.alerts.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Cleanup stale pool entries.
    pub fn cleanup(&self) {
        self.activity.retain(|_, v| v.last_update.elapsed().as_secs() < 120);
    }
}

use chrono::Timelike;
