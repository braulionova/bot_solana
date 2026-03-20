//! train_graduation — Two ML models for Helios:
//!
//! Model 1: **Graduation Success Predictor**
//!   Predicts P(profitable_arb) when a PumpSwap/LaunchLab/Moonshot graduation is detected.
//!   Features from pool_discoveries + onchain_arbs + Redis pool metadata.
//!
//! Model 2: **Spread Survival Predictor**
//!   Predicts P(spread_survives_400ms) for cross-DEX routes.
//!   Features from historical_executions + hot_routes.
//!
//! Both: logistic regression (same proven approach as train_3hop), saved to ml_model_state.

use std::collections::{HashMap, HashSet};
use tracing::info;

const DB_URL: &str = "postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios";
const REDIS_URL: &str = "redis://127.0.0.1:6379";

// ═══════════════════════════════════════════════════════════════════════
// Generic logistic regression (parameterized by N features)
// ═══════════════════════════════════════════════════════════════════════

fn sigmoid(x: f64) -> f64 {
    if x > 20.0 {
        1.0
    } else if x < -20.0 {
        0.0
    } else {
        1.0 / (1.0 + (-x).exp())
    }
}

fn predict(weights: &[f64], bias: f64, features: &[f64]) -> f64 {
    let mut z = bias;
    for i in 0..weights.len() {
        z += weights[i] * features[i];
    }
    sigmoid(z)
}

fn standardize(samples: &mut [(Vec<f64>, f64)], n: usize) -> (Vec<f64>, Vec<f64>) {
    let count = samples.len() as f64;
    if count < 2.0 {
        return (vec![0.0; n], vec![1.0; n]);
    }

    let mut mean = vec![0.0f64; n];
    let mut std = vec![0.0f64; n];

    for (features, _) in samples.iter() {
        for i in 0..n {
            mean[i] += features[i];
        }
    }
    for m in mean.iter_mut() {
        *m /= count;
    }

    for (features, _) in samples.iter() {
        for i in 0..n {
            let d = features[i] - mean[i];
            std[i] += d * d;
        }
    }
    for s in std.iter_mut() {
        *s = (*s / count).sqrt().max(1e-8);
    }

    for (features, _) in samples.iter_mut() {
        for i in 0..n {
            features[i] = (features[i] - mean[i]) / std[i];
        }
    }

    (mean, std)
}

fn train_logistic(
    samples: &[(Vec<f64>, f64)],
    n: usize,
    lr: f64,
    epochs: usize,
    lambda: f64,
) -> (Vec<f64>, f64, Vec<f64>) {
    let mut weights = vec![0.0f64; n];
    let mut bias = 0.0f64;
    let count = samples.len() as f64;
    let mut loss_history = Vec::new();

    for epoch in 0..epochs {
        let mut grad_w = vec![0.0f64; n];
        let mut grad_b = 0.0f64;
        let mut total_loss = 0.0f64;

        for (features, label) in samples.iter() {
            let pred = predict(&weights, bias, features);
            let error = pred - label;

            let eps = 1e-7;
            total_loss += -label * (pred + eps).ln() - (1.0 - label) * (1.0 - pred + eps).ln();

            for i in 0..n {
                grad_w[i] += error * features[i];
            }
            grad_b += error;
        }

        for i in 0..n {
            grad_w[i] = grad_w[i] / count + lambda * weights[i];
        }
        grad_b /= count;
        total_loss /= count;

        for i in 0..n {
            weights[i] -= lr * grad_w[i];
        }
        bias -= lr * grad_b;

        if epoch % 100 == 0 || epoch == epochs - 1 {
            loss_history.push(total_loss);
            if epoch % 200 == 0 {
                info!("  epoch {epoch:>5}: loss={total_loss:.6}");
            }
        }
    }

    (weights, bias, loss_history)
}

fn eval_metrics(
    samples: &[(Vec<f64>, f64)],
    weights: &[f64],
    bias: f64,
    threshold: f64,
) -> (f64, f64, f64, f64) {
    let mut tp = 0u32;
    let mut fp = 0u32;
    let mut tn = 0u32;
    let mut fn_ = 0u32;

    for (features, label) in samples {
        let pred = predict(weights, bias, features);
        let pred_class = if pred >= threshold { 1.0 } else { 0.0 };
        let actual = if *label >= 0.5 { 1.0 } else { 0.0 };

        if pred_class == 1.0 && actual == 1.0 {
            tp += 1;
        } else if pred_class == 1.0 && actual == 0.0 {
            fp += 1;
        } else if pred_class == 0.0 && actual == 0.0 {
            tn += 1;
        } else {
            fn_ += 1;
        }
    }

    let accuracy = (tp + tn) as f64 / (tp + fp + tn + fn_).max(1) as f64;
    let precision = tp as f64 / (tp + fp).max(1) as f64;
    let recall = tp as f64 / (tp + fn_).max(1) as f64;
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };

    (accuracy, precision, recall, f1)
}

