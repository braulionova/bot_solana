//! train_landing_model — Offline training for the landing predictor.
//!
//! Two training phases:
//!   Phase 1: Compute per-pool volatility features from pool_snapshots
//!   Phase 2: Train logistic regression using proxy labels from time-shifted re-quoting
//!
//! Usage:
//!   DATABASE_URL=postgresql://helios:helios_ml_2026@localhost/helios_ml \
//!   cargo run --release --bin train_landing_model
//!
//! The trained weights and volatility models are persisted to PostgreSQL.
//! On next helios-bot restart, they are loaded automatically.

use ml_scorer::landing_predictor::*;
use std::collections::HashMap;
use std::sync::Arc;

const DB_URL_DEFAULT: &str = "postgresql://helios:helios_ml_2026@localhost/helios_ml";
const LEARNING_RATE: f64 = 0.01;
const EPOCHS: usize = 30;
/// Time-shift for proxy labels: how many seconds after detection to re-quote.
/// ~0.5s = typical slot landing time for FAST_ARB (27ms build + 400ms slot).
const PROXY_SHIFT_SECS: f64 = 30.0;
/// Minimum profit margin (bps) in re-quoted route to label as "would land".
/// Accounts for on-chain slippage beyond reserve changes.
const PROXY_MIN_MARGIN_BPS: f64 = 10.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DB_URL_DEFAULT.to_string());

    let (client, connection) = tokio_postgres::connect(&db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    // ── Phase 1: Compute pool volatility models ────────────────────────────

    println!("=== Phase 1: Computing pool volatility features ===");

    // Get all pools with enough snapshots
    let pool_rows = client
        .query(
            "SELECT pool_address, count(*) as n
             FROM pool_snapshots_2026_03
             GROUP BY pool_address
             HAVING count(*) >= 5
             ORDER BY count(*) DESC",
            &[],
        )
        .await?;

    println!("Pools with ≥5 snapshots: {}", pool_rows.len());

    let predictor = LandingPredictor::new();
    let mut vol_computed = 0u64;

    for (idx, row) in pool_rows.iter().enumerate() {
        let pool: String = row.get(0);
        let n: i64 = row.get(1);

        // Fetch snapshots for this pool (ordered by time)
        let snap_rows = client
            .query(
                "SELECT EXTRACT(EPOCH FROM snapshot_at)::double precision,
                        reserve_a::double precision,
                        reserve_b::double precision
                 FROM pool_snapshots_2026_03
                 WHERE pool_address = $1
                 ORDER BY snapshot_at",
                &[&pool],
            )
            .await?;

        let snapshots: Vec<(f64, f64, f64)> = snap_rows
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();

        predictor.update_volatility(&pool, &snapshots);
        vol_computed += 1;

        if idx % 500 == 0 {
            println!("  [{}/{}] pool={} snapshots={}", idx, pool_rows.len(), &pool[..8], n);
        }
    }

    // Save volatility to PG
    let saved = predictor.save_volatility_to_pg(&db_url).await?;
    println!("Volatility models saved: {saved}");

    // ── Phase 2: Build training dataset with proxy labels ──────────────────

    println!("\n=== Phase 2: Building training dataset ===");

    // Load all opportunities with their hops
    let opp_rows = client
        .query(
            "SELECT o.id, o.slot, o.detected_at,
                    o.source_strategy, o.borrow_amount, o.net_profit, o.score,
                    o.risk_factor, o.n_hops
             FROM opportunities o
             ORDER BY o.id",
            &[],
        )
        .await?;

    println!("Opportunities: {}", opp_rows.len());

    // Load hops for all opportunities
    let hop_rows = client
        .query(
            "SELECT oh.opp_db_id, oh.hop_index, oh.pool, oh.dex_type,
                    oh.token_in, oh.token_out,
                    oh.amount_in, oh.amount_out, oh.price_impact,
                    oh.pool_reserve_a, oh.pool_reserve_b
             FROM opportunity_hops oh
             ORDER BY oh.opp_db_id, oh.hop_index",
            &[],
        )
        .await?;

    // Group hops by opp_db_id
    let mut hops_by_opp: HashMap<i64, Vec<HopData>> = HashMap::new();
    for row in &hop_rows {
        let opp_id: i64 = row.get(0);
        let hop = HopData {
            pool: row.get(2),
            dex_type: row.get(3),
            token_in: row.get(4),
            token_out: row.get(5),
            amount_in: row.get::<_, Option<i64>>(6).unwrap_or(0),
            amount_out: row.get::<_, Option<i64>>(7).unwrap_or(0),
            price_impact: row.get::<_, Option<f64>>(8).unwrap_or(0.0),
            reserve_a: row.get::<_, Option<i64>>(9).unwrap_or(0),
            reserve_b: row.get::<_, Option<i64>>(10).unwrap_or(0),
        };
        hops_by_opp.entry(opp_id).or_default().push(hop);
    }

    // Load execution outcomes (all negative for now)
    let exec_rows = client
        .query(
            "SELECT opp_db_id, landed, error_message, detect_to_build_us, total_us
             FROM executions",
            &[],
        )
        .await?;

    let mut exec_by_opp: HashMap<i64, ExecData> = HashMap::new();
    for row in &exec_rows {
        let opp_id: i64 = row.get(0);
        exec_by_opp.insert(opp_id, ExecData {
            landed: row.get(1),
            error: row.get::<_, Option<String>>(2),
            detect_to_build_us: row.get::<_, Option<i64>>(3).unwrap_or(0),
            total_us: row.get::<_, Option<i64>>(4).unwrap_or(0),
        });
    }

    // Build feature vectors with proxy labels
    let mut dataset: Vec<([f64; 18], f64)> = Vec::new();
    let mut n_positive = 0u64;
    let mut n_negative = 0u64;

    for row in &opp_rows {
        let opp_id: i64 = row.get(0);
        let detected_at: chrono::DateTime<chrono::Utc> = row.get(2);
        let strategy: String = row.get(3);
        let borrow_amount: i64 = row.get(4);
        let net_profit: i64 = row.get(5);
        let score: f64 = row.get(6);
        let n_hops: i16 = row.get(8);

        let hops = match hops_by_opp.get(&opp_id) {
            Some(h) => h,
            None => continue,
        };

        if hops.is_empty() || borrow_amount <= 0 {
            continue;
        }

        // Build features
        let hop_pools: Vec<&str> = hops.iter().map(|h| h.pool.as_str()).collect();
        let hop_impacts: Vec<f64> = hops.iter().map(|h| h.price_impact).collect();

        // ML scorer features: default for training (no live MlScorer available)
        let ml_rates: Vec<(f64, f64)> = vec![(0.5, 0.0); hops.len()];

        let features = predictor.build_features(
            &hop_pools,
            &hop_impacts,
            borrow_amount as u64,
            net_profit,
            score,
            &strategy,
            0.0, // detection age not known in historical data
            &ml_rates,
        );

        // ── Proxy label: time-shifted re-quote ──
        // For each hop, check pool_snapshots ~30s after detection.
        // If reserves shifted enough to kill the profit, label = 0.
        // (30s is the snapshot interval; we want the nearest snapshot after detection)
        let label = compute_proxy_label(
            &client,
            &hops,
            borrow_amount,
            net_profit,
            &detected_at,
            &exec_by_opp.get(&opp_id),
        )
        .await;

        let arr = features.to_array();
        if label > 0.5 {
            n_positive += 1;
        } else {
            n_negative += 1;
        }
        dataset.push((arr, label));
    }

    println!(
        "Training set: {} samples ({} positive, {} negative, {:.1}% positive rate)",
        dataset.len(),
        n_positive,
        n_negative,
        if !dataset.is_empty() {
            n_positive as f64 / dataset.len() as f64 * 100.0
        } else {
            0.0
        }
    );

    if dataset.is_empty() {
        println!("No training data — saving volatility models only.");
        return Ok(());
    }

    // ── Phase 3: Train logistic regression ─────────────────────────────────

    println!("\n=== Phase 3: Training logistic regression ===");

    let mut weights = LogisticWeights::default();

    // Compute normalization stats
    let features_only: Vec<[f64; 18]> = dataset.iter().map(|(f, _)| *f).collect();
    weights.fit_normalization(&features_only);

    // SGD training
    for epoch in 0..EPOCHS {
        let mut total_loss = 0.0;
        let mut correct = 0u64;

        for (features, label) in &dataset {
            let pred_before = weights.predict(features);
            let loss = weights.train_step(features, *label, LEARNING_RATE);
            total_loss += loss;

            let predicted_class = if pred_before >= 0.5 { 1.0 } else { 0.0 };
            if (predicted_class - label).abs() < 0.01 {
                correct += 1;
            }
        }

        let avg_loss = total_loss / dataset.len() as f64;
        let accuracy = correct as f64 / dataset.len() as f64;

        if epoch % 5 == 0 || epoch == EPOCHS - 1 {
            println!(
                "  epoch {}/{}: loss={:.4} accuracy={:.3} bias={:.3}",
                epoch + 1,
                EPOCHS,
                avg_loss,
                accuracy,
                weights.bias
            );
        }
    }

    // Print learned weights
    println!("\nLearned weights:");
    let feature_names = [
        "log_net_profit", "profit_margin_bps", "n_hops", "max_price_impact",
        "total_price_impact", "strategy_id", "log_borrow_amount", "score",
        "min_mean_reversion_ms", "max_activity_rate", "avg_price_cv",
        "min_log_reserves", "max_reserve_velocity", "hour_sin", "hour_cos",
        "detect_age_ms", "pool_success_rate", "pool_competition",
    ];
    for (i, name) in feature_names.iter().enumerate() {
        println!("  {:25} w={:+.4}  mean={:.2}  std={:.2}",
            name, weights.weights[i], weights.means[i], weights.stds[i]);
    }
    println!("  {:25} {:.4}", "bias", weights.bias);

    // Evaluate on training set
    let mut tp = 0u64;
    let mut fp = 0u64;
    let mut tn = 0u64;
    let mut fn_ = 0u64;
    for (features, label) in &dataset {
        let p = weights.predict(features);
        let pred = if p >= 0.5 { 1.0 } else { 0.0 };
        if pred > 0.5 && *label > 0.5 {
            tp += 1;
        } else if pred > 0.5 && *label < 0.5 {
            fp += 1;
        } else if pred < 0.5 && *label < 0.5 {
            tn += 1;
        } else {
            fn_ += 1;
        }
    }
    let precision = if tp + fp > 0 { tp as f64 / (tp + fp) as f64 } else { 0.0 };
    let recall = if tp + fn_ > 0 { tp as f64 / (tp + fn_) as f64 } else { 0.0 };
    let f1 = if precision + recall > 0.0 { 2.0 * precision * recall / (precision + recall) } else { 0.0 };
    println!("\nEvaluation: TP={tp} FP={fp} TN={tn} FN={fn_}");
    println!("  Precision={:.3} Recall={:.3} F1={:.3}", precision, recall, f1);

    // Save weights to PG
    predictor.set_weights(weights);
    predictor.save_weights_to_pg(&db_url).await?;
    println!("\nModel saved to PostgreSQL ✓");

    Ok(())
}

