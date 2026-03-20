// signal_bus.rs – SpySignal type definitions and crossbeam channel helpers.

use crossbeam_channel::{bounded, Receiver, Sender};
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use std::time::Instant;

// ---------------------------------------------------------------------------
// LiquidityEventType
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum LiquidityEventType {
    /// Liquidity was added to the pool.
    Added { amount_a: u64, amount_b: u64 },
    /// Liquidity was removed from the pool.
    Removed { amount_a: u64, amount_b: u64 },
    /// A large single-sided deposit (possible manipulation).
    SingleSided { token_mint: Pubkey, amount: u64 },
}

// ---------------------------------------------------------------------------
// SpySignal
// ---------------------------------------------------------------------------

/// Primary signal emitted from the spy-node pipeline to the market-engine.
#[derive(Debug, Clone)]
pub enum SpySignal {
    /// A new transaction was observed in a shred before block confirmation.
    NewTransaction {
        slot: u64,
        tx: VersionedTransaction,
        detected_at: Instant,
    },

    /// A swap whose pool impact exceeds the whale threshold (≥ 1 %).
    WhaleSwap {
        slot: u64,
        tx: VersionedTransaction,
        pool: Pubkey,
        /// Estimated pool impact expressed as a percentage (0.0 – 100.0).
        impact_pct: f64,
    },

    /// A token has graduated from a launchpad (PumpFun, LaunchLab/BONK.fun, Moonshot).
    Graduation {
        slot: u64,
        token_mint: Pubkey,
        pump_pool: Pubkey,
        /// Source launchpad identifier (e.g. "PumpSwap", "LaunchLab", "Moonshot").
        source: &'static str,
        /// Creator pubkey from create_pool (needed for PumpSwap creator fee accounts).
        creator: Option<Pubkey>,
        /// Pool's base_mint from create_pool (determines account ordering in buy IX).
        pool_base_mint: Option<Pubkey>,
        /// Pool's quote_mint from create_pool.
        pool_quote_mint: Option<Pubkey>,
    },

    /// A liquidity event (add / remove) was detected on a monitored pool.
    LiquidityEvent {
        slot: u64,
        pool: Pubkey,
        event_type: LiquidityEventType,
    },
}

// ---------------------------------------------------------------------------
// Channel helpers
// ---------------------------------------------------------------------------

pub type SignalSender = Sender<SpySignal>;
pub type SignalReceiver = Receiver<SpySignal>;

/// Create a bounded (slot, signal) channel pair.
pub fn signal_channel(capacity: usize) -> (SignalSender, SignalReceiver) {
    bounded(capacity)
}
