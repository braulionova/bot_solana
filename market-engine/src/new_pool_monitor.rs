//! new_pool_monitor.rs – Monitor PumpFun/PumpSwap graduated tokens for second pool creation.
//!
//! **Insight**: When a token graduates from PumpFun to PumpSwap, it starts with only
//! 1 pool. Minutes to hours later, someone creates a pool on Raydium/Orca/Meteora.
//! At that moment, the prices are almost always different → arbitrage opportunity
//! that lasts seconds to minutes.
//!
//! O(1) lookups via DashMap. Watch list grows ~100-500 tokens/day (graduation rate).
//! 24h TTL keeps it bounded.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use crate::cross_dex::scan_pair_opportunities;
use crate::pool_state::PoolStateCache;
use crate::pump_graduation_arb::PumpGraduationArb;
use crate::types::{DexType, RouteParams};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Remove tokens from the watch list after 24h — if no second pool by then, opportunity passed.
const WATCH_TTL: Duration = Duration::from_secs(24 * 3600);

/// Minimum pool impact (%) to flag a high-volatility swap on a watched token.
const HIGH_VOLATILITY_IMPACT_PCT: f64 = 2.0;

/// WSOL mint.
const WSOL: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");

/// USDC mint.
const USDC: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// Borrow amounts to scan arb with (lamports).
const BORROW_AMOUNTS: &[u64] = &[100_000_000, 500_000_000, 1_000_000_000]; // 0.1, 0.5, 1 SOL

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A token we are watching for a second pool creation.
#[derive(Debug, Clone)]
struct WatchedToken {
    /// The PumpSwap pool from graduation.
    pumpswap_pool: Pubkey,
    /// Slot at which the graduation was detected.
    graduation_slot: u64,
    /// Instant when the graduation was detected.
    graduated_at: Instant,
    /// Known base reserve at graduation time (raw lamports).
    base_reserve: u64,
    /// Known quote reserve at graduation time (raw lamports).
    quote_reserve: u64,
}

/// Arb opportunity detected when a second pool is created for a watched token.
#[derive(Debug, Clone)]
pub struct SecondPoolArbOpportunity {
    /// The token mint being watched.
    pub token_mint: Pubkey,
    /// The original PumpSwap pool from graduation.
    pub pumpswap_pool: Pubkey,
    /// The newly created pool on another DEX.
    pub new_pool: Pubkey,
    /// DEX type of the new pool.
    pub new_pool_dex: DexType,
    /// Time elapsed since graduation.
    pub time_since_graduation: Duration,
    /// Arb routes found (if any).
    pub routes: Vec<RouteParams>,
}

// ---------------------------------------------------------------------------
// NewPoolMonitor
// ---------------------------------------------------------------------------

pub struct NewPoolMonitor {
    /// Tokens from recent PumpSwap graduations that only have 1 pool.
    /// Key: token_mint, Value: WatchedToken info.
    watching: DashMap<Pubkey, WatchedToken>,
    pool_cache: Arc<PoolStateCache>,
    pump_grad_arb: Arc<PumpGraduationArb>,
}

impl NewPoolMonitor {
    pub fn new(pool_cache: Arc<PoolStateCache>, pump_grad_arb: Arc<PumpGraduationArb>) -> Self {
        Self {
            watching: DashMap::with_capacity(1024),
            pool_cache,
            pump_grad_arb,
        }
    }

    /// Number of tokens currently being watched.
    pub fn watching_count(&self) -> usize {
        self.watching.len()
    }

    // -----------------------------------------------------------------------
    // Event: graduation detected
    // -----------------------------------------------------------------------

    /// Called when a graduation event is detected. Adds the token to the watch list.
    pub fn on_graduation(&self, token_mint: Pubkey, pumpswap_pool: Pubkey, slot: u64) {
        // Look up initial reserves from pool cache (if already hydrated).
        let (base_reserve, quote_reserve) = self
            .pool_cache
            .get(&pumpswap_pool)
            .map(|p| (p.reserve_a, p.reserve_b))
            .unwrap_or((0, 0));

        // Check if token already has multiple pools — if so, no need to watch.
        let existing_pools = self.pool_cache.pools_for_token(&token_mint);
        if existing_pools.len() >= 2 {
            debug!(
                token = %token_mint,
                pools = existing_pools.len(),
                "token already has multiple pools at graduation time, skipping watch"
            );
            return;
        }

        self.watching.insert(
            token_mint,
            WatchedToken {
                pumpswap_pool,
                graduation_slot: slot,
                graduated_at: Instant::now(),
                base_reserve,
                quote_reserve,
            },
        );

        info!(
            token = %token_mint,
            pool = %pumpswap_pool,
            slot,
            base_reserve,
            quote_reserve,
            watching = self.watching.len(),
            "added graduated token to new-pool watch list"
        );
    }

