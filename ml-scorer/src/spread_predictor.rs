//! spread_predictor.rs — Predicts P(spread survives 400ms slot time).
//!
//! Uses a logistic regression model trained and stored in
//! `ml_model_state` (model_name = 'spread_survival_predictor').
//!
//! 8 features, standardized via stored mean/std, inference ~50ns (dot product + sigmoid).

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const N_FEATURES: usize = 8;

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

pub struct SpreadPredictor {
    weights: Vec<f64>,
    bias: f64,
    means: Vec<f64>,
    stds: Vec<f64>,
    loaded: bool,
}

impl SpreadPredictor {
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
                "SELECT weights_json FROM ml_model_state WHERE model_name = 'spread_survival_predictor'",
                &[],
            )
            .await?;

        let Some(row) = row else {
            anyhow::bail!("no model found for 'spread_survival_predictor' in ml_model_state");
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
            "spread-predictor: loaded model from PG"
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

    /// Score a route. Returns P(spread survives 400ms slot time).
    ///
    /// Features:
    ///   0. spread_bps      — spread in basis points
    ///   1. min_reserve_log  — ln(min reserve across hops)
    ///   2. fee_total_bps    — total fees in basis points across all hops
    ///   3. dex_combo        — encoded DEX combination (u8)
    ///   4. hour_utc         — hour of day (0-23)
    ///   5. n_hops           — number of hops in the route
    ///   6. is_pumpswap      — 1.0 if any hop is PumpSwap, else 0.0
    ///   7. pass_rate        — historical pass rate for this route type
    ///
    /// If the model is not loaded, returns 0.5 (neutral — do not filter).
    pub fn predict(
        &self,
        spread_bps: f64,
        min_reserve_log: f64,
        fee_total_bps: f64,
        dex_combo: u8,
        hour_utc: u32,
        n_hops: u32,
        is_pumpswap: bool,
        pass_rate: f64,
    ) -> f64 {
        if !self.loaded {
            return 0.5;
        }

        let features = [
            spread_bps,
            min_reserve_log,
            fee_total_bps,
            dex_combo as f64,
            hour_utc as f64,
            n_hops as f64,
            if is_pumpswap { 1.0 } else { 0.0 },
            pass_rate,
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
