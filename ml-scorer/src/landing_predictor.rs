//! landing_predictor.rs — Predicts which arbitrage opportunities will actually land on-chain.
//!
//! Three components:
//!   1. PoolVolatilityModel: per-pool features from pool_snapshots time-series
//!      (reserve velocity, mean reversion speed, activity rate, price CV)
//!   2. Logistic regression classifier: P(land) from 17 features
//!      (opportunity features + pool volatility + timing + historical success)
//!   3. Optimal borrow selector: maximize EV = P(land|amount) * net_profit(amount)
//!
//! Inference: ~50ns (dot product + sigmoid). All data in DashMap for lock-free reads.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Number of features in the logistic regression model.
const N_FEATURES: usize = 18;
/// Strategy IDs for one-hot-ish encoding.
const STRATEGY_BELLMAN_FORD: f64 = 0.0;
const STRATEGY_CROSS_DEX: f64 = 1.0;
const STRATEGY_WHALE_BACKRUN: f64 = 2.0;
const STRATEGY_LST_ARB: f64 = 3.0;
/// Default mean reversion half-life for unknown pools (ms).
const DEFAULT_MEAN_REVERSION_MS: f64 = 5000.0;
/// Minimum landing probability to recommend execution.
const DEFAULT_MIN_P_LAND: f64 = 0.10;

// ─── Pool Volatility Model ─────────────────────────────────────────────────

/// Per-pool features computed from pool_snapshots time-series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolVolatilityModel {
    pub pool: String,
    /// Average |delta_reserve_a| per second.
    pub reserve_a_velocity: f64,
    /// Average |delta_reserve_b| per second.
    pub reserve_b_velocity: f64,
    /// Variance of reserve_a changes.
    pub reserve_a_variance: f64,
    /// Variance of reserve_b changes.
    pub reserve_b_variance: f64,
    /// Estimated time for reserves to revert halfway to mean (ms).
    /// Lower = faster reversion = shorter arb window = harder to land.
    pub mean_reversion_half_life_ms: f64,
    /// Reserve changes per minute (proxy for swap frequency).
    pub activity_rate: f64,
    /// Coefficient of variation of price ratio (std/mean).
    pub price_cv: f64,
    /// Linear trend slope of reserve_a over window.
    pub reserve_a_trend: f64,
    /// Linear trend slope of reserve_b over window.
    pub reserve_b_trend: f64,
    /// log(reserve_a) — larger pools = more persistent arbs.
    pub log_reserve_a: f64,
    /// log(reserve_b).
    pub log_reserve_b: f64,
    /// Number of snapshots used to compute these features.
    pub n_samples: u64,
    /// Last update epoch secs.
    pub last_updated: i64,
}

impl PoolVolatilityModel {
    pub fn default_for(pool: String) -> Self {
        Self {
            pool,
            reserve_a_velocity: 0.0,
            reserve_b_velocity: 0.0,
            reserve_a_variance: 0.0,
            reserve_b_variance: 0.0,
            mean_reversion_half_life_ms: DEFAULT_MEAN_REVERSION_MS,
            activity_rate: 0.0,
            price_cv: 0.0,
            reserve_a_trend: 0.0,
            reserve_b_trend: 0.0,
            log_reserve_a: 0.0,
            log_reserve_b: 0.0,
            n_samples: 0,
            last_updated: chrono::Utc::now().timestamp(),
        }
    }

