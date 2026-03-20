//! pool_scanner.rs – Async background module that scans for new pools/coins and
//! logs route candidates + pool discoveries to PostgreSQL for ML training.
//!
//! This module runs in a background thread and:
//! 1. Monitors pool_cache for newly added pools (compares to known set)
//! 2. Logs each new pool to `pool_discoveries` table with initial liquidity
//! 3. Periodically refreshes materialized views for ML queries
//! 4. Provides a scan_new_pools interface for the main pipeline

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

use market_engine::pool_state::PoolStateCache;

/// Spawn the pool scanner background thread.
/// Monitors pool_cache every `interval` for new pools and logs to PG.
pub fn spawn_pool_scanner(
    pool_cache: Arc<PoolStateCache>,
    logger: Arc<strategy_logger::StrategyLogger>,
    interval: Duration,
) {
    std::thread::Builder::new()
        .name("pool-scanner".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("pool-scanner tokio rt");

            rt.block_on(async move {
                run_pool_scanner(pool_cache, logger, interval).await;
            });
        })
        .expect("spawn pool-scanner thread");

    info!(interval_s = interval.as_secs(), "pool-scanner started");
}

async fn run_pool_scanner(
    pool_cache: Arc<PoolStateCache>,
    logger: Arc<strategy_logger::StrategyLogger>,
    interval: Duration,
) {
    let mut known_pools: HashSet<Pubkey> = HashSet::new();
    let mut last_mv_refresh = Instant::now();
    let mv_refresh_interval = Duration::from_secs(300); // refresh materialized views every 5 min

    // Initialize known_pools with current cache contents.
    for pool in pool_cache.inner_iter() {
        known_pools.insert(pool.pool_address);
    }
    info!(initial_pools = known_pools.len(), "pool-scanner: initialized known pool set");

    loop {
        tokio::time::sleep(interval).await;

        let mut new_count = 0u32;
        for pool in pool_cache.inner_iter() {
            if known_pools.contains(&pool.pool_address) {
                continue;
            }
            known_pools.insert(pool.pool_address);
            new_count += 1;

            // Log discovery to PG
            logger.log_pool_discovery(strategy_logger::types::LogPoolDiscovery {
                pool_address: pool.pool_address.to_string(),
                dex_type: format!("{:?}", pool.dex_type),
                token_a: pool.token_a.to_string(),
                token_b: pool.token_b.to_string(),
                initial_reserve_a: if pool.reserve_a > 0 { Some(pool.reserve_a) } else { None },
                initial_reserve_b: if pool.reserve_b > 0 { Some(pool.reserve_b) } else { None },
                discovery_source: "pool_cache".to_string(),
                discovery_slot: None,
            });
        }

        if new_count > 0 {
            info!(new_pools = new_count, total = known_pools.len(), "pool-scanner: new pools discovered");
        }

        // Periodically refresh materialized views for ML queries.
        if last_mv_refresh.elapsed() > mv_refresh_interval {
            last_mv_refresh = Instant::now();
            refresh_materialized_views().await;
        }
    }
}

/// Refresh the mv_hot_routes materialized view for ML queries.
async fn refresh_materialized_views() {
    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => return,
    };

    match tokio_postgres::connect(&db_url, tokio_postgres::NoTls).await {
        Ok((client, connection)) => {
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    warn!("pool-scanner pg connection error: {e}");
                }
            });

            match client.batch_execute("REFRESH MATERIALIZED VIEW CONCURRENTLY mv_hot_routes").await {
                Ok(_) => info!("pool-scanner: mv_hot_routes refreshed"),
                Err(e) => {
                    // CONCURRENTLY requires unique index — fall back to non-concurrent
                    match client.batch_execute("REFRESH MATERIALIZED VIEW mv_hot_routes").await {
                        Ok(_) => info!("pool-scanner: mv_hot_routes refreshed (non-concurrent)"),
                        Err(e2) => warn!("pool-scanner: mv_hot_routes refresh failed: {e2}"),
                    }
                }
            }
        }
        Err(e) => {
            warn!("pool-scanner: PG connect failed for MV refresh: {e}");
        }
    }
}
