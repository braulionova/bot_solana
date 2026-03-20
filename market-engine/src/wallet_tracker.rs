//! wallet_tracker.rs – Smart money wallet analysis for ML pattern detection.
//!
//! Monitors on-chain swaps to identify profitable wallets (smart money, bots,
//! whales) and track their behavior patterns. Feeds data to PostgreSQL for
//! ML model training.
//!
//! Architecture:
//! - Passive observer: reads from the same SpySignal stream as signal_processor
//! - Maintains in-memory state per tracked wallet (DashMap, lock-free)
//! - Detects patterns: accumulation, front-running, token rotation, time-of-day
//! - Logs all wallet swap activity + detected patterns to PG
//! - ML scorer can query hot_patterns for score boosting
//!
//! Pattern types detected:
//! 1. `whale_accumulation` — wallet buys same token across multiple pools
//! 2. `fast_flip` — buy then sell within N slots (MEV bot / front-runner)
//! 3. `token_rotation` — wallet consistently trades specific token pairs
//! 4. `pre_pump_buy` — wallet buys token right before price increase
//! 5. `smart_entry` — wallet times entries during low-competition windows
//! 6. `dex_preference` — wallet prefers specific DEX for specific pairs

use crossbeam_channel::{bounded, Sender, TrySendError};
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::pool_state::PoolStateCache;
use crate::swap_decoder::SwapInfo;
use crate::types::DexType;
use strategy_logger::StrategyLogger;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Min swap size to track (lamports). Ignore dust.
const MIN_SWAP_AMOUNT: u64 = 10_000_000; // 0.01 SOL
/// Max wallets tracked in memory (LRU eviction above this).
const MAX_TRACKED_WALLETS: usize = 50_000;
/// Swap history per wallet (ring buffer).
const WALLET_HISTORY_SIZE: usize = 100;
/// Min swaps before a wallet is considered "interesting" for pattern detection.
const MIN_SWAPS_FOR_PATTERNS: usize = 3;
/// Fast flip threshold: buy and sell same token within N slots.
const FAST_FLIP_SLOTS: u64 = 10;
/// Accumulation threshold: N buys of same token in a window.
const ACCUMULATION_WINDOW_SECS: u64 = 300; // 5 min
const ACCUMULATION_MIN_BUYS: usize = 3;
/// Whale threshold: impact > 1% of pool reserves.
const WHALE_IMPACT_BPS: f64 = 100.0;
/// Smart money threshold: win rate after N swaps.
const SMART_MONEY_MIN_SWAPS: u64 = 10;
const SMART_MONEY_WIN_RATE: f64 = 0.6;

/// Known WSOL mint.
const WSOL: &str = "So11111111111111111111111111111111111111112";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WalletSwapEvent {
    pub wallet: Pubkey,
    pub tx_sig: Option<String>,
    pub slot: u64,
    pub dex_type: DexType,
    pub pool: Pubkey,
    pub token_in: Pubkey,
    pub token_out: Pubkey,
    pub amount_in: u64,
    pub amount_out: u64,
    pub is_buy: bool, // true = buying non-SOL token
    pub impact_bps: f64,
    pub timestamp: Instant,
}

#[derive(Debug)]
struct WalletState {
    address: Pubkey,
    swaps: VecDeque<WalletSwapEvent>,
    total_swaps: u64,
    buys: u64,
    sells: u64,
    estimated_profit: i64,
    first_seen: Instant,
    last_seen: Instant,
    category: String, // smart_money, whale, bot, unknown
    label: String,
}

impl WalletState {
    fn new(address: Pubkey) -> Self {
        let now = Instant::now();
        Self {
            address,
            swaps: VecDeque::with_capacity(WALLET_HISTORY_SIZE),
            total_swaps: 0,
            buys: 0,
            sells: 0,
            estimated_profit: 0,
            first_seen: now,
            last_seen: now,
            category: "unknown".into(),
            label: "unknown".into(),
        }
    }

