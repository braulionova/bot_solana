//! prewarm_alts — Pre-creates on-chain ALTs for the most likely 2-hop arb routes.
//!
//! Run once before the bot starts (or periodically) to eliminate the ~20s cold-start
//! penalty on the first execution of each unique route.
//!
//! Usage:
//!   cargo build --release --bin prewarm_alts
//!   set -a && source /etc/helios/helios.env && source /root/spy_node/flash_accounts.env && set +a
//!   BORROW_AMOUNT=1000000000 ./target/release/prewarm_alts

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use solana_sdk::signature::{read_keypair_file, Signer};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use executor::{
    alt_loader::{parse_alt_env, AltCache},
    flash_loans,
    tx_builder::TxBuilder,
};
use market_engine::{
    flash_selector::FlashSelector,
    pool_hydrator::{bootstrap_cache, load_mapped_pools, PoolHydrator},
    pool_state::PoolStateCache,
    route_engine::RouteEngine,
};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let rpc_url = std::env::var("RPC_URL")
        .or_else(|_| std::env::var("UPSTREAM_RPC_URL"))
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let sim_rpc_url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| rpc_url.clone());
    let payer_path = std::env::var("PAYER_KEYPAIR")
        .unwrap_or_else(|_| "/root/solana-bot/wallet.json".to_string());
    let pools_file = "/root/solana-bot/mapped_pools.json";
    let borrow: u64 = std::env::var("BORROW_AMOUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000_000);

    let payer = read_keypair_file(&payer_path)
        .unwrap_or_else(|_| solana_sdk::signature::Keypair::new());
    info!(pubkey = %payer.pubkey(), "payer loaded");

    // ── Pool cache ────────────────────────────────────────────────────────────
    let pool_cache = Arc::new(PoolStateCache::new());
    let n = bootstrap_cache(&pool_cache, &rpc_url);
    info!(pools = n, "bootstrap done");

    let hydrator = Arc::new(PoolHydrator::new(&rpc_url, pool_cache.clone(), pools_file));
    info!("hydrating all pools (this takes ~60s)...");
    match load_mapped_pools(pools_file) {
        Ok(mapped) => {
            if let Err(e) = hydrator.refresh_all(&mapped) {
                warn!(error = %e, "hydration error");
            }
            info!(pools = mapped.len(), "hydration complete");
        }
        Err(e) => warn!(error = %e, "failed to load mapped pools"),
    }

    // ── Route engine ──────────────────────────────────────────────────────────
    let oracle = market_engine::oracle::OracleCache::new();
    let engine = RouteEngine::new(pool_cache.clone(), Some(oracle));
    let selector = FlashSelector::new();

    info!(borrow, "running route engine...");
    let routes = engine.find_opportunities(borrow);
    info!(routes = routes.len(), "routes found");

    if routes.is_empty() {
        info!("no routes at this moment — market is efficient. Try again during high volatility.");
        return Ok(());
    }

    // ── ALT loader ────────────────────────────────────────────────────────────
    let alt_cache = AltCache::new(&sim_rpc_url, &parse_alt_env(), 300);
    let builder = TxBuilder::new(payer.insecure_clone(), &sim_rpc_url);

    let dummy_bh = solana_sdk::hash::Hash::default();
    let alts_empty: Vec<solana_sdk::address_lookup_table::AddressLookupTableAccount> = vec![];

    let mut warmed = 0usize;
    let mut skipped = 0usize;

    for (i, route) in routes.iter().enumerate() {
        let flash_provider = match selector.select(route.borrow_amount) {
            Ok(p) => p,
            Err(e) => { warn!(error = %e, "no flash provider"); continue; }
        };

        info!(
            route = i + 1,
            net_profit = route.net_profit,
            hops = route.hops.len(),
            pools = ?route.hops.iter().map(|h| h.pool.to_string()[..8].to_string()).collect::<Vec<_>>(),
            "pre-warming ALT for route"
        );

        // Build TX to get the full account list.
        let tx = match builder.build(route, flash_provider, dummy_bh, &alts_empty, &pool_cache) {
            Ok(tx) => tx,
            Err(e) => {
                warn!(error = %e, "TX build failed for route {}", i + 1);
                skipped += 1;
                continue;
            }
        };

        let tx_size = bincode::serialize(&tx).map(|b| b.len()).unwrap_or(0);
        info!(tx_size, "TX built");

        if tx_size <= 1232 {
            info!("TX already fits ({}B) — no ALT needed", tx_size);
            skipped += 1;
            continue;
        }

        // Extract all static keys from the message.
        let static_keys: Vec<solana_sdk::pubkey::Pubkey> = match &tx.message {
            solana_sdk::message::VersionedMessage::V0(msg) => msg.account_keys.clone(),
            solana_sdk::message::VersionedMessage::Legacy(msg) => msg.account_keys.clone(),
        };

        info!(accounts = static_keys.len(), "creating/reusing ALT on-chain...");
        match alt_cache.get_or_create_route_alt(&payer, static_keys) {
            Ok(alt_pk) => {
                info!(%alt_pk, "ALT ready for route {}", i + 1);
                warmed += 1;

                // Verify TX is now small enough with this ALT.
                let alts_with = alt_cache.all();
                match builder.build(route, flash_provider, dummy_bh, &alts_with, &pool_cache) {
                    Ok(tx2) => {
                        let sz2 = bincode::serialize(&tx2).map(|b| b.len()).unwrap_or(0);
                        if sz2 <= 1232 {
                            info!(tx_size = sz2, "✅ TX fits with ALT — route is executable");
                        } else {
                            warn!(tx_size = sz2, "⚠️  TX still oversized with ALT (route too complex?)");
                        }
                    }
                    Err(e) => warn!(error = %e, "TX rebuild with ALT failed"),
                }
            }
            Err(e) => {
                warn!(error = %e, "ALT creation failed for route {}", i + 1);
                skipped += 1;
            }
        }

        // Brief pause between ALT creations to avoid rate limits.
        std::thread::sleep(Duration::from_secs(2));
    }

    info!(warmed, skipped, "prewarm complete — ALTs saved to route_alt_cache.json");
    Ok(())
}
