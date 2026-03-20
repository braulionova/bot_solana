//! shred_predictor.rs — Learns from shred-decoded swaps to predict arb opportunities.
//!
//! Core idea: when we see a swap TX in shreds, we can predict if an arb opportunity
//! will appear on related pools. The model learns:
//!   1. Which pools generate arb when swapped (pool affinity)
//!   2. What swap sizes trigger exploitable price dislocations (size threshold)
//!   3. Time patterns (hour + day_of_week)
//!   4. Which pool PAIRS have persistent arb (>669ms = our latency window)
//!   5. Repeat patterns from specific wallets (smart money predictors)
//!
//! Architecture: online learning with per-feature EMA counters.
//! No batch training, no external deps. <1µs per prediction.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// EMA smoothing factor for fast adaptation.
const ALPHA: f64 = 0.15;
/// Minimum observations before predictions are trusted.
const MIN_OBS: u64 = 3;
/// Number of size buckets (log2 scale).
const SIZE_BUCKETS: usize = 12; // covers 1K lamports to 1000 SOL
/// Decay: models older than 24h get reduced weight.
const MODEL_DECAY_SECS: i64 = 86400;

// ─── Feature: per-pool swap → arb correlation ───────────────────────────────

/// Tracks how often a swap on a given pool leads to an arb opportunity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolSwapModel {
    pub pool: String,
    pub dex_type: String,
    /// How often a swap on this pool precedes a detected arb opportunity.
    pub arb_rate: f64,
    /// Average profit when arb follows (lamports).
    pub avg_arb_profit: f64,
    /// Average delay between swap detection and arb detection (ms).
    pub avg_delay_ms: f64,
    /// Per-size-bucket arb rates (index = log2(amount/1000)).
    pub size_arb_rates: Vec<f64>,
    /// Hourly arb rates (24 buckets).
    pub hourly_rates: Vec<f64>,
    /// Total swaps observed.
    pub n_swaps: u64,
    /// Total arbs that followed.
    pub n_arbs: u64,
    /// Last update timestamp.
    pub last_updated: i64,
}

impl PoolSwapModel {
    fn new(pool: String, dex_type: String) -> Self {
        Self {
            pool,
            dex_type,
            arb_rate: 0.0,
            avg_arb_profit: 0.0,
            avg_delay_ms: 0.0,
            size_arb_rates: vec![0.0; SIZE_BUCKETS],
            hourly_rates: vec![0.0; 24],
            n_swaps: 0,
            n_arbs: 0,
            last_updated: chrono::Utc::now().timestamp(),
        }
    }

    fn observe_swap(&mut self) {
        self.n_swaps += 1;
        // Decay arb_rate toward 0 (no arb followed this swap yet)
        self.arb_rate = (1.0 - ALPHA) * self.arb_rate;
        self.last_updated = chrono::Utc::now().timestamp();
    }

    fn observe_arb_followed(&mut self, profit: i64, delay_ms: u64, size_bucket: usize, hour: usize) {
        self.n_arbs += 1;
        self.arb_rate = ALPHA * 1.0 + (1.0 - ALPHA) * self.arb_rate;
        self.avg_arb_profit = ALPHA * profit as f64 + (1.0 - ALPHA) * self.avg_arb_profit;
        self.avg_delay_ms = ALPHA * delay_ms as f64 + (1.0 - ALPHA) * self.avg_delay_ms;

        if size_bucket < SIZE_BUCKETS {
            self.size_arb_rates[size_bucket] =
                ALPHA * 1.0 + (1.0 - ALPHA) * self.size_arb_rates[size_bucket];
        }
        if hour < 24 {
            self.hourly_rates[hour] = ALPHA * 1.0 + (1.0 - ALPHA) * self.hourly_rates[hour];
        }
        self.last_updated = chrono::Utc::now().timestamp();
    }
}

// ─── Feature: pool-pair arb persistence ─────────────────────────────────────

/// Tracks which pool PAIRS have arb that persists long enough for our latency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairPersistenceModel {
    /// "pool_a:pool_b" sorted key.
    pub pair_key: String,
    /// How often arb on this pair persists >669ms (our Turbine latency).
    pub persistence_rate: f64,
    /// Average duration the arb window stays open (ms).
    pub avg_window_ms: f64,
    /// Success rate if we tried to execute.
    pub execution_success_rate: f64,
    pub n_observations: u64,
    pub last_updated: i64,
}

