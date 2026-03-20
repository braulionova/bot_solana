//! strategy-logger – Zero-latency async PostgreSQL logger for ML training.
//!
//! All `log_*` methods are fire-and-forget via a bounded crossbeam channel.
//! A dedicated background thread drains events and batch-inserts into PostgreSQL.
//! If the channel is full, events are dropped (backpressure > blocking).

pub mod types;
mod writer;

use crossbeam_channel::{bounded, Sender, TrySendError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};
use types::*;

/// Channel capacity: 8192 events before backpressure kicks in.
const CHANNEL_CAP: usize = 8192;

/// Main logger handle. Clone-safe via Arc internals.
#[derive(Clone)]
pub struct StrategyLogger {
    tx: Sender<LogEvent>,
    dropped: Arc<AtomicU64>,
}

impl StrategyLogger {
    /// Initialize the logger. Returns None if DATABASE_URL is not set.
    /// Spawns a background OS thread with its own tokio runtime for PG writes.
    pub fn init() -> Option<Arc<Self>> {
        let db_url = std::env::var("DATABASE_URL").ok()?;
        if db_url.is_empty() {
            return None;
        }

        let (tx, rx) = bounded::<LogEvent>(CHANNEL_CAP);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_clone = dropped.clone();

        // Background writer thread — dedicated OS thread, not on the main tokio runtime
        std::thread::Builder::new()
            .name("pg-logger".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("pg-logger tokio rt");
                rt.block_on(writer::run_writer(rx, db_url, dropped_clone));
            })
            .expect("spawn pg-logger thread");

        info!("StrategyLogger initialized (channel_cap={CHANNEL_CAP})");
        Some(Arc::new(Self { tx, dropped }))
    }

    /// Log a detected opportunity with its route hops.
    pub fn log_opportunity(&self, opp: LogOpportunity) {
        self.send(LogEvent::Opportunity(opp));
    }

    /// Log a TX execution attempt.
    pub fn log_execution(&self, exec: LogExecution) {
        self.send(LogEvent::Execution(exec));
    }

    /// Update execution result (landed/failed).
    pub fn update_execution(&self, update: LogExecutionUpdate) {
        self.send(LogEvent::ExecutionUpdate(update));
    }

    /// Log a sniper event.
    pub fn log_sniper(&self, event: LogSniperEvent) {
        self.send(LogEvent::Sniper(event));
    }

    /// Update sniper sell result.
    pub fn update_sniper_sell(&self, update: LogSniperSellUpdate) {
        self.send(LogEvent::SniperSellUpdate(update));
    }

    /// Log pool snapshots (batch).
    pub fn log_pool_snapshots(&self, snapshots: Vec<LogPoolSnapshot>) {
        if !snapshots.is_empty() {
            self.send(LogEvent::PoolSnapshots(snapshots));
        }
    }

    /// Update pool performance stats (after execution result).
    pub fn update_pool_perf(&self, update: LogPoolPerfUpdate) {
        self.send(LogEvent::PoolPerfUpdate(update));
    }

    /// Log a route candidate (ALL routes found by BF, including rejected — for ML training).
    pub fn log_route_candidate(&self, candidate: LogRouteCandidate) {
        self.send(LogEvent::RouteCandidate(candidate));
    }

    /// Log a newly discovered pool.
    pub fn log_pool_discovery(&self, discovery: LogPoolDiscovery) {
        self.send(LogEvent::PoolDiscovery(discovery));
    }

    /// Log a swap from a tracked wallet.
    pub fn log_wallet_swap(&self, swap: LogWalletSwap) {
        self.send(LogEvent::WalletSwap(swap));
    }

    /// Update tracked wallet stats.
    pub fn update_wallet(&self, update: LogWalletUpdate) {
        self.send(LogEvent::WalletUpdate(update));
    }

    /// Update or create a detected pattern.
    pub fn update_pattern(&self, pattern: LogPatternUpdate) {
        self.send(LogEvent::PatternUpdate(pattern));
    }

    fn send(&self, event: LogEvent) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(event) {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed);
            if n % 1000 == 0 {
                warn!("pg-logger channel full, dropped {} events total", n + 1);
            }
        }
    }

    /// Number of events dropped due to backpressure.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
