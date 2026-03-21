// jupiter_arb_scanner.rs — Continuous cross-DEX arb scanner using Jupiter Quote API.
//
// Instead of maintaining our own reserves (stale without Geyser), we use Jupiter's
// real-time quotes to find cross-DEX price discrepancies. When a profitable route is
// found, we build and send the TX via our flash loan + Jito pipeline.
//
// Flow:
// 1. ML predicts high-activity cross-DEX token
// 2. Jupiter scanner quotes buy/sell across different DEXes
// 3. If spread > fees → emit route to executor
// 4. Executor builds flash loan TX + sends via Jito bundle

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use crate::pool_state::PoolStateCache;

const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Jupiter arb opportunity.
#[derive(Debug, Clone)]
pub struct JupiterArbOpportunity {
    pub token: Pubkey,
    pub borrow_amount: u64,
    pub buy_amount_out: u64,
    pub sell_amount_out: u64,
    pub profit_lamports: i64,
    pub profit_pct: f64,
    pub buy_dexes: Vec<String>,
    pub sell_dexes: Vec<String>,
}

pub struct JupiterArbScanner {
    api_key: String,
    client: reqwest::blocking::Client,
    pool_cache: Arc<PoolStateCache>,
    /// Rate limiter per token (max 1 scan per 5 seconds).
    scan_cooldown: DashMap<Pubkey, Instant>,
    /// Track best opportunities for logging.
    pub best_profit: std::sync::atomic::AtomicI64,
    pub scans: std::sync::atomic::AtomicU64,
    pub opportunities: std::sync::atomic::AtomicU64,
}

impl JupiterArbScanner {
    pub fn new(api_key: String, pool_cache: Arc<PoolStateCache>) -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("reqwest client"),
            api_key,
            pool_cache,
            scan_cooldown: DashMap::new(),
            best_profit: std::sync::atomic::AtomicI64::new(0),
            scans: std::sync::atomic::AtomicU64::new(0),
            opportunities: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Scan a token for cross-DEX arb using Jupiter quotes.
    /// Returns the best opportunity if profitable.
    pub fn scan_token(&self, token: &Pubkey, borrow_amounts: &[u64]) -> Option<JupiterArbOpportunity> {
        // Rate limit
        if let Some(last) = self.scan_cooldown.get(token) {
            if last.elapsed().as_secs() < 3 { return None; }
        }
        self.scan_cooldown.insert(*token, Instant::now());
        self.scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let token_str = token.to_string();
        let mut best: Option<JupiterArbOpportunity> = None;

        for &amount in borrow_amounts {
            // Strategy: buy token with SOL via cheapest route, sell back via most expensive
            let buy = self.quote(WSOL, &token_str, amount)?;
            let tokens_received: u64 = buy.out_amount.parse().ok()?;
            if tokens_received == 0 { continue; }

            let sell = self.quote(&token_str, WSOL, tokens_received)?;
            let sol_received: u64 = sell.out_amount.parse().ok()?;

            let profit = sol_received as i64 - amount as i64;
            let profit_pct = profit as f64 / amount as f64 * 100.0;

            // Only if routes use DIFFERENT DEXes (same DEX = no arb, just slippage)
            let buy_dex = buy.route_labels();
            let sell_dex = sell.route_labels();
            let same_dex = !buy_dex.is_empty() && !sell_dex.is_empty()
                && buy_dex[0] == sell_dex[0] && buy_dex.len() == 1 && sell_dex.len() == 1;

            if profit > 10_000 && !same_dex { // > 10K lamports (0.00001 SOL)
                let opp = JupiterArbOpportunity {
                    token: *token,
                    borrow_amount: amount,
                    buy_amount_out: tokens_received,
                    sell_amount_out: sol_received,
                    profit_lamports: profit,
                    profit_pct,
                    buy_dexes: buy_dex,
                    sell_dexes: sell_dex,
                };

                if best.as_ref().map_or(true, |b| profit > b.profit_lamports) {
                    best = Some(opp);
                }
            }
        }

        if let Some(ref opp) = best {
            self.opportunities.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let prev = self.best_profit.load(std::sync::atomic::Ordering::Relaxed);
            if opp.profit_lamports > prev {
                self.best_profit.store(opp.profit_lamports, std::sync::atomic::Ordering::Relaxed);
            }
            info!(
                token = %opp.token,
                borrow = opp.borrow_amount,
                profit = opp.profit_lamports,
                profit_pct = format!("{:.3}%", opp.profit_pct),
                buy = ?opp.buy_dexes,
                sell = ?opp.sell_dexes,
                "💰 JUPITER ARB: profitable cross-DEX route found"
            );
        }

        best
    }

    /// Get a list of active cross-DEX tokens to scan.
    pub fn get_scan_targets(&self) -> Vec<Pubkey> {
        let wsol: Pubkey = WSOL.parse().unwrap();
        let mut token_counts: HashMap<Pubkey, usize> = HashMap::new();

        for pool in self.pool_cache.inner_iter() {
            let token = if pool.token_a == wsol { pool.token_b }
                       else if pool.token_b == wsol { pool.token_a }
                       else { continue };
            *token_counts.entry(token).or_default() += 1;
        }

        // Return tokens with 2+ pools (cross-DEX potential)
        let mut targets: Vec<(Pubkey, usize)> = token_counts.into_iter()
            .filter(|(_, count)| *count >= 2)
            .collect();
        targets.sort_by(|a, b| b.1.cmp(&a.1));
        targets.into_iter().map(|(t, _)| t).collect()
    }

    fn quote(&self, input_mint: &str, output_mint: &str, amount: u64) -> Option<JupiterQuote> {
        let url = format!(
            "https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps=100",
            input_mint, output_mint, amount
        );
        let resp = self.client.get(&url)
            .header("x-api-key", &self.api_key)
            .send()
            .ok()?;

        if !resp.status().is_success() { return None; }

        let body: serde_json::Value = resp.json().ok()?;
        let out_amount = body.get("outAmount")?.as_str()?.to_string();
        let route_plan = body.get("routePlan")?.as_array()?;

        let labels: Vec<String> = route_plan.iter()
            .filter_map(|r| r.get("swapInfo")?.get("label")?.as_str().map(String::from))
            .collect();

        Some(JupiterQuote { out_amount, labels })
    }
}

struct JupiterQuote {
    out_amount: String,
    labels: Vec<String>,
}

impl JupiterQuote {
    fn route_labels(&self) -> Vec<String> {
        self.labels.clone()
    }
}
