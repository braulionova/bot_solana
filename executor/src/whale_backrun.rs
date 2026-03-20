//! whale_backrun.rs — Whale Backrun Strategy (P0)
//!
//! When a WhaleSwap signal arrives (pool impact >= 1%), simulate the whale's
//! effect on pool reserves using constant-product (XY=K), then scan cross-DEX
//! counterpart pools for the same token pair.  If the whale pushed the price on
//! one DEX while the counterpart still has the old price, build a 2-hop arb
//! route: buy cheap on the un-affected pool → sell expensive on the whale-affected
//! pool (or vice-versa).
//!
//! Non-blocking: vault refresh via RpcPool has a 400ms timeout per endpoint.

use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use market_engine::cross_dex::dex_program_for;
use market_engine::pool_state::{DexPool, PoolStateCache};
use market_engine::types::{Hop, RouteParams};

use crate::rpc_pool::RpcPool;

/// WSOL mint — all profits denominated in SOL.
const WSOL: &str = "So11111111111111111111111111111111111111112";

/// Borrow amounts to try (lamports): 0.1, 0.5, 1.0 SOL.
const BORROW_AMOUNTS: [u64; 3] = [100_000_000, 500_000_000, 1_000_000_000];

/// Minimum net profit (lamports) to emit a route.
const MIN_PROFIT_LAMPORTS: u64 = 5_000;

/// Minimum whale impact (%) to bother scanning counterparts.
const MIN_IMPACT_PCT: f64 = 1.0;

/// Maximum signal age for counterpart reserves (seconds).
const MAX_COUNTERPART_AGE_SECS: u64 = 7200; // 2h — RPC refresh in step 4 will get fresh data

pub struct WhaleBackrunner {
    pub pool_cache: Arc<PoolStateCache>,
    pub vault_pool: Arc<RpcPool>,
}

impl WhaleBackrunner {
    pub fn new(pool_cache: Arc<PoolStateCache>, vault_pool: Arc<RpcPool>) -> Self {
        Self {
            pool_cache,
            vault_pool,
        }
    }

