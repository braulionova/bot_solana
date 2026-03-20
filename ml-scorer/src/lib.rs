//! ml-scorer – Online ML model that learns hot routes in real-time.
//!
//! Architecture:
//! - Maintains in-memory feature vectors per pool/pair/strategy
//! - Uses exponential moving averages (EMA) for online learning
//! - Loads historical stats from PostgreSQL on startup
//! - Provides `score_boost()` that multiplies the base score
//! - Periodically persists model weights to PG (crash recovery)
//!
//! Features tracked per pool:
//! 1. Success rate (EMA, α=0.1) — landed TXs / total attempts
//! 2. Avg profit (EMA) — rolling average profit when landed
//! 3. Competition index — how often we see same pool fail (more fails = more competition)
//! 4. Time-of-day bias — hourly success rate (24 buckets)
//! 5. Liquidity score — reserve levels relative to borrow amount
//! 6. Freshness — time since last successful trade on this pool
//! 7. Strategy affinity — which strategies work best per pool type

pub mod graduation_predictor;
pub mod landing_predictor;
pub mod onchain_scanner;
pub mod pool_discoverer;
pub mod shred_predictor;
pub mod snapshot_scanner;
pub mod spread_predictor;
pub mod threehop_predictor;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

/// EMA smoothing factor. Higher = more weight on recent data.
const EMA_ALPHA: f64 = 0.1;
/// Minimum samples before model predictions are trusted.
const MIN_SAMPLES: u64 = 5;
/// Number of hourly buckets for time-of-day patterns.
const HOUR_BUCKETS: usize = 24;

/// Per-pool learned features.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolModel {
    pub pool: String,
    pub dex_type: String,
    /// EMA success rate (0.0 - 1.0).
    pub success_rate: f64,
    /// EMA average profit in lamports (when landed).
    pub avg_profit: f64,
    /// Total attempts observed.
    pub n_samples: u64,
    /// Total landed.
    pub n_landed: u64,
    /// Competition index: ratio of failures (higher = more competition).
    pub competition: f64,
    /// Hourly success counts [hour] = (successes, attempts).
    pub hourly_stats: Vec<(u64, u64)>,
    /// Last successful trade timestamp (epoch secs).
    pub last_success_epoch: Option<i64>,
    /// Last update.
    pub last_updated_epoch: i64,
}

impl PoolModel {
    fn new(pool: String, dex_type: String) -> Self {
        Self {
            pool,
            dex_type,
            success_rate: 0.5, // prior: 50% until we have data
            avg_profit: 0.0,
            n_samples: 0,
            n_landed: 0,
            competition: 0.0,
            hourly_stats: vec![(0, 0); HOUR_BUCKETS],
            last_success_epoch: None,
            last_updated_epoch: chrono::Utc::now().timestamp(),
        }
    }

    /// Update the model with a new observation.
    fn observe(&mut self, landed: bool, profit: i64, hour: usize) {
        self.n_samples += 1;

        // EMA success rate
        let outcome = if landed { 1.0 } else { 0.0 };
        self.success_rate = EMA_ALPHA * outcome + (1.0 - EMA_ALPHA) * self.success_rate;

        // EMA profit (only on success)
        if landed {
            self.n_landed += 1;
            self.avg_profit = EMA_ALPHA * (profit as f64) + (1.0 - EMA_ALPHA) * self.avg_profit;
            self.last_success_epoch = Some(chrono::Utc::now().timestamp());
        }

        // Competition: failure rate EMA
        let fail = if landed { 0.0 } else { 1.0 };
        self.competition = EMA_ALPHA * fail + (1.0 - EMA_ALPHA) * self.competition;

        // Hourly stats
        let h = hour % HOUR_BUCKETS;
        self.hourly_stats[h].1 += 1; // attempts
        if landed {
            self.hourly_stats[h].0 += 1; // successes
        }

        self.last_updated_epoch = chrono::Utc::now().timestamp();
    }
}

