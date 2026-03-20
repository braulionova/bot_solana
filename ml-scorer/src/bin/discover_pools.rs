//! discover_pools — Scan all DEX programs for new pools, save to PG + mapped_pools.json.
//!
//! Usage:
//!   RPC_URL=http://127.0.0.1:9000 \
//!   DB_URL=postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios \
//!   POOLS_JSON=/root/solana-bot/mapped_pools.json \
//!   ./target/release/discover_pools

use std::collections::HashSet;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.helius-rpc.com/?api-key=YOUR_HELIUS_KEY".into());
    let db_url = std::env::var("DB_URL")
        .unwrap_or_else(|_| "postgres://postgres:YOUR_PASSWORD@127.0.0.1/helios".into());
    let pools_json = std::env::var("POOLS_JSON")
        .unwrap_or_else(|_| "/root/solana-bot/mapped_pools.json".into());

    info!("Pool Discoverer starting");
    info!("RPC: {rpc_url}");

    // Load known pools from JSON
    let data = std::fs::read_to_string(&pools_json)?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&data)?;
    let known: HashSet<String> = entries.iter()
        .filter_map(|e| {
            e.get("pool_id").and_then(|v| v.as_str()).map(|s| s.to_string())
        })
        .collect();

    info!(known_pools = known.len(), "loaded existing pools");

    // Scan all DEX programs
    let new_pools = ml_scorer::pool_discoverer::scan_all_dex_pools(&rpc_url, &known).await;

    if new_pools.is_empty() {
        info!("No new pools found");
        return Ok(());
    }

    info!(new = new_pools.len(), "new pools discovered!");

    // Save to PG
    if let Err(e) = ml_scorer::pool_discoverer::save_discoveries(&db_url, &new_pools).await {
        tracing::warn!(error = %e, "failed to save to PG");
    }

    // Append to mapped_pools.json
    match ml_scorer::pool_discoverer::append_to_mapped_pools(&pools_json, &new_pools) {
        Ok(added) => info!(added, "pools added to mapped_pools.json"),
        Err(e) => tracing::warn!(error = %e, "failed to update mapped_pools.json"),
    }

    // Summary by DEX
    let mut by_dex: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for p in &new_pools {
        *by_dex.entry(&p.dex).or_default() += 1;
    }
    for (dex, count) in &by_dex {
        info!(dex, count, "new pools by DEX");
    }

    Ok(())
}