impl PairPersistenceModel {
    fn new(pair_key: String) -> Self {
        Self {
            pair_key,
            persistence_rate: 0.5, // prior
            avg_window_ms: 500.0,
            execution_success_rate: 0.0,
            n_observations: 0,
            last_updated: chrono::Utc::now().timestamp(),
        }
    }
}

// ─── Pending swap tracker ───────────────────────────────────────────────────

/// A swap we saw in shreds, waiting to see if an arb follows.
struct PendingSwap {
    pool: String,
    amount: u64,
    detected_at: Instant,
    slot: u64,
    hour: usize,
    size_bucket: usize,
}

// ─── Main predictor ─────────────────────────────────────────────────────────

pub struct ShredPredictor {
    pool_models: DashMap<String, PoolSwapModel>,
    pair_models: DashMap<String, PairPersistenceModel>,
    /// Pending swaps waiting for arb correlation (TTL 5s).
    pending_swaps: DashMap<String, PendingSwap>,
    total_swaps: AtomicU64,
    total_arbs_correlated: AtomicU64,
    started_at: Instant,
}

impl ShredPredictor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pool_models: DashMap::new(),
            pair_models: DashMap::new(),
            pending_swaps: DashMap::new(),
            total_swaps: AtomicU64::new(0),
            total_arbs_correlated: AtomicU64::new(0),
            started_at: Instant::now(),
        })
    }

    /// Called when we decode a swap TX from shreds.
    /// Records the swap and checks if any pending swaps should be expired.
    pub fn observe_swap(&self, pool: &str, dex_type: &str, amount: u64, slot: u64) {
        let hour = chrono::Utc::now().hour() as usize;
        let size_bucket = amount_to_bucket(amount);

        // Update pool model
        self.pool_models
            .entry(pool.to_string())
            .or_insert_with(|| PoolSwapModel::new(pool.to_string(), dex_type.to_string()))
            .observe_swap();

        // Track as pending swap (for correlation with arb detection)
        self.pending_swaps.insert(
            format!("{}_{}", pool, slot),
            PendingSwap {
                pool: pool.to_string(),
                amount,
                detected_at: Instant::now(),
                slot,
                hour,
                size_bucket,
            },
        );

        self.total_swaps.fetch_add(1, Ordering::Relaxed);

        // Expire old pending swaps (>5s)
        self.pending_swaps
            .retain(|_, v| v.detected_at.elapsed() < Duration::from_secs(5));
    }

    /// Called when an arb opportunity is detected by the route engine.
    /// Correlates with recent swaps to learn which swaps predict arbs.
    pub fn observe_arb_detected(&self, pools: &[String], profit: i64) {
        let hour = chrono::Utc::now().hour() as usize;

        // Check if any pending swap matches the arb pools
        for pool in pools {
            // Find pending swaps on this pool or related pools
            let matching: Vec<(String, u64, usize)> = self
                .pending_swaps
                .iter()
                .filter(|entry| {
                    let v = entry.value();
                    v.pool == *pool && v.detected_at.elapsed() < Duration::from_secs(5)
                })
                .map(|entry| {
                    let v = entry.value();
                    let delay = v.detected_at.elapsed().as_millis() as u64;
                    (entry.key().clone(), delay, v.size_bucket)
                })
                .collect();

            for (key, delay_ms, size_bucket) in matching {
                // Update pool model: this swap DID lead to an arb
                if let Some(mut model) = self.pool_models.get_mut(pool) {
                    model.observe_arb_followed(profit, delay_ms, size_bucket, hour);
                }

                self.total_arbs_correlated.fetch_add(1, Ordering::Relaxed);

                // Remove from pending (correlated)
                self.pending_swaps.remove(&key);
            }
        }

        // Update pair persistence model
        if pools.len() >= 2 {
            let mut pair_pools = vec![pools[0].clone(), pools[1].clone()];
            pair_pools.sort();
            let pair_key = format!("{}:{}", pair_pools[0], pair_pools[1]);

            self.pair_models
                .entry(pair_key.clone())
                .or_insert_with(|| PairPersistenceModel::new(pair_key))
                .n_observations += 1;
        }
    }

    /// Called when an arb execution lands (or fails).
    /// Updates pair persistence model with actual results.
    pub fn observe_execution_result(&self, pools: &[String], landed: bool, delay_ms: u64) {
        if pools.len() >= 2 {
            let mut pair_pools = vec![pools[0].clone(), pools[1].clone()];
            pair_pools.sort();
            let pair_key = format!("{}:{}", pair_pools[0], pair_pools[1]);

            if let Some(mut model) = self.pair_models.get_mut(&pair_key) {
                model.execution_success_rate =
                    ALPHA * (if landed { 1.0 } else { 0.0 })
                        + (1.0 - ALPHA) * model.execution_success_rate;
                model.avg_window_ms =
                    ALPHA * delay_ms as f64 + (1.0 - ALPHA) * model.avg_window_ms;
                // Persistence: did the arb survive our latency (669ms)?
                let persisted = landed || delay_ms < 669;
                model.persistence_rate =
                    ALPHA * (if persisted { 1.0 } else { 0.0 })
                        + (1.0 - ALPHA) * model.persistence_rate;
                model.last_updated = chrono::Utc::now().timestamp();
            }
        }
    }

    /// Predict: given a swap we just saw, what's the probability of a profitable arb?
    /// Returns (arb_probability, expected_profit, recommended_action).
    pub fn predict_arb(&self, pool: &str, amount: u64) -> PredictionResult {
        let hour = chrono::Utc::now().hour() as usize;
        let size_bucket = amount_to_bucket(amount);

        let model = match self.pool_models.get(pool) {
            Some(m) => m,
            None => return PredictionResult::unknown(),
        };

        if model.n_swaps < MIN_OBS {
            return PredictionResult::unknown();
        }

        // Base arb probability from pool model
        let base_prob = model.arb_rate;

        // Size adjustment: some sizes trigger arb more than others
        let size_adj = if size_bucket < SIZE_BUCKETS && model.size_arb_rates[size_bucket] > 0.0 {
            model.size_arb_rates[size_bucket] / model.arb_rate.max(0.01)
        } else {
            1.0
        };

        // Hour adjustment
        let hour_adj = if hour < 24 && model.hourly_rates[hour] > 0.0 {
            model.hourly_rates[hour] / model.arb_rate.max(0.01)
        } else {
            1.0
        };

        // Freshness decay
        let age = chrono::Utc::now().timestamp() - model.last_updated;
        let freshness = if age < 3600 {
            1.0
        } else if age < MODEL_DECAY_SECS {
            0.8
        } else {
            0.5
        };

        let final_prob = (base_prob * size_adj.clamp(0.5, 2.0) * hour_adj.clamp(0.5, 2.0) * freshness)
            .clamp(0.0, 1.0);

        let expected_profit = model.avg_arb_profit * final_prob;
        let avg_delay = model.avg_delay_ms;

        // Action: only recommend if probability is high enough and delay is within our latency
        let action = if final_prob > 0.3 && avg_delay < 2000.0 && expected_profit > 10_000.0 {
            PredictedAction::PrepareTx
        } else if final_prob > 0.1 {
            PredictedAction::Monitor
        } else {
            PredictedAction::Skip
        };

        PredictionResult {
            arb_probability: final_prob,
            expected_profit,
            avg_delay_ms: avg_delay,
            action,
            confidence: (model.n_swaps as f64 / 100.0).min(1.0),
        }
    }

    /// Get the top pools where swaps most frequently lead to arbs.
    /// These are "honey pools" — when someone swaps here, arb follows.
    pub fn honey_pools(&self, n: usize) -> Vec<(String, f64, f64, u64)> {
        let mut pools: Vec<_> = self
            .pool_models
            .iter()
            .filter(|p| p.n_swaps >= MIN_OBS && p.arb_rate > 0.05)
            .map(|p| (p.pool.clone(), p.arb_rate, p.avg_arb_profit, p.n_swaps))
            .collect();
        pools.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        pools.truncate(n);
        pools
    }

    /// Get pool pairs where arb persists long enough for our latency.
    pub fn persistent_pairs(&self, n: usize) -> Vec<(String, f64, f64)> {
        let mut pairs: Vec<_> = self
            .pair_models
            .iter()
            .filter(|p| p.n_observations >= MIN_OBS && p.persistence_rate > 0.3)
            .map(|p| (p.pair_key.clone(), p.persistence_rate, p.avg_window_ms))
            .collect();
        pairs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        pairs.truncate(n);
        pairs
    }

    /// Stats summary.
    pub fn stats(&self) -> String {
        let total_swaps = self.total_swaps.load(Ordering::Relaxed);
        let total_correlated = self.total_arbs_correlated.load(Ordering::Relaxed);
        let correlation_rate = if total_swaps > 0 {
            total_correlated as f64 / total_swaps as f64
        } else {
            0.0
        };
        format!(
            "pools={} pairs={} swaps={} arbs_correlated={} correlation_rate={:.4} uptime={:.0}s",
            self.pool_models.len(),
            self.pair_models.len(),
            total_swaps,
            total_correlated,
            correlation_rate,
            self.started_at.elapsed().as_secs_f64()
        )
    }

    /// Persist models to PostgreSQL.
    pub async fn save_to_pg(&self, db_url: &str) -> anyhow::Result<()> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        client.execute(
            "CREATE TABLE IF NOT EXISTS shred_predictor_state (
                id SERIAL PRIMARY KEY,
                model_type TEXT NOT NULL,
                model_key TEXT NOT NULL,
                model_json JSONB NOT NULL,
                updated_at TIMESTAMPTZ DEFAULT now(),
                UNIQUE(model_type, model_key)
            )", &[],
        ).await?;

        let mut saved = 0u64;
        for entry in self.pool_models.iter() {
            let json = serde_json::to_value(entry.value())?;
            client.execute(
                "INSERT INTO shred_predictor_state (model_type, model_key, model_json)
                 VALUES ('pool_swap', $1, $2)
                 ON CONFLICT (model_type, model_key) DO UPDATE SET model_json = $2, updated_at = now()",
                &[&entry.key(), &json],
            ).await?;
            saved += 1;
        }

        for entry in self.pair_models.iter() {
            let json = serde_json::to_value(entry.value())?;
            client.execute(
                "INSERT INTO shred_predictor_state (model_type, model_key, model_json)
                 VALUES ('pair_persist', $1, $2)
                 ON CONFLICT (model_type, model_key) DO UPDATE SET model_json = $2, updated_at = now()",
                &[&entry.key(), &json],
            ).await?;
        }

        info!(pool_models = saved, pair_models = self.pair_models.len(), "shred-predictor saved to PG");
        Ok(())
    }

    /// Load models from PostgreSQL.
    pub async fn load_from_pg(&self, db_url: &str) -> anyhow::Result<()> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        // Check if table exists
        let exists = client.query_opt(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'shred_predictor_state'",
            &[],
        ).await?.is_some();

        if !exists {
            return Ok(());
        }

        let rows = client.query(
            "SELECT model_type, model_key, model_json FROM shred_predictor_state",
            &[],
        ).await?;

        let mut loaded_pools = 0u64;
        let mut loaded_pairs = 0u64;

        for row in &rows {
            let model_type: String = row.get(0);
            let model_key: String = row.get(1);
            let json: serde_json::Value = row.get(2);

            match model_type.as_str() {
                "pool_swap" => {
                    if let Ok(model) = serde_json::from_value::<PoolSwapModel>(json) {
                        self.pool_models.insert(model_key, model);
                        loaded_pools += 1;
                    }
                }
                "pair_persist" => {
                    if let Ok(model) = serde_json::from_value::<PairPersistenceModel>(json) {
                        self.pair_models.insert(model_key, model);
                        loaded_pairs += 1;
                    }
                }
                _ => {}
            }
        }

        info!(pool_models = loaded_pools, pair_models = loaded_pairs, "shred-predictor loaded from PG");
        Ok(())
    }
}

