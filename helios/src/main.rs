//! helios/src/main.rs – Integrated Helios Gold V2 bot.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod daily_report;
mod geyser_feed;
mod geyser_local;
mod pool_scanner;
mod sqlite_route_feed;
mod ws_dex_feed;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossbeam_channel::bounded;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Signer};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use spy_node::{
    deduper::Deduper, fec_assembler::FecAssembler, gossip_client::GossipClient,
    shred_parser::parse_shred, sig_verifier::SigVerifier, signal_bus::SpySignal,
    tx_decoder::TxDecoder, udp_ingest::spawn_ingest_threads,
};
use strategy_logger::StrategyLogger;
use ml_scorer::MlScorer;
use ml_scorer::landing_predictor::LandingPredictor;
use ml_scorer::graduation_predictor::GraduationPredictor;
use ml_scorer::spread_predictor::SpreadPredictor;
use ml_scorer::threehop_predictor::ThreeHopPredictor;

use market_engine::{
    cyclic_arb::CyclicArbScanner,
    dlmm_whirlpool_arb::DlmmWhirlpoolArb,
    flash_selector::FlashSelector,
    new_pool_monitor::NewPoolMonitor,
    pool_hydrator::{bootstrap_cache, load_mapped_pools, PoolHydrator},
    pool_state::PoolStateCache,
    pump_graduation_arb::PumpGraduationArb,
    route_engine::RouteEngine,
    scoring::Scorer,
    shred_delta_tracker::ShredDeltaTracker,
    signal_processor::SignalProcessor,
    types::OpportunityBundle,
};

/// Try multiple borrow amounts to find arbs at different scales.
/// Smaller amounts have less price impact; larger amounts have more absolute profit.
const BORROW_AMOUNTS: &[u64] = &[
    50_000_000,      // 0.05 SOL — low impact, small profit
    100_000_000,     // 0.1 SOL  — matches graph ref_amount
    500_000_000,     // 0.5 SOL
    1_000_000_000,   // 1 SOL
    5_000_000_000,   // 5 SOL — only for deep liquidity pools
];