    // -----------------------------------------------------------------------
    // Event: new pool detected on any DEX
    // -----------------------------------------------------------------------

    /// Called when a new pool creation event is detected (Raydium Initialize2,
    /// Orca InitializePool, Meteora InitializePair, etc.).
    ///
    /// Returns an arb opportunity if the token_mint is in our watch list.
    pub fn on_new_pool_detected(
        &self,
        token_mint: Pubkey,
        new_pool: Pubkey,
        dex_type: DexType,
        slot: u64,
    ) -> Option<SecondPoolArbOpportunity> {
        let watched = self.watching.get(&token_mint)?;

        let time_since = watched.graduated_at.elapsed();
        let pumpswap_pool = watched.pumpswap_pool;
        info!(
            token = %token_mint,
            pumpswap_pool = %pumpswap_pool,
            new_pool = %new_pool,
            dex = ?dex_type,
            slot,
            time_since_graduation_ms = time_since.as_millis(),
            "SECOND POOL CREATED for watched token — scanning arb"
        );

        // Scan for cross-DEX arb: PumpSwap price vs new pool price.
        let mut routes = Vec::new();

        // 1. PumpGraduationArb scanner (specialized for graduation spreads).
        let grad_routes = self.pump_grad_arb.scan_graduation_arb(
            &token_mint,
            &pumpswap_pool,
            BORROW_AMOUNTS,
        );
        routes.extend(grad_routes);

        // Release DashMap read lock before further operations.
        drop(watched);

        // 2. Generic cross-DEX pair scan (token vs WSOL).
        let pair_routes_wsol =
            scan_pair_opportunities(&self.pool_cache, token_mint, WSOL, BORROW_AMOUNTS);
        routes.extend(pair_routes_wsol);

        // 3. Also scan token vs USDC in case the new pool is USDC-paired.
        let pair_routes_usdc =
            scan_pair_opportunities(&self.pool_cache, token_mint, USDC, BORROW_AMOUNTS);
        routes.extend(pair_routes_usdc);

        if !routes.is_empty() {
            info!(
                token = %token_mint,
                routes_found = routes.len(),
                best_profit = routes.iter().map(|r| r.net_profit).max().unwrap_or(0),
                "arb routes found for second-pool event"
            );
        } else {
            debug!(
                token = %token_mint,
                "no arb routes found yet for second-pool event (reserves may not be hydrated)"
            );
        }

        // Remove from watch list — second pool found, no need to keep watching.
        self.watching.remove(&token_mint);

        Some(SecondPoolArbOpportunity {
            token_mint,
            pumpswap_pool,
            new_pool,
            new_pool_dex: dex_type,
            time_since_graduation: time_since,
            routes,
        })
    }

    // -----------------------------------------------------------------------
    // Event: swap detected on a watched token's pool
    // -----------------------------------------------------------------------

