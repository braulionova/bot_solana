pub mod alt_loader;
pub mod flash_loans;
pub mod helius_fast_sender;
pub mod helius_rebate;
pub mod jet_sender;
pub mod jito_sender;
pub mod preflight;
pub mod simulator;
pub mod sniper;
pub mod telegram;
pub mod tpu_sender;
pub mod turbo_sender;
pub mod turbo_learner;
pub mod ultra_executor;
pub mod rdtsc;
pub mod rpc_pool;
pub mod tx_builder;
pub mod volume_claimer;
pub mod volume_miner;
pub mod whale_backrun;

use crossbeam_channel::Receiver;
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tracing::{debug, error, info, warn};

use market_engine::types::{FlashProvider, OpportunityBundle};

use crate::alt_loader::AltCache;
use crate::jito_sender::JitoSender;
use crate::simulator::Simulator;
use crate::tpu_sender::TpuSender;
use crate::tx_builder::TxBuilder;
use market_engine::pool_state::PoolStateCache;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;

/// Cooldown antes de volver a procesar la misma ruta (mismo par de pools).
/// 15s — persistent cross-dex discrepancies last <1s in practice; if still there
/// after 15s, reserves will have refreshed and re-quote catches phantom profits.
const ROUTE_DEDUP_COOLDOWN: Duration = Duration::from_secs(2); // JITO_ONLY: 0 cost, retry fast
/// After on-chain/simulation failure: exponential backoff starting from 30s, max 5 min.
/// Was 10s — too aggressive, caused 1000+ wasted simulations/hour on same stale route.
const FAILURE_BACKOFF_BASE: Duration = Duration::from_secs(1);
const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// TTL del cache de ATAs existentes.
const ATA_CACHE_TTL: Duration = Duration::from_secs(60);

/// FAST_ARB mode: skip simulation, single provider (MarginFi), build once with real slippage,
/// fire-and-forget send. Mirrors sniper's blind-send approach. Flash loans are atomic —
/// failed TX = 0 cost in Jito bundles (tip only charged on success).
/// Enable with FAST_ARB=true in env.
fn fast_arb_enabled() -> bool {
    std::env::var("FAST_ARB").map(|v| v == "true" || v == "1").unwrap_or(false)
}

/// JITO_ONLY mode: send ONLY via Jito bundles (no TPU, no Agave sendTx, no Helius sendTx).
/// Failed bundles = 0 SOL lost (Jito doesn't include failed bundles in blocks).
/// TPU/sendTx channels land failed TXs and charge base fees (~5K lamports each).
/// Enable with JITO_ONLY=true in env.
fn jito_only_enabled() -> bool {
    std::env::var("JITO_ONLY").map(|v| v == "true" || v == "1").unwrap_or(false)
}

/// DIRECT_SWAP mode: build TX with direct DEX swap instructions instead of helios-arb CPI.
/// Saves ~30-50% CUs, smaller TX, faster execution. Only works with MarginFi (0% fee).
/// Falls back to helios-arb if direct swap build fails.
/// Enable with DIRECT_SWAP=true in env.
fn direct_swap_enabled() -> bool {
    std::env::var("DIRECT_SWAP").map(|v| v == "true" || v == "1").unwrap_or(false)
}

