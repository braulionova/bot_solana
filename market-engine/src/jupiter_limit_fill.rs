// jupiter_limit_fill.rs — Monitor Jupiter DCA/Limit orders and fill when profitable.
//
// Jupiter Limit Orders are public on-chain accounts. When market price crosses
// the limit price, anyone can fill the order and pocket the spread between
// market price and limit price.
//
// Strategy: periodically scan Jupiter limit orders → compare with DEX prices
// from pool cache → if fillable at profit → build fill TX.

use std::sync::Arc;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info};

use crate::pool_state::PoolStateCache;
use crate::types::{DexType, Hop, RouteParams};

/// Jupiter Limit Order V2 program.
const JUPITER_LIMIT: &str = "jupoNjAxXgZ4rjzxzPMP4oxduvQsQtQVG7YU6LFHX9k";

const WSOL: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
const USDC: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// A pending Jupiter limit order discovered on-chain.
#[derive(Debug, Clone)]
pub struct LimitOrder {
    pub order_account: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// Amount of input_mint the maker wants to sell.
    pub making_amount: u64,
    /// Amount of output_mint the maker wants to receive.
    pub taking_amount: u64,
    /// Effective limit price: taking_amount / making_amount.
    pub limit_price: f64,
}

pub struct JupiterLimitFillScanner {
    pool_cache: Arc<PoolStateCache>,
    /// Cached pending orders (refreshed periodically via RPC).
    orders: parking_lot::RwLock<Vec<LimitOrder>>,
}

impl JupiterLimitFillScanner {
    pub fn new(pool_cache: Arc<PoolStateCache>) -> Self {
        Self {
            pool_cache,
            orders: parking_lot::RwLock::new(Vec::new()),
        }
    }

    /// Update cached orders (call from background thread via getProgramAccounts).
    pub fn update_orders(&self, orders: Vec<LimitOrder>) {
        let count = orders.len();
        *self.orders.write() = orders;
        debug!(count, "jupiter-limit: orders cache updated");
    }

    /// Scan cached orders against current DEX prices.
    /// Returns routes where filling a limit order is profitable.
    pub fn scan(&self, borrow_amounts: &[u64]) -> Vec<RouteParams> {
        let orders = self.orders.read();
        let mut routes = Vec::new();

        for order in orders.iter() {
            // We need a DEX pool for the same pair to buy/sell at market price.
            let pools = self.pool_cache.pools_for_pair(&order.input_mint, &order.output_mint);
            if pools.is_empty() {
                continue;
            }

            // Find best DEX price for buying output_mint with input_mint.
            let test_amount = order.making_amount.min(100_000_000); // cap at 0.1 SOL equiv
            for pool in &pools {
                if pool.reserve_a == 0 || pool.reserve_b == 0 {
                    continue;
                }

                // Market price: how much output_mint we get for test_amount of input_mint.
                let market_out = if pool.token_a == order.input_mint {
                    pool.quote_a_to_b(test_amount)
                } else {
                    pool.quote_b_to_a(test_amount)
                };
                if market_out == 0 {
                    continue;
                }

                let market_price = market_out as f64 / test_amount as f64;

                // Limit order gives: taking_amount output for making_amount input.
                // Limit price = taking_amount / making_amount.
                // If market_price > limit_price → we can buy cheaper on DEX, fill order at limit.
                // Profit = (market_price - limit_price) * amount.

                let spread = (market_price / order.limit_price - 1.0) * 10_000.0;
                if spread < 30.0 {
                    // Less than 30bps spread after fees → not worth it.
                    continue;
                }

                // Build route: buy on DEX at market → fill Jupiter order at limit price.
                for &borrow in borrow_amounts {
                    let amount_in = borrow.min(order.making_amount);
                    let dex_out = if pool.token_a == order.input_mint {
                        pool.quote_a_to_b(amount_in)
                    } else {
                        pool.quote_b_to_a(amount_in)
                    };

                    // Fill order: give making_amount, receive taking_amount (proportional).
                    let fill_fraction = amount_in as f64 / order.making_amount as f64;
                    let fill_receive = (order.taking_amount as f64 * fill_fraction) as u64;

                    if dex_out > fill_receive {
                        // We get more from DEX than from filling → wrong direction.
                        continue;
                    }

                    // Profit: fill_receive (from Jupiter order) - amount_in (what we spent).
                    // This is simplified — actual profit depends on exact fill mechanics.
                    let gross = fill_receive.saturating_sub(amount_in);
                    let net = gross as i64 - 15_000; // TX fee + tip estimate

                    if net > 5_000 {
                        info!(
                            order = %order.order_account,
                            spread_bps = spread as u64,
                            net_profit = net,
                            "jupiter-limit: fillable order found"
                        );

                        let dex_program: Pubkey = match pool.dex_type {
                            DexType::RaydiumAmmV4 => "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8".parse().unwrap(),
                            DexType::PumpSwap => "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".parse().unwrap(),
                            DexType::OrcaWhirlpool => "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc".parse().unwrap(),
                            DexType::MeteoraDlmm => "LBUZKhRxPF3XUpBCjp4YzTKgLLjgGmGuiVkdseHQcpS".parse().unwrap(),
                            _ => continue,
                        };

                        routes.push(RouteParams {
                            hops: vec![
                                Hop {
                                    pool: pool.pool_address,
                                    token_in: order.input_mint,
                                    token_out: order.output_mint,
                                    dex_program,
                                    amount_out: dex_out,
                                    price_impact: 0.0,
                                },
                            ],
                            borrow_amount: amount_in,
                            gross_profit: gross,
                            net_profit: net,
                            risk_factor: 0.1,
                            strategy: "jupiter_limit_fill",
                            tier: 2,
                        });
                        break; // best borrow for this pool
                    }
                }
            }
        }

        routes
    }
}
