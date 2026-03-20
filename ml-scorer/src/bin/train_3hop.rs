//! train_3hop — Train a logistic regression model to predict which 3-hop
//! arbitrage routes will succeed given the bot's ~27-87ms detection-to-wire
//! latency.
//!
//! Data sources (PostgreSQL):
//!   - hot_routes (494): pool liquidity, fees, price impact, competition
//!   - historical_executions (141K): pass/block rates per route pattern
//!   - pool_discoveries (283K): reserve sizes, DEX distribution
//!   - onchain_arbs (5): what routes actually landed profitably
//!
//! Model: logistic regression (pure Rust, no external ML crates)
//!   w · x + b  →  sigmoid  →  P(route lands profitably)
//!
//! Output: model weights saved to `ml_model_state` table as JSON.

use std::collections::HashMap;
use tracing::info;

const DB_URL: &str = "postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios";

// Feature indices
const F_RESERVE_DEPTH_LOG: usize = 0;
const F_FEE_TOTAL_BPS: usize = 1;
const F_DEX_DIVERSITY: usize = 2;
const F_AVG_PRICE_IMPACT: usize = 3;
const F_COMPETITION_SCORE: usize = 4;
const F_HOUR_SIN: usize = 5;
const F_HOUR_COS: usize = 6;
const F_ROUTE_UNIQUENESS: usize = 7;
const F_MIN_POOL_LIQ_LOG: usize = 8;
const F_PASS_RATE: usize = 9;
const F_BLOCK_RATE_LIQ: usize = 10;
const F_BLOCK_RATE_PROFIT: usize = 11;
const F_NET_PROFIT_LOG: usize = 12;
const F_IS_MULTI_DEX: usize = 13;
const F_HAS_LANDED_BEFORE: usize = 14;
const NUM_FEATURES: usize = 15;

const FEATURE_NAMES: [&str; NUM_FEATURES] = [
    "reserve_depth_log",
    "fee_total_bps",
    "dex_diversity",
    "avg_price_impact",
    "competition_score",
    "hour_sin",
    "hour_cos",
    "route_uniqueness",
    "min_pool_liq_log",
    "pass_rate",
    "block_rate_liquidity",
    "block_rate_profit",
    "net_profit_log",
    "is_multi_dex",
    "has_landed_before",
];

fn sigmoid(x: f64) -> f64 {
    if x > 20.0 {
        1.0
    } else if x < -20.0 {
        0.0
    } else {
        1.0 / (1.0 + (-x).exp())
    }
}

fn predict(weights: &[f64; NUM_FEATURES], bias: f64, features: &[f64; NUM_FEATURES]) -> f64 {
    let mut z = bias;
    for i in 0..NUM_FEATURES {
        z += weights[i] * features[i];
    }
    sigmoid(z)
}

/// Standardize features: subtract mean, divide by std.
fn standardize(samples: &mut [([f64; NUM_FEATURES], f64)]) -> ([f64; NUM_FEATURES], [f64; NUM_FEATURES]) {
    let n = samples.len() as f64;
    if n < 2.0 {
        return ([0.0; NUM_FEATURES], [1.0; NUM_FEATURES]);
    }

    let mut mean = [0.0f64; NUM_FEATURES];
    let mut std = [0.0f64; NUM_FEATURES];

    for (features, _) in samples.iter() {
        for i in 0..NUM_FEATURES {
            mean[i] += features[i];
        }
    }
    for m in mean.iter_mut() {
        *m /= n;
    }

    for (features, _) in samples.iter() {
        for i in 0..NUM_FEATURES {
            let d = features[i] - mean[i];
            std[i] += d * d;
        }
    }
    for s in std.iter_mut() {
        *s = (*s / n).sqrt().max(1e-8);
    }

    for (features, _) in samples.iter_mut() {
        for i in 0..NUM_FEATURES {
            features[i] = (features[i] - mean[i]) / std[i];
        }
    }

    (mean, std)
}