pub fn executor_loop(
    rx: Receiver<OpportunityBundle>,
    builder: Arc<TxBuilder>,
    simulator: Arc<Simulator>,
    sender: Arc<JitoSender>,
    tpu: Option<Arc<TpuSender>>,
    jet: Option<Arc<jet_sender::JetSender>>,
    alt_cache: Arc<AltCache>,
    pool_cache: Arc<PoolStateCache>,
    rt: Runtime,
    rpc_url: String,
    min_profit: i64,
    dry_run: bool,
    strategy_logger: Option<Arc<strategy_logger::StrategyLogger>>,
    ml_scorer: Option<Arc<ml_scorer::MlScorer>>,
    shred_blockhash: Option<Arc<spy_node::blockhash_deriver::BlockhashDeriver>>,
) {
    use solana_rpc_client::rpc_client::RpcClient;
    use solana_sdk::commitment_config::CommitmentConfig;

    // Calibrate RDTSC for sub-microsecond timing in hot path.
    crate::rdtsc::calibrate();
    info!("executor: RDTSC calibrated");

    let tg = crate::telegram::TelegramBot::from_env();
    let helius_rebate = Arc::new(crate::helius_rebate::HeliusRebateSender::from_env(
        &builder.payer.pubkey().to_string(),
    ));
    let helius_fast = Arc::new(crate::helius_fast_sender::HeliusFastSender::from_env());

    // Route ALL executor RPC calls through sim-server (localhost:8081).
    // sim-server serves getLatestBlockhash from its 400ms local cache (~0ms),
    // and transparently proxies all other methods through its HTTP/2 pool.
    let exec_rpc_url = std::env::var("SIM_RPC_URL").unwrap_or(rpc_url);
    let rpc = RpcClient::new_with_commitment(exec_rpc_url.clone(), CommitmentConfig::confirmed());
    // RPC pool for vault refresh: rotates across free endpoints to avoid 429.
    // 400ms timeout per call, tries next endpoint on failure.
    let vault_rpc_urls: Vec<String> = std::env::var("VAULT_RPC_URLS")
        .unwrap_or_else(|_| "https://solana-rpc.publicnode.com,https://api.mainnet-beta.solana.com".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let vault_rpc_refs: Vec<&str> = vault_rpc_urls.iter().map(|s| s.as_str()).collect();
    let vault_pool = crate::rpc_pool::RpcPool::new(&vault_rpc_refs);
    info!(endpoints = vault_pool.len(), "vault RPC pool initialized");

    // In-process blockhash cache: refresh every 400ms in background thread.
    // Retries on startup until a valid blockhash is obtained.
    let cached_blockhash: Arc<parking_lot::RwLock<solana_sdk::hash::Hash>> =
        Arc::new(parking_lot::RwLock::new(solana_sdk::hash::Hash::default()));
    {
        let bh_ref = Arc::clone(&cached_blockhash);
        let bh_rpc_url = exec_rpc_url.clone();
        std::thread::Builder::new()
            .name("bh-refresh".into())
            .spawn(move || {
                let bh_rpc = RpcClient::new_with_commitment(bh_rpc_url, CommitmentConfig::confirmed());
                let mut consecutive_errors = 0u32;
                loop {
                    match bh_rpc.get_latest_blockhash() {
                        Ok(bh) => {
                            *bh_ref.write() = bh;
                            if consecutive_errors > 0 {
                                info!("bh-refresh: recovered after {} errors, blockhash={}", consecutive_errors, bh);
                                consecutive_errors = 0;
                            }
                        }
                        Err(e) => {
                            consecutive_errors += 1;
                            if consecutive_errors <= 3 || consecutive_errors % 20 == 0 {
                                warn!("bh-refresh: error ({}x): {}", consecutive_errors, e);
                            }
                        }
                    }
                    // Faster retry on startup (100ms) until first success, then normal 400ms.
                    let delay = if *bh_ref.read() == solana_sdk::hash::Hash::default() {
                        Duration::from_millis(100)
                    } else {
                        Duration::from_millis(400)
                    };
                    std::thread::sleep(delay);
                }
            })
            .expect("spawn bh-refresh thread");
    }

    // Dedup: (pool_a, pool_b) → (last_processed_at, cooldown, consecutive_failures).
    // Evita re-procesar la misma ruta cientos de veces cuando el route engine la encola en ráfaga.
    // Arc<DashMap> so the async phase3 polling task can write failure backoff.
    let route_dedup: Arc<DashMap<(Pubkey, Pubkey), (Instant, Duration, u32)>> =
        Arc::new(DashMap::new());

    // ATA cache: ATAs cuya existencia ya confirmamos. TTL = 60s.
    let mut known_atas: HashSet<Pubkey> = HashSet::new();
    let mut ata_cache_filled_at = Instant::now();

    for bundle in rx {
        // ── Latency T0: bundle received by executor ───────────────────────────
        let t_recv = Instant::now();
        let detection_ms = bundle.detected_at.elapsed().as_millis();

        // ── Dedup: drop if same route was processed recently ─────────────────
        let route_key: (Pubkey, Pubkey) = {
            let hops = &bundle.route.hops;
            let a = hops.first().map(|h| h.pool).unwrap_or_default();
            let b = hops.last().map(|h| h.pool).unwrap_or_default();
            (a, b)
        };
        let now = Instant::now();
        if let Some(prev) = route_dedup.get(&route_key) {
            if now.duration_since(prev.0) < prev.1 {
                continue;
            }
        }
        // Insert with base cooldown, preserving failure count if exists.
        let prev_failures = route_dedup.get(&route_key).map(|e| e.2).unwrap_or(0);
        route_dedup.insert(route_key, (now, ROUTE_DEDUP_COOLDOWN, prev_failures));
        // Evitar que el DashMap crezca sin límite
        if route_dedup.len() > 1000 {
            route_dedup.retain(|_, t| now.duration_since(t.0) < FAILURE_BACKOFF_MAX);
        }

        // ── ATA cache TTL reset ────────────────────────────────────────────────
        if now.duration_since(ata_cache_filled_at) > ATA_CACHE_TTL {
            known_atas.clear();
            ata_cache_filled_at = now;
        }

        if bundle.route.net_profit < min_profit {
            info!(
                net_profit = bundle.route.net_profit,
                min = min_profit,
                detection_ms,
                "opportunity below min profit threshold, skipping"
            );
            continue;
        }

        // Observability: log pools in route for debugging
        let route_pools: Vec<String> = bundle.route.hops.iter()
            .map(|h| format!("{}→{}", &h.pool.to_string()[..8], &h.token_out.to_string()[..4]))
            .collect();
        info!(
            id = bundle.id,
            slot = bundle.slot,
            strategy = bundle.route.strategy,
            score = bundle.score,
            net_profit = bundle.route.net_profit,
            hops = bundle.route.hops.len(),
            route = ?route_pools,
            detection_ms, // shred-decode → executor-receive (includes route engine)
            "processing opportunity"
        );
        // Only alert on Telegram for high-value opportunities (>100K lamports) to avoid spam.
        if bundle.route.net_profit > 100_000 {
            let tg_c = tg.clone();
            let bundle_id = bundle.id;
            let net_profit = bundle.route.net_profit;
            let hops = bundle.route.hops.len();
            let score = bundle.score;
            let strategy = bundle.route.strategy;
            let borrow_sol = bundle.route.borrow_amount as f64 / 1e9;
            // Extract DEX venue names from hops for new strategy alerts.
            let dex_venues: String = bundle.route.hops.iter()
                .map(|h| {
                    let prog = h.dex_program.to_string();
                    if prog.starts_with("675k") { "RaydiumV4" }
                    else if prog.starts_with("whir") { "Orca" }
                    else if prog.starts_with("LBU") { "Meteora" }
                    else if prog.starts_with("pAMM") { "PumpSwap" }
                    else if prog.starts_with("FLUX") { "FluxBeam" }
                    else if prog.starts_with("SSwp") { "Saber" }
                    else if prog.starts_with("CPMM") { "CPMM" }
                    else { "DEX" }
                })
                .collect::<Vec<_>>()
                .join("→");
            rt.spawn(async move {
                tg_c.alert_opportunity_detected(bundle_id, net_profit, hops, score, strategy, dry_run)
                    .await;
                // Extra detail for new strategies
                if matches!(strategy, "pump_graduation_arb" | "cyclic_3leg" | "dlmm_whirlpool_arb") {
                    tg_c.alert_new_strategy_opportunity(
                        strategy, net_profit, hops, &dex_venues, borrow_sol,
                    ).await;
                }
            });
        }

        // ── FAST ARB: 4-pilar architecture (fresh reserves + fresh blockhash + retry) ──
        // Pilar 1: fetch current vault balances from RPC (commitment: processed)
        // Pilar 2: fresh blockhash with lastValidBlockHeight tracking
        // Pilar 3: retry loop — re-fetch reserves on each attempt
        // Pilar 4: staleness check — skip if reserves clearly stale
        if fast_arb_enabled() && !dry_run {

            // ── Pilar 1: Live vault refresh (optional — enable when local RPC available) ──
            // DISABLED: vault refresh RPC causes 20s+ blocking in executor hot path.
            // Reserves come from delta tracker (shreds) + xdex-refresh (background).
            let vault_refresh_enabled = false;
            let t_refresh = Instant::now();
            if vault_refresh_enabled {
                // Collect vault keys for all pools in the route.
                let mut vault_keys: Vec<solana_sdk::pubkey::Pubkey> = Vec::new();
                let mut vault_map: Vec<(solana_sdk::pubkey::Pubkey, bool)> = Vec::new();
                let mut pumpswap_pools: Vec<solana_sdk::pubkey::Pubkey> = Vec::new();

                for hop in &bundle.route.hops {
                    if let Some(pool) = pool_cache.get(&hop.pool) {
                        let mut has_vaults = false;
                        if let Some(meta) = &pool.raydium_meta {
                            if meta.vault_a != solana_sdk::pubkey::Pubkey::default() {
                                vault_keys.push(meta.vault_a);
                                vault_map.push((hop.pool, true));
                                vault_keys.push(meta.vault_b);
                                vault_map.push((hop.pool, false));
                                has_vaults = true;
                            }
                        }
                        if let Some(meta) = &pool.orca_meta {
                            if meta.token_vault_a != solana_sdk::pubkey::Pubkey::default() {
                                vault_keys.push(meta.token_vault_a);
                                vault_map.push((hop.pool, true));
                                vault_keys.push(meta.token_vault_b);
                                vault_map.push((hop.pool, false));
                                has_vaults = true;
                            }
                        }
                        if let Some(meta) = &pool.meteora_meta {
                            if meta.token_vault_a != solana_sdk::pubkey::Pubkey::default() {
                                vault_keys.push(meta.token_vault_a);
                                vault_map.push((hop.pool, true));
                                vault_keys.push(meta.token_vault_b);
                                vault_map.push((hop.pool, false));
                                has_vaults = true;
                            }
                        }
                        // PumpSwap pools: need to read pool account for vault addresses.
                        if !has_vaults && pool.dex_type == market_engine::types::DexType::PumpSwap {
                            pumpswap_pools.push(hop.pool);
                        }
                    }
                }

                // PumpSwap: fetch pool accounts → extract vaults → fetch vault balances.
                if !pumpswap_pools.is_empty() {
                    if let Ok(accs) = vault_pool.get_multiple_accounts(&pumpswap_pools).ok_or(anyhow::anyhow!("pool failed")) {
                        for (i, maybe_acc) in accs.iter().enumerate() {
                            if let Some(acc) = maybe_acc {
                                if acc.data.len() >= 203 {
                                    let va = solana_sdk::pubkey::Pubkey::try_from(&acc.data[139..171]).unwrap_or_default();
                                    let vb = solana_sdk::pubkey::Pubkey::try_from(&acc.data[171..203]).unwrap_or_default();
                                    if va != solana_sdk::pubkey::Pubkey::default() {
                                        vault_keys.push(va);
                                        vault_map.push((pumpswap_pools[i], true));
                                        vault_keys.push(vb);
                                        vault_map.push((pumpswap_pools[i], false));
                                    }
                                }
                            }
                        }
                    }
                }

                // Single batch RPC call: getMultipleAccounts for all vaults.
                if !vault_keys.is_empty() {
                    if let Ok(accounts) = vault_pool.get_multiple_accounts(&vault_keys).ok_or(anyhow::anyhow!("pool failed")) {
                        for (i, maybe_acc) in accounts.iter().enumerate() {
                            if let Some(acc) = maybe_acc {
                                if acc.data.len() >= 72 {
                                    let amount = u64::from_le_bytes(
                                        acc.data[64..72].try_into().unwrap_or([0u8; 8]),
                                    );
                                    let (pool_addr, is_a) = vault_map[i];
                                    pool_cache.update_reserve_by_vault(&pool_addr, is_a, amount);
                                }
                            }
                        }
                    }
                }
            }
            let refresh_ms = t_refresh.elapsed().as_millis();

            // ── Pilar 4: Staleness check ──
            // If vault RPC refresh succeeded, reserves are fresh → re-quote is accurate.
            // If it failed (429/timeout), we still re-quote with cached reserves.
            // The re-quote below will filter truly unprofitable routes either way.
            let reserves_fresh = bundle.route.hops.iter().any(|h| {
                pool_cache.get(&h.pool).map_or(false, |p| p.last_updated.elapsed().as_secs() < 5)
            });

            // ── Re-quote route with LIVE reserves ──
            let mut fresh_route = bundle.route.clone();
            let mut current_in = fresh_route.borrow_amount;
            let mut route_still_profitable = true;
            for hop in &mut fresh_route.hops {
                if let Some(pool) = pool_cache.get(&hop.pool) {
                    let fresh_out = if hop.token_in == pool.token_a {
                        pool.quote_a_to_b(current_in)
                    } else {
                        pool.quote_b_to_a(current_in)
                    };
                    if fresh_out == 0 {
                        route_still_profitable = false;
                        break;
                    }
                    hop.amount_out = fresh_out;
                    current_in = fresh_out;
                }
            }
            // With JITO_ONLY=true, let on-chain decide profitability.
            // Re-quote with stale reserves is unreliable — if the route engine found
            // it profitable, send it. Failed TX = 0 cost via Jito bundle.
            let fresh_net = if route_still_profitable {
                let fresh_final = fresh_route.hops.last().map(|h| h.amount_out).unwrap_or(0);
                if fresh_final > fresh_route.borrow_amount {
                    fresh_final as i64 - fresh_route.borrow_amount as i64 - 10_000
                } else {
                    bundle.route.net_profit // use original estimate
                }
            } else {
                bundle.route.net_profit // use original estimate from route engine
            };
            if fresh_net < min_profit {
                route_dedup.insert(route_key, (now, Duration::from_secs(1), prev_failures + 1));
                continue;
            }
            // Flash loans are atomic: failed TX = 0 cost (only lost Jito tip ~10K lamports).
            // With 210ms latency, any margin check >0.1% filters out all real opportunities.
            // Data: 1323 executions with 0.5-2% min_pct → 0 landed. Spreads evaporate in <50ms.
            // Strategy: send everything with positive re-quoted profit. Let on-chain profit
            // check (min_profit=10K lamports) be the only guard. Cost of miss >> cost of failed TX.
            if fresh_net < min_profit {
                continue; // Only filter truly unprofitable routes
            }
            // Update route profits with fresh values.
            fresh_route.gross_profit = fresh_net.max(0) as u64;
            fresh_route.net_profit = fresh_net;

            info!(
                id = bundle.id,
                refresh_ms,
                reserves_fresh,
                fresh_net,
                old_net = bundle.route.net_profit,
                route = ?route_pools,
                "FAST_ARB: re-quoted — route profitable"
            );

            // ── Blockhash: shred-derived (0 RPC, 0 latency) → cache fallback ──
            let blockhash = if let Some(ref bh_deriver) = shred_blockhash {
                match bh_deriver.get_blockhash(bundle.slot, 150) {
                    Some((bh, _)) => bh,
                    None => *cached_blockhash.read(), // in-memory cache, ~0ns
                }
            } else {
                *cached_blockhash.read()
            };
            if blockhash == solana_sdk::hash::Hash::default() {
                continue; // no blockhash yet
            }

            // Single provider: MarginFi (0% fee, best rates).
            let provider = FlashProvider::MarginFi;
            let alts = alt_cache.all();
            // Dynamic tip: aggressive for competitive landing.
            // Jito docs recommend: sendBundle = 100% tip competition.
            // Scale tip with profit: low profit → p75 floor, high profit → up to 30% of profit.
            // 30% of profit as tip = we keep 70% = still very profitable.
            let p75_floor = sender.current_tip();
            let net = fresh_route.net_profit.max(0) as u64;
            // Dynamic tip based on route tier and strategy.
            // Graduation arb: 30% tip — proven profitable (4/4 confirmed TXs), aggressive landing.
            // Higher tier confidence → more aggressive tip for competitive landing.
            let tip_pct: u64 = if fresh_route.strategy == "pump_graduation_arb" {
                30  // Graduation arb: aggressive tip, proven profitable
            } else {
                match fresh_route.tier {
                    1 => 15,  // Raydium-only: moderate tip, good odds
                    2 => 15,  // Raydium+Meteora: moderate
                    3 => 25,  // PumpSwap graduation: aggressive tip, high conviction
                    4 => 10,  // Orca with liquidity: conservative, lower odds
                    _ => 10,  // Default conservative
                }
            };
            let profit_tip = net.saturating_mul(tip_pct) / 100;
            let tip_lamports = p75_floor.max(profit_tip).min(2_000_000); // cap at 2M lamports (0.002 SOL)
            let tip_ix = solana_sdk::system_instruction::transfer(
                &builder.payer.pubkey(),
                &sender.tip_account(),
                tip_lamports,
            );
            let fast_token_progs = builder.token_programs_for_route(&fresh_route);
            let t_build = Instant::now();

            // Try DIRECT_SWAP first (no on-chain program, raw DEX swaps, fewer CUs).
            // If unsupported DEX → fallback to CPI with min_profit=0 (no InsufficientProfit).
            let tx = match builder.build_direct_swap_tx(&fresh_route, provider, blockhash, &alts, &pool_cache, Some(tip_ix.clone())) {
                Ok(tx) => {
                    info!("DIRECT_SWAP: built (raw DEX, no CPI)");
                    tx
                }
                Err(e) => {
                    info!(error = %e, "DIRECT_SWAP: fallback CPI (min_profit=0)");
                    match builder.build_with_tip_cached(&fresh_route, provider, blockhash, &alts, &pool_cache, tip_ix.clone(), &fast_token_progs) {
                        Ok(tx) => tx,
                        Err(e2) => {
                            info!(error = %e2, "CPI build also failed, skip");
                            route_dedup.insert(route_key, (now, Duration::from_secs(2), prev_failures));
                            continue;
                        }
                    }
                }
            };
            let build_ms = t_build.elapsed().as_millis();

            // Pre-serialize TX once (reused for size check + TPU send).
            let tx_bytes = bincode::serialize(&tx).unwrap_or_default();
            let tx_size = tx_bytes.len();
            if tx_size > 1232 {
                debug!(tx_size, "FAST_ARB: TX oversized, skipping (JITO_ONLY requires ≤1232 bytes)");
                route_dedup.insert(route_key, (now, Duration::from_secs(2), prev_failures + 1));
                continue; // Skip — don't fall to slow path which uses sendTransaction
            } else {
                // BLIND SEND: skip simulation, send immediately.
                // Flash loans are atomic — failed TX = 0 SOL cost (only Jito tip ~10K lamports).
                // Data: 1323 simulations, 0 passed. Simulation adds 70-150ms latency that
                // guarantees the spread closes before our TX lands. The on-chain min_profit
                // check (10K lamports) is the real safety net.
                // EV calculation: 2% landing rate * 0.005 SOL avg profit = 0.0001 SOL/attempt
                // vs 10K lamports tip/attempt = 0.00001 SOL/attempt. EV positive by 10x.

                let total_ms = t_recv.elapsed().as_millis();
                info!(
                    id = bundle.id,
                    provider = ?provider,
                    detection_ms,
                    refresh_ms,
                    build_ms,
                    tx_size,
                    tip_lamports,
                    total_ms,
                    fresh_net = fresh_net,
                    route = ?route_pools,
                    "FAST_ARB: BLIND SEND — skipping simulation"
                );

                // Fire-and-forget send. Arc avoids 4× clone (~40-100µs savings).
                let tx = Arc::new(tx);
                let tpu_ref = tpu.clone();
                let jet_sender_ref = jet.clone();
                let sender_task = Arc::clone(&sender);
                let rebate_ref = Arc::clone(&helius_rebate);
                let fast_ref = Arc::clone(&helius_fast);
                let tg_task = tg.clone();
                let current_slot = bundle.slot;
                let detected_at = bundle.detected_at;
                let payer_pubkey = builder.payer.pubkey().to_string();
                let exec_rpc_for_balance = exec_rpc_url.clone();
                let net = bundle.route.net_profit;
                let hops = bundle.route.hops.len();
                let strategy = bundle.route.strategy;
                let dex_venues_str: String = bundle.route.hops.iter()
                    .map(|h| {
                        let prog = h.dex_program.to_string();
                        if prog.starts_with("675k") { "RaydiumV4" }
                        else if prog.starts_with("whir") { "Orca" }
                        else if prog.starts_with("LBU") { "Meteora" }
                        else if prog.starts_with("pAMM") { "PumpSwap" }
                        else if prog.starts_with("FLUX") { "FluxBeam" }
                        else if prog.starts_with("SSwp") { "Saber" }
                        else if prog.starts_with("CPMM") { "CPMM" }
                        else { "DEX" }
                    })
                    .collect::<Vec<_>>()
                    .join("→");

                let tx_sig = tx.signatures.first().map(|s| s.to_string()).unwrap_or_default();

                // ── PHASE 1: JetSender only (gRPC live leader routing) ──
                let jito_only = jito_only_enabled();
                let tpu_sent = if !jito_only {
                    if let Some(ref jet) = jet_sender_ref {
                        jet.send(&tx)
                    } else {
                        0
                    }
                } else {
                    0
                };

                let fast_send_ms = t_recv.elapsed().as_millis();
                info!(
                    tx_sig = %tx_sig,
                    tpu_sent,
                    detection_ms,
                    build_ms,
                    fast_send_ms,
                    "FAST_ARB[on-wire]: TPU+Jito parallel"
                );

                // Log execution to PostgreSQL
                if let Some(ref logger) = strategy_logger {
                    logger.log_execution(strategy_logger::types::LogExecution {
                        opp_id: bundle.id,
                        tx_signature: Some(tx_sig.clone()),
                        send_channels: vec!["TPU".into(), "Agave".into(), "Jito".into()],
                        fast_arb: true,
                        detect_to_build_us: Some(build_ms as u64 * 1000),
                        build_to_send_us: Some((fast_send_ms - build_ms) as u64 * 1000),
                        total_us: Some(fast_send_ms as u64 * 1000),
                        jito_tip: Some(tip_lamports),
                        tpu_leaders: Some(tpu_sent as i16),
                    });
                }

                // ── PHASE 2: Slow channels in background — don't block ──
                let tx_sig_poll = tx_sig.clone();
                let exec_rpc_poll = exec_rpc_url.clone();
                let logger_phase2 = strategy_logger.clone();
                let ml_phase2 = ml_scorer.clone();
                let route_pools_ml: Vec<String> = bundle.route.hops.iter().map(|h| h.pool.to_string()).collect();
                let route_strategy_ml = bundle.route.strategy.to_string();
                let dedup_ref = Arc::clone(&route_dedup);
                let dedup_key = route_key;
                let dedup_failures = prev_failures;
                let score = bundle.score;

                let tx_for_turbo = Arc::clone(&tx);
                let tx_for_jito_fallback = Arc::clone(&tx);
                rt.spawn(async move {
                    // TURBO SENDER: multi-path parallel fanout to 10+ endpoints.
                    // Replaces manual 5-channel code with adaptive routing.
                    // Jito bundles (0-cost on failure) + Helius staked + Atlas + public RPCs.
                    let turbo = turbo_sender::TurboSender::new();
                    match turbo.send_turbo(&tx_for_turbo, tip_lamports).await {
                        Ok((sig, sent_count)) => {
                            info!(
                                tx_sig = %sig,
                                endpoints = sent_count,
                                fast_send_ms,
                                "TURBO[phase2]: {} endpoints fired",
                                sent_count
                            );
                        }
                        Err(e) => {
                            warn!(error = %e, "TURBO: send failed, falling back to Jito only");
                            // Fallback: Jito bundle only
                            let jito_bundle = [(*tx_for_jito_fallback).clone()];
                            let _ = sender_task.send_bundle_multi_region(&jito_bundle).await;
                        }
                    }

                    // TurboSender handles all endpoints — skip to Phase 3 polling.

                    // ── PHASE 3: Poll TX status ──
                    let poll_client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(2))
                        .build()
                        .unwrap_or_default();

                    for i in 0..20 {
                        tokio::time::sleep(Duration::from_millis(200)).await;

                        // Poll TX signature via RPC.
                        let body = serde_json::json!({
                            "jsonrpc": "2.0", "id": 1,
                            "method": "getSignatureStatuses",
                            "params": [[&tx_sig_poll], {"searchTransactionHistory": false}]
                        });
                        if let Ok(resp) = poll_client.post(&exec_rpc_poll).json(&body).send().await {
                            if let Ok(json) = resp.json::<serde_json::Value>().await {
                                if let Some(status) = json["result"]["value"][0].as_object() {
                                    if status.get("err").map_or(false, |e| !e.is_null()) {
                                        // Failure backoff: exponential cooldown for this route.
                                        let f = dedup_failures + 1;
                                        let backoff_secs = 10u64.saturating_mul(2u64.pow(f.min(5)));
                                        let backoff = std::time::Duration::from_secs(backoff_secs.min(300));
                                        dedup_ref.insert(dedup_key, (std::time::Instant::now(), backoff, f));
                                        warn!(
                                            tx_sig = %tx_sig_poll,
                                            err = ?status["err"],
                                            backoff_s = backoff_secs.min(300),
                                            failures = f,
                                            "FAST_ARB: TX failed on-chain"
                                        );
                                        // ML: record failure
                                        if let Some(ref ml) = ml_phase2 {
                                            for p in &route_pools_ml {
                                                ml.observe(p, "", &route_strategy_ml, false, 0);
                                            }
                                        }
                                        if let Some(ref lg) = logger_phase2 {
                                            lg.update_execution(strategy_logger::types::LogExecutionUpdate {
                                                tx_signature: tx_sig_poll.clone(),
                                                landed: false,
                                                landed_slot: None,
                                                actual_profit: None,
                                                error_message: Some(format!("{:?}", status["err"])),
                                            });
                                        }
                                        // Only send Telegram for on-chain failures (not sim failures)
                                        // to avoid spamming 1000+ alerts/hour.
                                        return;
                                    }
                                    if status.get("confirmationStatus").and_then(|s| s.as_str())
                                        .map_or(false, |s| s == "confirmed" || s == "finalized")
                                    {
                                        let landed_slot_val = status.get("slot").and_then(|s| s.as_u64());
                                        info!(
                                            tx_sig = %tx_sig_poll,
                                            end_to_end_ms = detected_at.elapsed().as_millis(),
                                            expected_profit = net,
                                            "FAST_ARB LANDED"
                                        );
                                        let sol_profit = net as f64 / 1e9;
                                        tg_task.alert_trade_executed(sol_profit, net, hops, strategy, &tx_sig_poll).await;
                                        if matches!(strategy, "pump_graduation_arb" | "cyclic_3leg" | "dlmm_whirlpool_arb") {
                                            tg_task.alert_new_strategy_landed(strategy, net, hops, &dex_venues_str, &tx_sig_poll).await;
                                        }
                                        tg_task.alert_wallet_balance(&exec_rpc_for_balance, &payer_pubkey, net).await;
                                        // ML: record success
                                        if let Some(ref ml) = ml_phase2 {
                                            for p in &route_pools_ml {
                                                ml.observe(p, "", &route_strategy_ml, true, net);
                                            }
                                        }
                                        if let Some(ref lg) = logger_phase2 {
                                            lg.update_execution(strategy_logger::types::LogExecutionUpdate {
                                                tx_signature: tx_sig_poll.clone(),
                                                landed: true,
                                                landed_slot: landed_slot_val,
                                                actual_profit: Some(net),
                                                error_message: None,
                                            });
                                            for p in &route_pools_ml {
                                                lg.update_pool_perf(strategy_logger::types::LogPoolPerfUpdate {
                                                    pool_address: p.clone(),
                                                    dex_type: route_strategy_ml.clone(),
                                                    landed: true,
                                                    profit: net,
                                                    score: score as f64,
                                                });
                                            }
                                        }
                                        return;
                                    }
                                }
                            }
                        }

                        // Jito bundle status check (skip — TurboSender handles this)
                        if false {
                            if let Some(ref bid) = Option::<String>::None {
                                if let Ok(status) = sender_task.get_bundle_status(bid).await {
                                    if status.is_landed() {
                                        let landed_slot_val = status.landed_slot;
                                        info!(bundle_id = %bid, end_to_end_ms = detected_at.elapsed().as_millis(), "FAST_ARB LANDED (Jito)");
                                        let sol_profit = net as f64 / 1e9;
                                        tg_task.alert_trade_executed(sol_profit, net, hops, strategy, bid).await;
                                        if matches!(strategy, "pump_graduation_arb" | "cyclic_3leg" | "dlmm_whirlpool_arb") {
                                            tg_task.alert_new_strategy_landed(strategy, net, hops, &dex_venues_str, bid).await;
                                        }
                                        tg_task.alert_wallet_balance(&exec_rpc_for_balance, &payer_pubkey, net).await;
                                        // ML: record success (Jito path)
                                        if let Some(ref ml) = ml_phase2 {
                                            for p in &route_pools_ml {
                                                ml.observe(p, "", &route_strategy_ml, true, net);
                                            }
                                        }
                                        if let Some(ref lg) = logger_phase2 {
                                            lg.update_execution(strategy_logger::types::LogExecutionUpdate {
                                                tx_signature: tx_sig_poll.clone(),
                                                landed: true,
                                                landed_slot: landed_slot_val,
                                                actual_profit: Some(net),
                                                error_message: None,
                                            });
                                            for p in &route_pools_ml {
                                                lg.update_pool_perf(strategy_logger::types::LogPoolPerfUpdate {
                                                    pool_address: p.clone(),
                                                    dex_type: route_strategy_ml.clone(),
                                                    landed: true,
                                                    profit: net,
                                                    score: score as f64,
                                                });
                                            }
                                        }
                                        return;
                                    }
                                    if status.is_failed() {
                                        // ML: record Jito failure
                                        if let Some(ref ml) = ml_phase2 {
                                            for p in &route_pools_ml {
                                                ml.observe(p, "", &route_strategy_ml, false, 0);
                                            }
                                        }
                                        if let Some(ref lg) = logger_phase2 {
                                            lg.update_execution(strategy_logger::types::LogExecutionUpdate {
                                                tx_signature: tx_sig_poll.clone(),
                                                landed: false,
                                                landed_slot: None,
                                                actual_profit: None,
                                                error_message: Some("Jito bundle failed".into()),
                                            });
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    info!(tx_sig = %tx_sig_poll, "FAST_ARB: poll timeout — TX expired");
                    // ML: record as failure (TX never confirmed)
                    if let Some(ref ml) = ml_phase2 {
                        for p in &route_pools_ml {
                            ml.observe(p, "", &route_strategy_ml, false, 0);
                        }
                    }
                    if let Some(ref lg) = logger_phase2 {
                        lg.update_execution(strategy_logger::types::LogExecutionUpdate {
                            tx_signature: tx_sig_poll.clone(),
                            landed: false,
                            landed_slot: None,
                            actual_profit: None,
                            error_message: Some("poll timeout — TX expired".into()),
                        });
                    }
                });

                continue; // Skip the slow path.
            }
        }

        // ── T1: blockhash (from in-process cache ≈ 0µs) ────────────────
        let t_bh = Instant::now();
        let blockhash = *cached_blockhash.read();
        if blockhash == solana_sdk::hash::Hash::default() {
            error!("no cached blockhash available");
            continue;
        }
        info!(
            blockhash_us = t_bh.elapsed().as_micros(),
            "blockhash from cache"
        );

        // ── T2: ATA check (con cache local para evitar rate limit) ────────────
        let t_ata = Instant::now();
        let mut alts = alt_cache.all();
        if let Err(e) =
            ensure_route_atas(&rpc, &builder, &bundle, blockhash, dry_run, &mut known_atas)
        {
            // ATA creation can fail for Token-2022 tokens (wrong program ID) or transient
            // RPC errors. Don't block the route — proceed to simulation which will catch
            // any real account issues with a clear error.
            warn!(error = ?e, "ATA setup failed, continuing to simulation");
        }
        let ata_ms = t_ata.elapsed().as_millis();
        if ata_ms > 5 {
            info!(ata_ms, "ATA check completed");
        }

        // ── T3: Build TX for every flash provider ────────────────────────────
        // Optimization: fetch token programs ONCE, then build all providers with cached progs.
        // Saves ~15-25ms per extra provider (eliminates redundant RPC calls).
        let t_build = Instant::now();
        let all_candidates = crate::flash_loans::fallback_candidates(bundle.flash_provider);
        let token_progs = builder.token_programs_for_route(&bundle.route);
        let mut tx_candidates: Vec<(
            FlashProvider,
            solana_sdk::transaction::VersionedTransaction,
            usize,
        )> = Vec::new();

        for provider in &all_candidates {
            match builder.build_for_simulation_cached(&bundle.route, *provider, blockhash, &alts, &pool_cache, &token_progs) {
                Err(e) => {
                    warn!(provider = ?provider, error = %e, "tx build failed for provider");
                }
                Ok(tx) => {
                    let sz = bincode::serialize(&tx).map(|b| b.len()).unwrap_or(0);
                    tx_candidates.push((*provider, tx, sz));
                }
            }
        }

        // If any TX exceeds the packet limit, create one dynamic ALT and rebuild.
        if tx_candidates.iter().any(|(_, _, sz)| *sz > 1232) {
            // Use the first oversized TX's static keys as the ALT seed.
            if let Some((prov, oversized_tx, sz)) =
                tx_candidates.iter().find(|(_, _, sz)| *sz > 1232)
            {
                let static_keys: Vec<Pubkey> = match &oversized_tx.message {
                    VersionedMessage::V0(msg) => msg.account_keys.clone(),
                    VersionedMessage::Legacy(msg) => msg.account_keys.clone(),
                };
                warn!(
                    provider = ?prov,
                    tx_size = sz,
                    keys = static_keys.len(),
                    "oversized tx – creating/reusing dynamic ALT"
                );
                // Non-blocking: instant on cache hit, spawns background creation on miss.
                match alt_cache.try_get_or_spawn_create(&builder.payer, static_keys) {
                    Ok(alt_pubkey) => {
                        info!(%alt_pubkey, "dynamic ALT ready");
                        alts = alt_cache.all();
                        // Rebuild all providers with cached token progs + updated ALTs.
                        tx_candidates.clear();
                        for provider in &all_candidates {
                            match builder.build_for_simulation_cached(
                                &bundle.route,
                                *provider,
                                blockhash,
                                &alts,
                                &pool_cache,
                                &token_progs,
                            ) {
                                Err(e) => {
                                    warn!(provider = ?provider, error = %e, "tx rebuild failed");
                                }
                                Ok(tx) => {
                                    let sz = bincode::serialize(&tx).map(|b| b.len()).unwrap_or(0);
                                    tx_candidates.push((*provider, tx, sz));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "dynamic ALT creation failed");
                    }
                }
            }
        }

        // Drop any providers that are still oversized after ALT.
        tx_candidates.retain(|(prov, _, sz)| {
            if *sz > 1232 {
                warn!(provider = ?prov, tx_size = sz, "tx still oversized after ALT, skipping");
                false
            } else {
                true
            }
        });

        if tx_candidates.is_empty() {
            let build_ms = t_build.elapsed().as_millis();
            warn!(
                id = bundle.id,
                detection_ms,
                build_ms,
                total_ms = t_recv.elapsed().as_millis(),
                route = ?route_pools,
                "MISS[build]: no valid TXs — blacklisted 5min"
            );
            route_dedup.insert(route_key, (now, Duration::from_secs(300), prev_failures + 1));
            continue;
        }

        let build_ms = t_build.elapsed().as_millis();
        info!(
            providers = tx_candidates.len(),
            alt_count = alts.len(),
            build_ms,
            t_detect_to_build_ms = detection_ms + build_ms,
            "T2→T3: TX built, simulating all providers in parallel"
        );

        // ── T3b: In-process preflight (0-5µs, no network) ────────────────────
        // Runs before simulation to reject structurally invalid TXs instantly.
        // If all candidates fail preflight, we skip the simulation RPC entirely.
        tx_candidates.retain(|(provider, tx, _sz)| {
            let pf = crate::preflight::check(tx, &bundle.route, min_profit);
            if !pf.is_ok() {
                if let crate::preflight::PreflightResult::Rejected(reason) = pf {
                    warn!(provider = ?provider, reason, "preflight reject");
                }
                return false;
            }
            true
        });

        if tx_candidates.is_empty() {
            warn!(id = bundle.id, "all providers failed in-process preflight");
            continue;
        }

        // ── T4: Parallel simulation (top-3 providers simultaneously) ────────
        // Simulate up to 3 providers in parallel using thread::scope.
        // Takes first success. Saves 80-150ms when first provider fails.
        let t_sim = Instant::now();
        let mut winner: Option<(FlashProvider, solana_sdk::transaction::VersionedTransaction, crate::simulator::SimulationResult)> = None;

        // Split: simulate first 3 in parallel, rest sequentially if all fail.
        let parallel_count = tx_candidates.len().min(3);
        let (parallel_batch, sequential_batch): (Vec<_>, Vec<_>) = {
            let mut iter = tx_candidates.into_iter();
            let par: Vec<_> = iter.by_ref().take(parallel_count).collect();
            let seq: Vec<_> = iter.collect();
            (par, seq)
        };

        if parallel_batch.len() >= 2 {
            // Simulate top providers in parallel.
            let sim_ref = &simulator;
            let route_pools_ref = &route_pools;
            let results: Vec<Option<(FlashProvider, solana_sdk::transaction::VersionedTransaction, crate::simulator::SimulationResult)>> =
                std::thread::scope(|s| {
                    let handles: Vec<_> = parallel_batch
                        .into_iter()
                        .map(|(provider, tx, tx_size)| {
                            s.spawn(move || {
                                let t0 = Instant::now();
                                info!(provider = ?provider, tx_size, "simulating (parallel)");
                                match sim_ref.simulate(&tx) {
                                    Ok(result) if result.success => {
                                        info!(
                                            provider = ?provider,
                                            sim_us = t0.elapsed().as_micros(),
                                            "simulation PASSED"
                                        );
                                        Some((provider, tx, result))
                                    }
                                    Ok(result) => {
                                        warn!(
                                            provider = ?provider,
                                            error = ?result.error,
                                            sim_us = t0.elapsed().as_micros(),
                                            route = ?route_pools_ref,
                                            "MISS[sim]: simulation rejected"
                                        );
                                        None
                                    }
                                    Err(e) => {
                                        warn!(
                                            provider = ?provider,
                                            sim_us = t0.elapsed().as_micros(),
                                            error = %e,
                                            "simulation RPC error"
                                        );
                                        None
                                    }
                                }
                            })
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().ok().flatten()).collect()
                });
            // Take first success (preserving provider preference order).
            for r in results {
                if r.is_some() {
                    winner = r;
                    break;
                }
            }
        } else if let Some((provider, tx, tx_size)) = parallel_batch.into_iter().next() {
            // Only 1 candidate — simulate directly.
            let t0 = Instant::now();
            info!(provider = ?provider, tx_size, "simulating");
            match simulator.simulate(&tx) {
                Ok(result) if result.success => {
                    info!(provider = ?provider, sim_us = t0.elapsed().as_micros(), "simulation PASSED");
                    winner = Some((provider, tx, result));
                }
                Ok(result) => {
                    warn!(provider = ?provider, error = ?result.error, sim_us = t0.elapsed().as_micros(), "MISS[sim]");
                }
                Err(e) => {
                    warn!(provider = ?provider, sim_us = t0.elapsed().as_micros(), error = %e, "sim RPC error");
                }
            }
        }

        // If parallel batch didn't produce a winner, try remaining sequentially.
        if winner.is_none() {
            for (provider, tx, tx_size) in sequential_batch {
                let t0 = Instant::now();
                info!(provider = ?provider, tx_size, "simulating (sequential fallback)");
                match simulator.simulate(&tx) {
                    Ok(result) if result.success => {
                        info!(provider = ?provider, sim_us = t0.elapsed().as_micros(), "simulation PASSED");
                        winner = Some((provider, tx, result));
                        break;
                    }
                    Ok(result) => {
                        warn!(
                            provider = ?provider,
                            error = ?result.error,
                            sim_us = t0.elapsed().as_micros(),
                            route = ?route_pools,
                            "MISS[sim]: simulation rejected, trying next provider"
                        );
                    }
                    Err(e) => {
                        warn!(provider = ?provider, sim_us = t0.elapsed().as_micros(), error = %e, "simulation RPC error");
                    }
                }
            }
        }
        let sim_ms = t_sim.elapsed().as_millis();

        let (provider, _sim_tx, sim_result) = match winner {
            Some(w) => w,
            None => {
                warn!(
                    id = bundle.id,
                    detection_ms,
                    build_ms,
                    sim_ms,
                    total_ms = t_recv.elapsed().as_millis(),
                    route = ?route_pools,
                    "MISS[sim-all]: all flash loan providers failed simulation — blacklisted 5min"
                );
                route_dedup.insert(route_key, (now, Duration::from_secs(300), prev_failures + 1));
                continue;
            }
        };

        info!(
            provider = ?provider,
            units   = sim_result.units_consumed,
            profit  = ?sim_result.estimated_profit,
            detection_ms,
            build_ms,
            sim_ms,
            t_detect_to_sim_ms = detection_ms + build_ms + sim_ms,
            route = ?route_pools,
            "HIT[sim]: simulation succeeded → rebuilding with real slippage"
        );

        // Rebuild with real slippage (min_out > 1) + Jito tip for the actual send.
        let tip_ix = sender.tip_instruction(&builder.payer.pubkey());
        let tx = match builder.build_with_tip(&bundle.route, provider, blockhash, &alts, &pool_cache, tip_ix) {
            Ok(tx) => tx,
            Err(e) => {
                warn!(provider = ?provider, error = %e, "rebuild with real slippage failed");
                continue;
            }
        };
        let provider_str = format!("{:?}", provider);
        let units = sim_result.units_consumed;
        let net = bundle.route.net_profit;
        let hops = bundle.route.hops.len();
        let tg_c = tg.clone();
        rt.spawn(async move {
            tg_c.alert_simulation_succeeded(&provider_str, hops, net, units, dry_run)
                .await;
        });
        if dry_run {
            flip_to_live(&rt, &tg, &format!("{:?}", provider), net);
        }

        let mut executed = false;

        if dry_run {
            let total_ms = t_recv.elapsed().as_millis();
            info!(
                id = bundle.id,
                provider = ?provider,
                detection_ms,
                build_ms,
                sim_ms,
                total_ms,
                "DRY RUN – would send bundle"
            );
            executed = true;
        } else {
            // ── T5: Send — fire-and-forget tokio task ─────────────────────────
            // Both TPU and Jito run inside an async task so the executor loop
            // is freed immediately to process the next opportunity.
            let t_send_log = Instant::now();
            let total_ms_pre = t_recv.elapsed().as_millis();
            info!(
                provider = ?provider,
                detection_ms,
                build_ms,
                sim_ms,
                total_ms = total_ms_pre,
                "submitting bundle (Jito + TPU fanout) — non-blocking"
            );

            let tx_for_tpu = tx.clone();
            let tx_for_rebate = tx.clone();
            let tx_for_fast = tx.clone();
            let tx_for_agave = tx.clone();
            let tpu_ref = tpu.clone();
            let sender_task = Arc::clone(&sender);
            let rebate_ref = Arc::clone(&helius_rebate);
            let fast_ref = Arc::clone(&helius_fast);
            let tg_task = tg.clone();
            let current_slot = bundle.slot;
            let detected_at = bundle.detected_at;
            let payer_pubkey = builder.payer.pubkey().to_string();
            let exec_rpc_for_balance = exec_rpc_url.clone();
            let strategy = bundle.route.strategy;
            let slow_dex_venues: String = bundle.route.hops.iter()
                .map(|h| {
                    let prog = h.dex_program.to_string();
                    if prog.starts_with("675k") { "RaydiumV4" }
                    else if prog.starts_with("whir") { "Orca" }
                    else if prog.starts_with("LBU") { "Meteora" }
                    else if prog.starts_with("pAMM") { "PumpSwap" }
                    else if prog.starts_with("FLUX") { "FluxBeam" }
                    else if prog.starts_with("SSwp") { "Saber" }
                    else if prog.starts_with("CPMM") { "CPMM" }
                    else { "DEX" }
                })
                .collect::<Vec<_>>()
                .join("→");
            let logger_slow = strategy_logger.clone();
            let ml_slow = ml_scorer.clone();
            let slow_pools: Vec<String> = bundle.route.hops.iter().map(|h| h.pool.to_string()).collect();
            let slow_strategy = bundle.route.strategy.to_string();
            let slow_opp_id = bundle.id;
            let slow_score = bundle.score;

            // Log execution to PG (slow path)
            if let Some(ref logger) = strategy_logger {
                logger.log_execution(strategy_logger::types::LogExecution {
                    opp_id: bundle.id,
                    tx_signature: None, // Jito bundle_id used instead
                    send_channels: vec!["TPU".into(), "Jito".into(), "Agave".into(), "Helius".into()],
                    fast_arb: false,
                    detect_to_build_us: Some(build_ms as u64 * 1000),
                    build_to_send_us: Some(sim_ms as u64 * 1000),
                    total_us: Some(total_ms_pre as u64 * 1000),
                    jito_tip: None,
                    tpu_leaders: None,
                });
            }

            rt.spawn(async move {
                let t_send = Instant::now();
                let jito_only = jito_only_enabled();

                // TPU fanout — SKIP in JITO_ONLY mode (charges fees on failed TXs).
                let tpu_handle = if !jito_only {
                    Some(tokio::task::spawn_blocking(move || -> usize {
                        if let Some(tpu_sender) = tpu_ref {
                            tpu_sender.send(&tx_for_tpu, current_slot)
                        } else {
                            0
                        }
                    }))
                } else {
                    None
                };

                // Jito bundle send to ALL regions — ALWAYS (0-cost on failure).
                let jito_result = sender_task.send_bundle_multi_region(&[tx]).await;

                if !jito_only {
                    // Helius backrun rebate (50% MEV rebate).
                    let rebate_result = rebate_ref.send(&tx_for_rebate).await;
                    match &rebate_result {
                        Ok(sig) => info!(sig, "Helius rebate TX submitted"),
                        Err(e) => {
                            let err_str = e.to_string();
                            if !err_str.contains("not enabled") {
                                tracing::debug!(error = %e, "Helius rebate send failed");
                            }
                        }
                    }

                    // Helius Atlas fast sender.
                    let fast_result = fast_ref.send(&tx_for_fast).await;
                    match &fast_result {
                        Ok(sig) => info!(sig, "Helius Atlas fast TX submitted"),
                        Err(e) => {
                            let err_str = e.to_string();
                            if !err_str.contains("not enabled") {
                                tracing::debug!(error = %e, "Helius fast send failed");
                            }
                        }
                    }

                    // Agave local RPC sendTransaction (gossip propagation).
                    let agave_result = agave_send(&tx_for_agave).await;
                    match &agave_result {
                        Ok(sig) => info!(sig = %sig, "Agave local sendTransaction OK"),
                    Err(e) => warn!(error = %e, "Agave local send failed"),
                    }
                } // end if !jito_only

                // Collect TPU result.
                let tpu_sent = if let Some(h) = tpu_handle { h.await.unwrap_or(0) } else { 0 };
                let send_ms = t_send.elapsed().as_millis();
                let _ = t_send_log; // suppress unused warning

                let bundle_id = match jito_result {
                    Ok(id) => {
                        let end_to_end_send_ms = detected_at.elapsed().as_millis();
                        info!(
                            provider = ?provider,
                            bundle_id = %id,
                            tpu_sent,
                            detection_ms,
                            build_ms,
                            sim_ms,
                            send_ms,
                            end_to_end_send_ms,  // T0→T5: shred detected → on wire
                            "T5[send]: bundle submitted (Jito + TPU fanout)"
                        );
                        id
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        // Distinguish: Jito 429 (not authorized) vs other errors.
                        if err_str.contains("429") || err_str.contains("rate limit") {
                            // TPU fanout still ran; this is expected until Jito auth
                            if tpu_sent > 0 {
                                info!(tpu_sent, "Jito 429 (unauth) but TPU sent — coverage maintained");
                            }
                        } else {
                            warn!(provider = ?provider, error = %e, tpu_sent, "Jito bundle send failed (TPU may have succeeded)");
                        }
                        return;
                    }
                };

                // ── Phase 3: Poll bundle status (non-blocking async sleep) ────
                for _ in 0..20 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    match sender_task.get_bundle_status(&bundle_id).await {
                        Ok(status) if status.is_landed() => {
                            let end_to_end_ms = detected_at.elapsed().as_millis();
                            info!(
                                provider = ?provider,
                                bundle_id   = %bundle_id,
                                landed_slot = ?status.landed_slot,
                                detection_ms,
                                build_ms,
                                sim_ms,
                                send_ms,
                                end_to_end_ms,  // shred detected → landed
                                expected_profit_lamports = net,
                                "✅ LANDED: bundle confirmed on-chain"
                            );
                            let sol_profit = net as f64 / 1e9;
                            tg_task.alert_trade_executed(sol_profit, net, hops, strategy, &bundle_id).await;
                            if matches!(strategy, "pump_graduation_arb" | "cyclic_3leg" | "dlmm_whirlpool_arb") {
                                tg_task.alert_new_strategy_landed(strategy, net, hops, &slow_dex_venues, &bundle_id).await;
                            }
                            tg_task.alert_wallet_balance(&exec_rpc_for_balance, &payer_pubkey, net).await;
                            // ML: record success (slow path)
                            if let Some(ref ml) = ml_slow {
                                for p in &slow_pools {
                                    ml.observe(p, "", &slow_strategy, true, net);
                                }
                            }
                            if let Some(ref lg) = logger_slow {
                                lg.update_execution(strategy_logger::types::LogExecutionUpdate {
                                    tx_signature: bundle_id.clone(),
                                    landed: true,
                                    landed_slot: status.landed_slot,
                                    actual_profit: Some(net),
                                    error_message: None,
                                });
                                for p in &slow_pools {
                                    lg.update_pool_perf(strategy_logger::types::LogPoolPerfUpdate {
                                        pool_address: p.clone(),
                                        dex_type: slow_strategy.clone(),
                                        landed: true,
                                        profit: net,
                                        score: slow_score as f64,
                                    });
                                }
                            }
                            break;
                        }
                        Ok(status) if status.is_failed() => {
                            warn!(
                                provider = ?provider,
                                bundle_id = %bundle_id,
                                status = ?status,
                                end_to_end_ms = detected_at.elapsed().as_millis(),
                                "MISS[land]: bundle FAILED on-chain (spread may have evaporated)"
                            );
                            // ML: record failure (slow path)
                            if let Some(ref ml) = ml_slow {
                                for p in &slow_pools {
                                    ml.observe(p, "", &slow_strategy, false, 0);
                                }
                            }
                            if let Some(ref lg) = logger_slow {
                                lg.update_execution(strategy_logger::types::LogExecutionUpdate {
                                    tx_signature: bundle_id.clone(),
                                    landed: false,
                                    landed_slot: None,
                                    actual_profit: None,
                                    error_message: Some(format!("{:?}", status)),
                                });
                            }
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!(provider = ?provider, error = %e, "bundle status check failed");
                        }
                    }
                }
            });

            executed = true;
        }

        if !executed {
            warn!(id = bundle.id, "opportunity not executed");
        }
    }

    error!("opportunity channel closed – executor shutting down");
}

fn ensure_route_atas(
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    builder: &TxBuilder,
    bundle: &OpportunityBundle,
    recent_blockhash: solana_sdk::hash::Hash,
    dry_run: bool,
    known_atas: &mut HashSet<Pubkey>,
) -> anyhow::Result<()> {
    let required = builder.required_token_accounts(&bundle.route);
    if required.is_empty() {
        return Ok(());
    }

    // Filtrar las ATAs que ya sabemos que existen (cache local, evita RPC rate limit).
    let unchecked: Vec<(Pubkey, Pubkey)> = required
        .iter()
        .filter(|(_, ata)| !known_atas.contains(ata))
        .copied()
        .collect();

    if unchecked.is_empty() {
        return Ok(());
    }

    let ata_keys: Vec<Pubkey> = unchecked.iter().map(|(_, ata)| *ata).collect();
    let accounts = rpc.get_multiple_accounts(&ata_keys)?;

    let mut missing: Vec<Pubkey> = Vec::new();
    for ((_, ata), maybe_account) in unchecked.iter().zip(accounts.iter()) {
        if maybe_account.is_some() {
            known_atas.insert(*ata); // marcar como existente en cache
        } else {
            missing.push(*ata);
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    warn!(
        missing = missing.len(),
        ?missing,
        "route requires missing ATAs"
    );
    if dry_run {
        return Ok(());
    }

    let Some(ata_tx) = builder.build_ata_setup_transaction(&bundle.route, recent_blockhash)? else {
        return Ok(());
    };
    // Non-blocking: send TX but don't wait for confirmation.
    // The ATA will be ready by the next opportunity using these mints.
    match rpc.send_transaction(&ata_tx) {
        Ok(sig) => {
            info!(%sig, created = missing.len(), "ATA creation TX sent (non-blocking)");
            // Optimistically add to cache — if TX fails, next check will re-create.
            for (_, ata) in &required {
                known_atas.insert(*ata);
            }
        }
        Err(e) => {
            warn!(error = %e, "ATA creation TX send failed");
        }
    }
    Ok(())
}

/// Flip DRY_RUN=false en /etc/helios/helios.env y reinicia el servicio.
/// Llamado una sola vez cuando la primera simulación exitosa ocurre en modo DRY_RUN.
fn flip_to_live(
    rt: &tokio::runtime::Runtime,
    tg: &crate::telegram::TelegramBot,
    provider: &str,
    net_profit: i64,
) {
    const ENV_FILE: &str = "/etc/helios/helios.env";

    // Actualizar el env file
    let flip_ok = match std::fs::read_to_string(ENV_FILE) {
        Ok(content) => {
            let updated = content.replace("DRY_RUN=true", "DRY_RUN=false");
            match std::fs::write(ENV_FILE, updated) {
                Ok(_) => {
                    info!("DRY_RUN → false written to {}", ENV_FILE);
                    true
                }
                Err(e) => {
                    error!(error = %e, "failed to write {} for live flip", ENV_FILE);
                    false
                }
            }
        }
        Err(e) => {
            error!(error = %e, "failed to read {} for live flip", ENV_FILE);
            false
        }
    };

    // Notificar vía Telegram
    let tg_c = tg.clone();
    let provider_owned = provider.to_string();
    rt.spawn(async move {
        tg_c.alert_going_live(&provider_owned, net_profit).await;
    });

    if !flip_ok {
        return;
    }

    // Reiniciar el servicio (non-blocking — systemd mata este proceso y arranca uno nuevo)
    info!("restarting helios-bot service to apply DRY_RUN=false...");
    if let Err(e) = std::process::Command::new("systemctl")
        .args(["restart", "helios-bot"])
        .spawn()
    {
        error!(error = %e, "failed to spawn systemctl restart");
    }
}

/// Send a transaction via the local Agave RPC (no rate limits, gossip propagation).
/// Send via Helius staked RPC (SWQoS priority — routes through staked validators).
/// skipPreflight + maxRetries=0 for minimum latency.
async fn helius_staked_send(tx: &solana_sdk::transaction::VersionedTransaction) -> anyhow::Result<String> {
    use anyhow::Context;
    let url = std::env::var("RPC_URL").unwrap_or_else(|_| "https://mainnet.helius-rpc.com".into());
    let encoded = bincode::serialize(tx).context("serialize")?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encoded);
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "sendTransaction",
        "params": [b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build().unwrap_or_default();
    let resp = client.post(&url).json(&body).send().await.context("helius POST")?;
    let json: serde_json::Value = resp.json().await.context("helius parse")?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("helius error: {}", err);
    }
    Ok(json["result"].as_str().unwrap_or("ok").to_string())
}

/// Uses AGAVE_RPC_URL env (default http://127.0.0.1:9000).
async fn agave_send(tx: &solana_sdk::transaction::VersionedTransaction) -> anyhow::Result<String> {
    use anyhow::Context;

    let url = std::env::var("AGAVE_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let encoded = bincode::serialize(tx).context("serialize tx for agave")?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encoded);

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default();

    let resp = client.post(&url).json(&body).send().await.context("agave POST")?;
    let json: serde_json::Value = resp.json().await.context("agave parse")?;

    if let Some(err) = json.get("error") {
        anyhow::bail!("agave RPC error: {}", err);
    }

    let sig = json["result"].as_str().unwrap_or("unknown").to_string();
    Ok(sig)
}