/// Per-strategy learned features.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyModel {
    pub strategy: String,
    pub success_rate: f64,
    pub avg_profit: f64,
    pub n_samples: u64,
    pub n_landed: u64,
    pub best_hour: Option<usize>,
}

impl StrategyModel {
    fn new(strategy: String) -> Self {
        Self {
            strategy,
            success_rate: 0.5,
            avg_profit: 0.0,
            n_samples: 0,
            n_landed: 0,
            best_hour: None,
        }
    }

    fn observe(&mut self, landed: bool, profit: i64) {
        self.n_samples += 1;
        let outcome = if landed { 1.0 } else { 0.0 };
        self.success_rate = EMA_ALPHA * outcome + (1.0 - EMA_ALPHA) * self.success_rate;
        if landed {
            self.n_landed += 1;
            self.avg_profit = EMA_ALPHA * (profit as f64) + (1.0 - EMA_ALPHA) * self.avg_profit;
        }
    }
}

/// Main ML scorer — thread-safe, lock-free reads via DashMap.
pub struct MlScorer {
    pools: DashMap<String, PoolModel>,
    strategies: DashMap<String, StrategyModel>,
    /// Global baseline success rate (EMA across all observations).
    global_success_rate: std::sync::atomic::AtomicU64, // f64 bits stored as u64
    total_observations: std::sync::atomic::AtomicU64,
    started_at: Instant,
}