/// Train logistic regression with gradient descent + L2 regularization.
fn train_logistic(
    samples: &[([f64; NUM_FEATURES], f64)],
    lr: f64,
    epochs: usize,
    lambda: f64,
) -> ([f64; NUM_FEATURES], f64, Vec<f64>) {
    let mut weights = [0.0f64; NUM_FEATURES];
    let mut bias = 0.0f64;
    let n = samples.len() as f64;
    let mut loss_history = Vec::new();

    for epoch in 0..epochs {
        let mut grad_w = [0.0f64; NUM_FEATURES];
        let mut grad_b = 0.0f64;
        let mut total_loss = 0.0f64;

        for (features, label) in samples.iter() {
            let pred = predict(&weights, bias, features);
            let error = pred - label;

            // Binary cross-entropy loss
            let eps = 1e-7;
            total_loss += -label * (pred + eps).ln() - (1.0 - label) * (1.0 - pred + eps).ln();

            for i in 0..NUM_FEATURES {
                grad_w[i] += error * features[i];
            }
            grad_b += error;
        }

        // Average + L2 regularization
        for i in 0..NUM_FEATURES {
            grad_w[i] = grad_w[i] / n + lambda * weights[i];
        }
        grad_b /= n;
        total_loss /= n;

        // Update
        for i in 0..NUM_FEATURES {
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

/// Compute accuracy, precision, recall, F1 at threshold.
fn eval_metrics(
    samples: &[([f64; NUM_FEATURES], f64)],
    weights: &[f64; NUM_FEATURES],
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_url = std::env::var("DB_URL").unwrap_or_else(|_| DB_URL.to_string());
    info!("train_3hop — 3-Hop Route Success Predictor");
    info!("============================================");

    let (client, connection) = tokio_postgres::connect(&db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    // ═══════════════════════════════════════════════════════════════
    // 1. COLLECT DATA FROM PG
    // ═══════════════════════════════════════════════════════════════
    info!("\n=== Phase 1: Collecting Training Data ===");

    // 1a. Hot routes (all 3-hop)
    let hot_routes = client
        .query(
            "SELECT route_key, dex_types, borrow_amount, gross_profit_lamports,
                    net_profit_lamports, min_liquidity_usd, total_fee_bps,
                    price_impact_pct, competition_score, ml_score,
                    route_uniqueness, dex_diversity, pools
             FROM hot_routes",
            &[],
        )
        .await?;
    info!("  hot_routes: {} rows", hot_routes.len());

    // 1b. Historical execution stats per route pattern
    let exec_stats = client
        .query(
            "SELECT route,
                    COUNT(*) as total,
                    SUM(CASE WHEN status='passed' OR status='confirmed' THEN 1 ELSE 0 END) as passed,
                    SUM(CASE WHEN status='confirmed' THEN 1 ELSE 0 END) as confirmed,
                    SUM(CASE WHEN reason_code='blocked_by_liquidity' THEN 1 ELSE 0 END) as blk_liq,
                    SUM(CASE WHEN reason_code='blocked_by_profit' THEN 1 ELSE 0 END) as blk_profit,
                    SUM(CASE WHEN reason_code='blocked_by_edge' THEN 1 ELSE 0 END) as blk_edge,
                    AVG(latency_ms) FILTER (WHERE latency_ms > 0) as avg_lat
             FROM historical_executions
             GROUP BY route",
            &[],
        )
        .await?;
    info!("  historical_executions: {} route patterns", exec_stats.len());

    // Build route → stats map
    let mut route_pass_rate: HashMap<String, f64> = HashMap::new();
    let mut route_blk_liq: HashMap<String, f64> = HashMap::new();
    let mut route_blk_profit: HashMap<String, f64> = HashMap::new();

    let mut total_execs = 0i64;
    let mut total_passed = 0i64;
    let mut total_confirmed = 0i64;

    for row in &exec_stats {
        let route: String = row.get(0);
        let total: i64 = row.get(1);
        let passed: i64 = row.get(2);
        let confirmed: i64 = row.get(3);
        let blk_liq: i64 = row.get(4);
        let blk_profit: i64 = row.get(5);

        total_execs += total;
        total_passed += passed;
        total_confirmed += confirmed;

        let pass_r = if total > 0 {
            (passed + confirmed) as f64 / total as f64
        } else {
            0.0
        };
        let liq_r = if total > 0 {
            blk_liq as f64 / total as f64
        } else {
            0.0
        };
        let prof_r = if total > 0 {
            blk_profit as f64 / total as f64
        } else {
            0.0
        };

        route_pass_rate.insert(route.clone(), pass_r);
        route_blk_liq.insert(route.clone(), liq_r);
        route_blk_profit.insert(route.clone(), prof_r);
    }
    info!(
        "  Execution summary: total={total_execs}, passed={total_passed}, confirmed={total_confirmed}"
    );

    // 1c. On-chain arbs (landed successfully)
    let arbs = client
        .query(
            "SELECT route_key, profit_lamports, hour, dex_types, pools
             FROM onchain_arbs",
            &[],
        )
        .await?;
    info!("  onchain_arbs: {} rows (actual landed TXs)", arbs.len());

    let mut landed_route_keys: HashMap<String, i64> = HashMap::new();
    let mut landed_hours: HashMap<i32, u32> = HashMap::new();
    let mut landed_dex_combos: HashMap<String, u32> = HashMap::new();

    for row in &arbs {
        let route_key: String = row.get(0);
        let profit: i64 = row.get(1);
        let hour: i16 = row.get(2);
        let dex_types: Vec<String> = row.get(3);

        landed_route_keys.insert(route_key, profit);
        *landed_hours.entry(hour as i32).or_default() += 1;
        let dex_combo = dex_types.join("+");
        *landed_dex_combos.entry(dex_combo).or_default() += 1;
    }

    // 1d. Pool stats by DEX type
    let pool_dex_stats = client
        .query(
            "SELECT dex_type, COUNT(*) as cnt,
                    AVG(initial_reserve_a) as avg_ra,
                    AVG(initial_reserve_b) as avg_rb,
                    PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY GREATEST(initial_reserve_a, initial_reserve_b)) as median_max_reserve
             FROM pool_discoveries
             WHERE initial_reserve_a > 0 AND initial_reserve_b > 0
             GROUP BY dex_type",
            &[],
        )
        .await?;

    let mut dex_median_reserve: HashMap<String, f64> = HashMap::new();
    for row in &pool_dex_stats {
        let dex: String = row.get(0);
        let cnt: i64 = row.get(1);
        let median: f64 = row.try_get(4).unwrap_or(0.0);
        dex_median_reserve.insert(dex.clone(), median);
        info!("  Pool stats: {dex} — {cnt} pools, median_max_reserve={median:.0}");
    }

    // ═══════════════════════════════════════════════════════════════
    // 2. BUILD TRAINING SAMPLES
    // ═══════════════════════════════════════════════════════════════
    info!("\n=== Phase 2: Building Feature Vectors ===");

    // We build training data from hot_routes.
    // Label: combination of pass_rate from historical executions + whether route
    // has actually landed on-chain.
    // For routes with no execution history, we use the hot_routes ml_score as proxy.

    let mut samples: Vec<([f64; NUM_FEATURES], f64)> = Vec::new();

    // Global average pass rate as fallback
    let global_pass_rate = if total_execs > 0 {
        total_passed as f64 / total_execs as f64
    } else {
        0.1
    };

    for row in &hot_routes {
        let route_key: String = row.get(0);
        let dex_types: Vec<String> = row.get(1);
        let _borrow_amount: i64 = row.get(2);
        let _gross_profit: i64 = row.get(3);
        let net_profit: i64 = row.get(4);
        let min_liq_usd: f64 = row.get(5);
        let total_fee_bps: i32 = row.get(6);
        let price_impact: f64 = row.get(7);
        let competition: f64 = row.get(8);
        let ml_score: f64 = row.get(9);
        let uniqueness: f64 = row.get(10);
        let dex_div: i16 = row.get(11);
        let pools: Vec<String> = row.get(12);

        // Build a route pattern key that matches historical_executions.route format
        // historical_executions uses patterns like "_free_->_free_" — try to match
        let dex_combo = dex_types.join("->");

        // Look up pass rate — try exact route key first, then dex combo pattern
        let pass_rate = route_pass_rate
            .get(&route_key)
            .or_else(|| route_pass_rate.get(&dex_combo))
            .copied()
            .unwrap_or(global_pass_rate);

        let blk_liq = route_blk_liq
            .get(&route_key)
            .or_else(|| route_blk_liq.get(&dex_combo))
            .copied()
            .unwrap_or(0.3);

        let blk_profit = route_blk_profit
            .get(&route_key)
            .or_else(|| route_blk_profit.get(&dex_combo))
            .copied()
            .unwrap_or(0.2);

        let has_landed = if landed_route_keys.contains_key(&route_key) {
            1.0
        } else {
            // Check if any pool in this route was in a landed arb
            let any_pool_landed = pools.iter().any(|p| {
                landed_route_keys
                    .keys()
                    .any(|k| k.contains(p.as_str()))
            });
            if any_pool_landed {
                0.5
            } else {
                0.0
            }
        };

        // Use hour 14 UTC as default (best hour from onchain_arbs data)
        let hour = 14.0f64;
        let hour_rad = hour * std::f64::consts::PI * 2.0 / 24.0;

        // Min reserve depth from pool discoveries (use min_liq_usd as proxy)
        let reserve_depth_log = (min_liq_usd.max(0.01)).ln();
        let net_profit_log = ((net_profit.max(0) as f64) + 1.0).ln();
        let min_pool_liq_log = (min_liq_usd.max(0.01)).ln();

        let features: [f64; NUM_FEATURES] = [
            reserve_depth_log,                   // F_RESERVE_DEPTH_LOG
            total_fee_bps as f64,                // F_FEE_TOTAL_BPS
            dex_div as f64,                      // F_DEX_DIVERSITY
            price_impact / 3.0,                  // F_AVG_PRICE_IMPACT (per hop)
            competition,                         // F_COMPETITION_SCORE
            hour_rad.sin(),                      // F_HOUR_SIN
            hour_rad.cos(),                      // F_HOUR_COS
            uniqueness,                          // F_ROUTE_UNIQUENESS
            min_pool_liq_log,                    // F_MIN_POOL_LIQ_LOG
            pass_rate,                           // F_PASS_RATE
            blk_liq,                             // F_BLOCK_RATE_LIQ
            blk_profit,                          // F_BLOCK_RATE_PROFIT
            net_profit_log,                      // F_NET_PROFIT_LOG
            if dex_div > 1 { 1.0 } else { 0.0 }, // F_IS_MULTI_DEX
            has_landed,                          // F_HAS_LANDED_BEFORE
        ];

        // Label: blended score
        // Routes that actually landed on-chain get label 1.0
        // Routes with high pass rate and good ml_score get proportional label
        // Routes blocked by liquidity/profit get lower labels
        let label = if landed_route_keys.contains_key(&route_key) {
            1.0
        } else {
            // Blend: pass_rate contributes 40%, ml_score (normalized) 30%,
            //        low block rates 30%
            let ml_norm = (ml_score / 5.0).clamp(0.0, 1.0);
            let block_penalty = 1.0 - (blk_liq * 0.5 + blk_profit * 0.5);
            let raw = pass_rate * 0.4 + ml_norm * 0.3 + block_penalty * 0.3;
            raw.clamp(0.0, 0.95)
        };

        samples.push((features, label));
    }

    // Augment with synthetic negative samples from execution failures
    // to balance the dataset
    let exec_failures = client
        .query(
            "SELECT route, reason_code, COUNT(*) as cnt
             FROM historical_executions
             WHERE status = 'blocked' AND reason_code IN ('blocked_by_liquidity', 'blocked_by_profit', 'blocked_by_edge')
             GROUP BY route, reason_code
             HAVING COUNT(*) > 50
             ORDER BY cnt DESC LIMIT 50",
            &[],
        )
        .await?;

    for row in &exec_failures {
        let _route: String = row.get(0);
        let reason: String = row.get(1);
        let cnt: i64 = row.get(2);

        // Create synthetic negative sample for each common failure pattern
        let blk_liq = if reason == "blocked_by_liquidity" {
            0.8
        } else {
            0.2
        };
        let blk_profit = if reason == "blocked_by_profit" {
            0.8
        } else {
            0.2
        };

        let features: [f64; NUM_FEATURES] = [
            2.0,   // reserve_depth_log (low)
            60.0,  // fee_total_bps (high)
            1.0,   // dex_diversity
            5.0,   // avg_price_impact (high)
            0.8,   // competition_score (high)
            0.0,   // hour_sin
            1.0,   // hour_cos
            0.3,   // route_uniqueness (low)
            2.0,   // min_pool_liq_log (low)
            0.05,  // pass_rate (very low)
            blk_liq,
            blk_profit,
            10.0,  // net_profit_log (low)
            0.0,   // is_multi_dex
            0.0,   // has_landed_before
        ];

        // Weight by frequency: add proportional number of negative samples
        let n_neg = (cnt / 500).clamp(1, 5) as usize;
        for _ in 0..n_neg {
            samples.push((features, 0.05));
        }
    }

    info!("  Total training samples: {}", samples.len());

    let pos = samples.iter().filter(|(_, l)| *l >= 0.5).count();
    let neg = samples.len() - pos;
    info!("  Positive (label >= 0.5): {pos}");
    info!("  Negative (label < 0.5): {neg}");

    if samples.is_empty() {
        info!("No training samples available. Exiting.");
        return Ok(());
    }

    // ═══════════════════════════════════════════════════════════════
    // 3. STANDARDIZE + TRAIN
    // ═══════════════════════════════════════════════════════════════
    info!("\n=== Phase 3: Training Logistic Regression ===");

    let (feat_mean, feat_std) = standardize(&mut samples);

    // Train with gradient descent
    let lr = 0.05;
    let epochs = 2000;
    let lambda = 0.01; // L2 regularization

    let (weights, bias, loss_history) = train_logistic(&samples, lr, epochs, lambda);

    info!(
        "  Final loss: {:.6} (started at {:.6})",
        loss_history.last().unwrap_or(&0.0),
        loss_history.first().unwrap_or(&0.0)
    );

    // ═══════════════════════════════════════════════════════════════
    // 4. EVALUATE MODEL
    // ═══════════════════════════════════════════════════════════════
    info!("\n=== Phase 4: Evaluation ===");

    for threshold in [0.2, 0.3, 0.4, 0.5, 0.6] {
        let (acc, prec, rec, f1) = eval_metrics(&samples, &weights, bias, threshold);
        info!("  threshold={threshold:.1}: accuracy={acc:.3} precision={prec:.3} recall={rec:.3} F1={f1:.3}");
    }

    // Feature importance (by absolute weight magnitude)
    let mut feat_importance: Vec<(usize, f64)> = weights
        .iter()
        .enumerate()
        .map(|(i, &w)| (i, w.abs()))
        .collect();
    feat_importance.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    info!("\n  Feature importance (by |weight|):");
    for (i, (idx, w)) in feat_importance.iter().enumerate() {
        let sign = if weights[*idx] >= 0.0 { "+" } else { "-" };
        info!(
            "    {i:>2}. {sign}{w:.4}  {}",
            FEATURE_NAMES[*idx]
        );
    }

    // ═══════════════════════════════════════════════════════════════
    // 5. GENERATE INSIGHTS
    // ═══════════════════════════════════════════════════════════════
    info!("\n=== Phase 5: Insights ===");

    // Best hour of day for 3-hop arbs
    info!("\n  Hour-of-day analysis (on-chain landed arbs):");
    let mut hours_sorted: Vec<_> = landed_hours.iter().collect();
    hours_sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (hour, count) in &hours_sorted {
        info!("    Hour {hour:>2} UTC: {count} landed TXs");
    }
    if hours_sorted.is_empty() {
        info!("    (no on-chain arbs recorded yet)");
    }

    // Best DEX combinations
    info!("\n  DEX combinations (on-chain landed):");
    let mut dex_sorted: Vec<_> = landed_dex_combos.iter().collect();
    dex_sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (combo, count) in &dex_sorted {
        info!("    {combo}: {count} landed");
    }
    if dex_sorted.is_empty() {
        info!("    (no on-chain arbs recorded yet)");
    }

    // Top routes by predicted score
    info!("\n  Top 10 routes by predicted P(land):");
    let mut route_scores: Vec<(String, f64, f64)> = Vec::new();
    for row in &hot_routes {
        let route_key: String = row.get(0);
        let net_profit: i64 = row.get(4);
        let min_liq_usd: f64 = row.get(5);
        let total_fee_bps: i32 = row.get(6);
        let price_impact: f64 = row.get(7);
        let competition: f64 = row.get(8);
        let uniqueness: f64 = row.get(10);
        let dex_div: i16 = row.get(11);
        let pools: Vec<String> = row.get(12);

        let pass_rate = route_pass_rate.get(&route_key).copied().unwrap_or(global_pass_rate);
        let blk_liq = route_blk_liq.get(&route_key).copied().unwrap_or(0.3);
        let blk_profit = route_blk_profit.get(&route_key).copied().unwrap_or(0.2);
        let has_landed = if landed_route_keys.contains_key(&route_key) {
            1.0
        } else {
            let any = pools.iter().any(|p| landed_route_keys.keys().any(|k| k.contains(p.as_str())));
            if any { 0.5 } else { 0.0 }
        };

        let hour_rad = 14.0f64 * std::f64::consts::PI * 2.0 / 24.0;

        let mut features: [f64; NUM_FEATURES] = [
            (min_liq_usd.max(0.01)).ln(),
            total_fee_bps as f64,
            dex_div as f64,
            price_impact / 3.0,
            competition,
            hour_rad.sin(),
            hour_rad.cos(),
            uniqueness,
            (min_liq_usd.max(0.01)).ln(),
            pass_rate,
            blk_liq,
            blk_profit,
            ((net_profit.max(0) as f64) + 1.0).ln(),
            if dex_div > 1 { 1.0 } else { 0.0 },
            has_landed,
        ];

        // Standardize with training stats
        for i in 0..NUM_FEATURES {
            features[i] = (features[i] - feat_mean[i]) / feat_std[i];
        }

        let p = predict(&weights, bias, &features);
        route_scores.push((route_key, p, net_profit as f64));
    }

    route_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    for (i, (key, prob, profit)) in route_scores.iter().take(10).enumerate() {
        let short_key = if key.len() > 30 {
            format!("{}...", &key[..30])
        } else {
            key.clone()
        };
        info!(
            "    {i:>2}. P={prob:.3}  profit={:.4} SOL  {short_key}",
            profit / 1e9
        );
    }

    // Recommended thresholds
    let routes_above_03 = route_scores.iter().filter(|(_, p, _)| *p > 0.3).count();
    let routes_above_05 = route_scores.iter().filter(|(_, p, _)| *p > 0.5).count();
    info!("\n  Routes with P(land) > 0.3: {routes_above_03} / {}", route_scores.len());
    info!("  Routes with P(land) > 0.5: {routes_above_05} / {}", route_scores.len());

    // Recommended MIN_RESERVE threshold
    // Find the min_liquidity_usd where pass rate drops significantly
    let mut liq_bins: Vec<(f64, f64)> = Vec::new(); // (min_liq, pred_score)
    for row in &hot_routes {
        let min_liq_usd: f64 = row.get(5);
        let route_key: String = row.get(0);
        if let Some((_, p, _)) = route_scores.iter().find(|(k, _, _)| k == &route_key) {
            liq_bins.push((min_liq_usd, *p));
        }
    }
    liq_bins.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let mid = liq_bins.len() / 2;
    if mid > 0 {
        let low_avg: f64 = liq_bins[..mid].iter().map(|(_, p)| p).sum::<f64>() / mid as f64;
        let high_avg: f64 = liq_bins[mid..].iter().map(|(_, p)| p).sum::<f64>()
            / (liq_bins.len() - mid) as f64;
        let median_liq = liq_bins[mid].0;
        info!(
            "\n  Liquidity analysis: low-liq routes avg P={low_avg:.3}, high-liq avg P={high_avg:.3}"
        );
        info!("  Median liquidity cutoff: ${median_liq:.2}");
        info!(
            "  Recommended MIN_LIQUIDITY_USD: ${:.0} (routes below have {:.0}% lower success rate)",
            median_liq,
            (high_avg - low_avg) * 100.0
        );
    }

    // ═══════════════════════════════════════════════════════════════
    // 6. SAVE MODEL TO PG
    // ═══════════════════════════════════════════════════════════════
    info!("\n=== Phase 6: Saving Model to PostgreSQL ===");

    let weights_vec: Vec<f64> = weights.to_vec();
    let mean_vec: Vec<f64> = feat_mean.to_vec();
    let std_vec: Vec<f64> = feat_std.to_vec();
    let feature_names: Vec<&str> = FEATURE_NAMES.to_vec();

    let model_json = serde_json::json!({
        "model_type": "logistic_regression",
        "version": 2,
        "trained_at": chrono::Utc::now().to_rfc3339(),
        "num_features": NUM_FEATURES,
        "feature_names": feature_names,
        "weights": weights_vec,
        "bias": bias,
        "standardization": {
            "mean": mean_vec,
            "std": std_vec
        },
        "training_stats": {
            "n_samples": samples.len(),
            "n_positive": pos,
            "n_negative": neg,
            "final_loss": loss_history.last().unwrap_or(&0.0),
            "epochs": epochs,
            "learning_rate": lr,
            "lambda_l2": lambda,
        },
        "recommended_threshold": 0.3,
        "data_sources": {
            "hot_routes": hot_routes.len(),
            "historical_executions": total_execs,
            "pool_discoveries": pool_dex_stats.iter().map(|r| r.get::<_, i64>(1)).sum::<i64>(),
            "onchain_arbs": arbs.len()
        },
        "insights": {
            "best_hours_utc": landed_hours,
            "best_dex_combos": landed_dex_combos,
            "top_routes": route_scores.iter().take(20).map(|(k, p, prof)| {
                serde_json::json!({"route_key": k, "p_land": p, "net_profit_lamports": prof})
            }).collect::<Vec<_>>(),
        },
        "usage": {
            "description": "Logistic regression: P(land) = sigmoid(w . standardize(x) + b)",
            "input": "15 features: see feature_names",
            "output": "probability 0.0-1.0",
            "threshold": "attempt routes with P(land) > 0.3",
            "latency_context_ms": "27-87ms detection to wire"
        }
    });

    client
        .execute(
            "INSERT INTO ml_model_state (model_name, weights_json, n_samples)
             VALUES ('route_3hop_predictor', $1, $2)
             ON CONFLICT (model_name) DO UPDATE SET
             weights_json = $1, n_samples = $2, updated_at = now()",
            &[&model_json, &(samples.len() as i64)],
        )
        .await?;

    info!("Model saved to PostgreSQL as 'route_3hop_predictor'");

    // ═══════════════════════════════════════════════════════════════
    // FINAL SUMMARY
    // ═══════════════════════════════════════════════════════════════
    info!("\n============================================");
    info!("TRAINING COMPLETE");
    info!("============================================");
    info!("  Model: logistic regression, {} features", NUM_FEATURES);
    info!("  Samples: {} ({} positive, {} negative)", samples.len(), pos, neg);
    info!(
        "  Final loss: {:.6}",
        loss_history.last().unwrap_or(&0.0)
    );
    info!("  Saved to: ml_model_state.route_3hop_predictor");
    info!("");
    info!("  To use in executor:");
    info!("    1. Load weights_json from ml_model_state WHERE model_name='route_3hop_predictor'");
    info!("    2. Build feature vector (15 features, see feature_names)");
    info!("    3. Standardize: x_std = (x - mean) / std");
    info!("    4. P(land) = sigmoid(dot(weights, x_std) + bias)");
    info!("    5. Attempt route if P(land) > 0.3");

    Ok(())
}
