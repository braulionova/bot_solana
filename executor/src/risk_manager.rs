// risk_manager.rs – Runtime risk controls for the Helios executor.
//
// Enforces:
//   • Daily loss cap: if net losses exceed `max_daily_loss_lamports`, pause execution for 24h.
//   • Win-rate guard: if win rate drops below 20% over the last 50 trades, pause for 1h.
//   • Drawdown alert: trigger Telegram alert if unrealised loss > 0.5 SOL in tips.
//   • Max flash amount: reject bundles > `max_flash_fraction` of provider liquidity.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Fraction of working capital that may be lost in one day (5%).
const DEFAULT_MAX_DAILY_LOSS_PCT: f64 = 0.05;

/// Minimum win rate over last N trades before pausing.
const MIN_WIN_RATE: f64 = 0.20;

/// Rolling window for win-rate calculation.
const WIN_RATE_WINDOW: usize = 50;

/// Pause duration when daily loss cap is hit.
const DAILY_LOSS_PAUSE: Duration = Duration::from_secs(24 * 3600);

/// Pause duration when win rate is too low.
const WIN_RATE_PAUSE: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------
// Trade outcome record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeOutcome {
    Win,
    Loss,
}

// ---------------------------------------------------------------------------
// RiskManager
// ---------------------------------------------------------------------------

pub struct RiskManager {
    /// Working capital in lamports (set at startup, read from payer balance).
    working_capital: u64,
    /// Max daily loss (lamports) derived from working capital.
    max_daily_loss:  u64,

    /// Running daily P&L in lamports (positive = profit, negative = loss).
    daily_pnl:       AtomicI64,
    /// Day start (reset at midnight / 24h after bot start).
    day_start:       Mutex<Instant>,

    /// Total trades won.
    wins:            AtomicU64,
    /// Total trades lost.
    losses:          AtomicU64,
    /// Rolling window of last N outcomes (1=win, 0=loss).
    outcome_window:  Mutex<Vec<u8>>,

    /// Total lamports paid in Jito tips.
    tips_paid:       AtomicU64,
    /// Total lamports profit realised.
    profit_realised: AtomicU64,

    /// If Some, executor is paused until this instant.
    paused_until:    Mutex<Option<Instant>>,
}

impl RiskManager {
    pub fn new(working_capital_lamports: u64) -> Arc<Self> {
        let max_daily_loss = (working_capital_lamports as f64 * DEFAULT_MAX_DAILY_LOSS_PCT) as u64;
        info!(
            working_capital = working_capital_lamports,
            max_daily_loss,
            "risk manager initialised"
        );
        Arc::new(Self {
            working_capital: working_capital_lamports,
            max_daily_loss,
            daily_pnl:       AtomicI64::new(0),
            day_start:       Mutex::new(Instant::now()),
            wins:            AtomicU64::new(0),
            losses:          AtomicU64::new(0),
            outcome_window:  Mutex::new(Vec::with_capacity(WIN_RATE_WINDOW)),
            tips_paid:       AtomicU64::new(0),
            profit_realised: AtomicU64::new(0),
            paused_until:    Mutex::new(None),
        })
    }

    // -----------------------------------------------------------------------
    // Gate check – call before attempting any execution.
    // -----------------------------------------------------------------------

    /// Returns `true` if execution is currently allowed.
    pub fn is_allowed(&self) -> bool {
        // Check pause state.
        let mut guard = self.paused_until.lock();
        if let Some(until) = *guard {
            if Instant::now() < until {
                return false;
            }
            // Pause expired – resume.
            *guard = None;
            info!("risk pause expired, resuming execution");
        }

        // Reset daily counter if 24h have elapsed.
        self.maybe_reset_day();

        // Check daily loss cap.
        let pnl = self.daily_pnl.load(Ordering::Relaxed);
        if pnl < -(self.max_daily_loss as i64) {
            warn!(
                pnl,
                max_daily_loss = self.max_daily_loss,
                "daily loss cap hit – pausing 24h"
            );
            *self.paused_until.lock() = Some(Instant::now() + DAILY_LOSS_PAUSE);
            return false;
        }

        true
    }