    /// Compute volatility features from a sorted time-series of (timestamp_secs, reserve_a, reserve_b).
    pub fn from_snapshots(pool: String, snapshots: &[(f64, f64, f64)]) -> Self {
        let n = snapshots.len();
        if n < 3 {
            return Self::default_for(pool);
        }

        let mut delta_a: Vec<f64> = Vec::with_capacity(n - 1);
        let mut delta_b: Vec<f64> = Vec::with_capacity(n - 1);
        let mut delta_t: Vec<f64> = Vec::with_capacity(n - 1);
        let mut prices: Vec<f64> = Vec::with_capacity(n);

        for i in 0..n {
            let (_, ra, rb) = snapshots[i];
            if rb > 0.0 {
                prices.push(ra / rb);
            }
            if i > 0 {
                let dt = (snapshots[i].0 - snapshots[i - 1].0).max(0.001);
                delta_a.push((snapshots[i].1 - snapshots[i - 1].1).abs());
                delta_b.push((snapshots[i].2 - snapshots[i - 1].2).abs());
                delta_t.push(dt);
            }
        }

        // Reserve velocity: avg |delta| / dt
        let total_dt: f64 = delta_t.iter().sum();
        let reserve_a_velocity = if total_dt > 0.0 {
            delta_a.iter().sum::<f64>() / total_dt
        } else {
            0.0
        };
        let reserve_b_velocity = if total_dt > 0.0 {
            delta_b.iter().sum::<f64>() / total_dt
        } else {
            0.0
        };

        // Variance of reserve changes
        let mean_da = delta_a.iter().sum::<f64>() / delta_a.len() as f64;
        let mean_db = delta_b.iter().sum::<f64>() / delta_b.len() as f64;
        let reserve_a_variance = delta_a.iter().map(|d| (d - mean_da).powi(2)).sum::<f64>()
            / delta_a.len() as f64;
        let reserve_b_variance = delta_b.iter().map(|d| (d - mean_db).powi(2)).sum::<f64>()
            / delta_b.len() as f64;

        // Activity rate: changes/min (non-zero deltas)
        let non_zero_changes = delta_a.iter().filter(|d| **d > 0.0).count()
            + delta_b.iter().filter(|d| **d > 0.0).count();
        let span_minutes = total_dt / 60.0;
        let activity_rate = if span_minutes > 0.0 {
            non_zero_changes as f64 / span_minutes
        } else {
            0.0
        };

        // Price coefficient of variation
        let price_cv = if prices.len() >= 2 {
            let mean_p = prices.iter().sum::<f64>() / prices.len() as f64;
            if mean_p > 0.0 {
                let var = prices.iter().map(|p| (p - mean_p).powi(2)).sum::<f64>()
                    / prices.len() as f64;
                var.sqrt() / mean_p
            } else {
                0.0
            }
        } else {
            0.0
        };

        // Mean reversion: autocorrelation of price changes
        // Negative autocorrelation = mean reverting; use lag-1 autocorrelation
        let mean_reversion_half_life_ms = if prices.len() >= 4 {
            let price_changes: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
            let mean_pc = price_changes.iter().sum::<f64>() / price_changes.len() as f64;
            let var_pc: f64 = price_changes.iter().map(|c| (c - mean_pc).powi(2)).sum::<f64>()
                / price_changes.len() as f64;
            if var_pc > 1e-20 && price_changes.len() >= 3 {
                let autocov: f64 = price_changes
                    .windows(2)
                    .map(|w| (w[0] - mean_pc) * (w[1] - mean_pc))
                    .sum::<f64>()
                    / (price_changes.len() - 1) as f64;
                let rho = (autocov / var_pc).clamp(-0.99, 0.99);
                // half-life from AR(1): t_half = -ln(2) / ln(|rho|)
                // Negative rho = mean reverting (good); positive = trending
                if rho.abs() > 0.01 {
                    let avg_dt_ms = (total_dt / delta_t.len() as f64) * 1000.0;
                    let half_life_steps = -(2.0_f64.ln()) / rho.abs().ln();
                    (half_life_steps * avg_dt_ms).clamp(100.0, 300_000.0)
                } else {
                    DEFAULT_MEAN_REVERSION_MS // no significant autocorrelation
                }
            } else {
                DEFAULT_MEAN_REVERSION_MS
            }
        } else {
            DEFAULT_MEAN_REVERSION_MS
        };

        // Trend: simple linear regression slope for reserves
        let reserve_a_trend = linear_slope(&snapshots.iter().map(|s| (s.0, s.1)).collect::<Vec<_>>());
        let reserve_b_trend = linear_slope(&snapshots.iter().map(|s| (s.0, s.2)).collect::<Vec<_>>());

        // Log reserves (latest)
        let (_, last_a, last_b) = snapshots[n - 1];
        let log_reserve_a = (last_a.max(1.0)).ln();
        let log_reserve_b = (last_b.max(1.0)).ln();

        Self {
            pool,
            reserve_a_velocity,
            reserve_b_velocity,
            reserve_a_variance,
            reserve_b_variance,
            mean_reversion_half_life_ms,
            activity_rate,
            price_cv,
            reserve_a_trend,
            reserve_b_trend,
            log_reserve_a,
            log_reserve_b,
            n_samples: n as u64,
            last_updated: chrono::Utc::now().timestamp(),
        }
    }
}