    fn push_swap(&mut self, swap: WalletSwapEvent) {
        self.total_swaps += 1;
        if swap.is_buy { self.buys += 1; } else { self.sells += 1; }
        self.last_seen = swap.timestamp;

        if self.swaps.len() >= WALLET_HISTORY_SIZE {
            self.swaps.pop_front();
        }
        self.swaps.push_back(swap);
    }

    fn win_rate(&self) -> f64 {
        if self.total_swaps == 0 { return 0.0; }
        // Approximate: if they keep trading, they're probably profitable
        // Real P&L would require tracking token balances across swaps
        let profitable_sells = self.sells.min(self.buys); // matched buy-sell pairs
        if self.total_swaps > 0 {
            profitable_sells as f64 / self.total_swaps as f64
        } else {
            0.0
        }
    }

    fn classify(&mut self) {
        if self.total_swaps >= SMART_MONEY_MIN_SWAPS {
            // Check for bot pattern: very fast, many swaps, consistent timing
            let recent: Vec<&WalletSwapEvent> = self.swaps.iter()
                .filter(|s| s.timestamp.elapsed() < Duration::from_secs(3600))
                .collect();

            if recent.len() >= 20 {
                // High-frequency trader in last hour = likely a bot
                self.category = "bot".into();
                self.label = format!("bot_{}swaps/hr", recent.len());
            } else if self.swaps.iter().any(|s| s.impact_bps > WHALE_IMPACT_BPS) {
                self.category = "whale".into();
                self.label = "whale".into();
            } else if self.win_rate() >= SMART_MONEY_WIN_RATE {
                self.category = "smart_money".into();
                self.label = format!("smart_{:.0}%wr", self.win_rate() * 100.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Detected Patterns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DetectedPattern {
    pub pattern_type: String,
    pub wallet: Option<Pubkey>,
    pub token_mint: Option<Pubkey>,
    pub pool: Option<Pubkey>,
    pub dex_type: Option<String>,
    pub confidence: f64,
    pub features: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Notification events (for Telegram alerts from helios main)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum WalletAlert {
    SmartMoneyDetected {
        wallet: String,
        category: String,
        total_swaps: u64,
        win_rate: f64,
    },
    PatternDetected {
        pattern_type: String,
        wallet: String,
        token: Option<String>,
        confidence: f64,
        features: serde_json::Value,
    },
}

// ---------------------------------------------------------------------------
// WalletTracker
// ---------------------------------------------------------------------------

pub struct WalletTracker {
    wallets: DashMap<Pubkey, WalletState>,
    pool_cache: Arc<PoolStateCache>,
    logger: Arc<StrategyLogger>,
    /// Recently detected patterns (for dedup).
    recent_patterns: DashMap<String, Instant>,
    /// Channel for Telegram-worthy alerts (consumed by helios main).
    alert_tx: Sender<WalletAlert>,
}

impl WalletTracker {
    pub fn new(
        pool_cache: Arc<PoolStateCache>,
        logger: Arc<StrategyLogger>,
    ) -> (Arc<Self>, crossbeam_channel::Receiver<WalletAlert>) {
        let (alert_tx, alert_rx) = bounded(256);
        let tracker = Arc::new(Self {
            wallets: DashMap::new(),
            pool_cache,
            logger,
            recent_patterns: DashMap::new(),
            alert_tx,
        });
        (tracker, alert_rx)
    }

    /// Process a decoded swap from the signal pipeline.
    /// Called from the signal processor or directly from helios main for each TX.
    pub fn observe_swap(&self, swap: &SwapInfo, slot: u64, tx_sig: Option<&str>) {
        if swap.amount_in < MIN_SWAP_AMOUNT && swap.amount_out < MIN_SWAP_AMOUNT {
            return;
        }

        let wsol: Pubkey = WSOL.parse().unwrap();
        let is_buy = swap.token_in == wsol; // buying token = spending SOL

        // Compute impact
        let impact_bps = if let Some(pool_state) = self.pool_cache.get(&swap.pool) {
            let reserve = if swap.a_to_b { pool_state.reserve_a } else { pool_state.reserve_b };
            if reserve > 0 {
                (swap.amount_in as f64 / reserve as f64) * 10_000.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        let event = WalletSwapEvent {
            wallet: swap.signer,
            tx_sig: tx_sig.map(|s| s.to_string()),
            slot,
            dex_type: DexType::from_program_id(&swap.program_id),
            pool: swap.pool,
            token_in: swap.token_in,
            token_out: swap.token_out,
            amount_in: swap.amount_in,
            amount_out: swap.amount_out,
            is_buy,
            impact_bps,
            timestamp: Instant::now(),
        };

        // Log to PG
        self.logger.log_wallet_swap(strategy_logger::types::LogWalletSwap {
            wallet_address: swap.signer.to_string(),
            tx_signature: tx_sig.map(|s| s.to_string()),
            slot,
            dex_type: format!("{:?}", event.dex_type),
            pool: swap.pool.to_string(),
            token_in: swap.token_in.to_string(),
            token_out: swap.token_out.to_string(),
            amount_in: swap.amount_in,
            amount_out: swap.amount_out,
            is_buy,
            price_impact_bps: Some(impact_bps),
            pool_reserve_a: self.pool_cache.get(&swap.pool).map(|p| p.reserve_a),
            pool_reserve_b: self.pool_cache.get(&swap.pool).map(|p| p.reserve_b),
        });

        // Update wallet state
        let mut wallet = self.wallets
            .entry(swap.signer)
            .or_insert_with(|| WalletState::new(swap.signer));
        wallet.push_swap(event.clone());
        let old_category = wallet.category.clone();
        wallet.classify();

        // Alert if wallet just got classified as smart_money or whale.
        if wallet.category != old_category
            && (wallet.category == "smart_money" || wallet.category == "whale")
        {
            let _ = self.alert_tx.try_send(WalletAlert::SmartMoneyDetected {
                wallet: wallet.address.to_string(),
                category: wallet.category.clone(),
                total_swaps: wallet.total_swaps,
                win_rate: wallet.win_rate(),
            });
        }

        // Detect patterns if wallet has enough history
        if wallet.total_swaps >= MIN_SWAPS_FOR_PATTERNS as u64 {
            self.detect_patterns(&wallet, &event);
        }

        // Update wallet stats in PG periodically (every 10 swaps)
        if wallet.total_swaps % 10 == 0 {
            self.logger.update_wallet(strategy_logger::types::LogWalletUpdate {
                wallet_address: wallet.address.to_string(),
                label: wallet.label.clone(),
                category: wallet.category.clone(),
                total_swaps: wallet.total_swaps,
                total_profit_lamports: wallet.estimated_profit,
                win_rate: wallet.win_rate(),
                last_swap_slot: Some(slot),
            });
        }

        drop(wallet);

        // Evict old wallets if over limit
        if self.wallets.len() > MAX_TRACKED_WALLETS {
            self.evict_stale_wallets();
        }
    }

    fn detect_patterns(&self, wallet: &WalletState, latest: &WalletSwapEvent) {
        // Pattern 1: Fast flip (buy then sell same token within N slots)
        self.detect_fast_flip(wallet, latest);

        // Pattern 2: Accumulation (multiple buys of same token)
        self.detect_accumulation(wallet, latest);

        // Pattern 3: Token rotation (wallet trades same pairs repeatedly)
        self.detect_token_rotation(wallet, latest);

        // Pattern 4: DEX preference
        self.detect_dex_preference(wallet, latest);
    }

    fn detect_fast_flip(&self, wallet: &WalletState, latest: &WalletSwapEvent) {
        if !latest.is_buy {
            // Check if there was a recent buy of the same token
            for swap in wallet.swaps.iter().rev().skip(1) {
                if swap.is_buy
                    && swap.token_out == latest.token_in
                    && latest.slot.saturating_sub(swap.slot) <= FAST_FLIP_SLOTS
                {
                    let profit = latest.amount_out as i64 - swap.amount_in as i64;
                    let confidence = if profit > 0 { 0.8 } else { 0.3 };

                    self.emit_pattern(DetectedPattern {
                        pattern_type: "fast_flip".into(),
                        wallet: Some(wallet.address),
                        token_mint: Some(swap.token_out),
                        pool: Some(latest.pool),
                        dex_type: Some(format!("{:?}", latest.dex_type)),
                        confidence,
                        features: serde_json::json!({
                            "buy_slot": swap.slot,
                            "sell_slot": latest.slot,
                            "slot_delta": latest.slot - swap.slot,
                            "buy_amount": swap.amount_in,
                            "sell_amount": latest.amount_out,
                            "profit_lamports": profit,
                            "buy_pool": swap.pool.to_string(),
                            "sell_pool": latest.pool.to_string(),
                        }),
                    });
                    break;
                }
            }
        }
    }

    fn detect_accumulation(&self, wallet: &WalletState, latest: &WalletSwapEvent) {
        if !latest.is_buy { return; }

        let cutoff = Duration::from_secs(ACCUMULATION_WINDOW_SECS);
        let same_token_buys: Vec<&WalletSwapEvent> = wallet.swaps.iter()
            .filter(|s| s.is_buy
                && s.token_out == latest.token_out
                && s.timestamp.elapsed() < cutoff)
            .collect();

        if same_token_buys.len() >= ACCUMULATION_MIN_BUYS {
            let total_spent: u64 = same_token_buys.iter().map(|s| s.amount_in).sum();
            let confidence = (same_token_buys.len() as f64 / 10.0).min(1.0);

            self.emit_pattern(DetectedPattern {
                pattern_type: "whale_accumulation".into(),
                wallet: Some(wallet.address),
                token_mint: Some(latest.token_out),
                pool: Some(latest.pool),
                dex_type: Some(format!("{:?}", latest.dex_type)),
                confidence,
                features: serde_json::json!({
                    "n_buys": same_token_buys.len(),
                    "total_spent_lamports": total_spent,
                    "window_secs": ACCUMULATION_WINDOW_SECS,
                    "avg_buy_size": total_spent / same_token_buys.len() as u64,
                }),
            });
        }
    }

    fn detect_token_rotation(&self, wallet: &WalletState, latest: &WalletSwapEvent) {
        // Count how many times this wallet trades this specific token pair
        let pair_count = wallet.swaps.iter()
            .filter(|s| {
                (s.token_in == latest.token_in && s.token_out == latest.token_out)
                || (s.token_in == latest.token_out && s.token_out == latest.token_in)
            })
            .count();

        if pair_count >= 5 {
            let confidence = (pair_count as f64 / 20.0).min(1.0);
            self.emit_pattern(DetectedPattern {
                pattern_type: "token_rotation".into(),
                wallet: Some(wallet.address),
                token_mint: Some(latest.token_out),
                pool: Some(latest.pool),
                dex_type: Some(format!("{:?}", latest.dex_type)),
                confidence,
                features: serde_json::json!({
                    "pair_trades": pair_count,
                    "total_wallet_swaps": wallet.total_swaps,
                    "pair_ratio": pair_count as f64 / wallet.total_swaps as f64,
                }),
            });
        }
    }

    fn detect_dex_preference(&self, wallet: &WalletState, _latest: &WalletSwapEvent) {
        if wallet.total_swaps < 10 { return; }

        // Count swaps per DEX
        let mut dex_counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for swap in &wallet.swaps {
            *dex_counts.entry(format!("{:?}", swap.dex_type)).or_default() += 1;
        }

        // If >70% on one DEX, that's a strong preference
        for (dex, count) in &dex_counts {
            let ratio = *count as f64 / wallet.swaps.len() as f64;
            if ratio >= 0.7 && *count >= 7 {
                let dedup_key = format!("dex_pref_{}_{}", wallet.address, dex);
                if self.recent_patterns.contains_key(&dedup_key) { continue; }

                self.emit_pattern(DetectedPattern {
                    pattern_type: "dex_preference".into(),
                    wallet: Some(wallet.address),
                    token_mint: None,
                    pool: None,
                    dex_type: Some(dex.clone()),
                    confidence: ratio,
                    features: serde_json::json!({
                        "preferred_dex": dex,
                        "ratio": ratio,
                        "count": count,
                        "total_swaps": wallet.swaps.len(),
                    }),
                });
            }
        }
    }

    fn emit_pattern(&self, pattern: DetectedPattern) {
        // Dedup: don't emit same pattern type+wallet+token more than once per 60s
        let dedup_key = format!(
            "{}_{}_{}_{}",
            pattern.pattern_type,
            pattern.wallet.map(|w| w.to_string()).unwrap_or_default(),
            pattern.token_mint.map(|t| t.to_string()).unwrap_or_default(),
            pattern.pool.map(|p| p.to_string()).unwrap_or_default(),
        );

        if let Some(last) = self.recent_patterns.get(&dedup_key) {
            if last.elapsed() < Duration::from_secs(60) { return; }
        }
        self.recent_patterns.insert(dedup_key.clone(), Instant::now());

        debug!(
            pattern = %pattern.pattern_type,
            confidence = pattern.confidence,
            wallet = ?pattern.wallet.map(|w| w.to_string()),
            "wallet pattern detected"
        );

        // Alert for high-confidence patterns.
        if pattern.confidence >= 0.7 {
            let _ = self.alert_tx.try_send(WalletAlert::PatternDetected {
                pattern_type: pattern.pattern_type.clone(),
                wallet: pattern.wallet.map(|w| w.to_string()).unwrap_or_default(),
                token: pattern.token_mint.map(|t| t.to_string()),
                confidence: pattern.confidence,
                features: pattern.features.clone(),
            });
        }

        self.logger.update_pattern(strategy_logger::types::LogPatternUpdate {
            pattern_type: pattern.pattern_type,
            wallet_address: pattern.wallet.map(|w| w.to_string()),
            token_mint: pattern.token_mint.map(|t| t.to_string()),
            pool: pattern.pool.map(|p| p.to_string()),
            dex_type: pattern.dex_type,
            occurrences: 1,
            success_count: 0,
            avg_profit: 0.0,
            avg_delay_ms: None,
            confidence: pattern.confidence,
            features: pattern.features,
        });

        // GC old dedup entries
        if self.recent_patterns.len() > 10_000 {
            self.recent_patterns.retain(|_, v| v.elapsed() < Duration::from_secs(60));
        }
    }

    fn evict_stale_wallets(&self) {
        let cutoff = Duration::from_secs(3600); // evict wallets inactive for 1 hour
        let before = self.wallets.len();
        self.wallets.retain(|_, w| w.last_seen.elapsed() < cutoff);
        let after = self.wallets.len();
        if before != after {
            debug!(evicted = before - after, remaining = after, "wallet tracker: evicted stale wallets");
        }
    }

    /// Get count of tracked wallets.
    pub fn tracked_count(&self) -> usize {
        self.wallets.len()
    }

    /// Get count of smart money wallets.
    pub fn smart_money_count(&self) -> usize {
        self.wallets.iter()
            .filter(|w| w.category == "smart_money" || w.category == "whale")
            .count()
    }

    /// Check if a specific token has recent smart money activity.
    /// Used by ML scorer to boost opportunities involving tokens that smart money is accumulating.
    pub fn smart_money_signal(&self, token: &Pubkey) -> Option<f64> {
        let cutoff = Duration::from_secs(300); // last 5 minutes
        let mut smart_buys = 0u32;
        let mut smart_volume = 0u64;

        for wallet in self.wallets.iter() {
            if wallet.category != "smart_money" && wallet.category != "whale" {
                continue;
            }
            for swap in wallet.swaps.iter().rev() {
                if swap.timestamp.elapsed() > cutoff { break; }
                if swap.is_buy && swap.token_out == *token {
                    smart_buys += 1;
                    smart_volume += swap.amount_in;
                }
            }
        }

        if smart_buys > 0 {
            // Return a signal strength (0.0 - 1.0)
            let signal = (smart_buys as f64 / 5.0).min(1.0)
                * (smart_volume as f64 / 1_000_000_000.0).min(1.0).max(0.1);
            Some(signal)
        } else {
            None
        }
    }
}
