//! threehop_predictor.rs — Predicts P(land) for 3-hop arbitrage routes.
//!
//! Uses a logistic regression model trained by `train_3hop` and stored in
//! `ml_model_state` (model_name = 'route_3hop_predictor').
//!
//! 15 features, standardized via stored mean/std, inference ~50ns (dot product + sigmoid).

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;
use tracing::{info, warn};

const N_FEATURES: usize = 15;

/// Known fee_bps per DEX program (used to estimate total fees when not available per-hop).
pub fn fee_bps_for_dex(program_id: &str) -> f64 {
    match program_id {
        "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8" => 25.0,  // Raydium AMM V4
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK" => 25.0,  // Raydium CLMM
        "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C" => 25.0,  // Raydium CPMM
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc" => 30.0,  // Orca
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo" => 30.0,  // Meteora DLMM
        "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA" => 25.0,  // PumpSwap
        "SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ" => 4.0,   // Saber
        "FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSR1X4" => 30.0, // Fluxbeam
        _ => 30.0, // conservative default
    }
}

/// Hop-level data passed from the caller (avoids depending on market-engine types).
pub struct HopInfo<'a> {
    pub dex_program: &'a str,
    pub amount_out: u64,
    pub price_impact: f64,
}

/// Standardization parameters nested under "standardization" key.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Standardization {
    #[serde(default)]
    mean: Vec<f64>,
    #[serde(default, rename = "std")]
    std_dev: Vec<f64>,
}

/// Weights loaded from PG.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelWeights {
    weights: Vec<f64>,
    bias: f64,
    #[serde(default)]
    feature_names: Vec<String>,
    // Standardization can be nested under "standardization" key ...
    #[serde(default)]
    standardization: Standardization,
    // ... or flat at top level
    #[serde(default)]
    mean: Vec<f64>,
    #[serde(default, rename = "std")]
    std_dev: Vec<f64>,
    #[serde(default)]
    means: Vec<f64>,
    #[serde(default)]
    stds: Vec<f64>,
}

pub struct ThreeHopPredictor {
    // Weights behind RwLock for online updates
    model: parking_lot::RwLock<ModelParams>,
    feature_names: Vec<String>,
    means: Vec<f64>,
    stds: Vec<f64>,
    loaded: bool,
    // Online learning stats
    observations: std::sync::atomic::AtomicU64,
    successes: std::sync::atomic::AtomicU64,
    // Accumulated gradient for periodic batch update
    grad_accum: parking_lot::Mutex<GradAccum>,
}

struct ModelParams {
    weights: Vec<f64>,
    bias: f64,
}

struct GradAccum {
    weight_grads: Vec<f64>,
    bias_grad: f64,
    count: u64,
}

impl ThreeHopPredictor {
    /// Create an unloaded predictor that always returns 0.5 (neutral).
    pub fn new() -> Self {
        Self {
            model: parking_lot::RwLock::new(ModelParams { weights: Vec::new(), bias: 0.0 }),
            feature_names: Vec::new(),
            means: Vec::new(),
            stds: Vec::new(),
            loaded: false,
            observations: std::sync::atomic::AtomicU64::new(0),
            successes: std::sync::atomic::AtomicU64::new(0),
            grad_accum: parking_lot::Mutex::new(GradAccum {
                weight_grads: Vec::new(),
                bias_grad: 0.0,
                count: 0,
            }),
        }
    }

    /// Load model from PostgreSQL. Call once at startup.
    pub async fn load_from_pg(db_url: &str) -> anyhow::Result<Self> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let row = client
            .query_opt(
                "SELECT weights_json FROM ml_model_state WHERE model_name = 'route_3hop_predictor'",
                &[],
            )
            .await?;

        let Some(row) = row else {
            anyhow::bail!("no model found for 'route_3hop_predictor' in ml_model_state");
        };

        let json: serde_json::Value = row.get(0);
        let mw: ModelWeights = serde_json::from_value(json)?;

        // Accept standardization from nested "standardization" key, or flat "mean"/"std"/"means"/"stds"
        let means = if !mw.standardization.mean.is_empty() {
            mw.standardization.mean
        } else if !mw.means.is_empty() {
            mw.means
        } else {
            mw.mean
        };
        let stds = if !mw.standardization.std_dev.is_empty() {
            mw.standardization.std_dev
        } else if !mw.stds.is_empty() {
            mw.stds
        } else {
            mw.std_dev
        };