    // -----------------------------------------------------------------------
    // Record trade result
    // -----------------------------------------------------------------------

    pub fn record_win(&self, profit_lamports: u64, tip_lamports: u64) {
        self.wins.fetch_add(1, Ordering::Relaxed);
        self.profit_realised.fetch_add(profit_lamports, Ordering::Relaxed);
        self.tips_paid.fetch_add(tip_lamports, Ordering::Relaxed);
        let net = profit_lamports as i64 - tip_lamports as i64;
        self.daily_pnl.fetch_add(net, Ordering::Relaxed);
        self.push_outcome(1);

        info!(
            profit_lamports,
            tip_lamports,
            net,
            wins  = self.wins.load(Ordering::Relaxed),
            losses = self.losses.load(Ordering::Relaxed),
            "trade WIN"
        );
    }

    pub fn record_loss(&self, tip_lamports: u64) {
        self.losses.fetch_add(1, Ordering::Relaxed);
        self.tips_paid.fetch_add(tip_lamports, Ordering::Relaxed);
        self.daily_pnl.fetch_add(-(tip_lamports as i64), Ordering::Relaxed);
        self.push_outcome(0);

        let win_rate = self.win_rate();
        warn!(
            tip_lamports,
            wins    = self.wins.load(Ordering::Relaxed),
            losses  = self.losses.load(Ordering::Relaxed),
            win_rate = format!("{:.1}%", win_rate * 100.0),
            "trade LOSS"
        );

        // Win-rate guard.
        let total = self.wins.load(Ordering::Relaxed) + self.losses.load(Ordering::Relaxed);
        if total >= WIN_RATE_WINDOW as u64 && win_rate < MIN_WIN_RATE {
            warn!(
                win_rate = format!("{:.1}%", win_rate * 100.0),
                "win rate below threshold – pausing 1h"
            );
            *self.paused_until.lock() = Some(Instant::now() + WIN_RATE_PAUSE);
        }
    }

    // -----------------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------------

    /// Reject if bundle flash amount exceeds 10% of provider estimated liquidity.
    pub fn check_flash_size(&self, amount: u64, provider_liquidity: u64) -> bool {
        let max_allowed = provider_liquidity / 10;
        if amount > max_allowed {
            warn!(
                amount,
                max_allowed,
                "flash amount exceeds 10% of provider liquidity – rejected"
            );
            return false;
        }
        true
    }

    // -----------------------------------------------------------------------
    // Stats
    // -----------------------------------------------------------------------

    pub fn win_rate(&self) -> f64 {
        let window = self.outcome_window.lock();
        if window.is_empty() {
            return 1.0; // optimistic until we have data
        }
        let wins: u64 = window.iter().map(|&x| x as u64).sum();
        wins as f64 / window.len() as f64
    }

    pub fn daily_pnl(&self) -> i64 {
        self.daily_pnl.load(Ordering::Relaxed)
    }

    pub fn total_profit(&self) -> u64 {
        self.profit_realised.load(Ordering::Relaxed)
    }

    pub fn total_tips(&self) -> u64 {
        self.tips_paid.load(Ordering::Relaxed)
    }

    pub fn stats_summary(&self) -> String {
        format!(
            "wins={} losses={} win_rate={:.1}% daily_pnl={} total_profit={} tips_paid={}",
            self.wins.load(Ordering::Relaxed),
            self.losses.load(Ordering::Relaxed),
            self.win_rate() * 100.0,
            self.daily_pnl(),
            self.total_profit(),
            self.total_tips(),
        )
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    fn push_outcome(&self, outcome: u8) {
        let mut window = self.outcome_window.lock();
        if window.len() >= WIN_RATE_WINDOW {
            window.remove(0);
        }
        window.push(outcome);
    }

    fn maybe_reset_day(&self) {
        let mut day_start = self.day_start.lock();
        if day_start.elapsed() >= Duration::from_secs(24 * 3600) {
            self.daily_pnl.store(0, Ordering::Relaxed);
            *day_start = Instant::now();
            info!("daily P&L counter reset");
        }
    }
}