/// Simple linear regression slope: dy/dx.
fn linear_slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let sum_x: f64 = points.iter().map(|p| p.0).sum();
    let sum_y: f64 = points.iter().map(|p| p.1).sum();
    let sum_xy: f64 = points.iter().map(|p| p.0 * p.1).sum();
    let sum_xx: f64 = points.iter().map(|p| p.0 * p.0).sum();
    let denom = n * sum_xx - sum_x * sum_x;
    if denom.abs() < 1e-20 {
        return 0.0;
    }
    (n * sum_xy - sum_x * sum_y) / denom
}

// ─── Feature Vector for Logistic Regression ────────────────────────────────

/// Features used by the landing classifier.
/// All 18 features are f64 — precomputed in <1µs.
#[derive(Debug, Clone, Default)]
pub struct LandingFeatures {
    /// 0: log(net_profit)
    pub log_net_profit: f64,
    /// 1: net_profit / borrow_amount * 10000 (basis points)
    pub profit_margin_bps: f64,
    /// 2: number of hops
    pub n_hops: f64,
    /// 3: max(price_impact) across hops
    pub max_price_impact: f64,
    /// 4: sum(price_impact) across hops
    pub total_price_impact: f64,
    /// 5: strategy ID (0=bf, 1=crossdex, 2=whale, 3=lst)
    pub strategy_id: f64,
    /// 6: log(borrow_amount)
    pub log_borrow_amount: f64,
    /// 7: existing composite score
    pub score: f64,
    /// 8: min(mean_reversion_half_life_ms) across hop pools
    pub min_mean_reversion_ms: f64,
    /// 9: max(activity_rate) across hop pools
    pub max_activity_rate: f64,
    /// 10: avg(price_cv) across hop pools
    pub avg_price_cv: f64,
    /// 11: min(log_reserve) across all hop pools
    pub min_log_reserves: f64,
    /// 12: max(reserve_velocity) across hops (sum of a+b velocity)
    pub max_reserve_velocity: f64,
    /// 13: sin(hour * 2π/24) — cyclic hour encoding
    pub hour_sin: f64,
    /// 14: cos(hour * 2π/24) — cyclic hour encoding
    pub hour_cos: f64,
    /// 15: time since detection (ms)
    pub detect_age_ms: f64,
    /// 16: worst pool success_rate from MlScorer
    pub pool_success_rate: f64,
    /// 17: worst pool competition from MlScorer
    pub pool_competition: f64,
}

impl LandingFeatures {
    /// Convert to fixed-size array for dot product.
    pub fn to_array(&self) -> [f64; N_FEATURES] {
        [
            self.log_net_profit,
            self.profit_margin_bps,
            self.n_hops,
            self.max_price_impact,
            self.total_price_impact,
            self.strategy_id,
            self.log_borrow_amount,
            self.score,
            self.min_mean_reversion_ms,
            self.max_activity_rate,
            self.avg_price_cv,
            self.min_log_reserves,
            self.max_reserve_velocity,
            self.hour_sin,
            self.hour_cos,
            self.detect_age_ms,
            self.pool_success_rate,
            self.pool_competition,
        ]
    }
}