        if mw.weights.len() != N_FEATURES {
            anyhow::bail!(
                "expected {} weights, got {}",
                N_FEATURES,
                mw.weights.len()
            );
        }
        if means.len() != N_FEATURES || stds.len() != N_FEATURES {
            anyhow::bail!(
                "mean/std length mismatch: means={}, stds={}, expected={}",
                means.len(),
                stds.len(),
                N_FEATURES
            );
        }

        info!(
            bias = mw.bias,
            n_weights = mw.weights.len(),
            "3hop-predictor: loaded model from PG"
        );

        let n = mw.weights.len();
        Ok(Self {
            model: parking_lot::RwLock::new(ModelParams { weights: mw.weights, bias: mw.bias }),
            feature_names: mw.feature_names,
            means,
            stds,
            loaded: true,
            observations: std::sync::atomic::AtomicU64::new(0),
            successes: std::sync::atomic::AtomicU64::new(0),
            grad_accum: parking_lot::Mutex::new(GradAccum {
                weight_grads: vec![0.0; n],
                bias_grad: 0.0,
                count: 0,
            }),
        })
    }

    /// Whether a trained model was successfully loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Predict P(land) for a route with the given hops.
    ///
    /// Returns 0.0-1.0 probability.  If the model is not loaded or the route has
    /// fewer than 3 hops, returns 0.5 (neutral — do not filter).
    pub fn predict(&self, hops: &[HopInfo<'_>], net_profit: i64, hour_utc: u32) -> f64 {
        if !self.loaded || hops.len() < 3 {
            return 0.5;
        }

        let features = self.build_features(hops, net_profit, hour_utc);
        self.dot_sigmoid(&features)
    }

    /// Build the 15-feature vector from route data.
    fn build_features(&self, hops: &[HopInfo<'_>], net_profit: i64, hour_utc: u32) -> [f64; N_FEATURES] {
        // 1. reserve_depth_log: ln(min amount_out across hops) — proxy for min liquidity
        let min_amount_out = hops.iter().map(|h| h.amount_out).min().unwrap_or(1).max(1);
        let reserve_depth_log = (min_amount_out as f64).ln();

        // 2. fee_total_bps: sum of estimated fee_bps from each hop's DEX
        let fee_total_bps: f64 = hops.iter().map(|h| fee_bps_for_dex(h.dex_program)).sum();

        // 3. dex_diversity: count distinct dex_program values
        let mut dex_set: Vec<&str> = hops.iter().map(|h| h.dex_program).collect();
        dex_set.sort();
        dex_set.dedup();
        let dex_diversity = dex_set.len() as f64;

        // 4. avg_price_impact: mean of hop.price_impact
        let avg_price_impact = if !hops.is_empty() {
            hops.iter().map(|h| h.price_impact).sum::<f64>() / hops.len() as f64
        } else {
            0.0
        };

        // 5. competition_score: 0.5 default (not available per-route)
        let competition_score = 0.5;

        // 6-7. hour_sin, hour_cos: cyclic hour encoding
        let hour_rad = hour_utc as f64 * 2.0 * PI / 24.0;
        let hour_sin = hour_rad.sin();
        let hour_cos = hour_rad.cos();

        // 8. route_uniqueness: 0.5 default
        let route_uniqueness = 0.5;

        // 9. min_pool_liquidity_log: ln(min amount_out) — same as reserve_depth_log
        let min_pool_liquidity_log = reserve_depth_log;

        // 10. pass_rate: 0.34 default (global average from training)
        let pass_rate = 0.34;

        // 11. block_rate_liquidity: 0.1 default
        let block_rate_liquidity = 0.1;

        // 12. block_rate_profit: 0.08 default
        let block_rate_profit = 0.08;

        // 13. net_profit_log: ln(max(net_profit, 1))
        let net_profit_log = (net_profit.max(1) as f64).ln();

        // 14. is_multi_dex: 1.0 if dex_diversity > 1
        let is_multi_dex = if dex_diversity > 1.0 { 1.0 } else { 0.0 };

        // 15. has_landed_before: 0.0 default
        let has_landed_before = 0.0;

        [
            reserve_depth_log,      // 0
            fee_total_bps,          // 1
            dex_diversity,          // 2
            avg_price_impact,       // 3
            competition_score,      // 4
            hour_sin,               // 5
            hour_cos,               // 6
            route_uniqueness,       // 7
            min_pool_liquidity_log, // 8
            pass_rate,              // 9
            block_rate_liquidity,   // 10
            block_rate_profit,      // 11
            net_profit_log,         // 12
            is_multi_dex,           // 13
            has_landed_before,      // 14
        ]
    }

    /// Standardize features and compute sigmoid(w . x + bias).
    #[inline]
    fn dot_sigmoid(&self, features: &[f64; N_FEATURES]) -> f64 {
        let m = self.model.read();
        let mut z = m.bias;
        for i in 0..N_FEATURES.min(m.weights.len()) {
            let std = if self.stds[i] > 1e-10 { self.stds[i] } else { 1.0 };
            let normalized = (features[i] - self.means[i]) / std;
            z += m.weights[i] * normalized;
        }
        1.0 / (1.0 + (-z).exp())
    }

    // ── Online Learning ─────────────────────────────────────────────────

    /// Record an execution outcome for online learning.
    /// `landed`: true if TX confirmed on-chain with profit, false if failed/slippage.
    pub fn observe(&self, hops: &[HopInfo<'_>], net_profit: i64, hour_utc: u32, landed: bool) {
        if !self.loaded || hops.len() < 3 {
            return;
        }
        self.observations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if landed {
            self.successes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let features = self.build_features(hops, net_profit, hour_utc);
        let pred = self.dot_sigmoid(&features);
        let label = if landed { 1.0 } else { 0.0 };
        let error = pred - label; // gradient of cross-entropy for logistic regression

        let mut grad = self.grad_accum.lock();
        for i in 0..N_FEATURES {
            let std = if self.stds[i] > 1e-10 { self.stds[i] } else { 1.0 };
            let normalized = (features[i] - self.means[i]) / std;
            grad.weight_grads[i] += error * normalized;
        }
        grad.bias_grad += error;
        grad.count += 1;
    }

    /// Apply accumulated gradients to update weights (call periodically, e.g. every 5 min).
    /// Learning rate decays with total observations for stability.
    pub fn apply_gradients(&self) {
        let mut grad = self.grad_accum.lock();
        if grad.count == 0 {
            return;
        }
        let total_obs = self.observations.load(std::sync::atomic::Ordering::Relaxed);
        let lr = 0.01 / (1.0 + total_obs as f64 / 100.0);
        let n = grad.count as f64;

        let mut m = self.model.write();
        for i in 0..m.weights.len().min(N_FEATURES) {
            m.weights[i] -= lr * (grad.weight_grads[i] / n);
            grad.weight_grads[i] = 0.0;
        }
        m.bias -= lr * (grad.bias_grad / n);

        let successes = self.successes.load(std::sync::atomic::Ordering::Relaxed);
        info!(
            observations = total_obs,
            batch = grad.count,
            successes,
            lr = format!("{:.6}", lr),
            "3hop-predictor: online update applied"
        );
        grad.bias_grad = 0.0;
        grad.count = 0;
    }

    /// Save updated model back to PG.
    pub async fn save_to_pg(&self, db_url: &str) -> anyhow::Result<()> {
        if !self.loaded {
            return Ok(());
        }
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move { let _ = connection.await; });

        let total = self.observations.load(std::sync::atomic::Ordering::Relaxed);
        let success = self.successes.load(std::sync::atomic::Ordering::Relaxed);
        let m = self.model.read();
        let model_json = serde_json::json!({
            "weights": m.weights,
            "bias": m.bias,
            "feature_names": self.feature_names,
            "standardization": {
                "mean": self.means,
                "std": self.stds,
            },
            "online_stats": {
                "total_observations": total,
                "successes": success,
            },
        });

        client.execute(
            "UPDATE ml_model_state SET weights_json = $1, n_samples = n_samples + $2, updated_at = NOW() \
             WHERE model_name = 'route_3hop_predictor'",
            &[&model_json, &(total as i64)],
        ).await?;

        info!(observations = total, successes = success, "3hop-predictor: saved to PG");
        Ok(())
    }

    /// Stats string for periodic logging.
    pub fn stats(&self) -> String {
        let obs = self.observations.load(std::sync::atomic::Ordering::Relaxed);
        let suc = self.successes.load(std::sync::atomic::Ordering::Relaxed);
        let pending = self.grad_accum.lock().count;
        format!("obs={} success={} pending_grads={} loaded={}", obs, suc, pending, self.loaded)
    }
}
