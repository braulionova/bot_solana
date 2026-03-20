/// find_routes – Probe para validar rutas de arbitraje end-to-end.
///
/// Flujo:
///   1. Fetch oracle (Pyth Hermes) → precios USD actuales
///   2. Hydrate pool cache desde mapped_pools.json
///   3. RouteEngine::find_opportunities() → Bellman-Ford con filtro TVL $10k + guard Orca
///   4. Para las top N rutas: build TX + simulateTransaction con todos los flash providers
///   5. Reportar qué rutas pasan simulación
///
/// Variables de entorno (todas opcionales):
///   BORROW_AMOUNT   – lamports a pedir en flash loan (default: 1_000_000_000 = 1 SOL)
///   MAX_ROUTES      – número máximo de rutas a simular (default: 10)
///   PAYER_KEYPAIR   – ruta al keypair (default: /root/solana-bot/wallet.json)
///   RPC_URL         – endpoint RPC (default: https://api.mainnet-beta.solana.com)
///   DRY_SIM_ONLY    – si "1", no crea ALT dinámico, solo simula (default: 0)
use anyhow::{Context, Result};
use executor::alt_loader::{parse_alt_env, AltCache};
use executor::flash_loans::fallback_candidates;
use executor::tx_builder::TxBuilder;
use market_engine::{
    oracle::{refresh_blocking, OracleCache},
    pool_hydrator::{load_mapped_pools, PoolHydrator},
    pool_state::PoolStateCache,
    route_engine::RouteEngine,
    types::FlashProvider,
};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    message::VersionedMessage,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
};
use std::sync::Arc;