// ─── Logistic Regression Weights ────────────────────────────────────────────

/// Weight vector for logistic regression: P(land) = sigmoid(bias + w·x)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogisticWeights {
    pub bias: f64,
    pub weights: Vec<f64>,
    /// Feature means for z-score normalization.
    pub means: Vec<f64>,
    /// Feature stds for z-score normalization.
    pub stds: Vec<f64>,
}

impl Default for LogisticWeights {
    fn default() -> Self {
        // Initial weights: educated priors based on domain knowledge.
        // Positive = helps landing, negative = hurts landing.
        let mut weights = vec![0.0; N_FEATURES];
        weights[0] = 0.3;   // log_net_profit: higher profit → more margin → more likely to land
        weights[1] = 0.5;   // profit_margin_bps: KEY feature — wider margin survives slippage
        weights[2] = -0.2;  // n_hops: more hops → more things can go wrong
        weights[3] = -0.4;  // max_price_impact: high impact → reserves shift more → slippage
        weights[4] = -0.3;  // total_price_impact: cumulative impact
        weights[5] = 0.0;   // strategy_id: neutral prior
        weights[6] = -0.1;  // log_borrow_amount: larger borrows → more impact
        weights[7] = 0.2;   // score: existing score has useful signal
        weights[8] = 0.4;   // min_mean_reversion_ms: slower reversion → longer arb window → good
        weights[9] = -0.3;  // max_activity_rate: more active = more competition
        weights[10] = -0.2; // avg_price_cv: volatile prices = less predictable
        weights[11] = 0.3;  // min_log_reserves: deeper pools = more persistent arb
        weights[12] = -0.2; // max_reserve_velocity: fast-moving reserves = short window
        weights[13] = 0.0;  // hour_sin: learned from data
        weights[14] = 0.0;  // hour_cos: learned from data
        weights[15] = -0.1; // detect_age_ms: older detection = stale
        weights[16] = 0.3;  // pool_success_rate: historically successful pools
        weights[17] = -0.3; // pool_competition: high competition = low landing

        Self {
            bias: -1.0, // prior: most opportunities don't land
            weights,
            means: vec![0.0; N_FEATURES],
            stds: vec![1.0; N_FEATURES],
        }
    }
}

impl LogisticWeights {
    /// Predict P(land) from feature vector. ~50ns.
    #[inline]
    pub fn predict(&self, features: &[f64; N_FEATURES]) -> f64 {
        let mut z = self.bias;
        for i in 0..N_FEATURES {
            let std = if self.stds[i] > 1e-10 { self.stds[i] } else { 1.0 };
            let normalized = (features[i] - self.means[i]) / std;
            z += self.weights[i] * normalized;
        }
        sigmoid(z)
    }

    /// Train one step of SGD on a single example.
    /// Returns the loss for this example.
    pub fn train_step(&mut self, features: &[f64; N_FEATURES], label: f64, lr: f64) -> f64 {
        let pred = self.predict(features);
        let error = pred - label;
        // Gradient of log-loss: (pred - label) * feature_i
        for i in 0..N_FEATURES {
            let std = if self.stds[i] > 1e-10 { self.stds[i] } else { 1.0 };
            let normalized = (features[i] - self.means[i]) / std;
            self.weights[i] -= lr * error * normalized;
        }
        self.bias -= lr * error;
        // Binary cross-entropy loss
        -(label * (pred.max(1e-10)).ln() + (1.0 - label) * (1.0 - pred).max(1e-10).ln())
    }

    /// Compute means and stds from a dataset for normalization.
    pub fn fit_normalization(&mut self, dataset: &[[f64; N_FEATURES]]) {
        let n = dataset.len() as f64;
        if n < 2.0 {
            return;
        }
        for i in 0..N_FEATURES {
            let mean = dataset.iter().map(|row| row[i]).sum::<f64>() / n;
            let var = dataset.iter().map(|row| (row[i] - mean).powi(2)).sum::<f64>() / n;
            self.means[i] = mean;
            self.stds[i] = var.sqrt().max(1e-10);
        }
    }
}