/// Compute proxy label for an opportunity:
/// 1. If we have execution data and it landed → 1.0
/// 2. If execution had SlippageToleranceExceeded → 0.0 (price moved)
/// 3. Otherwise: check if pool reserves ~30s later still support the route
async fn compute_proxy_label(
    client: &tokio_postgres::Client,
    hops: &[HopData],
    borrow_amount: i64,
    net_profit: i64,
    detected_at: &chrono::DateTime<chrono::Utc>,
    exec: &Option<&ExecData>,
) -> f64 {
    // Real label if we have execution data
    if let Some(exec) = exec {
        if exec.landed {
            return 1.0;
        }
        // SlippageToleranceExceeded (6001): price definitely moved → 0
        if let Some(ref err) = exec.error {
            if err.contains("6001") {
                return 0.0;
            }
            // InsufficientProfit (40): profit evaporated → 0
            if err.contains("\"Custom\": Number(40)") {
                return 0.0;
            }
        }
    }

    // Proxy label: re-quote with reserves from ~30s later
    let shift = chrono::Duration::seconds(PROXY_SHIFT_SECS as i64);
    let target_time = *detected_at + shift;

    let mut current_amount = borrow_amount as f64;
    let mut any_reserve_data = false;

    for hop in hops {
        // Get closest snapshot after detection + shift
        let snap_row = client
            .query_opt(
                "SELECT reserve_a, reserve_b FROM pool_snapshots_2026_03
                 WHERE pool_address = $1 AND snapshot_at >= $2
                 ORDER BY snapshot_at ASC LIMIT 1",
                &[&hop.pool, &target_time],
            )
            .await;

        match snap_row {
            Ok(Some(row)) => {
                let future_a: i64 = row.get(0);
                let future_b: i64 = row.get(1);
                any_reserve_data = true;

                // Simple constant-product re-quote: out = (B * in) / (A + in)
                // Determine direction from original hop
                let (reserve_in, reserve_out) = if hop.amount_in > 0 && hop.reserve_a > 0 {
                    // Guess direction: if token_in reserve ~ reserve_a → A→B
                    // Use original reserves to infer direction
                    if (hop.reserve_a as f64 - hop.amount_in as f64).abs()
                        < (hop.reserve_b as f64 - hop.amount_in as f64).abs()
                    {
                        (future_a as f64, future_b as f64)
                    } else {
                        (future_b as f64, future_a as f64)
                    }
                } else {
                    (future_a as f64, future_b as f64)
                };

                if reserve_in <= 0.0 || reserve_out <= 0.0 {
                    return 0.0;
                }

                // XY=K quote with 0.3% fee (typical)
                let amount_in_after_fee = current_amount * 0.997;
                let out = (reserve_out * amount_in_after_fee) / (reserve_in + amount_in_after_fee);
                if out <= 0.0 {
                    return 0.0;
                }
                current_amount = out;
            }
            _ => {
                // No snapshot available — use original reserves
                // (less accurate proxy, but better than skipping)
                if hop.reserve_a > 0 && hop.reserve_b > 0 {
                    let amount_in_after_fee = current_amount * 0.997;
                    let (ri, ro) = (hop.reserve_a as f64, hop.reserve_b as f64);
                    let out = (ro * amount_in_after_fee) / (ri + amount_in_after_fee);
                    current_amount = out.max(0.0);
                }
            }
        }
    }

    // Check if re-quoted route is still profitable
    let re_quoted_profit = current_amount - borrow_amount as f64;
    let margin_bps = re_quoted_profit / borrow_amount as f64 * 10_000.0;

    if margin_bps >= PROXY_MIN_MARGIN_BPS && re_quoted_profit > 0.0 {
        1.0
    } else {
        0.0
    }
}

struct HopData {
    pool: String,
    dex_type: String,
    token_in: String,
    token_out: String,
    amount_in: i64,
    amount_out: i64,
    price_impact: f64,
    reserve_a: i64,
    reserve_b: i64,
}

struct ExecData {
    landed: bool,
    error: Option<String>,
    detect_to_build_us: i64,
    total_us: i64,
}