fn main() -> Result<()> {
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let mapped_pools_file = std::env::var("MAPPED_POOLS_FILE")
        .unwrap_or_else(|_| "/root/solana-bot/mapped_pools.json".to_string());
    let payer_path = std::env::var("PAYER_KEYPAIR")
        .unwrap_or_else(|_| "/root/solana-bot/wallet.json".to_string());
    let borrow_amount: u64 = std::env::var("BORROW_AMOUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000_000); // 1 SOL
    let max_routes: usize = std::env::var("MAX_ROUTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let dry_sim_only = std::env::var("DRY_SIM_ONLY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let payer = read_keypair_file(&payer_path)
        .map_err(|e| anyhow::anyhow!("read payer keypair {}: {}", payer_path, e))?;
    println!("payer={}", payer.pubkey());

    // ── 1. Oracle ────────────────────────────────────────────────────────────
    println!("\n[1/4] Fetching oracle prices (Pyth Hermes)...");
    let oracle = OracleCache::new();
    match refresh_blocking(&oracle) {
        Ok(()) => {
            let snap = oracle.snapshot();
            println!("  oracle prices loaded: {} tokens", snap.len());
            for (mint, price) in &snap {
                println!("    {} = ${:.6}", mint, price);
            }
        }
        Err(e) => {
            println!(
                "  WARNING: oracle fetch failed: {}. TVL filter will be inactive.",
                e
            );
        }
    }

    // ── 2. Pool cache ────────────────────────────────────────────────────────
    println!("\n[2/4] Hydrating pool cache from {}...", mapped_pools_file);
    let pool_cache = Arc::new(PoolStateCache::new());
    let hydrator = PoolHydrator::new(&rpc_url, pool_cache.clone(), &mapped_pools_file);
    let mapped = load_mapped_pools(&mapped_pools_file)
        .with_context(|| format!("load {}", mapped_pools_file))?;
    hydrator.load_from_file()?;
    hydrator.refresh_all(&mapped)?;
    let total_pools: usize = pool_cache.inner_iter().count();
    println!("  pools hydrated: {}", total_pools);

    // ── 3. Route engine ──────────────────────────────────────────────────────
    println!(
        "\n[3/4] Running RouteEngine (Bellman-Ford) with borrow_amount={}...",
        borrow_amount
    );
    let engine = RouteEngine::new(pool_cache.clone(), Some(oracle.clone()));
    let mut routes = engine.find_opportunities(borrow_amount);
    println!(
        "  routes found (above min_profit threshold): {}",
        routes.len()
    );

    if routes.is_empty() {
        println!("\nNo profitable routes found. Possible causes:");
        println!("  - oracle prices not loaded → TVL filter blocked all pools");
        println!("  - all cycles come from dust pools filtered by $10k TVL");
        println!("  - borrow_amount too small to overcome fees (try BORROW_AMOUNT=1000000000)");
        return Ok(());
    }

    routes.truncate(max_routes);

    // ── 4. Simulate top N routes ─────────────────────────────────────────────
    println!("\n[4/4] Simulating top {} routes...", routes.len());

    let rpc = RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed());
    let alt_cache = AltCache::new(&rpc_url, &parse_alt_env(), 300);
    let builder = TxBuilder::new(payer, "https://api.mainnet-beta.solana.com");
    let mut any_success = false;

    for (i, route) in routes.iter().enumerate() {
        println!(
            "\n  Route #{}: hops={} gross_profit={} net_profit={} risk={:.2}",
            i + 1,
            route.hops.len(),
            route.gross_profit,
            route.net_profit,
            route.risk_factor,
        );
        for hop in &route.hops {
            println!(
                "    {} → {} via {} ({:?})",
                hop.token_in, hop.token_out, hop.pool, hop.dex_program
            );
        }

        let blockhash = match rpc.get_latest_blockhash() {
            Ok(bh) => bh,
            Err(e) => {
                println!("    blockhash error: {}", e);
                continue;
            }
        };

        let mut route_ok = false;
        for provider in fallback_candidates(FlashProvider::MarginFi) {
            // Refresh pool metadata for these specific pools before building
            let route_pools: Vec<_> = mapped
                .iter()
                .filter(|m| {
                    route
                        .hops
                        .iter()
                        .any(|hop| m.pool_id.parse::<Pubkey>().ok() == Some(hop.pool))
                })
                .cloned()
                .collect();
            if !route_pools.is_empty() {
                let _ = hydrator.refresh_all(&route_pools);
            }

            let mut alts = alt_cache.all();

            let mut tx = match builder.build(route, provider, blockhash, &alts, &pool_cache) {
                Ok(tx) => tx,
                Err(e) => {
                    println!("    [{:?}] build failed: {}", provider, e);
                    continue;
                }
            };

            let tx_size = bincode::serialize(&tx).map(|b| b.len()).unwrap_or(0);
            println!("    [{:?}] tx size={} bytes", provider, tx_size);

            if tx_size > 1232 && !dry_sim_only {
                let static_keys: Vec<Pubkey> = match &tx.message {
                    VersionedMessage::V0(msg) => msg.account_keys.clone(),
                    VersionedMessage::Legacy(msg) => msg.account_keys.clone(),
                };
                println!(
                    "    [{:?}] oversized – creating dynamic ALT ({} keys)...",
                    provider,
                    static_keys.len()
                );
                match alt_cache.create_and_extend_alt(&builder.payer, static_keys) {
                    Ok(alt_key) => {
                        println!("    [{:?}] dynamic ALT created={}", provider, alt_key);
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        alts = alt_cache.all();
                        let bh2 = rpc.get_latest_blockhash().unwrap_or(blockhash);
                        tx = match builder.build(route, provider, bh2, &alts, &pool_cache) {
                            Ok(t) => t,
                            Err(e) => {
                                println!("    [{:?}] rebuild after ALT failed: {}", provider, e);
                                continue;
                            }
                        };
                        let new_size = bincode::serialize(&tx).map(|b| b.len()).unwrap_or(0);
                        println!("    [{:?}] rebuilt size={} bytes", provider, new_size);
                        if new_size > 1232 {
                            println!("    [{:?}] still oversized, skip", provider);
                            continue;
                        }
                    }
                    Err(e) => {
                        println!("    [{:?}] dynamic ALT failed: {}", provider, e);
                        continue;
                    }
                }
            } else if tx_size > 1232 {
                println!(
                    "    [{:?}] oversized, skipping (DRY_SIM_ONLY mode)",
                    provider
                );
                continue;
            }

            let sim = match rpc.simulate_transaction_with_config(
                &tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    ..Default::default()
                },
            ) {
                Ok(r) => r,
                Err(e) => {
                    println!("    [{:?}] sim RPC error: {}", provider, e);
                    continue;
                }
            };

            match sim.value.err {
                None => {
                    println!("    [{:?}] ✓ SIMULATION PASSED", provider);
                    if let Some(logs) = sim.value.logs {
                        for log in logs.iter().take(20) {
                            println!("      {}", log);
                        }
                    }
                    any_success = true;
                    route_ok = true;
                    break; // first passing provider is enough
                }
                Some(ref err) => {
                    println!("    [{:?}] ✗ sim failed: {:?}", provider, err);
                    if let Some(logs) = sim.value.logs {
                        // print last few logs for debugging
                        let start = logs.len().saturating_sub(8);
                        for log in &logs[start..] {
                            println!("      {}", log);
                        }
                    }
                }
            }
        }

        if route_ok {
            println!("  → Route #{} is EXECUTABLE", i + 1);
        }
    }

    if any_success {
        println!("\n✓ At least one route passed simulation. Bot is READY for DRY_RUN.");
    } else {
        println!("\n✗ No route passed simulation.");
        println!("  Common failures:");
        println!(
            "    InsufficientProfit (6001) → profit < fee; try larger borrow or different pools"
        );
        println!(
            "    Custom(1) (insufficient funds in wallet ATA) → expected for flash loan routes"
        );
        println!("    TickArraySequenceInvalidIndex (6038) → Orca pool missing tick arrays");
        println!("    Missing env vars (MARGINFI_GROUP etc.) → check flash_accounts.env is loaded");
    }

    Ok(())
}
