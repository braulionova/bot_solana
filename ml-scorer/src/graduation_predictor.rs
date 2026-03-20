//! graduation_predictor.rs — Predicts P(profitable graduation arb).
//!
//! Uses a logistic regression model trained by `train_graduation` and stored in
//! `ml_model_state` (model_name = 'graduation_predictor').
//!
//! 6 features, standardized via stored mean/std, inference ~50ns (dot product + sigmoid).

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const N_FEATURES: usize = 6;

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
    #[serde(default)]
    standardization: Standardization,
    #[serde(default)]
    mean: Vec<f64>,
    #[serde(default, rename = "std")]
    std_dev: Vec<f64>,
    #[serde(default)]
    means: Vec<f64>,
    #[serde(default)]
    stds: Vec<f64>,
}

pub struct GraduationPredictor {
    weights: Vec<f64>,
    bias: f64,
    means: Vec<f64>,
    stds: Vec<f64>,
    loaded: bool,
}

impl GraduationPredictor {
    /// Create an unloaded predictor that always returns 0.5 (neutral).
    pub fn new() -> Self {
        Self {
            weights: Vec::new(),
            bias: 0.0,
            means: Vec::new(),
            stds: Vec::new(),
            loaded: false,
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
                "SELECT weights_json FROM ml_model_state WHERE model_name = 'graduation_predictor'",
                &[],
            )
            .await?;

        let Some(row) = row else {
            anyhow::bail!("no model found for 'graduation_predictor' in ml_model_state");
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
            "graduation-predictor: loaded model from PG"
        );

        Ok(Self {
            weights: mw.weights,
            bias: mw.bias,
            means,
            stds,
            loaded: true,
        })
    }

    /// Whether a trained model was successfully loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Score a graduation event. Returns P(profitable_arb).
    ///
    /// Features:
    ///   0. initial_reserve_sol — initial SOL reserve of the graduated pool
    ///   1. has_cross_dex       — 1.0 if cross-DEX pools exist for this token, else 0.0
    ///   2. n_other_pools       — number of other pools for this token
    ///   3. hour_utc            — hour of day (0-23)
    ///   4. is_pumpswap         — 1.0 if source is PumpSwap, else 0.0
    ///   5. reserve_ratio       — ratio of reserve_a / reserve_b
    ///
    /// If the model is not loaded, returns 0.5 (neutral — do not filter).
    pub fn predict(
        &self,
        initial_reserve_sol: f64,
        has_cross_dex: bool,
        n_other_pools: u32,
        hour_utc: u32,
        is_pumpswap: bool,
        reserve_ratio: f64,
    ) -> f64 {
        if !self.loaded {
            return 0.5;
        }

        let features = [
            initial_reserve_sol,
            if has_cross_dex { 1.0 } else { 0.0 },
            n_other_pools as f64,
            hour_utc as f64,
            if is_pumpswap { 1.0 } else { 0.0 },
            reserve_ratio,
        ];

        self.dot_sigmoid(&features)
    }

    /// Standardize features and compute sigmoid(w . x + bias).
    #[inline]
    fn dot_sigmoid(&self, features: &[f64; N_FEATURES]) -> f64 {
        let mut z = self.bias;
        for i in 0..N_FEATURES {
            let std = if self.stds[i] > 1e-10 { self.stds[i] } else { 1.0 };
            let normalized = (features[i] - self.means[i]) / std;
            z += self.weights[i] * normalized;
        }
        1.0 / (1.0 + (-z).exp())
    }
}
