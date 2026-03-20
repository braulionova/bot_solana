//! turbo_learner.rs — Online learning module for TurboSender.
//!
//! Loads historical execution data from PostgreSQL (141K+ events from old bot + new events)
//! and learns:
//! 1. Which endpoints land TXs fastest (by route type, time of day, tip amount)
//! 2. Which routes are worth attempting (profit vs competition)
//! 3. Optimal tip sizing based on historical landing data
//! 4. Time-of-day patterns (when MEV is most profitable)
//!
//! The model continuously updates from new execution results.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

const EMA_ALPHA: f64 = 0.1;

/// Learned endpoint performance profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointProfile {
    pub label: String,
    pub sends: u64,
    pub landed: u64,
    pub landing_rate: f64,
    pub avg_latency_ms: f64,
    pub p50_latency_ms: f64,
    /// Landing rate by hour (24 buckets).
    pub hourly_rate: Vec<f64>,
}

/// Learned route performance profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteProfile {
    pub route_key: String,
    pub attempts: u64,
    pub landed: u64,
    pub landing_rate: f64,
    pub avg_profit: f64,
    pub avg_tip: f64,
    pub avg_latency_ms: f64,
    /// Optimal tip as % of profit (learned from successes).
    pub optimal_tip_pct: f64,
}

/// The learner model.
pub struct TurboLearner {
    endpoints: DashMap<String, EndpointProfile>,
    routes: DashMap<String, RouteProfile>,
    /// Global stats.
    pub total_observations: AtomicU64,
    pub total_landed: AtomicU64,
}