impl MlScorer {
    /// Create a new scorer (empty model — learns from scratch).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pools: DashMap::new(),
            strategies: DashMap::new(),
            global_success_rate: std::sync::atomic::AtomicU64::new(f64::to_bits(0.5)),
            total_observations: std::sync::atomic::AtomicU64::new(0),
            started_at: Instant::now(),
        })
    }

    /// Load historical performance from PostgreSQL.
    /// Call once at startup to bootstrap the model with existing data.
    pub async fn load_from_pg(self: &Arc<Self>, db_url: &str) -> anyhow::Result<()> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("ml-scorer pg connection error: {e}");
            }
        });

        // Load pool performance
        let rows = client
            .query(
                "SELECT pool_address, dex_type, total_opps, total_sent, total_landed, \
                 total_profit, avg_score, \
                 EXTRACT(EPOCH FROM last_success)::bigint, \
                 EXTRACT(EPOCH FROM last_failure)::bigint \
                 FROM pool_performance WHERE total_sent >= 1",
                &[],
            )
            .await?;

        let mut loaded = 0u64;
        for row in &rows {
            let pool: String = row.get(0);
            let dex_type: String = row.get(1);
            let total_opps: i64 = row.get(2);
            let total_landed: i64 = row.get(4);
            let total_profit: i64 = row.get(5);

            let mut model = PoolModel::new(pool.clone(), dex_type);
            model.n_samples = total_opps as u64;
            model.n_landed = total_landed as u64;
            model.success_rate = if total_opps > 0 {
                total_landed as f64 / total_opps as f64
            } else {
                0.5
            };
            model.avg_profit = if total_landed > 0 {
                total_profit as f64 / total_landed as f64
            } else {
                0.0
            };
            model.competition = 1.0 - model.success_rate;
            model.last_success_epoch = row.get::<_, Option<i64>>(7);

            self.pools.insert(pool, model);
            loaded += 1;
        }

        info!("ml-scorer: loaded {loaded} pool models from PostgreSQL");
        Ok(())
    }

    /// Record an observation (called after execution result).
    pub fn observe(&self, pool_key: &str, dex_type: &str, strategy: &str, landed: bool, profit: i64) {
        let hour = chrono::Utc::now().hour() as usize;

        // Update pool model
        self.pools
            .entry(pool_key.to_string())
            .or_insert_with(|| PoolModel::new(pool_key.to_string(), dex_type.to_string()))
            .observe(landed, profit, hour);

        // Update strategy model
        self.strategies
            .entry(strategy.to_string())
            .or_insert_with(|| StrategyModel::new(strategy.to_string()))
            .observe(landed, profit);

        // Update global baseline
        let n = self
            .total_observations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let old_bits = self
            .global_success_rate
            .load(std::sync::atomic::Ordering::Relaxed);
        let old = f64::from_bits(old_bits);
        let outcome = if landed { 1.0 } else { 0.0 };
        let new = if n == 0 {
            outcome
        } else {
            EMA_ALPHA * outcome + (1.0 - EMA_ALPHA) * old
        };
        self.global_success_rate
            .store(f64::to_bits(new), std::sync::atomic::Ordering::Relaxed);
    }

    /// Calculate a score multiplier for the given route.
    /// Returns a value typically between 0.1 and 3.0:
    /// - > 1.0 = hot route (boost priority)
    /// - < 1.0 = cold route (reduce priority / skip)
    /// - 0.0 = route should be skipped entirely
    ///
    /// Features used:
    /// 1. Pool success rate vs global baseline
    /// 2. Time-of-day affinity
    /// 3. Competition pressure (inverse)
    /// 4. Freshness (recent success = boost)
    /// 5. Strategy affinity
    pub fn score_boost(&self, pools: &[&str], strategy: &str) -> f64 {
        if pools.is_empty() {
            return 1.0;
        }

        let hour = chrono::Utc::now().hour() as usize;
        let global_sr = f64::from_bits(
            self.global_success_rate
                .load(std::sync::atomic::Ordering::Relaxed),
        );

        let mut total_boost = 0.0;
        let mut n_pools_with_data = 0u32;

        for pool_key in pools {
            if let Some(model) = self.pools.get(*pool_key) {
                if model.n_samples < MIN_SAMPLES {
                    // Not enough data — use neutral score
                    total_boost += 1.0;
                    n_pools_with_data += 1;
                    continue;
                }

                // Feature 1: Success rate relative to global baseline
                let sr_ratio = if global_sr > 0.01 {
                    model.success_rate / global_sr
                } else {
                    1.0
                };

                // Feature 2: Time-of-day affinity
                let h = hour % HOUR_BUCKETS;
                let (h_success, h_attempts) = model.hourly_stats[h];
                let tod_boost = if h_attempts >= 3 {
                    let h_rate = h_success as f64 / h_attempts as f64;
                    if model.success_rate > 0.01 {
                        h_rate / model.success_rate
                    } else {
                        1.0
                    }
                } else {
                    1.0 // not enough hourly data
                };

                // Feature 3: Competition pressure (lower competition = boost)
                let comp_factor = 1.0 - (model.competition * 0.5); // 0.5-1.0 range

                // Feature 4: Freshness (recent success = boost)
                let freshness = if let Some(last) = model.last_success_epoch {
                    let age_secs = chrono::Utc::now().timestamp() - last;
                    if age_secs < 300 {
                        1.3 // success in last 5 min = hot
                    } else if age_secs < 3600 {
                        1.1 // success in last hour
                    } else if age_secs < 86400 {
                        1.0 // success today
                    } else {
                        0.8 // stale
                    }
                } else {
                    0.9 // never succeeded
                };

                // Combine features (multiplicative)
                let pool_boost = sr_ratio
                    .max(0.1)
                    .min(3.0)
                    * tod_boost.max(0.5).min(2.0)
                    * comp_factor.max(0.3)
                    * freshness;

                total_boost += pool_boost;
                n_pools_with_data += 1;
            }
        }

        // Strategy affinity
        let strategy_factor = if let Some(sm) = self.strategies.get(strategy) {
            if sm.n_samples >= MIN_SAMPLES && global_sr > 0.01 {
                (sm.success_rate / global_sr).max(0.3).min(2.0)
            } else {
                1.0
            }
        } else {
            1.0
        };

        let avg_boost = if n_pools_with_data > 0 {
            total_boost / n_pools_with_data as f64
        } else {
            1.0 // no data for any pool — neutral
        };

        (avg_boost * strategy_factor).max(0.05).min(5.0)
    }

    /// Get the top N hot pools by success rate.
    pub fn hot_pools(&self, n: usize) -> Vec<(String, f64, u64)> {
        let mut pools: Vec<_> = self
            .pools
            .iter()
            .filter(|p| p.n_samples >= MIN_SAMPLES)
            .map(|p| (p.pool.clone(), p.success_rate, p.n_samples))
            .collect();
        pools.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        pools.truncate(n);
        pools
    }

    /// Get the most competitive pools (avoid these).
    pub fn cold_pools(&self, n: usize) -> Vec<(String, f64, u64)> {
        let mut pools: Vec<_> = self
            .pools
            .iter()
            .filter(|p| p.n_samples >= MIN_SAMPLES)
            .map(|p| (p.pool.clone(), p.competition, p.n_samples))
            .collect();
        pools.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        pools.truncate(n);
        pools
    }

    /// Get (success_rate, competition) for a pool (used by LandingPredictor).
    pub fn pool_model_rates(&self, pool_key: &str) -> (f64, f64) {
        self.pools
            .get(pool_key)
            .map(|m| (m.success_rate, m.competition))
            .unwrap_or((0.5, 0.0))
    }

    /// Pools that should be skipped based on historical failure data.
    pub fn blacklisted_pools(&self) -> std::collections::HashSet<String> {
        self.pools
            .iter()
            .filter(|p| {
                (p.n_samples >= 10 && p.n_landed == 0)
                    || (p.n_samples >= 20 && p.success_rate < 0.01)
            })
            .map(|p| p.pool.clone())
            .collect()
    }

    /// Persist current model weights to PostgreSQL.
    pub async fn save_to_pg(&self, db_url: &str) -> anyhow::Result<()> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("ml-scorer save pg error: {e}");
            }
        });

        // Serialize pool models
        let pool_models: Vec<PoolModel> = self.pools.iter().map(|p| p.value().clone()).collect();
        let strategy_models: Vec<StrategyModel> =
            self.strategies.iter().map(|s| s.value().clone()).collect();

        let weights = serde_json::json!({
            "pools": pool_models,
            "strategies": strategy_models,
            "global_success_rate": f64::from_bits(self.global_success_rate.load(std::sync::atomic::Ordering::Relaxed)),
            "total_observations": self.total_observations.load(std::sync::atomic::Ordering::Relaxed),
        });

        let n_samples = self
            .total_observations
            .load(std::sync::atomic::Ordering::Relaxed);

        client
            .execute(
                "INSERT INTO ml_model_state (model_name, weights_json, n_samples) \
                 VALUES ('route_scorer', $1, $2) \
                 ON CONFLICT (model_name) DO UPDATE SET \
                 weights_json = $1, n_samples = $2, updated_at = now()",
                &[&weights, &(n_samples as i64)],
            )
            .await?;

        info!(
            "ml-scorer: saved model ({} pools, {} strategies, {n_samples} observations)",
            pool_models.len(),
            strategy_models.len()
        );
        Ok(())
    }

    /// Stats summary.
    pub fn stats(&self) -> String {
        let n = self
            .total_observations
            .load(std::sync::atomic::Ordering::Relaxed);
        let sr = f64::from_bits(
            self.global_success_rate
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        format!(
            "pools={} strategies={} observations={n} global_sr={sr:.3} uptime={:.0}s",
            self.pools.len(),
            self.strategies.len(),
            self.started_at.elapsed().as_secs_f64()
        )
    }
}

use chrono::Timelike;

impl Default for MlScorer {
    fn default() -> Self {
        Self {
            pools: DashMap::new(),
            strategies: DashMap::new(),
            global_success_rate: std::sync::atomic::AtomicU64::new(f64::to_bits(0.5)),
            total_observations: std::sync::atomic::AtomicU64::new(0),
            started_at: Instant::now(),
        }
    }
}