    /// Check whether a whale swap on `pool_addr` creates a cross-DEX arb opportunity.
    ///
    /// Returns zero or more `RouteParams` ready for the executor pipeline.
    /// Non-blocking aside from a single RPC vault refresh (400ms timeout).
    pub fn check_backrun(
        &self,
        pool_addr: &Pubkey,
        amount_in: u64,
        a_to_b: bool,
        slot: u64,
    ) -> Vec<RouteParams> {
        // 1. Look up the whale's pool in cache.
        let whale_pool = match self.pool_cache.get(pool_addr) {
            Some(p) => p,
            None => {
                return vec![];
            }
        };

        // Quick check: find counterparts BEFORE expensive operations.
        // The "affected token" is the non-anchor token (the meme coin).
        // We look for other pools that trade the SAME meme coin.
        let wsol: Pubkey = WSOL.parse().unwrap();
        let affected_token = if whale_pool.token_a == wsol {
            whale_pool.token_b
        } else if whale_pool.token_b == wsol {
            whale_pool.token_a
        } else {
            // Neither side is WSOL — can't flash-borrow for this pair.
            return vec![];
        };
        let n_counterparts = self.pool_cache.pools_for_token(&affected_token)
            .iter()
            .filter(|p| p.pool_address != *pool_addr && p.reserve_a > 0)
            .count();
        if n_counterparts == 0 {
            return vec![];
        }
        info!(
            pool = %pool_addr,
            token = %affected_token,
            counterparts = n_counterparts,
            amount_in,
            "whale_backrun: checking"
        );

        // Compute impact to double-check threshold.
        let (ra, rb) = whale_pool.effective_reserves();
        if ra == 0 || rb == 0 {
            return vec![];
        }
        let impact = if a_to_b {
            amount_in as f64 / ra as f64 * 100.0
        } else {
            amount_in as f64 / rb as f64 * 100.0
        };
        if impact < MIN_IMPACT_PCT {
            debug!(pool = %pool_addr, impact, "whale_backrun: impact below threshold");
            return vec![];
        }

        // 2. Simulate post-whale reserves using XY=K.
        let (post_ra, post_rb) = simulate_post_swap(ra, rb, amount_in, a_to_b);
        if post_ra == 0 || post_rb == 0 {
            return vec![];
        }

        // 3. Find cross-DEX counterpart pools for the affected token.
        //    The whale moved the price of the token they bought (output token).
        let affected_token = if a_to_b { whale_pool.token_b } else { whale_pool.token_a };
        let counterparts = self.pool_cache.pools_for_token(&affected_token);
        let counterparts: Vec<DexPool> = counterparts
            .into_iter()
            .filter(|p| {
                p.pool_address != *pool_addr
                    && p.reserve_a > 0
                    && p.reserve_b > 0
                    && p.last_updated.elapsed().as_secs() < MAX_COUNTERPART_AGE_SECS
            })
            .collect();

        if counterparts.is_empty() {
            debug!(pool = %pool_addr, token = %affected_token, "whale_backrun: no counterparts");
            return vec![];
        }

        // 4. Refresh counterpart vaults via RPC pool for freshest reserves.
        let refreshed_counterparts = self.refresh_counterpart_vaults(&counterparts);

        // 5. For each counterpart, check arb profitability across borrow amounts.
        let wsol: Pubkey = WSOL.parse().unwrap();
        let mut routes = Vec::new();

        for cp in &refreshed_counterparts {
            // Both pools must share the same pair (token_a/token_b might be in different order).
            let (whale_has_a, whale_has_b) = (whale_pool.token_a, whale_pool.token_b);
            let (cp_has_a, cp_has_b) = (cp.token_a, cp.token_b);

            // Determine token alignment: which token is the "affected" one in each pool.
            let whale_affected_is_a_side = whale_has_a == affected_token;
            let cp_affected_is_a_side = cp_has_a == affected_token;

            // The other token in the pair (the one we borrow / route through).
            let other_token = if whale_affected_is_a_side {
                whale_has_b
            } else {
                whale_has_a
            };

            // Counterpart must have the same other token.
            if cp_affected_is_a_side && cp_has_b != other_token {
                continue;
            }
            if !cp_affected_is_a_side && cp_has_a != other_token {
                continue;
            }

            // We need one of the tokens to be WSOL (or route through WSOL for borrowing).
            // For simplicity, require that the "other" token is WSOL so we can borrow directly.
            if other_token != wsol {
                continue;
            }

            let (cp_ra, cp_rb) = cp.effective_reserves();
            if cp_ra == 0 || cp_rb == 0 {
                continue;
            }

            for &borrow in &BORROW_AMOUNTS {
                // Skip borrow amounts that exceed pool liquidity.
                if whale_affected_is_a_side && borrow > post_rb / 2 {
                    continue;
                }
                if !whale_affected_is_a_side && borrow > post_ra / 2 {
                    continue;
                }

                // Determine direction: whale bought affected_token (price up on whale pool).
                // So affected_token is cheaper on counterpart → buy on counterpart, sell on whale pool.
                //
                // Hop 1: WSOL → affected_token on counterpart (buy cheap).
                // Hop 2: affected_token → WSOL on whale pool (sell expensive, post-whale reserves).

                // Hop 1: buy affected_token on counterpart.
                let mid_amount = if cp_affected_is_a_side {
                    // WSOL is token_b on counterpart, affected is token_a.
                    // b_to_a: input WSOL (b), get affected (a).
                    cp.quote_b_to_a(borrow)
                } else {
                    // WSOL is token_a on counterpart, affected is token_b.
                    // a_to_b: input WSOL (a), get affected (b).
                    cp.quote_a_to_b(borrow)
                };

                if mid_amount == 0 {
                    continue;
                }

                // Hop 2: sell affected_token on whale pool (using post-whale reserves).
                let final_amount = if whale_affected_is_a_side {
                    // affected is token_a on whale pool → sell a for b (WSOL).
                    // a_to_b on post-whale reserves.
                    xy_k_quote(post_ra, post_rb, mid_amount, true, whale_pool.fee_bps)
                } else {
                    // affected is token_b on whale pool → sell b for a (WSOL).
                    // b_to_a on post-whale reserves.
                    xy_k_quote(post_ra, post_rb, mid_amount, false, whale_pool.fee_bps)
                };

                if final_amount == 0 {
                    continue;
                }

                let net_profit = final_amount as i64 - borrow as i64;
                if net_profit < MIN_PROFIT_LAMPORTS as i64 {
                    continue;
                }

                // Price impact on each pool.
                let impact_hop1 = borrow as f64
                    / if cp_affected_is_a_side {
                        cp_rb as f64
                    } else {
                        cp_ra as f64
                    }
                    * 100.0;
                let impact_hop2 = mid_amount as f64
                    / if whale_affected_is_a_side {
                        post_ra as f64
                    } else {
                        post_rb as f64
                    }
                    * 100.0;

                // Hop 1: buy on counterpart.
                let hop1_token_in = wsol;
                let hop1_token_out = affected_token;
                // Hop 2: sell on whale pool.
                let hop2_token_in = affected_token;
                let hop2_token_out = wsol;

                let route = RouteParams {
                    hops: vec![
                        Hop {
                            pool: cp.pool_address,
                            dex_program: dex_program_for(cp.dex_type),
                            token_in: hop1_token_in,
                            token_out: hop1_token_out,
                            amount_out: mid_amount,
                            price_impact: impact_hop1,
                        },
                        Hop {
                            pool: *pool_addr,
                            dex_program: dex_program_for(whale_pool.dex_type),
                            token_in: hop2_token_in,
                            token_out: hop2_token_out,
                            amount_out: final_amount,
                            price_impact: impact_hop2,
                        },
                    ],
                    borrow_amount: borrow,
                    gross_profit: net_profit as u64,
                    net_profit,
                    risk_factor: 0.3, // moderate — reserves are simulated, not confirmed
                    strategy: "whale_backrun",
                    tier: 2,
                };

                info!(
                    slot,
                    whale_pool = %pool_addr,
                    counterpart = %cp.pool_address,
                    borrow_sol = borrow as f64 / 1e9,
                    net_profit_lamports = net_profit,
                    impact_pct = impact,
                    "whale_backrun: arb opportunity found"
                );

                routes.push(route);
            }
        }

        // Deduplicate: keep best route per counterpart pool.
        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
        let mut seen_pools = std::collections::HashSet::new();
        routes.retain(|r| {
            let cp_pool = r.hops[0].pool;
            seen_pools.insert(cp_pool)
        });

        routes
    }

