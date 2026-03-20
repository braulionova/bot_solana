//! pool_health_check – Verifica cuántos pools tienen reserves > 0 tras el fix de hidratación.
//!
//! Uso: RPC_URL=... ./pool_health_check

use market_engine::{
    pool_hydrator::{load_mapped_pools, PoolHydrator},
    pool_state::PoolStateCache,
};
use std::sync::Arc;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    let pools_file = std::env::var("POOLS_FILE")
        .unwrap_or_else(|_| "/root/solana-bot/mapped_pools.json".to_string());

    let cache = Arc::new(PoolStateCache::new());
    let hydrator = PoolHydrator::new(&rpc_url, cache.clone(), &pools_file);

    tracing::info!("Loading pools from file...");
    let mapped = load_mapped_pools(&pools_file).expect("load mapped pools");
    tracing::info!(total = mapped.len(), "pools loaded from JSON");

    hydrator.load_from_file().expect("load_from_file");
    tracing::info!("Starting RPC refresh (this may take 30-60s for 5681 pools)...");

    hydrator.refresh_all(&mapped).expect("refresh_all");

    // Count pools with reserves
    let mut with_reserves = 0usize;
    let mut raydium_ok = 0usize;
    let mut orca_ok = 0usize;
    let mut meteora_ok = 0usize;
    let mut zero_reserves = 0usize;

    for pool in cache.inner_iter() {
        if pool.reserve_a > 0 && pool.reserve_b > 0 {
            with_reserves += 1;
            match pool.dex_type {
                market_engine::types::DexType::RaydiumAmmV4 => raydium_ok += 1,
                market_engine::types::DexType::OrcaWhirlpool => orca_ok += 1,
                market_engine::types::DexType::MeteoraDlmm => meteora_ok += 1,
                _ => {}
            }
        } else {
            zero_reserves += 1;
        }
    }

    println!("========================================");
    println!("POOL HEALTH CHECK RESULTS:");
    println!("  Total pools:           {}", cache.inner_iter().count());
    println!("  With reserves > 0:     {}", with_reserves);
    println!("  Zero reserves:         {}", zero_reserves);
    println!("  Raydium with reserves: {}", raydium_ok);
    println!("  Orca with reserves:    {}", orca_ok);
    println!("  Meteora with reserves: {}", meteora_ok);
    println!("========================================");

    // Sample the top 5 pools with highest reserves
    let mut pools: Vec<_> = cache
        .inner_iter()
        .filter(|p| p.reserve_a > 0 && p.reserve_b > 0)
        .collect();
    pools.sort_by(|a, b| (b.reserve_a + b.reserve_b).cmp(&(a.reserve_a + a.reserve_b)));

    println!("\nTop 5 pools by total reserves:");
    for p in pools.iter().take(5) {
        println!(
            "  {:?} {} reserve_a={} reserve_b={}",
            p.dex_type, p.pool_address, p.reserve_a, p.reserve_b,
        );
    }
}