#[inline]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// ─── Main Landing Predictor ─────────────────────────────────────────────────

pub struct LandingPredictor {
    /// Per-pool volatility features (loaded from PG or computed incrementally).
    volatility: DashMap<String, PoolVolatilityModel>,
    /// Logistic regression weights.
    weights: std::sync::RwLock<LogisticWeights>,
    /// Minimum landing probability to recommend execution.
    min_p_land: f64,
    started_at: Instant,
}

impl LandingPredictor {
    pub fn new() -> Arc<Self> {
        let min_p = std::env::var("MIN_LANDING_PROB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MIN_P_LAND);
        Arc::new(Self {
            volatility: DashMap::new(),
            weights: std::sync::RwLock::new(LogisticWeights::default()),
            min_p_land: min_p,
            started_at: Instant::now(),
        })
    }

    /// Get volatility model for a pool (returns default if unknown).
    pub fn get_volatility(&self, pool: &str) -> PoolVolatilityModel {
        self.volatility
            .get(pool)
            .map(|v| v.value().clone())
            .unwrap_or_else(|| PoolVolatilityModel::default_for(pool.to_string()))
    }

    /// Predict landing probability from precomputed features.
    /// Returns (p_land, should_execute).
    pub fn predict(&self, features: &LandingFeatures) -> (f64, bool) {
        let arr = features.to_array();
        let p = self.weights.read().unwrap().predict(&arr);
        (p, p >= self.min_p_land)
    }

    /// Build features for an opportunity.
    /// `hop_pools`: pool addresses per hop
    /// `hop_impacts`: price impact per hop
    /// `borrow_amount`, `net_profit`, `score`: from route
    /// `strategy`: strategy name
    /// `detect_age_ms`: time since detection
    /// `ml_success_rates`: (success_rate, competition) per pool from MlScorer
    pub fn build_features(
        &self,
        hop_pools: &[&str],
        hop_impacts: &[f64],
        borrow_amount: u64,
        net_profit: i64,
        score: f64,
        strategy: &str,
        detect_age_ms: f64,
        ml_success_rates: &[(f64, f64)], // (success_rate, competition) per pool
    ) -> LandingFeatures {
        let hour = chrono::Utc::now().hour() as usize;
        let hour_rad = hour as f64 * std::f64::consts::TAU / 24.0;

        let strategy_id = match strategy {
            "bellman-ford" => STRATEGY_BELLMAN_FORD,
            "cross-dex" => STRATEGY_CROSS_DEX,
            "whale-backrun" => STRATEGY_WHALE_BACKRUN,
            "lst-arb" => STRATEGY_LST_ARB,
            _ => 0.0,
        };

        // Pool volatility features: aggregate across hops
        let mut min_mean_reversion = f64::MAX;
        let mut max_activity = 0.0_f64;
        let mut sum_price_cv = 0.0;
        let mut min_log_res = f64::MAX;
        let mut max_velocity = 0.0_f64;

        for pool_key in hop_pools {
            let vol = self.get_volatility(pool_key);
            min_mean_reversion = min_mean_reversion.min(vol.mean_reversion_half_life_ms);
            max_activity = max_activity.max(vol.activity_rate);
            sum_price_cv += vol.price_cv;
            min_log_res = min_log_res.min(vol.log_reserve_a.min(vol.log_reserve_b));
            max_velocity = max_velocity.max(vol.reserve_a_velocity + vol.reserve_b_velocity);
        }

        let n_pools = hop_pools.len().max(1) as f64;
        if min_mean_reversion == f64::MAX {
            min_mean_reversion = DEFAULT_MEAN_REVERSION_MS;
        }
        if min_log_res == f64::MAX {
            min_log_res = 0.0;
        }

        // ML scorer features: worst pool
        let pool_success_rate = ml_success_rates
            .iter()
            .map(|s| s.0)
            .reduce(f64::min)
            .unwrap_or(0.5);
        let pool_competition = ml_success_rates
            .iter()
            .map(|s| s.1)
            .reduce(f64::max)
            .unwrap_or(0.0);

        LandingFeatures {
            log_net_profit: (net_profit.max(1) as f64).ln(),
            profit_margin_bps: if borrow_amount > 0 {
                net_profit as f64 / borrow_amount as f64 * 10_000.0
            } else {
                0.0
            },
            n_hops: hop_pools.len() as f64,
            max_price_impact: hop_impacts.iter().cloned().fold(0.0, f64::max),
            total_price_impact: hop_impacts.iter().sum(),
            strategy_id,
            log_borrow_amount: (borrow_amount.max(1) as f64).ln(),
            score,
            min_mean_reversion_ms: min_mean_reversion,
            max_activity_rate: max_activity,
            avg_price_cv: sum_price_cv / n_pools,
            min_log_reserves: min_log_res,
            max_reserve_velocity: max_velocity,
            hour_sin: hour_rad.sin(),
            hour_cos: hour_rad.cos(),
            detect_age_ms,
            pool_success_rate,
            pool_competition,
        }
    }

    /// Select optimal borrow amount from candidates by maximizing expected value.
    /// Returns (best_amount, best_p_land, best_ev).
    pub fn optimal_borrow(
        &self,
        candidates: &[(u64, i64, f64)], // (borrow_amount, net_profit, score) per candidate
        hop_pools: &[&str],
        hop_impacts: &[f64],
        strategy: &str,
        ml_success_rates: &[(f64, f64)],
    ) -> Option<(u64, f64, f64)> {
        let mut best: Option<(u64, f64, f64)> = None;
        for &(borrow, profit, score) in candidates {
            if profit <= 0 {
                continue;
            }
            let features = self.build_features(
                hop_pools,
                hop_impacts,
                borrow,
                profit,
                score,
                strategy,
                0.0,
                ml_success_rates,
            );
            let (p_land, _) = self.predict(&features);
            let ev = p_land * profit as f64;
            if best.map_or(true, |b| ev > b.2) {
                best = Some((borrow, p_land, ev));
            }
        }
        best
    }

    /// Update volatility model for a pool from new snapshots.
    pub fn update_volatility(&self, pool: &str, snapshots: &[(f64, f64, f64)]) {
        let model = PoolVolatilityModel::from_snapshots(pool.to_string(), snapshots);
        if model.n_samples >= 3 {
            self.volatility.insert(pool.to_string(), model);
        }
    }

    /// Load volatility models from PostgreSQL.
    pub async fn load_volatility_from_pg(&self, db_url: &str) -> anyhow::Result<u64> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let rows = client
            .query(
                "SELECT pool_address, reserve_a_velocity, reserve_b_velocity,
                        reserve_a_variance, reserve_b_variance,
                        mean_reversion_half_life_ms, activity_rate, price_cv,
                        reserve_a_trend, reserve_b_trend,
                        log_reserve_a, log_reserve_b, n_samples
                 FROM pool_volatility_models WHERE n_samples >= 3",
                &[],
            )
            .await?;

        let mut loaded = 0u64;
        for row in &rows {
            let pool: String = row.get(0);
            let model = PoolVolatilityModel {
                pool: pool.clone(),
                reserve_a_velocity: row.get(1),
                reserve_b_velocity: row.get(2),
                reserve_a_variance: row.get(3),
                reserve_b_variance: row.get(4),
                mean_reversion_half_life_ms: row.get(5),
                activity_rate: row.get(6),
                price_cv: row.get(7),
                reserve_a_trend: row.get(8),
                reserve_b_trend: row.get(9),
                log_reserve_a: row.get(10),
                log_reserve_b: row.get(11),
                n_samples: row.get::<_, i64>(12) as u64,
                last_updated: chrono::Utc::now().timestamp(),
            };
            self.volatility.insert(pool, model);
            loaded += 1;
        }

        info!("landing-predictor: loaded {loaded} volatility models from PG");
        Ok(loaded)
    }