    /// Called when a swap is detected on any pool. Checks if the pool belongs
    /// to a watched token and if the swap has high price impact.
    pub fn on_swap_detected(
        &self,
        pool: Pubkey,
        token_mint: Pubkey,
        amount_in: u64,
        slot: u64,
    ) {
        let _watched = match self.watching.get(&token_mint) {
            Some(w) => w,
            None => return, // Not a watched token — fast path exit.
        };

        let pool_data = match self.pool_cache.get(&pool) {
            Some(p) => p,
            None => return,
        };

        let reserve_in = pool_data.reserve_a.max(pool_data.reserve_b);
        if reserve_in == 0 {
            return;
        }

        let impact_pct = (amount_in as f64 / reserve_in as f64) * 100.0;
        if impact_pct > HIGH_VOLATILITY_IMPACT_PCT {
            info!(
                token = %token_mint,
                pool = %pool,
                amount_in,
                impact_pct = format!("{:.2}", impact_pct),
                slot,
                "high volatility swap on watched PumpSwap token"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Periodic: check if any watched tokens now have multiple pools
    // -----------------------------------------------------------------------

    /// Periodic scan: for each watched token, check the pool cache to see if
    /// a second pool has appeared (e.g., discovered via pool_hydrator or
    /// getProgramAccounts scan). Returns arb opportunities for any found.
    pub fn check_watched_tokens(&self) -> Vec<SecondPoolArbOpportunity> {
        let mut opportunities = Vec::new();
        let mut to_remove = Vec::new();

        for entry in self.watching.iter() {
            let token_mint = *entry.key();
            let watched = entry.value();

            // TTL check: remove stale entries.
            if watched.graduated_at.elapsed() > WATCH_TTL {
                to_remove.push(token_mint);
                continue;
            }

            // Check if token now has 2+ pools in the cache.
            let pools = self.pool_cache.pools_for_token(&token_mint);
            if pools.len() < 2 {
                continue;
            }

            // Find the non-PumpSwap pool(s).
            let new_pools: Vec<_> = pools
                .iter()
                .filter(|p| p.pool_address != watched.pumpswap_pool)
                .collect();

            if new_pools.is_empty() {
                continue;
            }

            let new_pool = &new_pools[0];
            let time_since = watched.graduated_at.elapsed();

            info!(
                token = %token_mint,
                pumpswap_pool = %watched.pumpswap_pool,
                new_pool = %new_pool.pool_address,
                new_dex = ?new_pool.dex_type,
                total_pools = pools.len(),
                time_since_graduation_s = time_since.as_secs(),
                "periodic check: second pool found for watched token"
            );

            // Scan arb routes.
            let mut routes = Vec::new();

            let grad_routes = self.pump_grad_arb.scan_graduation_arb(
                &token_mint,
                &watched.pumpswap_pool,
                BORROW_AMOUNTS,
            );
            routes.extend(grad_routes);

            let pair_routes =
                scan_pair_opportunities(&self.pool_cache, token_mint, WSOL, BORROW_AMOUNTS);
            routes.extend(pair_routes);

            let pair_routes_usdc =
                scan_pair_opportunities(&self.pool_cache, token_mint, USDC, BORROW_AMOUNTS);
            routes.extend(pair_routes_usdc);

            opportunities.push(SecondPoolArbOpportunity {
                token_mint,
                pumpswap_pool: watched.pumpswap_pool,
                new_pool: new_pool.pool_address,
                new_pool_dex: new_pool.dex_type,
                time_since_graduation: time_since,
                routes,
            });

            to_remove.push(token_mint);
        }

        // Clean up: remove tokens that expired or got matched.
        for mint in to_remove {
            self.watching.remove(&mint);
        }

        opportunities
    }

    // -----------------------------------------------------------------------
    // Cleanup: remove expired entries
    // -----------------------------------------------------------------------

    /// Remove tokens older than 24h from the watch list. Call periodically.
    pub fn cleanup_expired(&self) -> usize {
        let mut removed = 0;
        self.watching.retain(|_mint, watched| {
            if watched.graduated_at.elapsed() > WATCH_TTL {
                removed += 1;
                false
            } else {
                true
            }
        });
        if removed > 0 {
            debug!(removed, remaining = self.watching.len(), "cleaned up expired watched tokens");
        }
        removed
    }

    // -----------------------------------------------------------------------
    // Telegram message formatter
    // -----------------------------------------------------------------------

    /// Format a Telegram notification for a second pool event.
    pub fn format_telegram_alert(opp: &SecondPoolArbOpportunity) -> String {
        let best_profit = opp
            .routes
            .iter()
            .map(|r| r.net_profit)
            .max()
            .unwrap_or(0);
        let profit_sol = best_profit as f64 / 1_000_000_000.0;

        format!(
            "\u{1F525} *Second Pool Created!*\n\
             Token: `{}`\n\
             PumpSwap pool: `{}`\n\
             New pool: `{}` ({:?})\n\
             Time since graduation: {}s\n\
             Routes found: {}\n\
             Best profit: {:.6} SOL",
            opp.token_mint,
            opp.pumpswap_pool,
            opp.new_pool,
            opp.new_pool_dex,
            opp.time_since_graduation.as_secs(),
            opp.routes.len(),
            profit_sol,
        )
    }
}
