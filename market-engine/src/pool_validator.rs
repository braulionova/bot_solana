// pool_validator.rs — Background pool existence validator.
//
// Periodically checks if pools in the cache still exist on-chain.
// Removes closed/dead pools to prevent phantom route spam.
//
// The #1 cause of failed TXs was sending to pools that were CLOSED on-chain
// but still in our cache from stale Redis snapshot data.

use std::sync::Arc;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

use crate::pool_state::PoolStateCache;
use crate::rpc_pool_util::SimpleRpcPool;

/// Spawn a background thread that validates pools exist on-chain.
/// Removes dead pools from cache every `interval`.
pub fn spawn_pool_validator(
    pool_cache: Arc<PoolStateCache>,
    rpc_urls: Vec<String>,
    interval: Duration,
) {
    std::thread::Builder::new()
        .name("pool-validator".into())
        .spawn(move || {
            // Wait for initial hydration
            std::thread::sleep(Duration::from_secs(30));

            let rpc = SimpleRpcPool::new(&rpc_urls);

            loop {
                let mut to_remove = Vec::new();
                let mut checked = 0u32;
                let mut dead = 0u32;

                // Collect all pool addresses
                let pool_addrs: Vec<Pubkey> = pool_cache.inner_iter()
                    .map(|p| p.pool_address)
                    .collect();

                // Check in batches of 100
                for chunk in pool_addrs.chunks(100) {
                    match rpc.get_multiple_accounts(chunk) {
                        Some(accounts) => {
                            for (i, maybe_acc) in accounts.iter().enumerate() {
                                checked += 1;
                                if maybe_acc.is_none() {
                                    // Pool account closed on-chain
                                    to_remove.push(chunk[i]);
                                    dead += 1;
                                }
                            }
                        }
                        None => {
                            // RPC failed, skip this batch
                            std::thread::sleep(Duration::from_secs(2));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(200)); // rate limit
                }

                // Remove dead pools
                if !to_remove.is_empty() {
                    for addr in &to_remove {
                        pool_cache.remove(addr);
                    }
                    warn!(
                        checked,
                        dead,
                        removed = to_remove.len(),
                        "pool-validator: removed dead pools from cache"
                    );
                } else {
                    info!(checked, dead = 0, "pool-validator: all pools alive");
                }

                std::thread::sleep(interval);
            }
        })
        .expect("spawn pool-validator");
}