    /// Load logistic regression weights from PostgreSQL.
    pub async fn load_weights_from_pg(&self, db_url: &str) -> anyhow::Result<bool> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let row = client
            .query_opt(
                "SELECT weights_json FROM ml_model_state WHERE model_name = 'landing_predictor'",
                &[],
            )
            .await?;

        if let Some(row) = row {
            let json: serde_json::Value = row.get(0);
            if let Ok(w) = serde_json::from_value::<LogisticWeights>(json) {
                info!(
                    bias = w.bias,
                    n_weights = w.weights.len(),
                    "landing-predictor: loaded weights from PG"
                );
                *self.weights.write().unwrap() = w;
                return Ok(true);
            }
        }

        info!("landing-predictor: no trained weights found, using domain priors");
        Ok(false)
    }

    /// Save weights to PostgreSQL.
    pub async fn save_weights_to_pg(&self, db_url: &str) -> anyhow::Result<()> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let weights = self.weights.read().unwrap().clone();
        let json = serde_json::to_value(&weights)?;

        client
            .execute(
                "INSERT INTO ml_model_state (model_name, weights_json, n_samples)
                 VALUES ('landing_predictor', $1, 0)
                 ON CONFLICT (model_name) DO UPDATE SET
                 weights_json = $1, updated_at = now()",
                &[&json],
            )
            .await?;

        info!("landing-predictor: saved weights to PG");
        Ok(())
    }

    /// Save volatility models to PostgreSQL.
    pub async fn save_volatility_to_pg(&self, db_url: &str) -> anyhow::Result<u64> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let mut saved = 0u64;
        for entry in self.volatility.iter() {
            let v = entry.value();
            client
                .execute(
                    "INSERT INTO pool_volatility_models
                     (pool_address, reserve_a_velocity, reserve_b_velocity,
                      reserve_a_variance, reserve_b_variance,
                      mean_reversion_half_life_ms, activity_rate, price_cv,
                      reserve_a_trend, reserve_b_trend,
                      log_reserve_a, log_reserve_b, n_samples)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                     ON CONFLICT (pool_address) DO UPDATE SET
                     reserve_a_velocity=$2, reserve_b_velocity=$3,
                     reserve_a_variance=$4, reserve_b_variance=$5,
                     mean_reversion_half_life_ms=$6, activity_rate=$7, price_cv=$8,
                     reserve_a_trend=$9, reserve_b_trend=$10,
                     log_reserve_a=$11, log_reserve_b=$12,
                     n_samples=$13, updated_at=now()",
                    &[
                        &v.pool,
                        &v.reserve_a_velocity,
                        &v.reserve_b_velocity,
                        &v.reserve_a_variance,
                        &v.reserve_b_variance,
                        &v.mean_reversion_half_life_ms,
                        &v.activity_rate,
                        &v.price_cv,
                        &v.reserve_a_trend,
                        &v.reserve_b_trend,
                        &v.log_reserve_a,
                        &v.log_reserve_b,
                        &(v.n_samples as i64),
                    ],
                )
                .await?;
            saved += 1;
        }

        info!("landing-predictor: saved {saved} volatility models to PG");
        Ok(saved)
    }

    /// Set weights directly (used by training binary).
    pub fn set_weights(&self, weights: LogisticWeights) {
        *self.weights.write().unwrap() = weights;
    }

    /// Stats summary.
    pub fn stats(&self) -> String {
        let w = self.weights.read().unwrap();
        format!(
            "volatility_models={} bias={:.3} min_p_land={:.2} uptime={:.0}s",
            self.volatility.len(),
            w.bias,
            self.min_p_land,
            self.started_at.elapsed().as_secs_f64()
        )
    }
}

use chrono::Timelike;