    /// Refresh vault balances for counterpart pools via RPC.
    /// Collects all vault pubkeys, does one batched `get_multiple_accounts`,
    /// and updates the pool cache with fresh reserve values.
    fn refresh_counterpart_vaults(&self, counterparts: &[DexPool]) -> Vec<DexPool> {
        // Collect vault keys and their mapping back to pools.
        let mut vault_keys: Vec<Pubkey> = Vec::with_capacity(counterparts.len() * 2);
        // (pool_index, is_vault_a)
        let mut vault_map: Vec<(usize, bool)> = Vec::with_capacity(counterparts.len() * 2);

        for (i, pool) in counterparts.iter().enumerate() {
            let (va, vb) = get_vault_pubkeys(pool);
            if va == Pubkey::default() || vb == Pubkey::default() {
                continue;
            }
            let base = vault_keys.len();
            vault_keys.push(va);
            vault_keys.push(vb);
            vault_map.push((i, true));  // vault_a at base
            vault_map.push((i, false)); // vault_b at base+1
            let _ = base; // suppress unused warning
        }

        if vault_keys.is_empty() {
            return counterparts.to_vec();
        }

        // Batch fetch — RpcPool handles 400ms timeout + rotation.
        // Solana RPC limits getMultipleAccounts to 100 keys per call.
        let mut all_accounts: Vec<Option<solana_sdk::account::Account>> =
            vec![None; vault_keys.len()];
        for chunk_start in (0..vault_keys.len()).step_by(100) {
            let chunk_end = (chunk_start + 100).min(vault_keys.len());
            let chunk = &vault_keys[chunk_start..chunk_end];
            if let Some(accs) = self.vault_pool.get_multiple_accounts(chunk) {
                for (j, acc) in accs.into_iter().enumerate() {
                    all_accounts[chunk_start + j] = acc;
                }
            }
        }

        // Parse SPL token account balances (offset 64, 8 bytes LE = amount).
        let mut refreshed: Vec<DexPool> = counterparts.to_vec();
        for (idx, (pool_idx, is_vault_a)) in vault_map.iter().enumerate() {
            if let Some(Some(acc)) = all_accounts.get(idx) {
                if acc.data.len() >= 72 {
                    let amount =
                        u64::from_le_bytes(acc.data[64..72].try_into().unwrap_or([0u8; 8]));
                    if amount > 0 {
                        let pool = &mut refreshed[*pool_idx];
                        if *is_vault_a {
                            pool.reserve_a = amount;
                        } else {
                            pool.reserve_b = amount;
                        }
                        pool.last_updated = std::time::Instant::now();
                    }
                }
            }
        }

        refreshed
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simulate post-whale reserves using constant-product (XY=K).
///
/// If `a_to_b`: whale sends `amount_in` of token_a, receives some token_b.
///   new_ra = ra + amount_in
///   new_rb = rb - (rb * amount_in / (ra + amount_in))
///
/// If `b_to_a`: whale sends `amount_in` of token_b, receives some token_a.
///   new_rb = rb + amount_in
///   new_ra = ra - (ra * amount_in / (rb + amount_in))
fn simulate_post_swap(ra: u64, rb: u64, amount_in: u64, a_to_b: bool) -> (u64, u64) {
    if ra == 0 || rb == 0 || amount_in == 0 {
        return (ra, rb);
    }

    if a_to_b {
        let new_ra = ra.saturating_add(amount_in);
        // amount_out = rb * amount_in / (ra + amount_in)
        let amount_out = (rb as u128)
            .checked_mul(amount_in as u128)
            .and_then(|n| n.checked_div(new_ra as u128))
            .unwrap_or(0) as u64;
        let new_rb = rb.saturating_sub(amount_out);
        (new_ra, new_rb)
    } else {
        let new_rb = rb.saturating_add(amount_in);
        let amount_out = (ra as u128)
            .checked_mul(amount_in as u128)
            .and_then(|n| n.checked_div(new_rb as u128))
            .unwrap_or(0) as u64;
        let new_ra = ra.saturating_sub(amount_out);
        (new_ra, new_rb)
    }
}

/// Quote output amount using constant-product formula with fee deduction.
///
/// `a_to_b=true`: input is token_a, output is token_b.
/// `a_to_b=false`: input is token_b, output is token_a.
fn xy_k_quote(ra: u64, rb: u64, amount_in: u64, a_to_b: bool, fee_bps: u64) -> u64 {
    if ra == 0 || rb == 0 || amount_in == 0 {
        return 0;
    }

    // Deduct fee from input.
    let fee = (amount_in as u128 * fee_bps as u128) / 10_000;
    let net_in = amount_in as u128 - fee;

    if a_to_b {
        let num = (rb as u128).saturating_mul(net_in);
        let den = (ra as u128).saturating_add(net_in);
        if den == 0 {
            return 0;
        }
        (num / den) as u64
    } else {
        let num = (ra as u128).saturating_mul(net_in);
        let den = (rb as u128).saturating_add(net_in);
        if den == 0 {
            return 0;
        }
        (num / den) as u64
    }
}

/// Extract vault pubkeys from a DexPool based on its DEX-specific metadata.
fn get_vault_pubkeys(pool: &DexPool) -> (Pubkey, Pubkey) {
    if let Some(ref meta) = pool.raydium_meta {
        return (meta.vault_a, meta.vault_b);
    }
    if let Some(ref meta) = pool.orca_meta {
        return (meta.token_vault_a, meta.token_vault_b);
    }
    if let Some(ref meta) = pool.meteora_meta {
        return (meta.token_vault_a, meta.token_vault_b);
    }
    // PumpSwap / CPMM / unknown: no vault metadata available — skip refresh.
    (Pubkey::default(), Pubkey::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulate_post_swap_a_to_b() {
        // Pool: 1000 A, 2000 B. Whale swaps 100 A → B.
        let (new_ra, new_rb) = simulate_post_swap(1000, 2000, 100, true);
        assert_eq!(new_ra, 1100);
        // amount_out = 2000 * 100 / 1100 = 181
        assert_eq!(new_rb, 2000 - 181);
    }

    #[test]
    fn test_simulate_post_swap_b_to_a() {
        let (new_ra, new_rb) = simulate_post_swap(1000, 2000, 200, false);
        assert_eq!(new_rb, 2200);
        // amount_out = 1000 * 200 / 2200 = 90
        assert_eq!(new_ra, 1000 - 90);
    }

    #[test]
    fn test_xy_k_quote_with_fee() {
        // 25 bps fee, 1000/2000 pool, 100 input a_to_b.
        let out = xy_k_quote(1000, 2000, 100, true, 25);
        // net_in = 100 - 0 (100*25/10000=0.25, truncated to 0)
        // For amount_in=10000: fee = 10000*25/10000 = 25, net_in = 9975
        let out2 = xy_k_quote(1_000_000, 2_000_000, 10_000, true, 25);
        assert!(out2 > 0);
        assert!(out2 < 20_000); // should be less than 2x input
    }

    #[test]
    fn test_xy_k_quote_zero_reserves() {
        assert_eq!(xy_k_quote(0, 2000, 100, true, 25), 0);
        assert_eq!(xy_k_quote(1000, 0, 100, true, 25), 0);
        assert_eq!(xy_k_quote(1000, 2000, 0, true, 25), 0);
    }

    #[test]
    fn test_simulate_post_swap_zero() {
        let (a, b) = simulate_post_swap(0, 0, 100, true);
        assert_eq!((a, b), (0, 0));
    }
}