fn main() -> Result<()> {
    // -----------------------------------------------------------------------
    // Tracing & Configuration
    // -----------------------------------------------------------------------
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .with_thread_names(true)
        .init();

    let cfg = spy_node::config::Config::parse();
    let no_agave_mode = std::env::var("NO_AGAVE_MODE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false);
    info!(
        dry_run = cfg.dry_run,
        min_profit = cfg.min_net_profit,
        no_agave_mode,
        "config loaded"
    );
    if no_agave_mode {
        warn!("NO_AGAVE_MODE: relying on shred deltas for reserves — Agave/Richat not required");
    }

    // ── Redis cache (early init for ml-save thread) ──
    let redis_url_early = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".into());
    let redis_cache_early = std::sync::Arc::new(market_engine::redis_cache::RedisCache::new(&redis_url_early));

    // -----------------------------------------------------------------------
    // Identity & Shared State
    // -----------------------------------------------------------------------
    let identity = read_keypair_file(&cfg.identity_keypair).unwrap_or_else(|e| {
        warn!(error = %e, "could not read identity keypair, generating ephemeral");
        solana_sdk::signature::Keypair::new()
    });
    info!(identity = %identity.pubkey(), "identity loaded");

    let pool_cache = Arc::new(PoolStateCache::new());
    let deduper = Arc::new(Deduper::new(cfg.dedup_cache_size));

    // New strategy scanners (atomic flash loan arb only — no sniping).
    let pump_grad_arb = Arc::new(PumpGraduationArb::new(Arc::clone(&pool_cache)));
    let new_pool_monitor = Arc::new(NewPoolMonitor::new(Arc::clone(&pool_cache), Arc::clone(&pump_grad_arb)));
    let cyclic_scanner = Arc::new(CyclicArbScanner::new(Arc::clone(&pool_cache)));
    let dlmm_whirlpool = Arc::new(DlmmWhirlpoolArb::new(Arc::clone(&pool_cache)));

    // -----------------------------------------------------------------------
    // ML Scorer & Strategy Logger (PostgreSQL)
    // -----------------------------------------------------------------------
    let strategy_logger = StrategyLogger::init();
    if strategy_logger.is_some() {
        info!("strategy-logger: PostgreSQL logging ACTIVE");
    } else {
        info!("strategy-logger: disabled (set DATABASE_URL to enable)");
    }

    let ml_scorer = MlScorer::new();
    let shred_predictor = ml_scorer::shred_predictor::ShredPredictor::new();
    let landing_predictor = LandingPredictor::new();
    let threehop_predictor = Arc::new(
        if let Ok(db_url) = std::env::var("DATABASE_URL") {
            let rt_tmp = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("threehop rt");
            rt_tmp.block_on(ThreeHopPredictor::load_from_pg(&db_url))
                .unwrap_or_else(|e| {
                    warn!(error = %e, "3-hop predictor load failed, using defaults");
                    ThreeHopPredictor::new()
                })
        } else {
            ThreeHopPredictor::new()
        }
    );
    let grad_predictor = Arc::new(
        if let Ok(db_url) = std::env::var("DATABASE_URL") {
            let rt_tmp = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("grad_predictor rt");
            rt_tmp.block_on(GraduationPredictor::load_from_pg(&db_url))
                .unwrap_or_else(|e| {
                    warn!(error = %e, "graduation predictor load failed, using defaults");
                    GraduationPredictor::new()
                })
        } else {
            GraduationPredictor::new()
        }
    );
    let spread_predictor = Arc::new(
        if let Ok(db_url) = std::env::var("DATABASE_URL") {
            let rt_tmp = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("spread_predictor rt");
            rt_tmp.block_on(SpreadPredictor::load_from_pg(&db_url))
                .unwrap_or_else(|e| {
                    warn!(error = %e, "spread predictor load failed, using defaults");
                    SpreadPredictor::new()
                })
        } else {
            SpreadPredictor::new()
        }
    );
    let hot_routes: Arc<parking_lot::RwLock<Vec<market_engine::cross_dex::HotRoute>>> =
        Arc::new(parking_lot::RwLock::new(Vec::new()));
    let multihop_hot_routes: Arc<parking_lot::RwLock<Vec<market_engine::cross_dex::MultiHopHotRoute>>> =
        Arc::new(parking_lot::RwLock::new(Vec::new()));
    // Load historical pool performance from PG (if available)
    if let Ok(db_url) = std::env::var("DATABASE_URL") {
        let scorer_clone = ml_scorer.clone();
        let predictor_load = shred_predictor.clone();
        let landing_load = landing_predictor.clone();
        let hot_routes_load = hot_routes.clone();
        let multihop_hot_routes_load = multihop_hot_routes.clone();
        let db_url_clone = db_url.clone();
        std::thread::Builder::new()
            .name("ml-load".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("ml-load rt");
                if let Err(e) = rt.block_on(scorer_clone.load_from_pg(&db_url_clone)) {
                    warn!(error = %e, "ml-scorer: failed to load from PG (starting fresh)");
                }
                if let Err(e) = rt.block_on(predictor_load.load_from_pg(&db_url_clone)) {
                    warn!(error = %e, "shred-predictor: failed to load from PG (starting fresh)");
                }
                if let Err(e) = rt.block_on(landing_load.load_volatility_from_pg(&db_url_clone)) {
                    warn!(error = %e, "landing-predictor: failed to load volatility from PG");
                }
                if let Err(e) = rt.block_on(landing_load.load_weights_from_pg(&db_url_clone)) {
                    warn!(error = %e, "landing-predictor: failed to load weights from PG");
                }
                // Load hot routes (historically profitable pairs)
                match rt.block_on(market_engine::cross_dex::load_hot_routes(&db_url_clone)) {
                    Ok(hr) => {
                        info!(count = hr.len(), "hot-routes loaded for priority scanning");
                        *hot_routes_load.write() = hr;
                    }
                    Err(e) => warn!(error = %e, "hot-routes: failed to load from PG"),
                }
                // Load snapshot scanner multi-hop hot routes
                match rt.block_on(market_engine::cross_dex::load_snapshot_hot_routes(&db_url_clone)) {
                    Ok((hr2, hr3)) => {
                        info!(two_hop = hr2.len(), three_hop = hr3.len(), "snapshot hot routes loaded");
                        // Merge 2-hop into existing hot_routes
                        hot_routes_load.write().extend(hr2);
                        *multihop_hot_routes_load.write() = hr3;
                    }
                    Err(e) => warn!(error = %e, "snapshot hot-routes: failed to load"),
                }
            })
            .ok();

        // Periodic model save every 5 min
        let scorer_save = ml_scorer.clone();
        let predictor_save = shred_predictor.clone();
        let landing_save = landing_predictor.clone();
        let threehop_save = threehop_predictor.clone();
        let redis_save = redis_cache_early.clone();
        let pool_cache_save = pool_cache.clone();
        let hot_routes_refresh = hot_routes.clone();
        let multihop_refresh = multihop_hot_routes.clone();
        std::thread::Builder::new()
            .name("ml-save".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("ml-save rt");
                loop {
                    std::thread::sleep(Duration::from_secs(300));
                    info!("ml-scorer: {}", scorer_save.stats());
                    info!("shred-predictor: {}", predictor_save.stats());
                    info!("landing-predictor: {}", landing_save.stats());
                    info!("3hop-predictor: {}", threehop_save.stats());
                    // Online learning: apply accumulated gradients and save
                    threehop_save.apply_gradients();
                    if let Err(e) = rt.block_on(threehop_save.save_to_pg(&db_url)) {
                        warn!(error = %e, "3hop-predictor: failed to save model");
                    }
                    if let Err(e) = rt.block_on(scorer_save.save_to_pg(&db_url)) {
                        warn!(error = %e, "ml-scorer: failed to save model");
                    }
                    // Save pool metadata to Redis for instant restore on next restart
                    let saved = redis_save.save_all_metadata(&pool_cache_save);
                    if saved > 0 {
                        info!(pools = saved, "redis-cache: metadata saved");
                    }
                    if let Err(e) = rt.block_on(predictor_save.save_to_pg(&db_url)) {
                        warn!(error = %e, "shred-predictor: failed to save model");
                    }
                    // Refresh hot routes from PG
                    match rt.block_on(market_engine::cross_dex::load_hot_routes(&db_url)) {
                        Ok(hr) => {
                            info!(count = hr.len(), "hot-routes refreshed");
                            *hot_routes_refresh.write() = hr;
                        }
                        Err(e) => warn!(error = %e, "hot-routes: refresh failed"),
                    }
                    // Refresh snapshot multi-hop hot routes
                    match rt.block_on(market_engine::cross_dex::load_snapshot_hot_routes(&db_url)) {
                        Ok((hr2, hr3)) => {
                            info!(two_hop = hr2.len(), three_hop = hr3.len(), "snapshot hot routes refreshed");
                            hot_routes_refresh.write().extend(hr2);
                            *multihop_refresh.write() = hr3;
                        }
                        Err(e) => warn!(error = %e, "snapshot hot-routes: refresh failed"),
                    }
                }
            })
            .ok();

        // On-chain arb scanner: learn from other bots' successful arbs
        {
            let scan_rpc = std::env::var("RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into());
            let scan_db = std::env::var("DATABASE_URL").unwrap_or_default();
            let scan_scorer = ml_scorer.clone();
            ml_scorer::onchain_scanner::spawn_onchain_scanner(
                scan_rpc,
                scan_db,
                scan_scorer,
                Duration::from_secs(300), // scan every 5 min
            );
            info!("on-chain arb scanner started (learns hot routes from other bots)");
        }

        // Pool snapshot logger every 60s
        if let Some(ref logger) = strategy_logger {
            let logger_snap = logger.clone();
            let cache_snap = pool_cache.clone();
            std::thread::Builder::new()
                .name("pool-snap".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(Duration::from_secs(60));
                        let snaps: Vec<strategy_logger::types::LogPoolSnapshot> = cache_snap
                            .inner_iter()
                            .take(1000) // top 1000 pools
                            .map(|p| {
                                strategy_logger::types::LogPoolSnapshot {
                                    pool_address: p.pool_address.to_string(),
                                    dex_type: format!("{:?}", p.dex_type),
                                    token_a: p.token_a.to_string(),
                                    token_b: p.token_b.to_string(),
                                    reserve_a: p.reserve_a,
                                    reserve_b: p.reserve_b,
                                    fee_bps: p.fee_bps as i16,
                                }
                            })
                            .collect();
                        if !snaps.is_empty() {
                            info!(pools = snaps.len(), "pool-snap: logging to PG");
                            logger_snap.log_pool_snapshots(snaps);
                        }
                    }
                })
                .ok();
        }
    }

    // Pool scanner: monitor for new pools/coins and log discoveries to PG.
    if let Some(ref logger) = strategy_logger {
        pool_scanner::spawn_pool_scanner(
            pool_cache.clone(),
            logger.clone(),
            Duration::from_secs(30),
        );
    }

    // Wallet tracker: smart money analysis for ML pattern detection.
    let (wallet_tracker, wallet_alert_rx) = if let Some(ref logger) = strategy_logger {
        let (wt, rx) = market_engine::wallet_tracker::WalletTracker::new(
            pool_cache.clone(),
            logger.clone(),
        );
        info!("WalletTracker initialized for smart money analysis");
        (Some(wt), Some(rx))
    } else {
        (None, None)
    };

    // Spawn wallet alert consumer → Telegram notifications.
    if let Some(alert_rx) = wallet_alert_rx {
        let tg_wallet = executor::telegram::TelegramBot::from_env();
        thread::Builder::new()
            .name("wallet-alerts".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("wallet-alerts tokio rt");
                for alert in alert_rx {
                    let tg = tg_wallet.clone();
                    rt.block_on(async move {
                        match alert {
                            market_engine::wallet_tracker::WalletAlert::SmartMoneyDetected {
                                wallet: _, category: _, total_swaps: _, win_rate: _,
                            } => {
                                // Smart money Telegram notifications disabled.
                            }
                            market_engine::wallet_tracker::WalletAlert::PatternDetected {
                                pattern_type: _, wallet: _, token: _, confidence: _, features: _,
                            } => {
                                // Wallet pattern Telegram notifications disabled.
                            }
                        }
                    });
                }
            })
            .ok();
    }

    // Pool hydration uses a dedicated RPC endpoint to avoid consuming Helius credits.
    // Public mainnet handles bulk getMultipleAccounts; Helius reserved for hot-path sim.
    let pool_rpc_url = std::env::var("POOL_RPC_URL").unwrap_or_else(|_| {
        // Default: public Solana mainnet (free, handles bulk calls without rate-limiting)
        "https://api.mainnet-beta.solana.com".to_string()
    });
    info!(pool_rpc = %pool_rpc_url, "pool hydrator RPC endpoint");

    let n_pools = bootstrap_cache(&pool_cache, &pool_rpc_url);
    info!(pools = n_pools, "pool cache bootstrapped");

    // Fast vault hydration removed — pool hydrator + delta tracker handle reserves.

    let pools_file = "/root/solana-bot/mapped_pools.json";
    let hydrator = Arc::new(PoolHydrator::new(
        &pool_rpc_url,
        pool_cache.clone(),
        pools_file,
    ));

    // ── Redis cache: restore pool metadata + vault balances (sub-second) ──
    {
        // Step 1: fast MGET metadata for pools already in cache (vs slow KEYS scan)
        let restored = redis_cache_early.load_metadata_fast(&pool_cache);
        if restored > 0 {
            info!(pools = restored, "redis-cache: pool metadata restored (fast MGET)");
        }

        // Step 2: load vault balances → fills reserve_a/reserve_b from snapshot data
        let extra_vaults = market_engine::redis_cache::RedisCache::build_mapped_pools_vault_index(pools_file);
        let vaults_loaded = redis_cache_early.load_vault_balances(&pool_cache, &extra_vaults);
        if vaults_loaded > 0 {
            info!(reserves = vaults_loaded, "redis-cache: vault balances loaded — pool graph ready");
        }

        // Count pools with actual reserves
        let mut total = 0usize;
        let mut with_reserves = 0usize;
        for p in pool_cache.inner_iter() {
            total += 1;
            if p.reserve_a > 0 && p.reserve_b > 0 { with_reserves += 1; }
        }
        info!(total_pools = total, pools_with_reserves = with_reserves, "pool cache state after Redis hydration");

        // ── ONE-SHOT RPC BOOTSTRAP: refresh vault balances for pools in graph ──
        // This is the ONLY RPC call in the entire pipeline. After this,
        // everything runs on shreds (delta tracker 1300+ updates/s).
        // ~400 vault accounts = 4 batches × 100 = ~2 seconds total.
        {
            let vault_index = pool_cache.build_vault_index();
            let vault_keys: Vec<solana_sdk::pubkey::Pubkey> = vault_index.keys().copied().collect();
            info!(vaults = vault_keys.len(), "bootstrap: ONE-SHOT RPC refresh starting");

            let bootstrap_rpc = solana_rpc_client::rpc_client::RpcClient::new_with_timeout_and_commitment(
                "https://api.mainnet-beta.solana.com".to_string(),
                std::time::Duration::from_secs(10),
                solana_sdk::commitment_config::CommitmentConfig::processed(),
            );

            let mut refreshed = 0u32;
            let mut dead_pools = Vec::new();
            for chunk in vault_keys.chunks(100) {
                match bootstrap_rpc.get_multiple_accounts(chunk) {
                    Ok(accounts) => {
                        for (i, maybe_acc) in accounts.iter().enumerate() {
                            match maybe_acc {
                                Some(acc) if acc.data.len() >= 72 => {
                                    let amount = u64::from_le_bytes(
                                        acc.data[64..72].try_into().unwrap_or([0u8; 8]),
                                    );
                                    let vault_key = chunk[i];
                                    if let Some(&(pool_addr, is_a)) = vault_index.get(&vault_key) {
                                        if pool_cache.update_reserve_by_vault(&pool_addr, is_a, amount) {
                                            refreshed += 1;
                                        }
                                    }
                                }
                                None => {
                                    // Vault account doesn't exist → pool is dead
                                    if let Some(&(pool_addr, _)) = vault_index.get(&chunk[i]) {
                                        dead_pools.push(pool_addr);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "bootstrap RPC batch failed, continuing");
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }

            // Remove dead pools from cache
            for pool_addr in &dead_pools {
                pool_cache.remove(pool_addr);
            }

            info!(
                refreshed,
                dead = dead_pools.len(),
                "bootstrap: ONE-SHOT RPC complete — switching to 100% shreds"
            );
        }

        // Re-count after bootstrap
        let mut total = 0usize;
        let mut with_reserves = 0usize;
        for p in pool_cache.inner_iter() {
            total += 1;
            if p.reserve_a > 0 && p.reserve_b > 0 { with_reserves += 1; }
        }
        info!(total_pools = total, pools_with_reserves = with_reserves, "pool cache state after RPC bootstrap");
    }

    // ── EMBEDDED agave-lite-core: shared AccountCache ──────────────
    let account_cache = agave_lite_core::account_cache::AccountCache::new();
    let al_blockhash = agave_lite_core::feed::BlockhashCache::new();

    // Load snapshot data (instant, <1s).
    let pool_data_path = std::env::var("POOL_DATA_PATH")
        .unwrap_or_else(|_| "/root/agave-lite-ledger/pool_accounts.bin".into());
    let preloaded = agave_lite_core::pool_data_loader::load_pool_accounts(
        &pool_data_path, &account_cache,
    );
    info!(preloaded, "agave-lite-core: snapshot data loaded into shared cache");

    // Start account feed (RPC polling → writes to AccountCache).
    let vault_pubkeys: Vec<Pubkey> = match load_mapped_pools(pools_file) {
        Ok(ref mapped) => {
            let mut pks = Vec::with_capacity(mapped.len() * 3);
            for m in mapped {
                if let Ok(pk) = m.pool_id.parse::<Pubkey>() { pks.push(pk); }
                if let Ok(pk) = m.vault_a.parse::<Pubkey>() {
                    if pk != Pubkey::default() { pks.push(pk); }
                }
                if let Ok(pk) = m.vault_b.parse::<Pubkey>() {
                    if pk != Pubkey::default() { pks.push(pk); }
                }
            }
            pks.sort();
            pks.dedup();
            pks
        }
        Err(_) => Vec::new(),
    };
    info!(pubkeys = vault_pubkeys.len(), "agave-lite-core: vault pubkeys loaded");

    // Use public RPC for blockhash polling to avoid burning Helius credits.
    let al_feed_rpc = std::env::var("AL_FEED_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    agave_lite_core::feed::spawn_feed(
        agave_lite_core::feed::FeedConfig {
            rpc_url: al_feed_rpc,
            vault_pubkeys: vault_pubkeys.clone(),
            poll_interval_secs: 3,
            batch_size: 100,
            batch_sleep_ms: 1,
        },
        account_cache.clone(),
        al_blockhash.clone(),
    );

    // ── gRPC subscriber (Richat/Yellowstone — push-based, <1ms) ──
    if let Ok(grpc_endpoint) = std::env::var("GRPC_ENDPOINT") {
        if !grpc_endpoint.is_empty() {
            let grpc_metrics = agave_lite_core::grpc_subscriber::GrpcMetrics::new();
            let grpc_cache = account_cache.clone();
            let grpc_config = agave_lite_core::grpc_subscriber::GrpcConfig {
                endpoint: grpc_endpoint.clone(),
                x_token: std::env::var("GRPC_TOKEN").ok(),
                account_pubkeys: vault_pubkeys.clone(),
                subscribe_by_owner: true,
            };
            agave_lite_core::grpc_subscriber::spawn_grpc_subscriber(
                grpc_config, grpc_cache, grpc_metrics.clone(),
            );
            info!(endpoint = %grpc_endpoint, "gRPC subscriber started (push-based account updates)");
        }
    }

    // RPC-based hydrator (metadata decode — pool accounts, Serum markets, bin arrays).
    match load_mapped_pools(pools_file) {
        Ok(mapped) => {
            let h2 = Arc::clone(&hydrator);
            thread::Builder::new()
                .name("pool-init-refresh".into())
                .spawn(move || {
                    if let Err(err) = h2.refresh_all(&mapped) {
                        warn!(error = %err, "initial pool metadata refresh failed");
                    } else {
                        info!(pools = mapped.len(), "initial pool metadata refresh completed");
                    }
                })
                .expect("spawn pool-init-refresh");
        }
        Err(err) => warn!(error = %err, "failed to load mapped pools for initial refresh"),
    }

    // RPC-based periodic refresh DISABLED — 100% shred-driven after bootstrap.
    // Delta tracker handles 1,300+ vault updates/s from shred-decoded SPL transfers.
    // The ONE-SHOT RPC bootstrap above provides correct initial state.
    info!("pool hydrator RPC refresh DISABLED — running 100% on shreds");

    // Vault watcher: real-time vault tracking via WebSocket accountSubscribe.
    // Subscribes to cross-DEX vaults only (~200). Updates reserves in ~300ms.
    // Replaces Agave Geyser push — works with any public RPC WebSocket.
    market_engine::vault_watcher::spawn_vault_watcher(
        pool_cache.clone(),
        vec![
            "wss://api.mainnet-beta.solana.com".into(),
            "wss://solana-rpc.publicnode.com".into(),
            "wss://mainnet.helius-rpc.com/?api-key=7996d184-b857-45d5-8f7d-bfd1164e6a95".into(),
        ],
    );
    info!("vault-watcher: WebSocket real-time vault tracking started");

    // Pool validator: background check that pool accounts still exist on-chain.
    // Removes dead/closed pools that cause 100% of our failed TXs.
    market_engine::pool_validator::spawn_pool_validator(
        pool_cache.clone(),
        vec![
            "https://solana-rpc.publicnode.com".into(),
            "https://api.mainnet-beta.solana.com".into(),
        ],
        Duration::from_secs(300), // every 5 min
    );
    info!("pool validator started (removes dead pools every 5 min)");

    let oracle_cache = market_engine::oracle::OracleCache::new();
    market_engine::oracle::spawn_oracle_refresh(oracle_cache.clone(), Duration::from_secs(15));
    info!("oracle price cache started");

    let identity_clone = identity.insecure_clone();
    let gossip = Arc::new(GossipClient::new(identity, &cfg)?);
    let _ = gossip.refresh_leader_schedule();

    let sig_verifier = Arc::new(SigVerifier::new(gossip.leader_for_slot(0), cfg.sig_verify));

    // -----------------------------------------------------------------------
    // Channels
    // -----------------------------------------------------------------------
    let cap = cfg.channel_capacity;
    let (parsed_tx, parsed_rx) = bounded(cap);
    let (verified_tx, verified_rx) = bounded(cap);
    let (sig_tx, sig_rx) = bounded::<SpySignal>(cap);
    let (opp_tx, opp_rx) = bounded::<OpportunityBundle>(cap);
    let (pool_refresh_tx, pool_refresh_rx) = bounded(cap);
    // Dedicated graduation sniper channel (bypasses route engine).
    let (grad_tx, grad_rx) = bounded::<SpySignal>(256);

    {
        let hydrator = hydrator.clone();
        thread::Builder::new()
            .name("pool-refresh-worker".into())
            .spawn(move || {
                use std::collections::HashSet;

                while let Ok(first_pool) = pool_refresh_rx.recv() {
                    let mut batch = HashSet::new();
                    batch.insert(first_pool);
                    for pool in pool_refresh_rx.try_iter() {
                        batch.insert(pool);
                    }
                    for pool in batch {
                        hydrator.refresh_single_pool(&pool);
                    }
                }
            })?;
    }

    // -----------------------------------------------------------------------
    // 1. UDP Ingest (Spy Node)
    // -----------------------------------------------------------------------
    let (raw_rx_tvu, _tvu_h) = spawn_ingest_threads(cfg.tvu_addr(), cfg.udp_threads, cap);
    let (raw_rx_jito, _jito_h) = spawn_ingest_threads(cfg.jito_addr(), 2, cap);

    // -----------------------------------------------------------------------
    // 1b. Jito ShredStream gRPC client (nativo Rust — reemplaza el docker proxy)
    //     Autentica con el Block Engine y registra dest=127.0.0.1:JITO_PORT.
    //     El BE empuja shreds al UDP port que ya escucha spawn_ingest_threads.
    //     Requiere que el keypair esté en la whitelist de Jito ShredStream.
    // -----------------------------------------------------------------------
    if cfg.jito_shredstream_enabled {
        use spy_node::jito_shredstream_client::spawn_jito_shredstream;
        use std::sync::{atomic::AtomicBool, Arc};

        // Keypair: prefer JITO_SHREDSTREAM_AUTH_KEYPAIR (whitelisted), fallback to payer
        let auth_kp_path = std::env::var("JITO_SHREDSTREAM_AUTH_KEYPAIR")
            .unwrap_or_else(|_| cfg.payer_keypair.clone());
        let jito_keypair = Arc::new(
            solana_sdk::signature::read_keypair_file(&auth_kp_path)
                .unwrap_or_else(|_| identity_clone),
        );
        info!(pubkey = %jito_keypair.pubkey(), "ShredStream auth keypair loaded");

        let jito_exit = Arc::new(AtomicBool::new(false));
        let jito_port = cfg.jito_addr().port();
        let regions: Vec<String> = std::env::var("JITO_SHREDSTREAM_REGIONS")
            .or_else(|_| std::env::var("JITO_SHREDSTREAM_DESIRED_REGIONS"))
            .unwrap_or_else(|_| "frankfurt,amsterdam".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // dest_ip: public IP of this server so Jito can push UDP shreds here.
        let dest_ip =
            std::env::var("JITO_SHREDSTREAM_DEST_IP").unwrap_or_else(|_| "127.0.0.1".to_string());

        spawn_jito_shredstream(
            jito_keypair,
            cfg.jito_block_engine_url(),
            dest_ip,
            jito_port,
            regions,
            jito_exit,
        );
        info!(port = jito_port, "Jito ShredStream gRPC client started");
    } else {
        info!("Jito ShredStream disabled");
    }

    // -----------------------------------------------------------------------
    // 1c. Jito Searcher Mempool (no whitelist — SEARCHER role, any keypair)
    //     SubscribeMempoolTransactions para DEX programs → SpySignal triggers.
    //     Latencia equivalente a ShredStream (~50-150ms), sin necesidad de whitelist.
    // -----------------------------------------------------------------------
    if cfg.jito_searcher_enabled {
        use spy_node::jito_searcher_client::spawn_jito_searcher;
        use std::sync::{atomic::AtomicBool, Arc};

        let searcher_keypair = Arc::new(
            solana_sdk::signature::read_keypair_file(&cfg.payer_keypair)
                .unwrap_or_else(|_| solana_sdk::signature::Keypair::new()),
        );
        let searcher_exit = Arc::new(AtomicBool::new(false));

        spawn_jito_searcher(
            searcher_keypair,
            cfg.jito_block_engine_url(),
            sig_tx.clone(),
            searcher_exit,
        );
        info!("Jito searcher mempool subscription started (no whitelist required)");
    } else {
        info!("Jito searcher mempool subscription disabled");
    }

    // -----------------------------------------------------------------------
    // 1d-bis. Multi-source shred receiver (ERPC, SolanaCDN — no whitelist)
    //     Adds parallel signal sources for lower latency detection.
    //     ERPC Frankfurt: ~78ms vs 669ms Turbine (8.5× faster)
    //     SolanaCDN: ~78ms via Pipe Network POP mesh (future)
    //     Shredcaster/XDP: lowest overhead via Anza XDP (future)
    // -----------------------------------------------------------------------
    {
        let erpc_endpoint = std::env::var("ERPC_SHREDSTREAM_ENDPOINT")
            .unwrap_or_else(|_| "https://shreds-fra.erpc.global".to_string());
        let erpc_token = std::env::var("ERPC_X_TOKEN").unwrap_or_default();
        let erpc_enabled = std::env::var("ERPC_SHREDSTREAM_ENABLED")
            .map(|v| !matches!(v.as_str(), "0" | "false" | "FALSE"))
            .unwrap_or(true);

        let erpc_stats = if erpc_enabled {
            let stats = spy_node::multi_shred_source::ShredSourceStats::new();
            spy_node::multi_shred_source::spawn_erpc_source(
                erpc_endpoint.clone(),
                erpc_token,
                sig_tx.clone(),
                Arc::clone(&stats),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
            info!(endpoint = %erpc_endpoint, "ERPC gRPC source started (parallel with Turbine)");
            Some(stats)
        } else {
            None
        };

        // ERPC ShredstreamProxy — ~78ms shred-derived entries via gRPC
        // Endpoints: http://shreds-fra6-1.erpc.global (Frankfurt)
        let erpc_shreds_endpoint = std::env::var("ERPC_SHREDS_ENDPOINT")
            .unwrap_or_else(|_| "http://shreds-fra6-1.erpc.global".to_string());
        let erpc_shreds_token = std::env::var("ERPC_SHREDS_X_TOKEN").unwrap_or_default();
        let erpc_shreds_enabled = std::env::var("ERPC_SHREDS_ENABLED")
            .map(|v| !matches!(v.as_str(), "0" | "false" | "FALSE"))
            .unwrap_or(true);

        // Create delta tracker early so SolanaCDN can feed it
        let early_delta_tracker = Arc::new(ShredDeltaTracker::new(pool_cache.clone()));
        // Pre-register vaults from mapped_pools.json
        if let Ok(mapped) = market_engine::pool_hydrator::load_mapped_pools(pools_file) {
            for m in &mapped {
                let Ok(pool_pk) = m.pool_id.parse::<Pubkey>() else { continue };
                if let Ok(va) = m.vault_a.parse::<Pubkey>() {
                    if va != Pubkey::default() { early_delta_tracker.register_vault(va, pool_pk, true); }
                }
                if let Ok(vb) = m.vault_b.parse::<Pubkey>() {
                    if vb != Pubkey::default() { early_delta_tracker.register_vault(vb, pool_pk, false); }
                }
            }
        }

        let scdn_stats_opt = if erpc_shreds_enabled {
            let scdn_stats = spy_node::solanacdn_client::SolanaCdnStats::new();
            // Wrap delta tracker as a callback for the SolanaCDN client
            let dt_for_cdn = early_delta_tracker.clone();
            let cdn_tx_callback: spy_node::solanacdn_client::TxCallback = Arc::new(move |slot, tx| {
                dt_for_cdn.process_transaction(slot, tx);
            });

            spy_node::solanacdn_client::spawn_solanacdn_client(
                erpc_shreds_endpoint.clone(),
                erpc_shreds_token,
                sig_tx.clone(),
                Some(cdn_tx_callback),
                Arc::clone(&scdn_stats),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
            info!(
                endpoint = %erpc_shreds_endpoint,
                "ERPC ShredstreamProxy source started (parallel with Turbine)"
            );
            Some(scdn_stats)
        } else {
            None
        };

        // Unified stats logger for all multi-source shred feeds
        if let Some(erpc_stats) = erpc_stats {
            spy_node::multi_shred_source::spawn_stats_logger(erpc_stats, scdn_stats_opt);
        }
    }

    // -----------------------------------------------------------------------
    // 1d. Turbine TVU — gossip PUSH announcer (sin GossipService, sin whitelist)
    //     Construye Protocol::PushMessage manualmente (Protocol es pub(crate))
    //     y lo envía a los validators via UDP cada 20s.
    //     Efecto: entramos en las tablas CRDS → Turbine nos envía shreds a TVU.
    // -----------------------------------------------------------------------
    {
        use spy_node::turbine_announcer::spawn_turbine_announcer;

        let ta_identity = Arc::new(
            solana_sdk::signature::read_keypair_file(&cfg.identity_keypair)
                .unwrap_or_else(|_| solana_sdk::signature::Keypair::new()),
        );

        // Gossip addr: IP pública + puerto 8003 (8001 está tomado por solana-bot).
        // El puerto solo necesita estar en el rango UFW (8000:8100/udp permitido).
        let public_ip: std::net::IpAddr = std::env::var("JITO_SHREDSTREAM_DEST_IP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| cfg.tvu_addr().ip());
        let gossip_port: u16 = std::env::var("HELIOS_GOSSIP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8003);
        let our_gossip_addr = std::net::SocketAddr::new(public_ip, gossip_port);
        let our_tvu_addr = std::net::SocketAddr::new(public_ip, cfg.tvu_addr().port());

        let shred_version = gossip.cluster_info.my_shred_version();

        spawn_turbine_announcer(
            ta_identity.clone(),
            our_gossip_addr,
            our_tvu_addr,
            shred_version,
            gossip.entrypoint,
            cfg.rpc_url.clone(),
        );
        info!(
            tvu = %our_tvu_addr,
            gossip = %our_gossip_addr,
            shred_version,
            "Turbine announcer started — pushing ContactInfo to validators"
        );

        // Gossip PING/PONG responder: escucha en el mismo puerto gossip anunciado
        // y responde PINGs de los validators. Sin esto, los validators eliminan
        // nuestro ContactInfo del CRDS por falta de liveness → sin shreds Turbine.
        {
            use spy_node::gossip_pong_responder::spawn_gossip_pong_responder;
            let pong_addr = std::net::SocketAddr::new("0.0.0.0".parse().unwrap(), gossip_port);
            spawn_gossip_pong_responder(pong_addr, ta_identity);
            info!(addr = %pong_addr, "Gossip PONG responder started");
        }
    }

    // -----------------------------------------------------------------------
    // 2. Shred Parsing
    // -----------------------------------------------------------------------
    {
        let parsed_tx_tvu = parsed_tx.clone();
        thread::Builder::new()
            .name("shred-parser-tvu".into())
            .spawn(move || {
                // Pin to core 2 (cores 0-1 reserved for UDP ingest)
                if let Some(cores) = core_affinity::get_core_ids() {
                    if cores.len() > 2 { core_affinity::set_for_current(cores[2]); }
                }
                for pkt in raw_rx_tvu {
                    if let Ok(shred) = parse_shred(pkt.data) {
                        let _ = parsed_tx_tvu.send(shred);
                    }
                }
            })?;
        let parsed_tx_jito = parsed_tx;
        thread::Builder::new()
            .name("shred-parser-jito".into())
            .spawn(move || {
                // Pin to core 3
                if let Some(cores) = core_affinity::get_core_ids() {
                    if cores.len() > 3 { core_affinity::set_for_current(cores[3]); }
                }
                for pkt in raw_rx_jito {
                    if let Ok(shred) = parse_shred(pkt.data) {
                        let _ = parsed_tx_jito.send(shred);
                    }
                }
            })?;
    }

    // -----------------------------------------------------------------------
    // 3. Sig Verification
    // -----------------------------------------------------------------------
    {
        let verifier = sig_verifier.clone();
        let verified_tx = verified_tx.clone();
        thread::Builder::new()
            .name("sig-verifier".into())
            .spawn(move || {
                // Pin to core 4
                if let Some(cores) = core_affinity::get_core_ids() {
                    if cores.len() > 4 { core_affinity::set_for_current(cores[4]); }
                }
                verifier.run_batch(parsed_rx, verified_tx);
            })?;
    }

    // -----------------------------------------------------------------------
    // 4. Leader Sync
    // -----------------------------------------------------------------------
    {
        let gossip = gossip.clone();
        let verifier = sig_verifier.clone();
        thread::Builder::new()
            .name("leader-sync".into())
            .spawn(move || {
                let mut last_slot = 0u64;
                loop {
                    let approx_slot = (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64)
                        / 400;
                    if approx_slot != last_slot {
                        let leader = gossip.leader_for_slot(approx_slot);
                        verifier.update_leader(leader);
                        last_slot = approx_slot;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            })?;
    }

    // -----------------------------------------------------------------------
    // 5. Signal Processing (Market Engine)
    // -----------------------------------------------------------------------
    // Clone predictor and new_pool_monitor for route-engine thread BEFORE signal-processor takes ownership.
    let shred_predictor_for_route = shred_predictor.clone();
    let new_pool_monitor_for_route = new_pool_monitor.clone();

    // Blockhash deriver: extract usable blockhash from shred-decoded entries (0 RPC).
    let shred_blockhash = spy_node::blockhash_deriver::BlockhashDeriver::new();

    // Opportunity scanner: continuous cross-DEX arb detection on every vault update.
    let opp_scanner = Arc::new(market_engine::opportunity_scanner::OpportunityScanner::new(
        pool_cache.clone(),
    ));
    let (opp_scan_tx, opp_scan_rx) = crossbeam_channel::bounded::<market_engine::types::RouteParams>(256);

    {
        let pool_cache = pool_cache.clone();
        let sig_tx = sig_tx.clone();
        let wallet_tracker_sp = wallet_tracker.clone();

        let delta_tracker_raw = ShredDeltaTracker::new(pool_cache.clone())
            .with_opportunity_scanner(opp_scanner.clone(), opp_scan_tx);
        let delta_tracker = Arc::new(delta_tracker_raw);
        let dt_ref = delta_tracker.clone();

        let shred_bh_ref = shred_blockhash.clone();

        // Pre-register vault→pool mappings from mapped_pools.json (instant).
        // This ensures shred deltas apply BEFORE the hydrator decodes pool metadata.
        {
            if let Ok(mapped) = market_engine::pool_hydrator::load_mapped_pools(pools_file) {
                let mut registered = 0u32;
                for m in &mapped {
                    let Ok(pool_pk) = m.pool_id.parse::<Pubkey>() else { continue };
                    if let Ok(va) = m.vault_a.parse::<Pubkey>() {
                        if va != Pubkey::default() {
                            delta_tracker.register_vault(va, pool_pk, true);
                            registered += 1;
                        }
                    }
                    if let Ok(vb) = m.vault_b.parse::<Pubkey>() {
                        if vb != Pubkey::default() {
                            delta_tracker.register_vault(vb, pool_pk, false);
                            registered += 1;
                        }
                    }
                }
                info!(registered, "delta tracker: pre-registered vaults from JSON");
            }
        }

        // Periodically re-index vaults after hydration adds new metadata.
        {
            let dt_reindex = delta_tracker.clone();
            thread::Builder::new()
                .name("delta-reindex".into())
                .spawn(move || {
                    thread::sleep(Duration::from_secs(60));
                    loop {
                        dt_reindex.reindex();
                        tracing::debug!(vaults = dt_reindex.tracked_vaults(), "delta tracker reindexed");
                        thread::sleep(Duration::from_secs(30));
                    }
                })
                .expect("spawn delta-reindex");
        }

        // Dirty vault RPC refresh: drains vaults touched by shred swaps (Strategy C)
        // and fetches their real balances via RPC to correct optimistic estimates.
        // Runs every 2s, batches up to 50 vaults per tick.
        {
            let dt_dirty = delta_tracker.clone();
            let dirty_rpc_url = std::env::var("POOL_RPC_URL").unwrap_or_else(|_| {
                std::env::var("RPC_URL").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into())
            });
            thread::Builder::new()
                .name("dirty-vault-refresh".into())
                .spawn(move || {
                    let client = solana_rpc_client::rpc_client::RpcClient::new_with_timeout(
                        dirty_rpc_url, Duration::from_secs(10),
                    );
                    thread::sleep(Duration::from_secs(60)); // wait for initial hydration
                    info!("dirty-vault-refresh: started (2s interval, corrects shred optimistic reserves)");
                    loop {
                        let vaults = dt_dirty.drain_dirty_vaults(50);
                        if !vaults.is_empty() {
                            let n = vaults.len();
                            let mut balances: Vec<(Pubkey, u64)> = Vec::with_capacity(n);
                            // Batch into chunks of 100 for getMultipleAccounts
                            for chunk in vaults.chunks(100) {
                                match client.get_multiple_accounts(chunk) {
                                    Ok(accounts) => {
                                        for (pk, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                                            if let Some(acc) = maybe_acc {
                                                if acc.data.len() >= 72 {
                                                    let amount = u64::from_le_bytes(
                                                        acc.data[64..72].try_into().unwrap_or([0; 8]),
                                                    );
                                                    balances.push((*pk, amount));
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::debug!(error = %e, "dirty-vault-refresh: RPC error");
                                    }
                                }
                            }
                            if !balances.is_empty() {
                                dt_dirty.apply_vault_balances(&balances);
                                tracing::debug!(
                                    drained = n,
                                    fetched = balances.len(),
                                    "dirty-vault-refresh: corrected optimistic reserves"
                                );
                            }
                        }
                        thread::sleep(Duration::from_secs(2));
                    }
                })
                .expect("spawn dirty-vault-refresh");
        }
        thread::Builder::new()
            .name("signal-processor".into())
            .spawn(move || {
                let mut fec = FecAssembler::new();
                let decoder = TxDecoder::new(spy_node::tx_decoder::new_alt_cache());
                let mut signal_processor =
                    SignalProcessor::new(pool_cache, sig_tx, Some(pool_refresh_tx));
                if let Some(wt) = wallet_tracker_sp {
                    signal_processor = signal_processor.with_wallet_tracker(wt);
                }
                signal_processor = signal_processor.with_shred_predictor(Arc::clone(&shred_predictor));
                signal_processor = signal_processor.with_new_pool_monitor(Arc::clone(&new_pool_monitor));

                // Throughput counters (reset every 10s)
                let mut n_shreds: u64 = 0;
                let mut n_data_shreds: u64 = 0;
                let mut n_code_shreds: u64 = 0;
                let mut n_data_complete_flags: u64 = 0;
                let mut n_last_in_slot_flags: u64 = 0;
                let mut n_fec_complete: u64 = 0;
                let mut n_txs_decoded: u64 = 0;
                let mut last_stats = std::time::Instant::now();

                for shred in verified_rx {
                    if !deduper.accept(&shred) {
                        continue;
                    }
                    let slot = shred.slot;
                    n_shreds += 1;
                    match shred.shred_type {
                        spy_node::shred_parser::ShredType::Data => n_data_shreds += 1,
                        spy_node::shred_parser::ShredType::Code => n_code_shreds += 1,
                    }
                    if shred.data_complete {
                        n_data_complete_flags += 1;
                    }
                    if shred.last_in_slot {
                        n_last_in_slot_flags += 1;
                    }
                    if fec.ingest(&shred) {
                        for (slot, _idx, payloads) in fec.drain_complete() {
                            n_fec_complete += 1;
                            let (txs, entries) = decoder.decode_batch_with_entries(slot, &payloads);
                            // Feed entries to blockhash deriver (extract PoH hash for TX signing).
                            if !entries.is_empty() {
                                let is_last = shred.last_in_slot;
                                shred_bh_ref.on_entries_decoded(slot, &entries, is_last);
                            }
                            n_txs_decoded += txs.len() as u64;
                            for tx in txs {
                                // Real-time vault balance update from shred.
                                dt_ref.process_transaction(slot, &tx);
                                // Detect DEX swap activity → hot route + new pool discovery.
                                dt_ref.detect_dex_activity(slot, &tx);

                                let sig = SpySignal::NewTransaction {
                                    slot,
                                    tx,
                                    detected_at: std::time::Instant::now(),
                                };
                                let _ = signal_processor.process(sig);
                            }
                        }
                    }

                    if slot % 1000 == 0 {
                        fec.prune_old_slots(slot.saturating_sub(100));
                    }

                    // Log throughput every 10s
                    if last_stats.elapsed().as_secs() >= 10 {
                        let fec_stats = fec.stats_snapshot();
                        tracing::info!(
                            n_shreds,
                            n_data_shreds,
                            n_code_shreds,
                            n_data_complete_flags,
                            n_last_in_slot_flags,
                            n_fec_complete,
                            n_txs_decoded,
                            fec_sets = fec_stats.sets,
                            fec_slot_buffers = fec_stats.slot_buffers,
                            fec_buffered_data_shreds = fec_stats.buffered_data_shreds,
                            fec_completed_boundaries = fec_stats.completed_boundaries,
                            fec_sets_with_coding = fec_stats.sets_with_coding,
                            fec_sets_at_data_threshold = fec_stats.sets_at_data_threshold,
                            fec_recovery_attempts = fec_stats.recovery_attempts,
                            fec_merkle_recovery_attempts = fec_stats.merkle_recovery_attempts,
                            fec_legacy_recovery_attempts = fec_stats.legacy_recovery_attempts,
                            fec_recovered_data_shreds = fec_stats.recovered_data_shreds,
                            fec_recovery_errors = fec_stats.recovery_errors,
                            fec_recovery_invalid_index = fec_stats.recovery_invalid_index,
                            fec_recovery_invalid_shard_size = fec_stats.recovery_invalid_shard_size,
                            fec_recovery_too_few_shards = fec_stats.recovery_too_few_shards,
                            fec_recovery_other_errors = fec_stats.recovery_other_errors,
                            fec_recovery_empty_results = fec_stats.recovery_empty_results,
                            fec_deshred_attempts = fec_stats.deshred_attempts,
                            fec_deshred_failures = fec_stats.deshred_failures,
                            fec_code_header_inconsistencies = fec_stats.code_header_inconsistencies,
                            fec_code_header_invalid = fec_stats.code_header_invalid,
                            fec_sets_with_coding_meta = fec_stats.sets_with_coding_meta,
                            fec_sets_missing_coding_meta = fec_stats.sets_missing_coding_meta,
                            fec_avg_shreds_per_set_x10 = fec_stats.avg_shreds_per_set_x10,
                            fec_max_data_in_set = fec_stats.max_data_in_set,
                            fec_sets_all_data_present = fec_stats.sets_all_data_present,
                            shreds_per_s = n_shreds / 10,
                            txs_per_s = n_txs_decoded / 10,
                            delta_txs = dt_ref.txs_scanned.load(std::sync::atomic::Ordering::Relaxed),
                            delta_transfers = dt_ref.transfers_found.load(std::sync::atomic::Ordering::Relaxed),
                            delta_vault_updates = dt_ref.vault_updates.load(std::sync::atomic::Ordering::Relaxed),
                            delta_tracked_vaults = dt_ref.tracked_vaults(),
                            "pipeline throughput"
                        );
                        n_shreds = 0;
                        n_data_shreds = 0;
                        n_code_shreds = 0;
                        n_data_complete_flags = 0;
                        n_last_in_slot_flags = 0;
                        n_fec_complete = 0;
                        n_txs_decoded = 0;
                        last_stats = std::time::Instant::now();
                    }
                }
            })?;
    }

    // -----------------------------------------------------------------------
    // 5b. Signal Fan-out: Graduation → sniper fast path (bypass route engine)
    // -----------------------------------------------------------------------
    // All signals go to the route engine. Graduation signals ALSO go to the
    // dedicated graduation sniper (grad_tx) which bypasses Bellman-Ford entirely.
    {
        let (sig_for_route_tx, sig_for_route_rx_inner) = bounded::<SpySignal>(cap);
        // Rebind so route engine below uses the new channel.
        let grad_tx_fanout = grad_tx.clone();
        thread::Builder::new()
            .name("sig-fanout".into())
            .spawn(move || {
                for signal in sig_rx {
                    // Graduation: send to sniper fast path (clone) + route engine.
                    if matches!(signal, SpySignal::Graduation { .. }) {
                        let _ = grad_tx_fanout.try_send(signal.clone());
                    }
                    let _ = sig_for_route_tx.try_send(signal);
                }
            })?;

        // Shadow sig_rx with the new inner channel so route engine below picks it up.
        let sig_rx = sig_for_route_rx_inner;

    // -----------------------------------------------------------------------
    // 6. Route Engine (Bellman-Ford / Cycle Detection)
    // -----------------------------------------------------------------------
    {
        let pool_cache_re = pool_cache.clone();
        let opp_tx = opp_tx.clone();
        let re_logger = strategy_logger.clone();
        let decision_engine = market_engine::ml_models::DecisionEngine::new();
        let jito_only = std::env::var("JITO_ONLY").map(|v| v == "true").unwrap_or(false);
        let re_ml = ml_scorer.clone();
        let re_landing = landing_predictor.clone();
        let re_hot_routes = hot_routes.clone();
        let re_multihop_hot = multihop_hot_routes.clone();
        let re_predictor = shred_predictor_for_route;
        let re_pump_grad = pump_grad_arb.clone();
        let re_new_pool_monitor = new_pool_monitor_for_route;
        let re_cyclic = cyclic_scanner.clone();
        let re_dlmm_wp = dlmm_whirlpool.clone();
        // New strategies (Plan ARB-1)
        let re_whale_backrun = Arc::new(executor::whale_backrun::WhaleBackrunner::new(
            pool_cache.clone(),
            Arc::new(executor::rpc_pool::RpcPool::new(&[
                "https://solana-rpc.publicnode.com",
                "https://api.mainnet-beta.solana.com",
            ])),
        ));
        let re_stablecoin_arb = Arc::new(market_engine::stablecoin_arb::StablecoinArbScanner::new(
            pool_cache.clone(),
        ));
        let re_opp_scanner = opp_scanner.clone();
        let re_opp_rx = opp_scan_rx;
        let re_threehop = threehop_predictor.clone();
        let re_grad_pred = grad_predictor.clone();
        let re_spread_pred = spread_predictor.clone();
        thread::Builder::new()
            .name("route-engine".into())
            .spawn(move || {
                // Pin to core 5
                if let Some(cores) = core_affinity::get_core_ids() {
                    if cores.len() > 5 { core_affinity::set_for_current(cores[5]); }
                }
                let shred_predictor = re_predictor;
                let pool_cache_for_gen = pool_cache_re.clone();
                let engine = RouteEngine::new(pool_cache_re, Some(oracle_cache.clone()));
                let selector = FlashSelector::new();
                let scorer = Scorer::new();
                let mut next_id = 1u64;

                // Build opportunity scanner index after initial hydration.
                re_opp_scanner.rebuild_index();

                // Periodic diagnostics counters (info-level every 30s).
                let mut re_signals_total = 0u64;
                let mut re_signals_stale = 0u64;
                let mut re_cache_hits = 0u64;
                let mut re_bf_runs = 0u64;
                let mut re_cycles_found = 0u64;
                let mut re_last_log = std::time::Instant::now();
                let mut re_last_pools_added = 0usize;
                let mut re_last_total_pools = 0usize;
                // Track last emitted swap_gen to avoid spamming the same cached routes.
                let mut last_emitted_swap_gen = u64::MAX;
                // BF cooldown: skip BF if last run found 0 cycles recently and pool state barely changed.
                let mut last_bf_zero_time = std::time::Instant::now() - std::time::Duration::from_secs(10);
                let mut last_bf_zero_swap_gen = 0u64;

                loop {
                    // Non-blocking drain: consume ALL pending signals, keep only the freshest.
                    // This prevents the 380ms backlog that kills signal freshness.
                    let signal = match sig_rx.recv_timeout(std::time::Duration::from_millis(5)) {
                        Ok(s) => s,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break, // channel closed
                    };
                    let mut latest_signal = signal;
                    let mut drained = 0u64;
                    while let Ok(newer) = sig_rx.try_recv() {
                        latest_signal = newer;
                        drained += 1;
                    }
                    if drained > 0 {
                        re_signals_stale += drained;
                    }
                    re_signals_total += 1 + drained;

                    let (slot, detected_at, sig_type) = match &latest_signal {
                        SpySignal::NewTransaction {
                            slot, detected_at, ..
                        } => (*slot, *detected_at, "tx"),
                        SpySignal::WhaleSwap { slot, .. } => {
                            (*slot, std::time::Instant::now(), "whale")
                        }
                        SpySignal::Graduation { slot, .. } => {
                            (*slot, std::time::Instant::now(), "grad")
                        }
                        SpySignal::LiquidityEvent { slot, .. } => {
                            (*slot, std::time::Instant::now(), "liq")
                        }
                    };

                    // Drop stale signals — for competitive arb, >200ms is too old.
                    // Other bots with <50ms latency will have captured the opportunity.
                    // Exception: graduation events get 2000ms — spreads last 1-5s post-migration.
                    let max_signal_age_ms: u64 = std::env::var("MAX_SIGNAL_AGE_MS")
                        .ok().and_then(|v| v.parse().ok()).unwrap_or(200);
                    let max_age = if matches!(&latest_signal, SpySignal::Graduation { .. }) {
                        2000u128
                    } else {
                        max_signal_age_ms as u128
                    };
                    if detected_at.elapsed().as_millis() > max_age {
                        re_signals_stale += 1;
                        continue;
                    }

                    let current_swap_gen = pool_cache_for_gen.swap_generation();

                    // BF cooldown: if last run found 0 cycles AND elapsed < cooldown AND
                    // swap_gen hasn't moved much, skip to avoid wasting CPU.
                    let bf_cooldown_ms: u64 = std::env::var("BF_COOLDOWN_MS")
                        .ok().and_then(|v| v.parse().ok()).unwrap_or(200);
                    let swap_gen_delta = current_swap_gen.saturating_sub(last_bf_zero_swap_gen);
                    if last_bf_zero_time.elapsed().as_millis() < bf_cooldown_ms as u128 && swap_gen_delta < 10 {
                        re_signals_stale += 1;
                        continue;
                    }

                    let t_route = std::time::Instant::now();

                    // Feed competition detector with failed route data
                    // (observe is called from executor on TX result, but we also
                    // track requote failures here)

                    // FLASH LOAN ATOMIC ARB: 2-hop cross-DEX + 3-hop triangular + event-driven.
                    // All operations are atomic: flash borrow → swap hops → repay. Failed TX = 0 cost.

                    // ── PRIORITY #1: Pump Graduation Arb (proven profitable — 4/4 confirmed TXs) ──
                    // Run FIRST: graduation spreads are time-sensitive (1-5s window).
                    // Also immediately scan cross-DEX arb for the graduated token.
                    let mut routes: Vec<market_engine::types::RouteParams> = Vec::new();

                    // Drain opportunity scanner routes (from delta tracker vault updates).
                    while let Ok(route) = re_opp_rx.try_recv() {
                        routes.push(route);
                    }
                    let mut grad_pred_score: f64 = 0.0; // ML graduation predictor score (0 = not graduation)
                    if let SpySignal::Graduation { token_mint, pump_pool, slot: grad_slot, source, .. } = &latest_signal {
                        info!(
                            token = %token_mint,
                            pool = %pump_pool,
                            slot = grad_slot,
                            source,
                            "PRIORITY: graduation event — scanning cross-DEX arb FIRST"
                        );

                        // Telegram notification: graduation detected — sync HTTP (no async needed).
                        {
                            let tg_msg = format!(
                                "🎓 *Graduation Detected!*\nToken: `{}`\nPool: `{}`\nSource: {}\nSlot: {}\nScanning cross-DEX arb...",
                                &token_mint.to_string()[..8], &pump_pool.to_string()[..8], source, grad_slot
                            );
                            let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
                            let chat = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
                            if !token.is_empty() && !chat.is_empty() {
                                std::thread::spawn(move || {
                                    let _ = reqwest::blocking::Client::new()
                                        .post(format!("https://api.telegram.org/bot{}/sendMessage", token))
                                        .form(&[("chat_id", &chat), ("text", &tg_msg), ("parse_mode", &"Markdown".to_string())])
                                        .send();
                                });
                            }
                        }

                        // ML graduation predictor: score this graduation event.
                        let grad_hour = chrono::Timelike::hour(&chrono::Utc::now());
                        let grad_is_pumpswap = *source == "PumpSwap";
                        // Estimate reserve from pool cache (if available).
                        let (grad_reserve_sol, grad_ratio) = pool_cache_for_gen.get(pump_pool)
                            .map(|p| {
                                let sol = p.reserve_a as f64 / 1_000_000_000.0;
                                let ratio = if p.reserve_b > 0 { p.reserve_a as f64 / p.reserve_b as f64 } else { 1.0 };
                                (sol, ratio)
                            })
                            .unwrap_or((0.0, 1.0));
                        // Count other pools for this token (cross-DEX).
                        let grad_n_pools = pool_cache_for_gen.pools_for_token(token_mint)
                            .iter()
                            .filter(|p| p.pool_address != *pump_pool)
                            .count() as u32;
                        let p_grad = re_grad_pred.predict(
                            grad_reserve_sol,
                            grad_n_pools > 0,
                            grad_n_pools,
                            grad_hour,
                            grad_is_pumpswap,
                            grad_ratio,
                        );
                        if p_grad < 0.3 {
                            tracing::debug!(
                                p_grad = format!("{:.3}", p_grad),
                                token = %token_mint,
                                "graduation skipped by ML predictor (still scanning)"
                            );
                        } else {
                            tracing::info!(
                                p_grad = format!("{:.3}", p_grad),
                                token = %token_mint,
                                "graduation ML score OK"
                            );
                        }
                        grad_pred_score = p_grad;

                        // Graduation arb: cross-DEX price differences on the graduated token.
                        let grad_arb_routes = re_pump_grad.scan_graduation_arb(
                            token_mint, pump_pool, BORROW_AMOUNTS,
                        );
                        if !grad_arb_routes.is_empty() {
                            info!(
                                strategy = "pump_graduation_arb",
                                routes = grad_arb_routes.len(),
                                best_profit = grad_arb_routes[0].net_profit,
                                "PRIORITY graduation arb opportunities found"
                            );
                            routes.extend(grad_arb_routes);
                        }

                        // Also do targeted pair scan for the graduated token across all pools.
                        let pair_routes = market_engine::cross_dex::scan_pair_opportunities(
                            &pool_cache_for_gen,
                            *token_mint,
                            solana_sdk::pubkey!("So11111111111111111111111111111111111111112"),
                            BORROW_AMOUNTS,
                        );
                        if !pair_routes.is_empty() {
                            info!(
                                strategy = "graduation_pair_scan",
                                routes = pair_routes.len(),
                                "graduation targeted pair scan found routes"
                            );
                            routes.extend(pair_routes);
                        }
                    }

                    let run_bf = re_signals_total % 10 == 0;
                    if run_bf {
                        routes.extend(engine.find_opportunities_multi(BORROW_AMOUNTS));
                    }

                    // E6: Cross-DEX same-pair 2-hop arb (ALWAYS run — fast, proven profitable).
                    let cross_dex_routes = market_engine::cross_dex::scan_cross_dex_opportunities(
                        &pool_cache_for_gen,
                        BORROW_AMOUNTS,
                    );
                    if !cross_dex_routes.is_empty() {
                        tracing::debug!(count = cross_dex_routes.len(), "cross-dex 2-hop routes found");
                        routes.extend(cross_dex_routes);
                    }

                    // Cross-DEX 3-hop triangular arb.
                    {
                        let cdx3_routes = market_engine::cross_dex::scan_cross_dex_3hop(
                            &pool_cache_for_gen,
                            BORROW_AMOUNTS,
                        );
                        if !cdx3_routes.is_empty() {
                            tracing::info!(count = cdx3_routes.len(), "cross-dex 3-hop routes found");
                            routes.extend(cdx3_routes);
                        }
                    }

                    // Hot Routes: priority scan of historically profitable pairs.
                    {
                        let hr = re_hot_routes.read();
                        if !hr.is_empty() {
                            let hot = market_engine::cross_dex::scan_hot_routes(
                                &pool_cache_for_gen,
                                &hr,
                                BORROW_AMOUNTS,
                            );
                            if !hot.is_empty() {
                                tracing::debug!(count = hot.len(), "hot-route opportunities found");
                                routes.extend(hot);
                            }
                        }
                    }

                    // Multi-hop hot routes from snapshot scanner.
                    {
                        let mhr = re_multihop_hot.read();
                        if !mhr.is_empty() {
                            let multihop = market_engine::cross_dex::scan_multihop_hot_routes(
                                &pool_cache_for_gen,
                                &mhr,
                                BORROW_AMOUNTS,
                            );
                            if !multihop.is_empty() {
                                tracing::debug!(count = multihop.len(), "multihop hot-route opportunities");
                                routes.extend(multihop);
                            }
                        }
                    }

                    // Event-driven pair scan: on ANY swap signal (whale or normal),
                    // scan the affected pair for cross-DEX arb. Much faster than full scan
                    // because it only checks pools for one specific token pair (~1ms vs ~55ms).
                    match &latest_signal {
                        SpySignal::WhaleSwap { pool, .. } => {
                            if let Some(pool_entry) = pool_cache_for_gen.get(pool) {
                                let pair_routes = market_engine::cross_dex::scan_pair_opportunities(
                                    &pool_cache_for_gen,
                                    pool_entry.token_a,
                                    pool_entry.token_b,
                                    BORROW_AMOUNTS,
                                );
                                routes.extend(pair_routes);
                            }
                        }
                        SpySignal::NewTransaction { tx, .. } => {
                            // Extract pool from first detected swap to do targeted pair scan
                            let swaps = market_engine::swap_decoder::decode_swaps(tx);
                            for swap in swaps.iter().take(1) {
                                if let Some(pool_entry) = pool_cache_for_gen.get(&swap.pool) {
                                    let pair_routes = market_engine::cross_dex::scan_pair_opportunities(
                                        &pool_cache_for_gen,
                                        pool_entry.token_a,
                                        pool_entry.token_b,
                                        BORROW_AMOUNTS,
                                    );
                                    routes.extend(pair_routes);
                                }
                            }
                        }
                        _ => {}
                    }

                    // Pump Graduation Arb: handled in PRIORITY block above (runs FIRST).

                    // Cyclic 3-leg arb: niche venue combinations.
                    if run_bf {
                        let cyclic_routes = re_cyclic.scan_cyclic_opportunities(BORROW_AMOUNTS);
                        if !cyclic_routes.is_empty() {
                            tracing::info!(
                                strategy = "cyclic_3leg",
                                routes = cyclic_routes.len(),
                                "cyclic arb scan"
                            );
                            routes.extend(cyclic_routes);
                        }
                    }

                    // DLMM/Whirlpool divergence arb.
                    {
                        let dlmm_routes = re_dlmm_wp.scan_bin_divergence(BORROW_AMOUNTS);
                        if !dlmm_routes.is_empty() {
                            tracing::info!(
                                strategy = "dlmm_whirlpool_arb",
                                routes = dlmm_routes.len(),
                                "dlmm/whirlpool arb scan"
                            );
                            routes.extend(dlmm_routes);
                        }
                    }

                    // ── Strategy: Whale Backrun (P0) ──
                    // On whale swap, check cross-DEX spread with RPC-refreshed reserves.
                    if let SpySignal::WhaleSwap { pool, impact_pct, tx, .. } = &latest_signal {
                        // Extract amount_in and direction from the actual swap in the TX.
                        let swaps = market_engine::swap_decoder::decode_swaps(tx);
                        let whale_swap = swaps.iter().find(|s| s.pool == *pool);
                        let (est_amount_in, est_a_to_b) = if let Some(ws) = whale_swap {
                            (ws.amount_in, ws.a_to_b)
                        } else {
                            // Fallback: estimate from impact
                            let amt = pool_cache_for_gen.get(pool)
                                .map(|p| (p.reserve_a as f64 * impact_pct / 100.0) as u64)
                                .unwrap_or(100_000_000);
                            (amt, true)
                        };
                        let backrun_routes = re_whale_backrun.check_backrun(
                            pool, est_amount_in, est_a_to_b, slot,
                        );
                        if !backrun_routes.is_empty() {
                            tracing::info!(
                                strategy = "whale_backrun",
                                routes = backrun_routes.len(),
                                best_profit = backrun_routes[0].net_profit,
                                "whale backrun opportunities found"
                            );
                            routes.extend(backrun_routes);
                        }
                    }

                    // ── Strategy: Stablecoin Depeg Arb (P1) ──
                    // Check every ~10 signals (low frequency, stablecoin depegs are rare).
                    if re_signals_total % 10 == 0 {
                        let stable_routes = re_stablecoin_arb.scan(BORROW_AMOUNTS);
                        if !stable_routes.is_empty() {
                            tracing::info!(
                                strategy = "stablecoin_arb",
                                routes = stable_routes.len(),
                                "stablecoin depeg arb found"
                            );
                            routes.extend(stable_routes);
                        }
                    }

                    // New Pool Monitor: periodic check for watched tokens that now have 2+ pools.
                    // Lightweight O(N) where N = watched tokens (~100-500).
                    {
                        let npm_opps = re_new_pool_monitor.check_watched_tokens();
                        for opp in &npm_opps {
                            if !opp.routes.is_empty() {
                                tracing::info!(
                                    token = %opp.token_mint,
                                    new_pool = %opp.new_pool,
                                    dex = ?opp.new_pool_dex,
                                    routes = opp.routes.len(),
                                    time_since_grad_s = opp.time_since_graduation.as_secs(),
                                    strategy = "new_pool_arb",
                                    "new-pool-monitor: arb routes for second pool"
                                );
                                routes.extend(opp.routes.clone());
                            }

                            // Telegram alert for second pool creation — sync HTTP.
                            {
                                let tg_msg = NewPoolMonitor::format_telegram_alert(opp);
                                let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
                                let chat = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
                                if !token.is_empty() && !chat.is_empty() {
                                    std::thread::spawn(move || {
                                        let _ = reqwest::blocking::Client::new()
                                            .post(format!("https://api.telegram.org/bot{}/sendMessage", token))
                                            .form(&[("chat_id", &chat), ("text", &tg_msg), ("parse_mode", &"Markdown".to_string())])
                                            .send();
                                    });
                                }
                            }
                        }
                        // Periodic cleanup of expired entries (every 100 signals ~ every few seconds).
                        if re_signals_total % 100 == 0 {
                            re_new_pool_monitor.cleanup_expired();
                        }
                    }

                    // LST Arb: scan staking derivative pairs.
                    let lst_routes = market_engine::lst_scanner::scan_lst_opportunities(
                        &pool_cache_for_gen,
                        BORROW_AMOUNTS,
                    );
                    if !lst_routes.is_empty() {
                        tracing::debug!(count = lst_routes.len(), "lst-arb routes found");
                        routes.extend(lst_routes);
                    }

                    let route_ms = t_route.elapsed().as_millis();

                    // Track cache hits vs real Bellman-Ford runs.
                    if route_ms < 1 {
                        re_cache_hits += 1;
                    } else {
                        re_bf_runs += 1;
                        // Grab latest pool stats from engine.
                        re_last_total_pools = engine.last_total_pools();
                        re_last_pools_added = engine.last_pools_in_graph();
                    }
                    if !routes.is_empty() {
                        re_cycles_found += routes.len() as u64;
                    } else if route_ms >= 1 {
                        // Real BF run (not cache hit) found 0 cycles — start cooldown.
                        last_bf_zero_time = std::time::Instant::now();
                        last_bf_zero_swap_gen = current_swap_gen;
                    }

                    // Periodic summary every 30s (MUST be before the continue below).
                    if re_last_log.elapsed().as_secs() >= 30 {
                        tracing::info!(
                            signals = re_signals_total,
                            stale = re_signals_stale,
                            cache_hits = re_cache_hits,
                            bf_runs = re_bf_runs,
                            cycles_found = re_cycles_found,
                            total_pools = re_last_total_pools,
                            pools_in_graph = re_last_pools_added,
                            new_pool_watching = re_new_pool_monitor.watching_count(),
                            "route-engine stats"
                        );
                        re_signals_total = 0;
                        re_signals_stale = 0;
                        re_cache_hits = 0;
                        re_bf_runs = 0;
                        re_cycles_found = 0;
                        re_last_log = std::time::Instant::now();
                    }

                    // Only emit routes if swap_gen changed (avoid spamming the executor
                    // with the same cached routes on every signal).
                    // Filter out high-competition routes that always fail
                    routes.retain(|r| {
                        let route_key = r.hops.iter()
                            .map(|h| h.pool.to_string()[..8].to_string())
                            .collect::<Vec<_>>().join("_");
                        let comp = decision_engine.competition.score_multiplier(&route_key);
                        if comp == 0.0 {
                            tracing::debug!(route = %route_key, "BLACKLISTED route filtered");
                            false
                        } else {
                            true
                        }
                    });

                    // Filter 3-hop routes with ML predictor
                    let hour_utc = chrono::Timelike::hour(&chrono::Utc::now());
                    routes.retain(|r| {
                        if r.hops.len() >= 3 {
                            let dex_strs: Vec<String> = r.hops.iter()
                                .map(|h| h.dex_program.to_string()).collect();
                            let hop_infos: Vec<ml_scorer::threehop_predictor::HopInfo<'_>> = r.hops.iter()
                                .enumerate()
                                .map(|(i, h)| ml_scorer::threehop_predictor::HopInfo {
                                    dex_program: &dex_strs[i],
                                    amount_out: h.amount_out,
                                    price_impact: h.price_impact,
                                })
                                .collect();
                            let p = re_threehop.predict(&hop_infos, r.net_profit, hour_utc);
                            if p < 0.3 {
                                tracing::debug!(
                                    p_land_3hop = format!("{:.3}", p),
                                    hops = r.hops.len(),
                                    net_profit = r.net_profit,
                                    "3-hop route filtered by ML predictor"
                                );
                                return false;
                            }
                        }
                        true
                    });

                    if routes.is_empty() || current_swap_gen == last_emitted_swap_gen {
                        continue;
                    }
                    last_emitted_swap_gen = current_swap_gen;
                    tracing::info!(
                        routes = routes.len(),
                        route_ms,
                        signal_age_ms = detected_at.elapsed().as_millis(),
                        sig_type,
                        swap_gen = current_swap_gen,
                        "route engine completed"
                    );

                    // Sort: pump_graduation_arb FIRST (#1 proven strategy), then hot-route/whale,
                    // then by tier (lower=better), then by profit.
                    routes.sort_by(|a, b| {
                        let a_grad = a.strategy == "pump_graduation_arb";
                        let b_grad = b.strategy == "pump_graduation_arb";
                        if a_grad != b_grad {
                            return if a_grad { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
                        }
                        let a_priority = matches!(a.strategy, "hot-route" | "whale-backrun");
                        let b_priority = matches!(b.strategy, "hot-route" | "whale-backrun");
                        match (a_priority, b_priority) {
                            (true, false) => std::cmp::Ordering::Less,
                            (false, true) => std::cmp::Ordering::Greater,
                            _ => a.tier.cmp(&b.tier).then(b.net_profit.cmp(&a.net_profit)),
                        }
                    });

                    for route in routes {
                        // Collect route metadata for logging BEFORE any filtering
                        let route_pools: Vec<String> = route.hops.iter()
                            .map(|h| h.pool.to_string()).collect();
                        let route_mints: Vec<String> = {
                            let mut m: Vec<String> = route.hops.iter()
                                .flat_map(|h| vec![h.token_in.to_string(), h.token_out.to_string()])
                                .collect();
                            m.sort(); m.dedup(); m
                        };
                        let route_reserves: (Vec<i64>, Vec<i64>) = route.hops.iter()
                            .map(|h| {
                                pool_cache_for_gen.get(&h.pool)
                                    .map(|p| (p.reserve_a as i64, p.reserve_b as i64))
                                    .unwrap_or((0, 0))
                            })
                            .unzip();

                        let Ok(flash_provider) = selector.select(route.borrow_amount) else {
                            // Log rejected route (no flash provider)
                            if let Some(ref logger) = re_logger {
                                logger.log_route_candidate(strategy_logger::types::LogRouteCandidate {
                                    slot, detected_at: strategy_logger::types::instant_to_utc(detected_at),
                                    strategy: route.strategy.to_string(), borrow_amount: route.borrow_amount,
                                    n_hops: route.hops.len() as i16, gross_profit: route.gross_profit,
                                    net_profit: route.net_profit, score: 0.0, ml_boost: 1.0,
                                    risk_factor: route.risk_factor, emitted: false,
                                    reject_reason: Some("no_flash_provider".into()),
                                    pools: route_pools, token_mints: route_mints,
                                    reserves_a: route_reserves.0, reserves_b: route_reserves.1,
                                });
                            }
                            continue;
                        };

                        let mut bundle = OpportunityBundle {
                            id: next_id,
                            slot,
                            detected_at,
                            route,
                            flash_provider,
                            score: 0.0,
                        };
                        bundle.score = scorer.score(&bundle);
                        let is_high_conviction = matches!(
                            bundle.route.strategy,
                            "pump_graduation_arb" | "new_pool_arb" | "graduation_pair_scan"
                        );
                        if !scorer.is_viable(&bundle) && !is_high_conviction {
                            // Log rejected route (low score)
                            if let Some(ref logger) = re_logger {
                                logger.log_route_candidate(strategy_logger::types::LogRouteCandidate {
                                    slot, detected_at: strategy_logger::types::instant_to_utc(detected_at),
                                    strategy: bundle.route.strategy.to_string(),
                                    borrow_amount: bundle.route.borrow_amount,
                                    n_hops: bundle.route.hops.len() as i16,
                                    gross_profit: bundle.route.gross_profit,
                                    net_profit: bundle.route.net_profit,
                                    score: bundle.score, ml_boost: 1.0,
                                    risk_factor: bundle.route.risk_factor, emitted: false,
                                    reject_reason: Some("score_not_viable".into()),
                                    pools: route_pools, token_mints: route_mints,
                                    reserves_a: route_reserves.0, reserves_b: route_reserves.1,
                                });
                            }
                            continue;
                        }

                        // ML score boost: adjust score based on learned pool/strategy performance
                        let pool_keys: Vec<String> = bundle.route.hops.iter()
                            .map(|h| h.pool.to_string())
                            .collect();
                        let pool_refs: Vec<&str> = pool_keys.iter().map(|s| s.as_str()).collect();
                        let ml_boost = re_ml.score_boost(&pool_refs, bundle.route.strategy);
                        bundle.score *= ml_boost;

                        // Landing predictor: compute P(land) and log it
                        let hop_impacts: Vec<f64> = bundle.route.hops.iter()
                            .map(|h| h.price_impact).collect();
                        let ml_rates: Vec<(f64, f64)> = pool_refs.iter().map(|p| {
                            re_ml.pool_model_rates(p)
                        }).collect();
                        let landing_features = re_landing.build_features(
                            &pool_refs,
                            &hop_impacts,
                            bundle.route.borrow_amount,
                            bundle.route.net_profit,
                            bundle.score,
                            bundle.route.strategy,
                            detected_at.elapsed().as_millis() as f64,
                            &ml_rates,
                        );
                        let (p_land, _should_exec) = re_landing.predict(&landing_features);

                        // DecisionEngine: 6 ML models evaluate the route
                        let route_key = pool_keys.join("_");
                        let dex_combo = bundle.route.hops.iter()
                            .map(|h| format!("{:?}", market_engine::types::DexType::from_program_id(&h.dex_program)))
                            .collect::<Vec<_>>().join("+");
                        let hour = chrono::Timelike::hour(&chrono::Utc::now());
                        let reserve_fresh = detected_at.elapsed().as_millis() < 500;
                        let jito_p75 = 10_000u64; // TODO: get from sender

                        let decision = decision_engine.evaluate(
                            &route_key, &dex_combo,
                            bundle.route.net_profit, detected_at.elapsed().as_millis() as f64,
                            hour, bundle.route.hops.len() as u32, reserve_fresh, jito_p75,
                        );

                        if decision.is_none() && !is_high_conviction && !jito_only {
                            // DecisionEngine says skip (bypassed when JITO_ONLY=true: 0 cost on failure)
                            continue;
                        }

                        // Spread survival predictor: filter routes unlikely to survive 400ms slot time.
                        let spread_bps = if bundle.route.borrow_amount > 0 {
                            (bundle.route.net_profit as f64 / bundle.route.borrow_amount as f64) * 10_000.0
                        } else {
                            0.0
                        };
                        let min_reserve_log = {
                            let min_res = bundle.route.hops.iter()
                                .filter_map(|h| pool_cache_for_gen.get(&h.pool).map(|p| p.reserve_a.min(p.reserve_b)))
                                .min()
                                .unwrap_or(1)
                                .max(1);
                            (min_res as f64).ln()
                        };
                        let fee_total_bps: f64 = bundle.route.hops.iter()
                            .map(|h| ml_scorer::threehop_predictor::fee_bps_for_dex(&h.dex_program.to_string()))
                            .sum();
                        let spread_dex_combo: u8 = bundle.route.hops.iter()
                            .map(|h| {
                                let s = h.dex_program.to_string();
                                match s.as_str() {
                                    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8" => 1u8,
                                    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK" => 2,
                                    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc" => 3,
                                    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo" => 4,
                                    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA" => 5,
                                    _ => 6,
                                }
                            })
                            .fold(0u8, |acc, v| acc.wrapping_mul(7).wrapping_add(v));
                        let spread_is_pumpswap = bundle.route.hops.iter()
                            .any(|h| h.dex_program.to_string() == "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
                        let spread_pass_rate = {
                            let (sr, _) = pool_refs.first()
                                .map(|p| re_ml.pool_model_rates(p))
                                .unwrap_or((0.34, 0.0));
                            sr
                        };
                        let p_spread = re_spread_pred.predict(
                            spread_bps, min_reserve_log, fee_total_bps,
                            spread_dex_combo, hour, bundle.route.hops.len() as u32,
                            spread_is_pumpswap, spread_pass_rate,
                        );
                        if p_spread < 0.4 && !is_high_conviction && !jito_only {
                            tracing::debug!(
                                p_spread = format!("{:.3}", p_spread),
                                strategy = bundle.route.strategy,
                                net_profit = bundle.route.net_profit,
                                "route skipped by spread survival predictor"
                            );
                            if let Some(ref logger) = re_logger {
                                logger.log_route_candidate(strategy_logger::types::LogRouteCandidate {
                                    slot, detected_at: strategy_logger::types::instant_to_utc(detected_at),
                                    strategy: bundle.route.strategy.to_string(),
                                    borrow_amount: bundle.route.borrow_amount,
                                    n_hops: bundle.route.hops.len() as i16,
                                    gross_profit: bundle.route.gross_profit,
                                    net_profit: bundle.route.net_profit,
                                    score: bundle.score, ml_boost,
                                    risk_factor: bundle.route.risk_factor, emitted: false,
                                    reject_reason: Some(format!("spread_survival={:.3}", p_spread)),
                                    pools: route_pools, token_mints: route_mints,
                                    reserves_a: route_reserves.0, reserves_b: route_reserves.1,
                                });
                            }
                            continue;
                        }

                        // 3-hop ML predictor score (for logging)
                        let p_3hop = if bundle.route.hops.len() >= 3 {
                            let dex_strs: Vec<String> = bundle.route.hops.iter()
                                .map(|h| h.dex_program.to_string()).collect();
                            let hop_infos: Vec<ml_scorer::threehop_predictor::HopInfo<'_>> = bundle.route.hops.iter()
                                .enumerate()
                                .map(|(i, h)| ml_scorer::threehop_predictor::HopInfo {
                                    dex_program: &dex_strs[i],
                                    amount_out: h.amount_out,
                                    price_impact: h.price_impact,
                                })
                                .collect();
                            re_threehop.predict(&hop_infos, bundle.route.net_profit, hour)
                        } else {
                            1.0 // 2-hop routes are not filtered
                        };

                        let emit_ms = detected_at.elapsed().as_millis();
                        tracing::info!(
                            id = bundle.id,
                            strategy = bundle.route.strategy,
                            tier = bundle.route.tier,
                            score = bundle.score,
                            ml_boost = format!("{:.2}", ml_boost),
                            p_land = format!("{:.3}", p_land),
                            p_land_3hop = format!("{:.3}", p_3hop),
                            p_grad = format!("{:.3}", grad_pred_score),
                            p_spread = format!("{:.3}", p_spread),
                            net_profit = bundle.route.net_profit,
                            route_ms,
                            emit_ms,
                            "opportunity emitted"
                        );

                        // Feed arb to shred predictor for swap→arb correlation learning
                        {
                            let arb_pools: Vec<String> = bundle.route.hops.iter()
                                .map(|h| h.pool.to_string()).collect();
                            shred_predictor.observe_arb_detected(&arb_pools, bundle.route.net_profit as i64);
                        }

                        // Feed 3-hop route to ML predictor for online learning (emitted = positive signal)
                        if bundle.route.hops.len() >= 3 {
                            let dex_strs: Vec<String> = bundle.route.hops.iter()
                                .map(|h| h.dex_program.to_string()).collect();
                            let hop_infos: Vec<ml_scorer::threehop_predictor::HopInfo<'_>> = bundle.route.hops.iter()
                                .enumerate()
                                .map(|(i, h)| ml_scorer::threehop_predictor::HopInfo {
                                    dex_program: &dex_strs[i],
                                    amount_out: h.amount_out,
                                    price_impact: h.price_impact,
                                })
                                .collect();
                            let hour_utc = (chrono::Utc::now().timestamp() % 86400 / 3600) as u32;
                            re_threehop.observe(&hop_infos, bundle.route.net_profit, hour_utc, true);
                        }

                        // Log emitted route candidate to PG (positive example for ML)
                        if let Some(ref logger) = re_logger {
                            logger.log_route_candidate(strategy_logger::types::LogRouteCandidate {
                                slot,
                                detected_at: strategy_logger::types::instant_to_utc(detected_at),
                                strategy: bundle.route.strategy.to_string(),
                                borrow_amount: bundle.route.borrow_amount,
                                n_hops: bundle.route.hops.len() as i16,
                                gross_profit: bundle.route.gross_profit,
                                net_profit: bundle.route.net_profit,
                                score: bundle.score,
                                ml_boost,
                                risk_factor: bundle.route.risk_factor,
                                emitted: true,
                                reject_reason: None,
                                pools: pool_keys.clone(),
                                token_mints: {
                                    let mut m: Vec<String> = bundle.route.hops.iter()
                                        .flat_map(|h| vec![h.token_in.to_string(), h.token_out.to_string()])
                                        .collect();
                                    m.sort(); m.dedup(); m
                                },
                                reserves_a: bundle.route.hops.iter()
                                    .map(|h| pool_cache_for_gen.get(&h.pool).map(|p| p.reserve_a as i64).unwrap_or(0))
                                    .collect(),
                                reserves_b: bundle.route.hops.iter()
                                    .map(|h| pool_cache_for_gen.get(&h.pool).map(|p| p.reserve_b as i64).unwrap_or(0))
                                    .collect(),
                            });
                        }

                        // Log full opportunity details to PostgreSQL for ML training
                        if let Some(ref logger) = re_logger {
                            use market_engine::types::DexType;
                            let token_mints: Vec<String> = {
                                let mut mints: Vec<String> = bundle.route.hops.iter()
                                    .flat_map(|h| vec![h.token_in.to_string(), h.token_out.to_string()])
                                    .collect();
                                mints.sort();
                                mints.dedup();
                                mints
                            };
                            let hops: Vec<strategy_logger::types::LogHop> = bundle.route.hops.iter()
                                .enumerate()
                                .map(|(i, h)| {
                                    let (ra, rb, age) = pool_cache_for_gen
                                        .get(&h.pool)
                                        .map(|p| {
                                            let age = p.last_updated.elapsed().as_millis() as u64;
                                            (Some(p.reserve_a), Some(p.reserve_b), Some(age))
                                        })
                                        .unwrap_or((None, None, None));
                                    strategy_logger::types::LogHop {
                                        hop_index: i as i16,
                                        pool: h.pool.to_string(),
                                        dex_type: format!("{:?}", DexType::from_program_id(&h.dex_program)),
                                        token_in: h.token_in.to_string(),
                                        token_out: h.token_out.to_string(),
                                        amount_in: if i == 0 { Some(bundle.route.borrow_amount) } else { None },
                                        amount_out: h.amount_out,
                                        price_impact: h.price_impact,
                                        pool_reserve_a: ra,
                                        pool_reserve_b: rb,
                                        reserve_age_ms: age,
                                    }
                                })
                                .collect();
                            logger.log_opportunity(strategy_logger::types::LogOpportunity {
                                opp_id: bundle.id,
                                slot: bundle.slot,
                                detected_at: strategy_logger::types::instant_to_utc(bundle.detected_at),
                                signal_type: sig_type.to_string(),
                                source_strategy: bundle.route.strategy.to_string(),
                                token_mints,
                                borrow_amount: bundle.route.borrow_amount,
                                flash_provider: format!("{:?}", bundle.flash_provider),
                                gross_profit: bundle.route.gross_profit,
                                net_profit: bundle.route.net_profit,
                                score: bundle.score,
                                risk_factor: bundle.route.risk_factor,
                                n_hops: bundle.route.hops.len() as i16,
                                fast_arb: std::env::var("FAST_ARB").map(|v| v == "true").unwrap_or(false),
                                hops,
                            });
                        }

                        next_id = next_id.saturating_add(1);
                        let _ = opp_tx.send(bundle);
                    }
                }
            })?;
    }
    } // end sig-fanout scope

    // -----------------------------------------------------------------------
    // 6b. SQLite Route Feed (persisted candidates from solana-bot)
    // -----------------------------------------------------------------------
    if cfg.sqlite_route_feed {
        sqlite_route_feed::spawn_sqlite_route_feed(
            cfg.sqlite_route_db_path.clone(),
            Duration::from_secs(cfg.sqlite_route_poll_secs.max(1)),
            cfg.sqlite_route_top_n.max(1),
            Duration::from_secs(cfg.sqlite_route_cooldown_secs.max(1)),
            cfg.min_net_profit,
            pool_cache.clone(),
            opp_tx.clone(),
        );
        info!(
            db_path = %cfg.sqlite_route_db_path,
            poll_secs = cfg.sqlite_route_poll_secs,
            top_n = cfg.sqlite_route_top_n,
            "sqlite route feed enabled"
        );
    }

    // -----------------------------------------------------------------------
    // 6c. WebSocket DEX Feed (Helius logsSubscribe – fallback)
    // -----------------------------------------------------------------------
    if cfg.ws_dex_feed_enabled {
        ws_dex_feed::spawn_ws_dex_feed(&cfg.rpc_url, sig_tx.clone());
        info!("WS DEX feed started (Raydium, Orca, Meteora, PumpSwap)");
    } else {
        info!("WS DEX feed disabled");
    }

    // -----------------------------------------------------------------------
    // 6d. Richat gRPC Feed (<1ms latency from local Agave plugin)
    //     Connects to richat-plugin-agave gRPC at 127.0.0.1:10100.
    //     Fallback: any Yellowstone-compatible gRPC endpoint via GEYSER_ENDPOINT.
    //     When NO_AGAVE_MODE=true, skip Richat (Agave not replaying) and rely
    //     on shred delta tracker for reserve freshness.
    // -----------------------------------------------------------------------
    {
        let geyser_endpoint = std::env::var("GEYSER_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:10100".to_string());
        let geyser_token = std::env::var("GEYSER_TOKEN").unwrap_or_default();
        if no_agave_mode {
            info!("Richat gRPC feed SKIPPED (NO_AGAVE_MODE) — shred deltas provide reserve updates");
        } else if cfg.geyser_feed_enabled && !geyser_endpoint.is_empty() {
            geyser_feed::spawn_geyser_feed(
                geyser_endpoint.clone(),
                geyser_token,
                pool_cache.clone(),
                Some(sig_tx.clone()),
            );
            info!(endpoint = %geyser_endpoint, "Richat gRPC vault feed started");
        } else if !cfg.geyser_feed_enabled {
            info!("Richat gRPC feed disabled");
        }
    }

    // -----------------------------------------------------------------------
    // 6b. Geyser Local UDP Receiver (~0ms latency from Agave plugin)
    // -----------------------------------------------------------------------
    geyser_local::spawn_geyser_local(pool_cache.clone());

    // -----------------------------------------------------------------------
    // 7. Executor Loop (Transaction Execution)
    // -----------------------------------------------------------------------
    {
        let rpc_url = cfg.rpc_url.clone();
        // All executor RPC calls (getLatestBlockhash, getMultipleAccounts, simulate,
        // ALT creation) go through the local sim-server (HTTP/2 pool + blockhash cache).
        // sim-server transparently proxies unknown methods to Helius.
        let sim_rpc_url = std::env::var("SIM_RPC_URL").unwrap_or_else(|_| rpc_url.clone());
        let dry_run = cfg.dry_run;
        let min_profit = cfg.min_net_profit;
        let payer = read_keypair_file(&cfg.payer_keypair).unwrap_or_else(|_| {
            warn!("could not read payer keypair, using ephemeral for executor");
            solana_sdk::signature::Keypair::new()
        });

        let builder = Arc::new(executor::tx_builder::TxBuilder::new(payer, &sim_rpc_url));
        let simulator = Arc::new(executor::simulator::Simulator::new(&sim_rpc_url));
        let sender = Arc::new(executor::jito_sender::JitoSender::new(
            if cfg.jito_endpoint.is_empty() {
                None
            } else {
                Some(cfg.jito_endpoint.clone())
            },
            None, // use dynamic tip floor (p75 from Jito API)
        ));
        sender.spawn_tip_floor_refresh();
        sender.spawn_endpoint_latency_probe();
        // ALT cache uses a dedicated RPC for getSlot + sendTransaction + getSignatureStatuses.
        // Public mainnet RPC works fine since ALT creation is infrequent (~once per new route).
        let alt_rpc_url = std::env::var("ALT_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
        info!(alt_rpc = %alt_rpc_url, "ALT cache RPC endpoint");
        let alt_cache_exec = executor::alt_loader::AltCache::new(
            &alt_rpc_url,
            &executor::alt_loader::parse_alt_env(),
            300,
        );
        let rt = tokio::runtime::Runtime::new()?;
        let pool_cache_exec = pool_cache.clone();

        // TPU direct fanout sender: sends TX simultaneously to the next 4 leaders
        // via UDP, complementing the Jito bundle for ~1.6s coverage window.
        let tpu_sender: Option<Arc<executor::tpu_sender::TpuSender>> = {
            let fanout: usize = std::env::var("TPU_FANOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4);
            match executor::tpu_sender::TpuSender::new(&sim_rpc_url, Some(fanout)) {
                Ok(ts) => {
                    let ts = Arc::new(ts);
                    executor::tpu_sender::TpuSender::spawn_refresh_loop(ts.clone());
                    info!(fanout, "TPU direct fanout sender enabled");
                    Some(ts)
                }
                Err(e) => {
                    warn!(error = %e, "TPU direct fanout sender disabled");
                    None
                }
            }
        };

        let exec_logger = strategy_logger.clone();
        let exec_ml = Some(ml_scorer.clone());

        // JetSender: live leader tracking via gRPC + direct TPU routing
        let jet_sender = {
            let payer_kp = Arc::new(
                solana_sdk::signature::read_keypair_file(
                    &std::env::var("PAYER_KEYPAIR").unwrap_or_else(|_| "/root/solana-bot/wallet.json".into())
                ).expect("read payer keypair for JetSender")
            );
            let jet = executor::jet_sender::JetSender::new(payer_kp, sim_rpc_url.clone());
            let grpc_ep = std::env::var("GEYSER_ENDPOINT").ok()
                .or_else(|| std::env::var("GRPC_ENDPOINT").ok())
                .or_else(|| std::env::var("RICHAT_ENDPOINT").ok());
            jet.spawn_background(grpc_ep);
            info!("JetSender: live leader-tracking TPU sender ENABLED");
            Some(jet)
        };

        thread::Builder::new()
            .name("executor".into())
            .spawn(move || {
                executor::executor_loop(
                    opp_rx,
                    builder,
                    simulator,
                    sender,
                    tpu_sender,
                    jet_sender,
                    alt_cache_exec,
                    pool_cache_exec,
                    rt,
                    rpc_url,
                    min_profit,
                    dry_run,
                    exec_logger,
                    exec_ml,
                    Some(shred_blockhash.clone()),
                );
            })?;
    }

    // -----------------------------------------------------------------------
    // 8. Graduation Direct Sniper (fast path — Frankfurt, TPU + Jito)
    // -----------------------------------------------------------------------
    {
        let payer_sniper = read_keypair_file(&cfg.payer_keypair).unwrap_or_else(|_| {
            warn!("sniper: could not read payer keypair, sniper disabled");
            solana_sdk::signature::Keypair::new()
        });
        let payer_sniper = Arc::new(payer_sniper);
        let sender_sniper = Arc::new(executor::jito_sender::JitoSender::new(
            if cfg.jito_endpoint.is_empty() {
                None
            } else {
                Some(cfg.jito_endpoint.clone())
            },
            None, // use dynamic tip floor (p75)
        ));
        sender_sniper.spawn_tip_floor_refresh();
        let tpu_sniper: Option<Arc<executor::tpu_sender::TpuSender>> = {
            match executor::tpu_sender::TpuSender::new(&cfg.rpc_url, Some(4)) {
                Ok(ts) => Some(Arc::new(ts)),
                Err(_) => None,
            }
        };
        let sim_rpc_sniper = std::env::var("SIM_RPC_URL")
            .unwrap_or_else(|_| cfg.rpc_url.clone());
        let dry_run_sniper = cfg.dry_run;
        let sniper_logger = strategy_logger.clone();

        thread::Builder::new()
            .name("graduation-sniper".into())
            .spawn(move || {
                use executor::sniper::{fetch_cached_blockhash, GraduationSniper};
                use spy_node::signal_bus::SpySignal;

                let tg_sniper = executor::telegram::TelegramBot::from_env();
                let sniper = GraduationSniper::new(payer_sniper, sender_sniper, tpu_sniper, tg_sniper, sniper_logger);
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio rt graduation-sniper");

                // Guard: disable sniping when SNIPE_SOL_AMOUNT=0 (only atomic arb, no buying).
                let snipe_enabled = std::env::var("SNIPE_SOL_AMOUNT")
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|sol| sol > 0.0)
                    .unwrap_or(true);
                if !snipe_enabled {
                    info!("SNIPER: graduation sniping DISABLED (SNIPE_SOL_AMOUNT=0). Graduation arb still active in route engine.");
                }

                // Dedup: only snipe each token once per 5 minutes.
                let mut sniped_tokens: std::collections::HashMap<solana_sdk::pubkey::Pubkey, std::time::Instant>
                    = std::collections::HashMap::new();
                const SNIPE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

                for signal in grad_rx {
                    let (token_mint, pump_pool, slot, source, creator, pool_base_mint) = match signal {
                        SpySignal::Graduation { token_mint, pump_pool, slot, source, creator, pool_base_mint, .. } => {
                            (token_mint, pump_pool, slot, source, creator, pool_base_mint)
                        }
                        _ => continue,
                    };

                    // Skip sniping entirely when SNIPE_SOL_AMOUNT=0.
                    // Graduation arb (cross-DEX atomic flash loan) runs in the route engine thread.
                    if !snipe_enabled {
                        tracing::debug!(
                            token = %token_mint,
                            pool = %pump_pool,
                            "SNIPER: sniping disabled, graduation arb handled by route engine"
                        );
                        continue;
                    }

                    // Skip if we recently sniped this token.
                    if let Some(last) = sniped_tokens.get(&token_mint) {
                        if last.elapsed() < SNIPE_COOLDOWN {
                            tracing::debug!(token = %token_mint, "SNIPER: token already sniped recently, skipping");
                            continue;
                        }
                    }
                    sniped_tokens.insert(token_mint, std::time::Instant::now());

                    // Evict old entries to prevent unbounded growth.
                    sniped_tokens.retain(|_, t| t.elapsed() < SNIPE_COOLDOWN);

                    tracing::info!(
                        token = %token_mint,
                        pool = %pump_pool,
                        slot,
                        "SNIPER: graduation detected — building snipe TX"
                    );

                    // Telegram: migration detected
                    rt.block_on(sniper.notify_graduation_detected(
                        &token_mint, &pump_pool, slot, source,
                    ));

                    let blockhash = rt.block_on(fetch_cached_blockhash(&sim_rpc_sniper));

                    if let Err(e) = rt.block_on(sniper.snipe(
                        token_mint,
                        pump_pool,
                        blockhash,
                        slot,
                        dry_run_sniper,
                        creator,
                        pool_base_mint,
                        source,
                    )) {
                        tracing::warn!(error = %e, "SNIPER: snipe failed");
                    }
                }
            })?;
        info!("graduation sniper started (Frankfurt TPU+Jito fast path)");
    }

    // -----------------------------------------------------------------------
    // 9. Agave Local RPC Health Monitor (Telegram alerts on state change)
    // -----------------------------------------------------------------------
    {
        let agave_url = std::env::var("AGAVE_RPC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
        // Always monitor port 9000 directly (even if AGAVE_RPC_URL points at sim-server)
        let agave_direct = "http://127.0.0.1:9000".to_string();

        thread::Builder::new()
            .name("agave-monitor".into())
            .spawn(move || {
                let tg = executor::telegram::TelegramBot::from_env();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio rt agave-monitor");

                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap_or_else(|_| reqwest::blocking::Client::new());

                let mut last_healthy: Option<bool> = None;
                let mut last_slots_behind: u64 = u64::MAX;
                const HEALTHY_THRESHOLD: u64 = 150;

                loop {
                    thread::sleep(Duration::from_secs(60));

                    let body = serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "method": "getHealth"
                    });

                    let (healthy, slots_behind) = match client.post(&agave_direct).json(&body).send() {
                        Ok(resp) => {
                            if let Ok(json) = resp.json::<serde_json::Value>() {
                                if json.get("result").is_some() {
                                    (true, 0u64)
                                } else if let Some(data) = json["error"]["data"].as_object() {
                                    let behind = data.get("numSlotsBehind")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(u64::MAX);
                                    (behind <= HEALTHY_THRESHOLD, behind)
                                } else {
                                    (false, u64::MAX)
                                }
                            } else {
                                (false, u64::MAX)
                            }
                        }
                        Err(_) => (false, u64::MAX),
                    };

                    // Alert on state change
                    let state_changed = last_healthy != Some(healthy);
                    // Also alert when catching up significantly (every 10K slots)
                    let progress_alert = slots_behind < last_slots_behind
                        && last_slots_behind != u64::MAX
                        && (last_slots_behind - slots_behind) >= 10_000;

                    if state_changed || progress_alert {
                        let msg = if healthy {
                            format!(
                                "✅ Agave Local RPC HEALTHY\nSlots behind: {}\nURL: {}\nSendTransaction: ACTIVE",
                                slots_behind, agave_url
                            )
                        } else if slots_behind == u64::MAX {
                            "❌ Agave Local RPC OFFLINE\nNode not responding on port 9000".to_string()
                        } else {
                            format!(
                                "⏳ Agave Local RPC CATCHING UP\nSlots behind: {}\nHealthy threshold: {}",
                                slots_behind, HEALTHY_THRESHOLD
                            )
                        };

                        info!(healthy, slots_behind, "agave-monitor: state change");
                        let tg_c = tg.clone();
                        rt.block_on(async { tg_c.send_raw(&msg).await });
                    }

                    last_healthy = Some(healthy);
                    last_slots_behind = slots_behind;
                }
            })?;
        info!("Agave health monitor started (port 9000, 60s interval)");
    }

    // -----------------------------------------------------------------------
    // 10. Liquidation Monitor (Phase A — detection only)
    // -----------------------------------------------------------------------
    {
        let liq_rpc = std::env::var("POOL_RPC_URL")
            .unwrap_or_else(|_| cfg.rpc_url.clone());
        thread::Builder::new()
            .name("liquidation-monitor".into())
            .spawn(move || {
                let mut scanner = market_engine::liquidation_scanner::LiquidationScanner::new(&liq_rpc);
                let tg = executor::telegram::TelegramBot::from_env();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio rt liquidation-monitor");

                // Wait 60s before first scan (let pool hydration complete).
                thread::sleep(Duration::from_secs(60));
                info!("liquidation-monitor started");

                loop {
                    let opportunities = scanner.scan_once();
                    for opp in &opportunities {
                        let tg_c = tg.clone();
                        let obligation_str = opp.obligation.to_string();
                        let collateral_str = opp.collateral_mint.to_string();
                        let debt_str = opp.debt_mint.to_string();
                        let health = opp.health_factor;
                        let debt_sol = opp.max_repay_amount as f64 / 1e9;
                        let protocol = opp.protocol;
                        rt.block_on(async {
                            tg_c.alert_liquidation_detected(
                                protocol, &obligation_str, health, debt_sol,
                                &collateral_str, &debt_str,
                            ).await;
                        });
                    }
                    thread::sleep(scanner.poll_interval());
                }
            })?;
        info!("liquidation monitor started (Phase A — detection only, 10s interval)");
    }

    // -----------------------------------------------------------------------
    // 11. Daily Report (Telegram + Email)
    // -----------------------------------------------------------------------
    {
        let tg_daily = executor::telegram::TelegramBot::from_env();
        daily_report::spawn_daily_report(tg_daily, wallet_tracker);
    }

    // -----------------------------------------------------------------------
    // 12. PumpSwap Volume Rewards Claimer (once per day)
    // -----------------------------------------------------------------------
    {
        let claim_rpc = std::env::var("SIM_RPC_URL")
            .unwrap_or_else(|_| cfg.rpc_url.clone());
        let claim_payer = read_keypair_file(&cfg.payer_keypair).unwrap_or_else(|_| {
            warn!("volume-claimer: could not read payer keypair, claimer disabled");
            solana_sdk::signature::Keypair::new()
        });
        let claim_sender = Arc::new(executor::jito_sender::JitoSender::new(
            if cfg.jito_endpoint.is_empty() { None } else { Some(cfg.jito_endpoint.clone()) },
            None,
        ));
        thread::Builder::new()
            .name("volume-claimer".into())
            .spawn(move || {
                use std::sync::atomic::{AtomicU64, Ordering};
                static LAST_CLAIM: AtomicU64 = AtomicU64::new(0);

                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio rt volume-claimer");
                let tg = executor::telegram::TelegramBot::from_env();

                // Wait 5 min on startup before first attempt (let everything warm up).
                thread::sleep(Duration::from_secs(300));
                info!("volume-claimer: started (daily PUMP token claim)");

                loop {
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let last = LAST_CLAIM.load(Ordering::Relaxed);
                    if now_secs.saturating_sub(last) >= 86400 {
                        match rt.block_on(executor::volume_claimer::try_claim_volume_rewards(
                            &claim_rpc,
                            &claim_payer,
                            &claim_sender,
                        )) {
                            Ok(Some(sig)) => {
                                LAST_CLAIM.store(now_secs, Ordering::Relaxed);
                                info!(signature = %sig, "volume-claimer: PUMP rewards claimed!");
                                let tg_c = tg.clone();
                                let msg = format!(
                                    "PUMP Volume Rewards Claimed\nBundle: {}",
                                    sig,
                                );
                                rt.block_on(async { tg_c.send_raw(&msg).await });
                            }
                            Ok(None) => {
                                info!("volume-claimer: nothing to claim (no incentive program or no volume)");
                                // Still mark as attempted so we don't retry every 5 min
                                LAST_CLAIM.store(now_secs, Ordering::Relaxed);
                            }
                            Err(e) => {
                                warn!(error = %e, "volume-claimer: claim attempt failed, will retry next cycle");
                                // Don't update LAST_CLAIM — retry in 5 min
                            }
                        }
                    }
                    // Check every 5 min (aligns with ml-save cadence)
                    thread::sleep(Duration::from_secs(300));
                }
            })?;
        info!("PumpSwap volume rewards claimer started (daily)");
    }

    info!("Helios pipeline fully running – all threads active");

    // Main thread keeps the process alive
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