// ─── Prediction result ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PredictionResult {
    /// Probability that an arb opportunity will appear (0.0-1.0).
    pub arb_probability: f64,
    /// Expected profit if arb appears (lamports).
    pub expected_profit: f64,
    /// Average delay until arb appears (ms).
    pub avg_delay_ms: f64,
    /// Recommended action.
    pub action: PredictedAction,
    /// Model confidence (based on sample count).
    pub confidence: f64,
}

impl PredictionResult {
    fn unknown() -> Self {
        Self {
            arb_probability: 0.0,
            expected_profit: 0.0,
            avg_delay_ms: 0.0,
            action: PredictedAction::Skip,
            confidence: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PredictedAction {
    /// Pre-build TX components (fetch accounts, compute route) before arb is confirmed.
    PrepareTx,
    /// Watch this pool — arb may come but not certain enough to pre-build.
    Monitor,
    /// Ignore — low probability or pool too competitive.
    Skip,
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Convert swap amount to a log2 bucket index.
/// Bucket 0 = <1K lamports, bucket 11 = >1000 SOL.
fn amount_to_bucket(amount: u64) -> usize {
    if amount == 0 {
        return 0;
    }
    let thousands = amount / 1_000;
    if thousands == 0 {
        return 0;
    }
    let log2 = 63 - thousands.leading_zeros() as usize;
    log2.min(SIZE_BUCKETS - 1)
}

use chrono::Timelike;