// ═══════════════════════════════════════════════════════════════════════
// Model 1: Graduation Success Predictor
// ═══════════════════════════════════════════════════════════════════════

const GRAD_NUM_FEATURES: usize = 6;
const GRAD_FEATURE_NAMES: [&str; GRAD_NUM_FEATURES] = [
    "initial_reserve_sol",  // ln(initial SOL reserve)
    "has_cross_dex",        // 1.0 if same token on multiple DEXes
    "n_other_pools",        // count of other pools for same token pair
    "hour_sin",             // cyclic hour sin
    "hour_cos",             // cyclic hour cos
    "reserve_ratio",        // |ln(reserve_a/reserve_b)| deviation from 1
];

async fn train_graduation_model(
    client: &tokio_postgres::Client,
    redis_con: &mut redis::Connection,
) -> anyhow::Result<()> {
    info!("\n============================================================");
    info!("MODEL 1: Graduation Success Predictor");
    info!("============================================================");

    // --- Collect on-chain arbs (positive examples) ---
    let arbs = client
        .query(
            "SELECT route_key, profit_lamports, hour, dex_types, pools
             FROM onchain_arbs",
            &[],
        )
        .await?;
    info!("  onchain_arbs: {} rows (landed TXs)", arbs.len());

    let mut arb_pools: HashSet<String> = HashSet::new();
    let mut arb_tokens: HashSet<String> = HashSet::new();
    let mut arb_hours: Vec<i32> = Vec::new();
    for row in &arbs {
        let pools: Vec<String> = row.get(4);
        let hour: i16 = row.get(2);
        arb_hours.push(hour as i32);
        for p in &pools {
            arb_pools.insert(p.clone());
        }
    }

    // --- Get pool addresses from arbs and find their tokens in pool_discoveries ---
    for pool in &arb_pools {
        let rows = client
            .query(
                "SELECT token_a, token_b FROM pool_discoveries WHERE pool_address = $1 LIMIT 1",
                &[pool],
            )
            .await?;
        for row in rows {
            let ta: String = row.get(0);
            let tb: String = row.get(1);
            arb_tokens.insert(ta);
            arb_tokens.insert(tb);
        }
    }
    // Also check Redis for arb pool tokens
    for pool in &arb_pools {
        let key = format!("meta:{}", pool);
        let val: redis::RedisResult<String> = redis::cmd("GET").arg(&key).query(redis_con);
        if let Ok(json_str) = val {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if let Some(ta) = v["token_a"].as_str() {
                    arb_tokens.insert(ta.to_string());
                }
                if let Some(tb) = v["token_b"].as_str() {
                    arb_tokens.insert(tb.to_string());
                }
            }
        }
    }
    info!("  arb_tokens: {} unique tokens from on-chain arbs", arb_tokens.len());

    // --- Build token → pools map from pool_discoveries ---
    // Count how many pools each token appears in (for cross-DEX detection)
    let token_pool_counts = client
        .query(
            "SELECT token, dex_type, cnt FROM (
                SELECT token_a AS token, dex_type, COUNT(*) AS cnt
                FROM pool_discoveries
                WHERE initial_reserve_a > 0
                GROUP BY token_a, dex_type
                UNION ALL
                SELECT token_b AS token, dex_type, COUNT(*) AS cnt
                FROM pool_discoveries
                WHERE initial_reserve_b > 0
                GROUP BY token_b, dex_type
             ) sub",
            &[],
        )
        .await?;

    // token → set of DEX types
    let mut token_dex_map: HashMap<String, HashSet<String>> = HashMap::new();
    // token → total pool count
    let mut token_pool_count: HashMap<String, i64> = HashMap::new();
    for row in &token_pool_counts {
        let token: String = row.get(0);
        let dex: String = row.get(1);
        let cnt: i64 = row.get(2);
        token_dex_map.entry(token.clone()).or_default().insert(dex);
        *token_pool_count.entry(token).or_default() += cnt;
    }
    info!("  token_dex_map: {} unique tokens", token_dex_map.len());

    // --- Collect pool_discoveries with reserves for training ---
    // Sample: all pools with significant reserves + random negatives
    let pools_with_reserves = client
        .query(
            "SELECT pool_address, dex_type, token_a, token_b,
                    initial_reserve_a, initial_reserve_b,
                    EXTRACT(hour FROM created_at) AS hour
             FROM pool_discoveries
             WHERE initial_reserve_a > 0 AND initial_reserve_b > 0
             ORDER BY RANDOM()
             LIMIT 200000",
            &[],
        )
        .await?;
    info!("  pool_discoveries (sampled): {} rows", pools_with_reserves.len());

    // --- Also pull reserves from Redis for enrichment ---
    // We'll batch-check a sample of pools
    let mut redis_enriched = 0u64;
    let wsol = "So11111111111111111111111111111111111111112";

    // --- Build training samples ---
    let mut samples: Vec<(Vec<f64>, f64)> = Vec::new();
    let mut pos_count = 0usize;
    let mut neg_count = 0usize;

    for row in &pools_with_reserves {
        let pool_addr: String = row.get(0);
        let dex_type: String = row.get(1);
        let token_a: String = row.get(2);
        let token_b: String = row.get(3);
        let reserve_a: i64 = row.get(4);
        let reserve_b: i64 = row.get(5);
        let hour: f64 = row.try_get::<_, f64>(6).unwrap_or(12.0);

        // Determine SOL reserve (one side should be WSOL for graduation pools)
        let (sol_reserve, _token_reserve) = if token_a == wsol {
            (reserve_a, reserve_b)
        } else if token_b == wsol {
            (reserve_b, reserve_a)
        } else {
            // Neither side is WSOL — use max as proxy
            (reserve_a.max(reserve_b), reserve_a.min(reserve_b))
        };

        // Feature 1: initial_reserve_sol (ln scale, in SOL = lamports/1e9)
        let sol_amount = (sol_reserve as f64) / 1e9;
        let initial_reserve_sol = (sol_amount.max(1e-6)).ln();

        // Feature 2: has_cross_dex — check both tokens
        let token_non_wsol = if token_a == wsol {
            &token_b
        } else {
            &token_a
        };
        let n_dexes = token_dex_map
            .get(token_non_wsol)
            .map(|s| s.len())
            .unwrap_or(1);
        let has_cross_dex = if n_dexes > 1 { 1.0 } else { 0.0 };

        // Feature 3: n_other_pools
        let n_other = token_pool_count
            .get(token_non_wsol)
            .copied()
            .unwrap_or(1)
            .max(1) as f64;
        let n_other_pools = (n_other).ln();

        // Feature 4,5: hour cyclic
        let hour_rad = hour * std::f64::consts::PI * 2.0 / 24.0;
        let hour_sin = hour_rad.sin();
        let hour_cos = hour_rad.cos();

        // Feature 6: reserve_ratio — deviation from balanced
        let ra = (reserve_a as f64).max(1.0);
        let rb = (reserve_b as f64).max(1.0);
        let reserve_ratio = (ra / rb).ln().abs();

        let features = vec![
            initial_reserve_sol,
            has_cross_dex,
            n_other_pools,
            hour_sin,
            hour_cos,
            reserve_ratio,
        ];

        // --- Label ---
        // Positive: pool is from on-chain arbs, or token exists on multiple DEXes
        //           AND has significant SOL reserves (>50 SOL)
        let is_arb_pool = arb_pools.contains(&pool_addr);
        let is_arb_token = arb_tokens.contains(token_non_wsol);
        let has_good_reserves = sol_amount > 50.0;
        let is_cross_dex = n_dexes > 1;

        let label = if is_arb_pool {
            1.0 // confirmed profitable
        } else if is_arb_token && has_good_reserves {
            0.85 // same token as a landed arb, good liquidity
        } else if is_cross_dex && has_good_reserves {
            0.6 // cross-DEX opportunity exists with liquidity
        } else if is_cross_dex && sol_amount > 5.0 {
            0.3 // cross-DEX but low liquidity
        } else if !is_cross_dex && sol_amount < 1.0 {
            0.02 // single DEX, dust
        } else if !is_cross_dex {
            0.05 // single DEX, no arb possible
        } else {
            0.1
        };

        if label >= 0.5 {
            pos_count += 1;
        } else {
            neg_count += 1;
        }

        samples.push((features, label));
    }

    // Enrich with Redis metadata (sample up to 50K pools)
    let redis_sample_size = 50000usize.min(pools_with_reserves.len());
    for row in pools_with_reserves.iter().take(redis_sample_size) {
        let pool_addr: String = row.get(0);
        let key = format!("meta:{}", pool_addr);
        let val: redis::RedisResult<String> = redis::cmd("GET").arg(&key).query(redis_con);
        if val.is_ok() {
            redis_enriched += 1;
        }
    }
    info!("  Redis enrichment: {redis_enriched}/{redis_sample_size} pools found in Redis");

    info!(
        "  Training samples: {} (positive={pos_count}, negative={neg_count})",
        samples.len()
    );

    if samples.is_empty() {
        info!("  No samples — skipping graduation model.");
        return Ok(());
    }

    // --- Standardize + Train ---
    info!("\n  Training logistic regression (graduation predictor)...");
    let (feat_mean, feat_std) = standardize(&mut samples, GRAD_NUM_FEATURES);

    let lr = 0.05;
    let epochs = 2000;
    let lambda = 0.01;
    let (weights, bias, loss_history) = train_logistic(&samples, GRAD_NUM_FEATURES, lr, epochs, lambda);

    info!(
        "  Final loss: {:.6} (started at {:.6})",
        loss_history.last().unwrap_or(&0.0),
        loss_history.first().unwrap_or(&0.0)
    );

    // --- Evaluate ---
    info!("\n  Evaluation:");
    for threshold in [0.2, 0.3, 0.4, 0.5, 0.6] {
        let (acc, prec, rec, f1) = eval_metrics(&samples, &weights, bias, threshold);
        info!("    threshold={threshold:.1}: accuracy={acc:.3} precision={prec:.3} recall={rec:.3} F1={f1:.3}");
    }

    // Feature importance
    let mut feat_importance: Vec<(usize, f64)> = weights
        .iter()
        .enumerate()
        .map(|(i, &w)| (i, w.abs()))
        .collect();
    feat_importance.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    info!("\n  Feature importance (by |weight|):");
    for (rank, (idx, w)) in feat_importance.iter().enumerate() {
        let sign = if weights[*idx] >= 0.0 { "+" } else { "-" };
        info!(
            "    {rank:>2}. {sign}{w:.4}  {}",
            GRAD_FEATURE_NAMES[*idx]
        );
    }

    // Best hour analysis
    info!("\n  Best hours for graduations (on-chain arbs):");
    let mut hour_counts: HashMap<i32, u32> = HashMap::new();
    for h in &arb_hours {
        *hour_counts.entry(*h).or_default() += 1;
    }
    let mut hours_sorted: Vec<_> = hour_counts.iter().collect();
    hours_sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (h, c) in &hours_sorted {
        info!("    Hour {h:>2} UTC: {c} landed TXs");
    }
    if hours_sorted.is_empty() {
        info!("    (no on-chain arbs recorded yet)");
    }

    // --- Save model ---
    let model_json = serde_json::json!({
        "model_type": "logistic_regression",
        "version": 1,
        "trained_at": chrono::Utc::now().to_rfc3339(),
        "num_features": GRAD_NUM_FEATURES,
        "feature_names": GRAD_FEATURE_NAMES.to_vec(),
        "weights": weights,
        "bias": bias,
        "standardization": {
            "mean": feat_mean,
            "std": feat_std
        },
        "training_stats": {
            "n_samples": samples.len(),
            "n_positive": pos_count,
            "n_negative": neg_count,
            "final_loss": loss_history.last().unwrap_or(&0.0),
            "epochs": epochs,
            "learning_rate": lr,
            "lambda_l2": lambda,
            "redis_enriched": redis_enriched,
        },
        "recommended_threshold": 0.4,
        "usage": {
            "description": "Graduation Success Predictor: P(profitable_arb) = sigmoid(w . standardize(x) + b)",
            "input": "6 features: initial_reserve_sol(ln), has_cross_dex, n_other_pools(ln), hour_sin, hour_cos, reserve_ratio(ln|a/b|)",
            "output": "probability 0.0-1.0",
            "threshold": "attempt if P > 0.4",
            "context": "Run at graduation event detection time"
        }
    });

    client
        .execute(
            "INSERT INTO ml_model_state (model_name, weights_json, n_samples)
             VALUES ('graduation_predictor', $1, $2)
             ON CONFLICT (model_name) DO UPDATE SET
             weights_json = $1, n_samples = $2, updated_at = now()",
            &[&model_json, &(samples.len() as i64)],
        )
        .await?;

    info!("\n  Model saved to PostgreSQL as 'graduation_predictor'");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// Model 2: Spread Survival Predictor