impl TurboLearner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            endpoints: DashMap::new(),
            routes: DashMap::new(),
            total_observations: AtomicU64::new(0),
            total_landed: AtomicU64::new(0),
        })
    }

    /// Load historical data from PostgreSQL.
    pub async fn load_from_pg(self: &Arc<Self>, db_url: &str) -> anyhow::Result<()> {
        let (client, connection) =
            tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
        tokio::spawn(async move { let _ = connection.await; });

        // Load execution events
        let rows = client.query(
            "SELECT stage, status, reason_code, latency_ms, route, \
             EXTRACT(HOUR FROM ts)::int as hour \
             FROM historical_executions \
             WHERE latency_ms > 0",
            &[],
        ).await?;

        let mut landed_count = 0u64;
        let mut total = 0u64;

        for row in &rows {
            let status: String = row.get(1);
            let latency: i32 = row.get(3);
            let route: String = row.get(4);
            let hour: i32 = row.get(5);
            let landed = status == "confirmed";

            total += 1;
            if landed { landed_count += 1; }

            // Update route profile
            self.routes
                .entry(route.clone())
                .or_insert_with(|| RouteProfile {
                    route_key: route,
                    attempts: 0, landed: 0,
                    landing_rate: 0.5, avg_profit: 0.0, avg_tip: 0.0,
                    avg_latency_ms: 500.0, optimal_tip_pct: 20.0,
                })
                .attempts += 1;

            if landed {
                if let Some(mut rp) = self.routes.get_mut(&row.get::<_, String>(4)) {
                    rp.landed += 1;
                    rp.avg_latency_ms = EMA_ALPHA * (latency as f64) + (1.0 - EMA_ALPHA) * rp.avg_latency_ms;
                }
            }
        }

        // Compute landing rates
        for mut entry in self.routes.iter_mut() {
            let rp = entry.value_mut();
            if rp.attempts > 0 {
                rp.landing_rate = rp.landed as f64 / rp.attempts as f64;
            }
        }

        // Load missed opportunities for route scoring
        let missed_rows = client.query(
            "SELECT reason_code, COUNT(*) as cnt, AVG(est_profit_lamports) as avg_profit, \
             AVG(spread_bps) as avg_spread \
             FROM historical_missed \
             WHERE est_profit_lamports > 0 \
             GROUP BY reason_code \
             ORDER BY cnt DESC LIMIT 20",
            &[],
        ).await?;

        let mut missed_by_reason: Vec<(String, i64, f64)> = Vec::new();
        for row in &missed_rows {
            let reason: String = row.get(0);
            let cnt: i64 = row.get(1);
            let avg_profit: f64 = row.try_get(2).unwrap_or(0.0);
            missed_by_reason.push((reason, cnt, avg_profit));
        }

        self.total_observations.store(total, Ordering::Relaxed);
        self.total_landed.store(landed_count, Ordering::Relaxed);

        info!(
            total_events = total,
            landed = landed_count,
            routes = self.routes.len(),
            missed_reasons = missed_by_reason.len(),
            "TurboLearner: loaded historical data"
        );

        // Log key insights
        let global_rate = if total > 0 { landed_count as f64 / total as f64 } else { 0.0 };
        info!(
            global_landing_rate = format!("{:.2}%", global_rate * 100.0),
            "TurboLearner: baseline performance"
        );

        for (reason, cnt, profit) in &missed_by_reason {
            if *cnt > 1000 {
                info!(
                    reason = %reason,
                    count = cnt,
                    avg_profit = format!("{:.0}", profit),
                    "TurboLearner: top missed opportunity reason"
                );
            }
        }

        Ok(())
    }

    /// Record a new execution result.
    pub fn observe_execution(
        &self,
        route: &str,
        endpoint: &str,
        landed: bool,
        latency_ms: f64,
        profit: i64,
        tip: u64,
        hour: usize,
    ) {
        self.total_observations.fetch_add(1, Ordering::Relaxed);
        if landed {
            self.total_landed.fetch_add(1, Ordering::Relaxed);
        }

        // Update endpoint profile
        self.endpoints
            .entry(endpoint.to_string())
            .or_insert_with(|| EndpointProfile {
                label: endpoint.to_string(),
                sends: 0, landed: 0,
                landing_rate: 0.5, avg_latency_ms: 100.0, p50_latency_ms: 100.0,
                hourly_rate: vec![0.5; 24],
            })
            .sends += 1;

        if let Some(mut ep) = self.endpoints.get_mut(endpoint) {
            let outcome = if landed { 1.0 } else { 0.0 };
            ep.landing_rate = EMA_ALPHA * outcome + (1.0 - EMA_ALPHA) * ep.landing_rate;
            if landed {
                ep.landed += 1;
                ep.avg_latency_ms = EMA_ALPHA * latency_ms + (1.0 - EMA_ALPHA) * ep.avg_latency_ms;
            }
            let h = hour % 24;
            ep.hourly_rate[h] = EMA_ALPHA * outcome + (1.0 - EMA_ALPHA) * ep.hourly_rate[h];
        }

        // Update route profile
        self.routes
            .entry(route.to_string())
            .or_insert_with(|| RouteProfile {
                route_key: route.to_string(),
                attempts: 0, landed: 0,
                landing_rate: 0.5, avg_profit: 0.0, avg_tip: 0.0,
                avg_latency_ms: 500.0, optimal_tip_pct: 20.0,
            })
            .attempts += 1;

        if let Some(mut rp) = self.routes.get_mut(route) {
            let outcome = if landed { 1.0 } else { 0.0 };
            rp.landing_rate = EMA_ALPHA * outcome + (1.0 - EMA_ALPHA) * rp.landing_rate;
            if landed {
                rp.landed += 1;
                rp.avg_profit = EMA_ALPHA * (profit as f64) + (1.0 - EMA_ALPHA) * rp.avg_profit;
                rp.avg_latency_ms = EMA_ALPHA * latency_ms + (1.0 - EMA_ALPHA) * rp.avg_latency_ms;
                // Learn optimal tip %
                if profit > 0 {
                    let tip_pct = (tip as f64 / profit as f64) * 100.0;
                    rp.optimal_tip_pct = EMA_ALPHA * tip_pct + (1.0 - EMA_ALPHA) * rp.optimal_tip_pct;
                }
            }
        }
    }

    /// Get recommended tip % for a route.
    pub fn recommended_tip_pct(&self, route: &str) -> f64 {
        self.routes.get(route)
            .map(|rp| rp.optimal_tip_pct.max(10.0).min(50.0))
            .unwrap_or(20.0) // default 20%
    }

    /// Get best endpoints for current hour.
    pub fn best_endpoints(&self, hour: usize) -> Vec<(String, f64)> {
        let h = hour % 24;
        let mut eps: Vec<(String, f64)> = self.endpoints.iter()
            .map(|e| {
                let score = e.landing_rate * e.hourly_rate[h] / (1.0 + e.avg_latency_ms / 100.0);
                (e.label.clone(), score)
            })
            .collect();
        eps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        eps
    }

    /// Should we attempt this route? Based on historical success.
    pub fn should_attempt(&self, route: &str, profit: i64) -> bool {
        if let Some(rp) = self.routes.get(route) {
            // Skip routes with >20 attempts and <5% landing rate
            if rp.attempts > 20 && rp.landing_rate < 0.05 {
                return false;
            }
            // Skip if profit is less than avg tip needed
            if profit < (rp.avg_tip as i64) {
                return false;
            }
        }
        true // default: attempt
    }

    pub fn stats(&self) -> String {
        let total = self.total_observations.load(Ordering::Relaxed);
        let landed = self.total_landed.load(Ordering::Relaxed);
        let rate = if total > 0 { landed as f64 / total as f64 * 100.0 } else { 0.0 };
        format!(
            "observations={total} landed={landed} rate={rate:.1}% endpoints={} routes={}",
            self.endpoints.len(),
            self.routes.len()
        )
    }
}
