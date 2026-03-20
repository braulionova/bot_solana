// signal_processor.rs – Filters SpySignals and enriches them with pool state.
//
// Responsibilities:
//  1. Drop signals that don't involve monitored DEX programs.
//  2. Detect whale swaps (> WHALE_IMPACT_PCT pool impact).
//  3. Detect PumpSwap graduation events.
//  4. Update the pool state cache from observed transactions.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use solana_sdk::{message::VersionedMessage, pubkey::Pubkey, transaction::VersionedTransaction};
use tracing::{info, trace, warn};

use spy_node::signal_bus::SpySignal;

use crate::{
    new_pool_monitor::NewPoolMonitor,
    pool_state::PoolStateCache,
    swap_decoder::{decode_swaps, detect_pool_creations, enrich_swaps, detect_graduation as decode_graduation_event, PoolCreationDex},
    types::{dex_programs, DexType},
    wallet_tracker::WalletTracker,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Pool impact threshold (%) above which a swap is classified as a whale swap.
const WHALE_IMPACT_PCT: f64 = 1.0;

// ---------------------------------------------------------------------------
// SignalProcessor
// ---------------------------------------------------------------------------

pub struct SignalProcessor {
    monitored_programs: HashSet<Pubkey>,
    pool_cache: Arc<PoolStateCache>,
    output_tx: Sender<SpySignal>,
    targeted_refresh_tx: Option<Sender<Pubkey>>,
    wallet_tracker: Option<Arc<WalletTracker>>,
    shred_predictor: Option<Arc<ml_scorer::shred_predictor::ShredPredictor>>,
    new_pool_monitor: Option<Arc<NewPoolMonitor>>,
    /// Shred-based strategies: ultra-fast backrun + oracle arb.
    shred_engine: Option<Arc<crate::shred_strategies::ShredStrategyEngine>>,
    /// Direct route emitter: bypass route engine for shred-detected arb.
    direct_route_tx: Option<Sender<crate::types::RouteParams>>,
}

impl SignalProcessor {
    pub fn new(
        pool_cache: Arc<PoolStateCache>,
        output_tx: Sender<SpySignal>,
        targeted_refresh_tx: Option<Sender<Pubkey>>,
    ) -> Self {
        Self {
            monitored_programs: dex_programs::all().into_iter().collect(),
            pool_cache: pool_cache.clone(),
            output_tx,
            targeted_refresh_tx,
            wallet_tracker: None,
            shred_predictor: None,
            new_pool_monitor: None,
            shred_engine: Some(Arc::new(crate::shred_strategies::ShredStrategyEngine::new(pool_cache))),
            direct_route_tx: None,
        }
    }

    /// Attach a wallet tracker for smart money analysis.
    pub fn with_wallet_tracker(mut self, wt: Arc<WalletTracker>) -> Self {
        self.wallet_tracker = Some(wt);
        self
    }

    /// Attach a shred predictor for ML-based swap→arb correlation.
    pub fn with_shred_predictor(mut self, sp: Arc<ml_scorer::shred_predictor::ShredPredictor>) -> Self {
        self.shred_predictor = Some(sp);
        self
    }

    /// Attach a new pool monitor for second-pool arb detection.
    pub fn with_new_pool_monitor(mut self, npm: Arc<NewPoolMonitor>) -> Self {
        self.new_pool_monitor = Some(npm);
        self
    }

    pub fn with_direct_route_tx(mut self, tx: Sender<crate::types::RouteParams>) -> Self {
        self.direct_route_tx = Some(tx);
        self
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Process a stream of raw `SpySignal`s from the spy-node.
    pub fn run(&self, input: Receiver<SpySignal>) {
        for signal in input {
            if let Err(e) = self.process(signal) {
                warn!(error = %e, "signal processing error");
            }
        }
    }

    /// Process a single signal.
    pub fn process(&self, signal: SpySignal) -> Result<()> {
        match &signal {
            SpySignal::NewTransaction {
                slot,
                tx,
                detected_at,
            } => {
                self.handle_new_tx(*slot, tx, *detected_at)?;
            }
            SpySignal::WhaleSwap { .. }
            | SpySignal::Graduation { .. }
            | SpySignal::LiquidityEvent { .. } => {
                // Already classified – forward directly.
                self.output_tx.send(signal).ok();
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal handlers
    // -----------------------------------------------------------------------

    fn handle_new_tx(
        &self,
        slot: u64,
        tx: &VersionedTransaction,
        detected_at: Instant,
    ) -> Result<()> {
        let program_ids = extract_program_ids(tx);

        // Check ALL account keys (not just outer programs) for graduation detection.
        // PumpFun graduation TXs call PumpSwap via CPI — PumpSwap only appears in
        // account_keys, not as an outer instruction program.
        // Also check for Raydium LaunchLab (BONK.fun) and Moonshot migrations.
        let all_account_keys = extract_all_account_keys(tx);
        let launchlab: Pubkey = crate::swap_decoder::RAYDIUM_LAUNCHLAB.parse().unwrap();
        let moonshot: Pubkey = crate::swap_decoder::MOONSHOT.parse().unwrap();
        let grad_source: Option<&'static str> =
            if all_account_keys.contains(&launchlab) {
                Some("LaunchLab (BONK.fun)")
            } else if all_account_keys.contains(&moonshot) {
                Some("Moonshot")
            } else if all_account_keys.contains(&dex_programs::pumpswap()) {
                Some("PumpSwap")
            } else {
                None
            };
        if let Some(source) = grad_source {
            if let Some(grad) = decode_graduation_event(tx) {
                info!(slot, %grad.token_mint, %grad.pump_pool, source, "graduation event detected");

                // Insert the PumpSwap pool into the cache immediately so cross-DEX
                // arb scanners can find it. Use estimated reserves (~79 SOL typical).
                {
                    use crate::pool_state::DexPool;
                    let wsol: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
                    let (token_a, token_b) = match (&grad.pool_base_mint, &grad.pool_quote_mint) {
                        (Some(base), Some(quote)) => (*base, *quote),
                        _ => (wsol, grad.token_mint), // default: WSOL/token
                    };
                    // Use estimated reserves ONLY if token has ≤2 pools (fresh graduation).
                    // Tokens that re-graduate 10+ times (like 4wTV1YmiEk) create phantom routes.
                    let existing_pools = self.pool_cache.pools_for_token(&grad.token_mint).len();
                    let (est_reserve_a, est_reserve_b) = if existing_pools <= 2 {
                        // Fresh graduation: ~79 SOL + tokens (PumpFun standard)
                        if token_a == wsol {
                            (79_000_000_000u64, 206_000_000_000u64)
                        } else {
                            (206_000_000_000u64, 79_000_000_000u64)
                        }
                    } else {
                        // Re-graduation spam: insert with 0, wait for RPC refresh
                        (0u64, 0u64)
                    };
                    let pool = DexPool {
                        pool_address: grad.pump_pool,
                        dex_type: DexType::PumpSwap,
                        token_a,
                        token_b,
                        decimals_a: 9,
                        decimals_b: if token_a == wsol { 6 } else { 9 },
                        reserve_a: est_reserve_a,
                        reserve_b: est_reserve_b,
                        reserve_a_optimistic: 0,
                        reserve_b_optimistic: 0,
                        last_shred_update: std::time::Instant::now(),
                        pool_vault_a_balance: 0,
                        pool_vault_b_balance: 0,
                        market_vault_a_balance: 0,
                        market_vault_b_balance: 0,
                        last_updated: std::time::Instant::now(),
                        fee_bps: 25,
                        raydium_meta: None,
                        orca_meta: None,
                        meteora_meta: None,
                    };
                    self.pool_cache.upsert(pool);
                    info!(%grad.pump_pool, %grad.token_mint, existing_pools, "inserted PumpSwap graduation pool into cache");

                    // INLINE fast refresh: fetch real reserves in ~150ms via RPC
                    // This is 20x faster than the 3s background refresh.
                    {
                        let cache = self.pool_cache.clone();
                        let pool_addr = grad.pump_pool;
                        std::thread::spawn(move || {
                            use solana_rpc_client::rpc_client::RpcClient;
                            use solana_sdk::commitment_config::CommitmentConfig;
                            let rpc = RpcClient::new_with_timeout_and_commitment(
                                "https://solana-rpc.publicnode.com".to_string(),
                                std::time::Duration::from_millis(500),
                                CommitmentConfig::processed(),
                            );
                            // Step 1: fetch pool account to get vault addresses
                            if let Ok(Some(acc)) = rpc.get_account_with_commitment(
                                &pool_addr, CommitmentConfig::processed()
                            ).map(|r| r.value) {
                                if acc.data.len() >= 203 {
                                    let vault_a = solana_sdk::pubkey::Pubkey::try_from(&acc.data[139..171]).unwrap_or_default();
                                    let vault_b = solana_sdk::pubkey::Pubkey::try_from(&acc.data[171..203]).unwrap_or_default();
                                    // Step 2: fetch vault balances
                                    if let Ok(vaccs) = rpc.get_multiple_accounts(&[vault_a, vault_b]) {
                                        for (i, maybe) in vaccs.iter().enumerate() {
                                            if let Some(vacc) = maybe {
                                                if vacc.data.len() >= 72 {
                                                    let amount = u64::from_le_bytes(
                                                        vacc.data[64..72].try_into().unwrap_or([0u8; 8]),
                                                    );
                                                    cache.update_reserve_by_vault(&pool_addr, i == 0, amount);
                                                }
                                            }
                                        }
                                        tracing::info!(
                                            pool = %pool_addr,
                                            "graduation fast-refresh: real reserves loaded"
                                        );
                                    }
                                }
                            }
                        });
                    }

                    // Also request background RPC hydration (backup).
                    if let Some(tx) = &self.targeted_refresh_tx {
                        let _ = tx.try_send(grad.pump_pool);
                    }
                }

                // Notify new_pool_monitor: add graduated token to watch list.
                if let Some(ref npm) = self.new_pool_monitor {
                    npm.on_graduation(grad.token_mint, grad.pump_pool, slot);
                }

                self.output_tx
                    .send(SpySignal::Graduation {
                        slot,
                        token_mint: grad.token_mint,
                        pump_pool: grad.pump_pool,
                        source,
                        creator: grad.creator,
                        pool_base_mint: grad.pool_base_mint,
                        pool_quote_mint: grad.pool_quote_mint,
                    })
                    .ok();
                return Ok(());
            }
        }

        // ── Pool creation detection (for new_pool_monitor) ──
        if let Some(ref npm) = self.new_pool_monitor {
            let pool_creations = detect_pool_creations(tx);
            for creation in &pool_creations {
                let dex_type = match creation.dex {
                    PoolCreationDex::RaydiumAmmV4 => DexType::RaydiumAmmV4,
                    PoolCreationDex::RaydiumCpmm => DexType::RaydiumCpmm,
                    PoolCreationDex::OrcaWhirlpool => DexType::OrcaWhirlpool,
                    PoolCreationDex::MeteoraDlmm => DexType::MeteoraDlmm,
                };

                // Insert newly created pool into cache so arb scanners can find it.
                // Reserves are unknown at creation time — use 0 and rely on RPC hydration
                // or the initial liquidity deposit that follows immediately.
                if self.pool_cache.get(&creation.pool).is_none() {
                    use crate::pool_state::DexPool;
                    let pool = DexPool {
                        pool_address: creation.pool,
                        dex_type,
                        token_a: creation.token_a,
                        token_b: creation.token_b,
                        decimals_a: 9, decimals_b: 6,
                        reserve_a: 0, reserve_b: 0,
                        reserve_a_optimistic: 0, reserve_b_optimistic: 0,
                        last_shred_update: std::time::Instant::now(),
                        pool_vault_a_balance: 0, pool_vault_b_balance: 0,
                        market_vault_a_balance: 0, market_vault_b_balance: 0,
                        last_updated: std::time::Instant::now(),
                        fee_bps: 25,
                        raydium_meta: None, orca_meta: None, meteora_meta: None,
                    };
                    self.pool_cache.upsert(pool);
                    info!(%creation.pool, ?dex_type, %creation.token_a, %creation.token_b,
                        "inserted new pool into cache from pool creation event");

                    // Inline fast refresh for new pool (~150ms)
                    {
                        let cache = self.pool_cache.clone();
                        let pool_addr = creation.pool;
                        let is_pumpswap = dex_type == DexType::PumpSwap;
                        std::thread::spawn(move || {
                            use solana_rpc_client::rpc_client::RpcClient;
                            use solana_sdk::commitment_config::CommitmentConfig;
                            let rpc = RpcClient::new_with_timeout_and_commitment(
                                "https://solana-rpc.publicnode.com".to_string(),
                                std::time::Duration::from_millis(500),
                                CommitmentConfig::processed(),
                            );
                            if is_pumpswap {
                                // PumpSwap: pool data has vaults at offsets 139, 171
                                if let Ok(Some(acc)) = rpc.get_account_with_commitment(
                                    &pool_addr, CommitmentConfig::processed()
                                ).map(|r| r.value) {
                                    if acc.data.len() >= 203 {
                                        let va = solana_sdk::pubkey::Pubkey::try_from(&acc.data[139..171]).unwrap_or_default();
                                        let vb = solana_sdk::pubkey::Pubkey::try_from(&acc.data[171..203]).unwrap_or_default();
                                        if let Ok(vaccs) = rpc.get_multiple_accounts(&[va, vb]) {
                                            for (i, maybe) in vaccs.iter().enumerate() {
                                                if let Some(vacc) = maybe {
                                                    if vacc.data.len() >= 72 {
                                                        let amount = u64::from_le_bytes(
                                                            vacc.data[64..72].try_into().unwrap_or([0u8; 8]),
                                                        );
                                                        cache.update_reserve_by_vault(&pool_addr, i == 0, amount);
                                                    }
                                                }
                                            }
                                            tracing::info!(pool = %pool_addr, "pool-creation fast-refresh: reserves loaded");
                                        }
                                    }
                                }
                            }
                            // For Raydium/Orca/Meteora: use targeted_refresh (more complex account layout)
                        });
                    }

                    if let Some(tx) = &self.targeted_refresh_tx {
                        let _ = tx.try_send(creation.pool);
                    }
                }

                // Check both mints — the token could be on either side.
                if let Some(opp) = npm.on_new_pool_detected(creation.token_a, creation.pool, dex_type, slot) {
                    if !opp.routes.is_empty() {
                        info!(
                            token = %opp.token_mint,
                            new_pool = %opp.new_pool,
                            dex = ?opp.new_pool_dex,
                            routes = opp.routes.len(),
                            "second-pool arb routes emitted"
                        );
                    }
                }
                if let Some(opp) = npm.on_new_pool_detected(creation.token_b, creation.pool, dex_type, slot) {
                    if !opp.routes.is_empty() {
                        info!(
                            token = %opp.token_mint,
                            new_pool = %opp.new_pool,
                            dex = ?opp.new_pool_dex,
                            routes = opp.routes.len(),
                            "second-pool arb routes emitted"
                        );
                    }
                }
            }
        }

        // ── LP event detection (liquidity add/remove) ──
        {
            let lp_events = crate::liquidity_events::detect_lp_events(tx);
            for ev in &lp_events {
                if ev.is_remove && ev.estimated_amount > 0 {
                    // Large LP removal → amplified price impact on next swap.
                    // Emit as whale-like signal to trigger cross-DEX scan.
                    if let Some(pool_data) = self.pool_cache.get(&ev.pool) {
                        let impact = ev.estimated_amount as f64 / pool_data.reserve_a.max(1) as f64 * 100.0;
                        if impact > 1.0 {
                            info!(slot, pool = %ev.pool, impact = impact, "LP REMOVE detected (>1% impact)");
                            self.output_tx.send(SpySignal::LiquidityEvent {
                                slot,
                                pool: ev.pool,
                                event_type: spy_node::signal_bus::LiquidityEventType::Removed {
                                    amount_a: ev.estimated_amount,
                                    amount_b: 0,
                                },
                            }).ok();
                        }
                    }
                }
            }
        }

        // Filter: only care about transactions that touch at least one monitored DEX.
        let dex_programs_in_tx: Vec<Pubkey> = program_ids
            .iter()
            .filter(|pk| self.monitored_programs.contains(*pk))
            .copied()
            .collect();

        if dex_programs_in_tx.is_empty() {
            trace!(slot, "tx does not touch monitored DEX, dropping");
            return Ok(());
        }

        trace!(
            slot,
            dexes = dex_programs_in_tx.len(),
            "DEX transaction detected"
        );

        // ── Hot-path: decode swap and update pool reserves in real-time (0ms latency) ──
        // Uses the swap_decoder with real instruction discriminators to extract amount_in,
        // direction, and pool address. Applies XY=K delta to the in-memory pool cache,
        // invalidating the route cache so the next Bellman-Ford run sees fresh state.
        let decoded_swaps = decode_swaps(tx);
        let mut whale_detected = false;
        let mut refresh_queued = HashSet::new();

        for swap in &decoded_swaps {
            // Feed swap to shred predictor for ML correlation learning.
            if let Some(ref predictor) = self.shred_predictor {
                let dex_str = format!("{:?}", swap.dex);
                predictor.observe_swap(&swap.pool.to_string(), &dex_str, swap.amount_in, slot);
            }

            // Backrun analysis: calculate profit ceiling from user's slippage tolerance.
            if let Some(pool) = self.pool_cache.get(&swap.pool) {
                if swap.amount_out_min > 0 && swap.amount_in > 50_000_000 { // >0.05 SOL
                    let analysis = crate::backrun_calculator::analyze_backrun(swap, &pool);
                    if analysis.is_profitable {
                        info!(
                            slot,
                            pool = %swap.pool,
                            slippage_bps = analysis.slippage_bps,
                            profit_ceiling = analysis.profit_ceiling,
                            recommended = analysis.recommended_amount,
                            "BACKRUN opportunity: user slippage exploitable"
                        );
                    }
                }
            }

            // Update pool reserves immediately via XY=K approximation.
            if let Some(pool) = self.pool_cache.get(&swap.pool) {
                let amount_out = if swap.a_to_b {
                    pool.quote_a_to_b(swap.amount_in)
                } else {
                    pool.quote_b_to_a(swap.amount_in)
                };
                // apply_swap_delta increments swap_gen → invalidates route cache.
                self.pool_cache.apply_swap_delta(
                    &swap.pool,
                    swap.amount_in,
                    amount_out,
                    swap.a_to_b,
                );
                if refresh_queued.insert(swap.pool) {
                    if let Some(tx) = &self.targeted_refresh_tx {
                        let _ = tx.try_send(swap.pool);
                    }
                }

                // ── Shred strategy engine: ultra-fast backrun on every swap ──
                if let Some(ref engine) = self.shred_engine {
                    let shred_routes = engine.on_swap_detected(swap, slot);
                    if let Some(ref route_tx) = self.direct_route_tx {
                        for route in shred_routes {
                            info!(
                                strategy = route.strategy,
                                net_profit = route.net_profit,
                                hops = route.hops.len(),
                                "SHRED-STRATEGY: direct route emitted"
                            );
                            let _ = route_tx.try_send(route);
                        }
                    }
                }

                // ── Instant cross-DEX check: compare price with counterpart pool ──
                // After apply_swap_delta, THIS pool has fresh reserves.
                // Check if the same token has a pool on another DEX with a different price.
                // This detects spreads within the SAME SLOT as the swap that created them.
                {
                    let token = if swap.a_to_b { pool.token_b } else { pool.token_a };
                    let other_pools = self.pool_cache.pools_for_token(&token);
                    for other in &other_pools {
                        if other.pool_address == swap.pool { continue; }
                        if other.reserve_a == 0 || other.reserve_b == 0 { continue; }
                        // Compare rates: how much token_out do we get for 0.1 SOL?
                        let test_amount = 100_000_000u64; // 0.1 SOL
                        let updated_pool = self.pool_cache.get(&swap.pool).unwrap_or(pool.clone());
                        let rate_this = if updated_pool.token_a == token {
                            updated_pool.quote_b_to_a(test_amount) // SOL→token
                        } else {
                            updated_pool.quote_a_to_b(test_amount)
                        };
                        let rate_other = if other.token_a == token {
                            other.quote_b_to_a(test_amount)
                        } else {
                            other.quote_a_to_b(test_amount)
                        };
                        if rate_this == 0 || rate_other == 0 { continue; }
                        let spread_bps = if rate_this > rate_other {
                            ((rate_this as f64 / rate_other as f64) - 1.0) * 10_000.0
                        } else {
                            ((rate_other as f64 / rate_this as f64) - 1.0) * 10_000.0
                        };
                        // Fee: PumpSwap 25bps + Raydium 25bps = 50bps minimum
                        // Filter: both pools must have reasonably fresh reserves.
                        // Spreads >500% are almost certainly stale reserve artifacts.
                        if spread_bps > 60.0 && spread_bps < 5000.0 && other.last_updated.elapsed().as_secs() < 300 {
                            info!(
                                slot,
                                pool_a = %swap.pool,
                                pool_b = %other.pool_address,
                                dex_a = ?updated_pool.dex_type,
                                dex_b = ?other.dex_type,
                                spread_bps = spread_bps as u64,
                                "INSTANT cross-DEX spread detected after swap!"
                            );
                            // Emit as LiquidityEvent to trigger route engine scan
                            self.output_tx.send(SpySignal::LiquidityEvent {
                                slot,
                                pool: swap.pool,
                                event_type: spy_node::signal_bus::LiquidityEventType::Added { amount_a: 0, amount_b: 0 },
                            }).ok();
                        }
                    }
                }

                // Whale detection using real reserves.
                let reserve_in = if swap.a_to_b {
                    pool.reserve_a
                } else {
                    pool.reserve_b
                };
                let impact = if reserve_in > 0 {
                    (swap.amount_in as f64 / reserve_in as f64) * 100.0
                } else {
                    0.0
                };

                if impact >= WHALE_IMPACT_PCT {
                    info!(
                        slot,
                        pool = %swap.pool,
                        impact_pct = impact,
                        amount_in = swap.amount_in,
                        "whale swap detected"
                    );
                    self.output_tx
                        .send(SpySignal::WhaleSwap {
                            slot,
                            tx: tx.clone(),
                            pool: swap.pool,
                            impact_pct: impact,
                        })
                        .ok();
                    whale_detected = true;
                }

                // ── REACTIVE DISCOVERY: high-impact swap → check cross-DEX arb ──
                if impact >= 50.0 {
                    let wsol: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
                    let token = if pool.token_a == wsol { pool.token_b } else { pool.token_a };
                    let counterparts = self.pool_cache.pools_for_token(&token);
                    let n_cross = counterparts.iter()
                        .filter(|p| p.pool_address != swap.pool && p.reserve_a > 0)
                        .count();
                    if n_cross > 0 {
                        info!(
                            slot, pool = %swap.pool, token = %token,
                            impact_pct = impact, cross_dex = n_cross,
                            "HIGH-IMPACT + CROSS-DEX → ARB CANDIDATE"
                        );
                        for cp in counterparts.iter().filter(|p| p.pool_address != swap.pool) {
                            if let Some(ref rtx) = self.targeted_refresh_tx {
                                let _ = rtx.try_send(cp.pool_address);
                            }
                        }
                    }
                }
            } else {
                // Unknown pool — still bump swap_gen to trigger route re-evaluation.
                // A new pool may have appeared (graduation event).
                self.pool_cache
                    .apply_swap_delta(&swap.pool, 0, 0, swap.a_to_b);
            }

            // Notify new_pool_monitor of swap on watched tokens.
            if let Some(ref npm) = self.new_pool_monitor {
                // Resolve token mints from pool cache for the swap.
                if let Some(pool_data) = self.pool_cache.get(&swap.pool) {
                    // Check both token_a and token_b — either could be the watched token.
                    npm.on_swap_detected(swap.pool, pool_data.token_a, swap.amount_in, slot);
                    npm.on_swap_detected(swap.pool, pool_data.token_b, swap.amount_in, slot);
                }
            }
        }

        // Feed wallet tracker with enriched swap info (non-blocking).
        if let Some(ref wt) = self.wallet_tracker {
            if !decoded_swaps.is_empty() {
                let pc = &self.pool_cache;
                let enriched = enrich_swaps(
                    tx,
                    &decoded_swaps,
                    |pool| pc.get(pool).map(|p| (p.token_a, p.token_b)),
                    |pool, amt, a2b| {
                        pc.get(pool)
                            .map(|p| if a2b { p.quote_a_to_b(amt) } else { p.quote_b_to_a(amt) })
                            .unwrap_or(0)
                    },
                );
                for si in &enriched {
                    wt.observe_swap(si, slot, None);
                }
            }
        }

        if decoded_swaps.is_empty() {
            self.pool_cache.mark_market_activity();
        }

        // Graduation detection already handled above (before DEX filter).

        // Forward all DEX transactions to route engine for opportunity detection.
        // The route engine will re-run Bellman-Ford because swap_gen changed.
        if !whale_detected {
            self.output_tx
                .send(SpySignal::NewTransaction {
                    slot,
                    tx: tx.clone(),
                    detected_at,
                })
                .ok();
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract ALL account keys in a transaction (programs + accounts).
/// Used for detecting CPI targets like PumpSwap in graduation TXs.
fn extract_all_account_keys(tx: &VersionedTransaction) -> Vec<Pubkey> {
    match &tx.message {
        VersionedMessage::Legacy(msg) => msg.account_keys.clone(),
        VersionedMessage::V0(msg) => msg.account_keys.clone(),
    }
}

/// Extract all unique program IDs referenced in a transaction's message.
fn extract_program_ids(tx: &VersionedTransaction) -> Vec<Pubkey> {
    match &tx.message {
        VersionedMessage::Legacy(msg) => msg
            .instructions
            .iter()
            .map(|ix| msg.account_keys[ix.program_id_index as usize])
            .collect(),
        VersionedMessage::V0(msg) => msg
            .instructions
            .iter()
            .map(|ix| msg.account_keys[ix.program_id_index as usize])
            .collect(),
    }
}