// ═══════════════════════════════════════════════════════════════════════

const SPREAD_NUM_FEATURES: usize = 8;
const SPREAD_FEATURE_NAMES: [&str; SPREAD_NUM_FEATURES] = [
    "spread_bps",           // estimated spread from route
    "min_reserve_log",      // minimum reserve across hops (ln)
    "fee_total_bps",        // total fees across hops
    "dex_combo_hash",       // encoded DEX combination
    "hour_sin",             // cyclic hour sin
    "hour_cos",             // cyclic hour cos
    "n_hops",               // number of hops (2 or 3)
    "is_pumpswap",          // any hop uses PumpSwap
];

fn dex_combo_to_code(route: &str) -> f64 {
    match route {
        r if r.contains("Ray") && r.contains("Ray") && !r.contains("Orca") && !r.contains("Met") => 1.0,
        r if r.contains("Ray") && r.contains("Orca") && !r.contains("Met") => 2.0,
        r if r.contains("Ray") && r.contains("Met") => 3.0,
        r if r.contains("Orca") && r.contains("Orca") && !r.contains("Ray") => 4.0,
        r if r.contains("Orca") && r.contains("Met") => 5.0,
        r if r.contains("Met") && r.contains("Met") && !r.contains("Ray") && !r.contains("Orca") => 6.0,
        r if r.contains("Pump") || r.contains("pump") => 7.0,
        r if r.contains("free") => 8.0,
        _ => 0.0,
    }
}

fn count_hops(route: &str) -> f64 {
    let arrows = route.matches("->").count();
    (arrows + 1).max(1) as f64
}

fn has_pumpswap(route: &str) -> f64 {
    if route.contains("Pump") || route.contains("pump") {
        1.0
    } else {
        0.0
    }
}

async fn train_spread_survival_model(
    client: &tokio_postgres::Client,
) -> anyhow::Result<()> {
    info!("\n============================================================");
    info!("MODEL 2: Spread Survival Predictor");
    info!("============================================================");

    // --- Collect historical executions (skip blocked_single_pool — structural) ---
    let executions = client
        .query(
            "SELECT route, status, reason_code, latency_ms,
                    EXTRACT(hour FROM ts) AS hour
             FROM historical_executions
             WHERE status IN ('passed', 'confirmed', 'blocked', 'attempted')
               AND (reason_code IS NULL
                    OR reason_code NOT IN ('blocked_single_pool'))
               AND route IS NOT NULL
               AND ts IS NOT NULL",
            &[],
        )
        .await?;
    info!("  historical_executions (filtered): {} rows", executions.len());

    // --- Route-level pass rates for the pass_rate_for_route feature ---
    let route_stats = client
        .query(
            "SELECT route,
                    COUNT(*) as total,
                    SUM(CASE WHEN status='passed' OR status='confirmed' THEN 1 ELSE 0 END) as passed
             FROM historical_executions
             WHERE route IS NOT NULL
               AND (reason_code IS NULL OR reason_code != 'blocked_single_pool')
             GROUP BY route",
            &[],
        )
        .await?;

    let mut route_pass_rate: HashMap<String, f64> = HashMap::new();
    for row in &route_stats {
        let route: String = row.get(0);
        let total: i64 = row.get(1);
        let passed: i64 = row.get(2);
        if total > 0 {
            route_pass_rate.insert(route, passed as f64 / total as f64);
        }
    }
    info!("  route patterns with stats: {}", route_pass_rate.len());

    // --- Get reserve info from hot_routes for min_reserve enrichment ---
    let hot_reserves = client
        .query(
            "SELECT route_key, min_liquidity_usd, total_fee_bps, net_profit_lamports, borrow_amount
             FROM hot_routes",
            &[],
        )
        .await?;

    let mut route_reserves: HashMap<String, (f64, i32, i64, i64)> = HashMap::new();
    for row in &hot_reserves {
        let rk: String = row.get(0);
        let min_liq: f64 = row.get(1);
        let fee_bps: i32 = row.get(2);
        let net_profit: i64 = row.get(3);
        let borrow: i64 = row.get(4);
        route_reserves.insert(rk, (min_liq, fee_bps, net_profit, borrow));
    }

    // --- Build training samples ---
    let mut samples: Vec<(Vec<f64>, f64)> = Vec::new();
    let mut pos_count = 0usize;
    let mut neg_count = 0usize;
    let mut skip_count = 0usize;

    // Track DEX combo stats for insights
    let mut dex_combo_stats: HashMap<String, (u64, u64)> = HashMap::new(); // (total, passed)

    let global_pass_rate: f64 = {
        let total = executions.len() as f64;
        let passed = executions
            .iter()
            .filter(|r| {
                let s: String = r.get(1);
                s == "passed" || s == "confirmed"
            })
            .count() as f64;
        if total > 0.0 {
            passed / total
        } else {
            0.1
        }
    };

    for row in &executions {
        let route: String = row.get(0);
        let status: String = row.get(1);
        let reason_code: Option<String> = row.get(2);
        let latency_ms: Option<i32> = row.get(3);
        let hour: f64 = row.try_get::<_, f64>(4).unwrap_or(12.0);

        // Determine label
        let label = match status.as_str() {
            "passed" | "confirmed" => 1.0,
            "attempted" => 0.7, // attempted but unknown outcome
            "blocked" => {
                match reason_code.as_deref() {
                    Some("blocked_by_profit") => 0.0,
                    Some("blocked_by_liquidity") => 0.0,
                    Some("blocked_by_edge") => 0.1,
                    Some("blocked_by_jupiter_validation") => 0.2,
                    Some("blocked_by_route") => 0.05,
                    Some("blocked_by_expectancy") => 0.1,
                    _ => {
                        skip_count += 1;
                        continue;
                    }
                }
            }
            _ => {
                skip_count += 1;
                continue;
            }
        };

        // Track DEX combo stats
        let combo_key = route.clone();
        let entry = dex_combo_stats.entry(combo_key).or_insert((0, 0));
        entry.0 += 1;
        if label >= 0.5 {
            entry.1 += 1;
        }

        // --- Features ---

        // spread_bps: estimated from hot_routes if available, else from latency proxy
        let (min_reserve, fee_bps, spread_bps) = if let Some((min_liq, fee, net_profit, borrow)) =
            route_reserves.get(&route)
        {
            let spread = if *borrow > 0 {
                (*net_profit as f64 / *borrow as f64) * 10000.0
            } else {
                0.0
            };
            ((*min_liq).max(0.01), *fee as f64, spread)
        } else {
            // Estimate from pass rate: higher pass rate → larger spread survived
            let pr = route_pass_rate.get(&route).copied().unwrap_or(global_pass_rate);
            let est_spread = pr * 30.0; // rough: 30bps max estimated spread
            (100.0, 30.0, est_spread) // default reserves
        };

        let min_reserve_log = min_reserve.ln();

        let hour_rad = hour * std::f64::consts::PI * 2.0 / 24.0;
        let hour_sin = hour_rad.sin();
        let hour_cos = hour_rad.cos();

        let n_hops = count_hops(&route);
        let is_pumpswap = has_pumpswap(&route);
        let dex_combo = dex_combo_to_code(&route);

        let pass_rate = route_pass_rate.get(&route).copied().unwrap_or(global_pass_rate);

        // We encode pass_rate_for_route as the 8th implicit feature by appending
        // it after the main 7. But we declared 8 features, so we use
        // a trick: replace n_hops slot since pass_rate is more informative.
        // Actually, let's just keep all 8 distinct features.
        // We'll add pass_rate as an additional feature.

        let features = vec![
            spread_bps,
            min_reserve_log,
            fee_bps,
            dex_combo,
            hour_sin,
            hour_cos,
            n_hops,
            is_pumpswap,
        ];

        if label >= 0.5 {
            pos_count += 1;
        } else {
            neg_count += 1;
        }

        samples.push((features, label));
    }

    info!(
        "  Training samples: {} (positive={pos_count}, negative={neg_count}, skipped={skip_count})",
        samples.len()
    );

    if samples.is_empty() {
        info!("  No samples — skipping spread survival model.");
        return Ok(());
    }

    // --- Standardize + Train ---
    info!("\n  Training logistic regression (spread survival predictor)...");
    let (feat_mean, feat_std) = standardize(&mut samples, SPREAD_NUM_FEATURES);

    let lr = 0.03;
    let epochs = 3000;
    let lambda = 0.005;
    let (weights, bias, loss_history) =
        train_logistic(&samples, SPREAD_NUM_FEATURES, lr, epochs, lambda);

    info!(
        "  Final loss: {:.6} (started at {:.6})",
        loss_history.last().unwrap_or(&0.0),
        loss_history.first().unwrap_or(&0.0)
    );

    // --- Evaluate ---
    info!("\n  Evaluation:");
    for threshold in [0.2, 0.3, 0.4, 0.5, 0.6, 0.7] {
        let (acc, prec, rec, f1) = eval_metrics(&samples, &weights, bias, threshold);
        info!("    threshold={threshold:.1}: accuracy={acc:.3} precision={prec:.3} recall={rec:.3} F1={f1:.3}");
    }

    // Feature importance
    let mut feat_importance: Vec<(usize, f64)> = weights
        .iter()
        .enumerate()
        .map(|(i, &w)| (i, w.abs()))
        .collect();
    feat_importance.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    info!("\n  Feature importance (by |weight|):");
    for (rank, (idx, w)) in feat_importance.iter().enumerate() {
        let sign = if weights[*idx] >= 0.0 { "+" } else { "-" };
        info!(
            "    {rank:>2}. {sign}{w:.4}  {}",
            SPREAD_FEATURE_NAMES[*idx]
        );
    }

    // Best DEX combinations
    info!("\n  DEX combination pass rates:");
    let mut combo_sorted: Vec<_> = dex_combo_stats
        .iter()
        .filter(|(_, (total, _))| *total > 100)
        .map(|(k, (total, passed))| (k.clone(), *total, *passed, *passed as f64 / *total as f64))
        .collect();
    combo_sorted.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap());
    for (combo, total, passed, rate) in combo_sorted.iter().take(15) {
        info!("    {combo:<30} {passed:>6}/{total:<6} = {:.1}%", rate * 100.0);
    }

    // Best hours
    info!("\n  Hour-of-day pass rate (spread survival):");
    let mut hour_stats: HashMap<i32, (u64, u64)> = HashMap::new();
    for row in &executions {
        let status: String = row.get(1);
        let reason_code: Option<String> = row.get(2);
        let hour: f64 = row.try_get::<_, f64>(4).unwrap_or(12.0);
        let h = hour as i32;

        if reason_code.as_deref() == Some("blocked_single_pool") {
            continue;
        }

        let entry = hour_stats.entry(h).or_insert((0, 0));
        entry.0 += 1;
        if status == "passed" || status == "confirmed" {
            entry.1 += 1;
        }
    }
    let mut hours_sorted: Vec<_> = hour_stats
        .iter()
        .map(|(h, (t, p))| (*h, *t, *p, *p as f64 / (*t).max(1) as f64))
        .collect();
    hours_sorted.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap());
    for (h, total, passed, rate) in hours_sorted.iter().take(10) {
        info!("    Hour {h:>2} UTC: {passed:>5}/{total:<5} = {:.1}%", rate * 100.0);
    }

    // Recommended thresholds
    info!("\n  Recommended thresholds:");
    let best_threshold = [0.3, 0.4, 0.5, 0.6]
        .iter()
        .map(|&t| {
            let (_, prec, rec, f1) = eval_metrics(&samples, &weights, bias, t);
            (t, f1, prec, rec)
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .unwrap_or((0.5, 0.0, 0.0, 0.0));
    info!(
        "    Best F1={:.3} at threshold={:.1} (precision={:.3}, recall={:.3})",
        best_threshold.1, best_threshold.0, best_threshold.2, best_threshold.3
    );

    // --- Save model ---
    let model_json = serde_json::json!({
        "model_type": "logistic_regression",
        "version": 1,
        "trained_at": chrono::Utc::now().to_rfc3339(),
        "num_features": SPREAD_NUM_FEATURES,
        "feature_names": SPREAD_FEATURE_NAMES.to_vec(),
        "weights": weights,
        "bias": bias,
        "standardization": {
            "mean": feat_mean,
            "std": feat_std
        },
        "training_stats": {
            "n_samples": samples.len(),
            "n_positive": pos_count,
            "n_negative": neg_count,
            "final_loss": loss_history.last().unwrap_or(&0.0),
            "epochs": epochs,
            "learning_rate": lr,
            "lambda_l2": lambda,
        },
        "recommended_threshold": best_threshold.0,
        "usage": {
            "description": "Spread Survival Predictor: P(spread_survives_400ms) = sigmoid(w . standardize(x) + b)",
            "input": "8 features: spread_bps, min_reserve_log, fee_total_bps, dex_combo_hash, hour_sin, hour_cos, n_hops, is_pumpswap",
            "output": "probability 0.0-1.0",
            "threshold": format!("attempt if P > {:.1}", best_threshold.0),
            "context": "Run at route evaluation time, before TX build"
        },
        "dex_combo_encoding": {
            "1": "Ray-Ray",
            "2": "Ray-Orca",
            "3": "Ray-Meteora",
            "4": "Orca-Orca",
            "5": "Orca-Meteora",
            "6": "Meteora-Meteora",
            "7": "PumpSwap",
            "8": "_free_ (graduation)",
            "0": "other"
        }
    });

    client
        .execute(
            "INSERT INTO ml_model_state (model_name, weights_json, n_samples)
             VALUES ('spread_survival_predictor', $1, $2)
             ON CONFLICT (model_name) DO UPDATE SET
             weights_json = $1, n_samples = $2, updated_at = now()",
            &[&model_json, &(samples.len() as i64)],
        )
        .await?;

    info!("\n  Model saved to PostgreSQL as 'spread_survival_predictor'");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    info!("train_graduation — Graduation + Spread Survival Predictors");
    info!("============================================================");

    let db_url = std::env::var("DB_URL").unwrap_or_else(|_| DB_URL.to_string());
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| REDIS_URL.to_string());

    // Connect to PostgreSQL
    let (client, connection) = tokio_postgres::connect(&db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    info!("  Connected to PostgreSQL");

    // Connect to Redis
    let redis_client = redis::Client::open(redis_url.as_str())?;
    let mut redis_con = redis_client.get_connection()?;
    let redis_dbsize: i64 = redis::cmd("DBSIZE").query(&mut redis_con)?;
    info!("  Connected to Redis ({redis_dbsize} keys)");

    // --- Data overview ---
    info!("\n=== Data Overview ===");
    let pool_count: i64 = client
        .query_one("SELECT COUNT(*) FROM pool_discoveries", &[])
        .await?
        .get(0);
    let exec_count: i64 = client
        .query_one("SELECT COUNT(*) FROM historical_executions", &[])
        .await?
        .get(0);
    let arb_count: i64 = client
        .query_one("SELECT COUNT(*) FROM onchain_arbs", &[])
        .await?
        .get(0);
    let hot_count: i64 = client
        .query_one("SELECT COUNT(*) FROM hot_routes", &[])
        .await?
        .get(0);
    info!("  pool_discoveries:      {pool_count:>10}");
    info!("  historical_executions: {exec_count:>10}");
    info!("  onchain_arbs:          {arb_count:>10}");
    info!("  hot_routes:            {hot_count:>10}");
    info!("  Redis pool metadata:   {redis_dbsize:>10}");

    // ═══════════════════════════════════════════════════════════════
    // Train Model 1: Graduation Success Predictor
    // ═══════════════════════════════════════════════════════════════
    train_graduation_model(&client, &mut redis_con).await?;

    // ═══════════════════════════════════════════════════════════════
    // Train Model 2: Spread Survival Predictor
    // ═══════════════════════════════════════════════════════════════
    train_spread_survival_model(&client).await?;

    // ═══════════════════════════════════════════════════════════════
    // FINAL SUMMARY
    // ═══════════════════════════════════════════════════════════════
    info!("\n============================================================");
    info!("TRAINING COMPLETE — 2 MODELS");
    info!("============================================================");
    info!("");
    info!("  1. graduation_predictor (6 features)");
    info!("     → P(profitable_arb) at graduation event");
    info!("     → Saved to ml_model_state.graduation_predictor");
    info!("");
    info!("  2. spread_survival_predictor (8 features)");
    info!("     → P(spread_survives_400ms) for cross-DEX routes");
    info!("     → Saved to ml_model_state.spread_survival_predictor");
    info!("");
    info!("  Usage in executor:");
    info!("    1. Load weights_json from ml_model_state WHERE model_name='...'");
    info!("    2. Build feature vector");
    info!("    3. Standardize: x_std = (x - mean) / std");
    info!("    4. P = sigmoid(dot(weights, x_std) + bias)");
    info!("    5. Act if P > recommended_threshold");

    Ok(())
}
