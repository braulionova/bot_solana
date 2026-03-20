use anyhow::{anyhow, Context, Result};
use executor::tx_builder::TxBuilder;
use market_engine::{
    pool_hydrator::{load_mapped_pools, PoolHydrator},
    pool_state::{DexPool, PoolStateCache},
    route_engine::RouteEngine,
    types::{DexType, FlashProvider, Hop, RouteParams},
};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_program,
};
use std::sync::Arc;

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const HELIOS_ARB_PROGRAM_ID: &str = "8Hi69VoPFTufCZd1Ht2XHHt5mDxrGCMedea1WfCLwE9c";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const MAPPED_POOLS_FILE: &str = "/root/solana-bot/mapped_pools.json";

fn main() -> Result<()> {
    let payer_path = std::env::var("PAYER_KEYPAIR")
        .unwrap_or_else(|_| "/root/solana-bot/wallet.json".to_string());
    let payer = read_keypair_file(&payer_path)
        .map_err(|e| anyhow!("failed to read payer keypair from {}: {}", payer_path, e))?;
    let payer_pubkey = payer.pubkey();

    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let helios_program: Pubkey = HELIOS_ARB_PROGRAM_ID.parse().unwrap();
    let usdc_mint: Pubkey = USDC_MINT.parse().unwrap();
    let sol_mint: Pubkey = SOL_MINT.parse().unwrap();

    let (helios_config, _) = Pubkey::find_program_address(&[b"config"], &helios_program);
    initialize_helios_if_missing(&rpc, &payer, helios_program, helios_config)?;

    let pool_cache = Arc::new(PoolStateCache::new());
    let hydrator = PoolHydrator::new(RPC_URL, pool_cache.clone(), MAPPED_POOLS_FILE);
    let mapped = load_mapped_pools(MAPPED_POOLS_FILE)
        .with_context(|| format!("failed to load {}", MAPPED_POOLS_FILE))?;
    let loaded = hydrator.load_from_file()?;
    hydrator.refresh_all(&mapped)?;
    println!("mapped_pools_loaded={}", loaded);
    println!("mapped_pools_refreshed={}", mapped.len());

    // -----------------------------------------------------------------------
    // Diagnostics: print reserve state for USDC-involved pools
    // -----------------------------------------------------------------------
    let pools_with_usdc: Vec<DexPool> = pool_cache
        .inner_iter()
        .filter(|p| p.token_a == usdc_mint || p.token_b == usdc_mint)
        .collect();
    println!("usdc_pools_total={}", pools_with_usdc.len());
    let pools_with_reserves: Vec<&DexPool> = pools_with_usdc
        .iter()
        .filter(|p| p.reserve_a > 0 && p.reserve_b > 0)
        .collect();
    println!("usdc_pools_with_reserves={}", pools_with_reserves.len());
    for p in &pools_with_reserves {
        println!(
            "  pool={} dex={:?} reserve_a={} reserve_b={} fee={}bps",
            p.pool_address, p.dex_type, p.reserve_a, p.reserve_b, p.fee_bps
        );
    }

    let builder = TxBuilder::new(payer, "https://api.mainnet-beta.solana.com");
    let engine = RouteEngine::new(pool_cache.clone(), None);
    let alt_accounts = if std::env::var("NO_ALT").as_deref() == Ok("1") {
        println!("alt_disabled=true (testing without ALT)");
        Vec::new()
    } else {
        load_alt_accounts(&rpc)
    };
    let borrow_amounts = probe_borrow_amounts();
    let max_routes = probe_max_routes();
    let mut last_failure: Option<(RouteParams, String, Vec<String>)> = None;

    // -----------------------------------------------------------------------
    // Phase 1: Bellman-Ford route discovery
    // -----------------------------------------------------------------------
    for borrow_amount in &borrow_amounts {
        let borrow_amount = *borrow_amount;
        let all_routes = engine.find_opportunities(borrow_amount);
        println!(
            "bellman_ford_routes_total={} borrow_amount={}",
            all_routes.len(),
            borrow_amount
        );

        if std::env::var("DUMP_ALL_ROUTES").as_deref() == Ok("1") {
            for (ri, r) in all_routes.iter().enumerate() {
                let path: Vec<String> = r
                    .hops
                    .iter()
                    .map(|h| {
                        format!(
                            "{:.4}/{:?}",
                            h.token_in.to_string().get(..4).unwrap_or("?"),
                            DexType::from_program_id(&h.dex_program)
                        )
                    })
                    .collect();
                println!(
                    "route[{}] net_profit={} gross={} path=[{}]->{:.4}",
                    ri,
                    r.net_profit,
                    r.gross_profit,
                    path.join("→"),
                    r.hops
                        .last()
                        .map(|h| h.token_out.to_string())
                        .unwrap_or_default()
                        .get(..4)
                        .unwrap_or("?")
                );
            }
        }
        let candidates: Vec<RouteParams> = all_routes
            .into_iter()
            .filter(|route| route_looks_supported(route, usdc_mint))
            .take(max_routes)
            .collect();

        println!(
            "borrow_amount={} candidate_routes={}",
            borrow_amount,
            candidates.len()
        );

        for (idx, route) in candidates.into_iter().enumerate() {
            if let Some(result) = try_simulate(
                idx,
                borrow_amount,
                route,
                &rpc,
                &builder,
                &alt_accounts,
                &pool_cache,
                &payer_pubkey,
            )? {
                match result {
                    SimResult::Success => return Ok(()),
                    SimResult::Failure(r, err, logs) => {
                        last_failure = Some((r, err, logs));
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 1.5: Direct Orca SwapV2 simulation (no Port/helios CPI chain)
    // Set DIRECT_ORCA_TEST=1 to run this diagnostic.
    // -----------------------------------------------------------------------
    if std::env::var("DIRECT_ORCA_TEST").as_deref() == Ok("1") {
        println!("direct_orca_test=enabled");
        if let Some(pool_key) = std::env::var("FORCE_ORCA_POOL")
            .ok()
            .and_then(|s| s.parse::<Pubkey>().ok())
            .or_else(|| {
                pool_cache
                    .inner_iter()
                    .filter(|p| {
                        p.dex_type == DexType::OrcaWhirlpool
                            && (p.token_a == usdc_mint || p.token_b == usdc_mint)
                            && p.reserve_a > 0
                            && p.reserve_b > 0
                    })
                    .max_by_key(|p| p.reserve_a.saturating_add(p.reserve_b))
                    .map(|p| p.pool_address)
            })
        {
            if let Some(logs) = direct_orca_simulate(
                &rpc,
                &payer_pubkey,
                pool_key,
                usdc_mint,
                sol_mint,
                &pool_cache,
                &alt_accounts,
            )? {
                for log in &logs {
                    println!("{}", log);
                }
            }
        }
    }
    let recent_blockhash = rpc.get_latest_blockhash()?;

    // -----------------------------------------------------------------------
    // Phase 2: Force-test mode — build synthetic route to probe ABI
    // -----------------------------------------------------------------------
    if std::env::var("FORCE_TEST_ROUTE").as_deref() == Ok("1") {
        println!("force_test_route=enabled (bypassing profit filter for ABI validation)");

        for borrow_amount in &borrow_amounts {
            let borrow_amount = *borrow_amount;
            if let Some(route) = build_force_route(borrow_amount, usdc_mint, sol_mint, &pool_cache)
            {
                println!(
                    "force_route borrow_amount={} hops={} gross_profit={} net_profit={}",
                    route.borrow_amount,
                    route.hops.len(),
                    route.gross_profit,
                    route.net_profit
                );
                for (i, hop) in route.hops.iter().enumerate() {
                    println!(
                        "  hop={} dex={:?} pool={} amount_out={}",
                        i + 1,
                        DexType::from_program_id(&hop.dex_program),
                        hop.pool,
                        hop.amount_out
                    );
                }

                if let Some(result) = try_simulate(
                    0,
                    borrow_amount,
                    route,
                    &rpc,
                    &builder,
                    &alt_accounts,
                    &pool_cache,
                    &payer_pubkey,
                )? {
                    match result {
                        SimResult::Success => return Ok(()),
                        SimResult::Failure(r, err, logs) => {
                            last_failure = Some((r, err, logs));
                            // Don't break — try other borrow sizes.
                        }
                    }
                }
            } else {
                println!(
                    "force_route borrow_amount={} skipped (pool data missing)",
                    borrow_amount
                );
            }
        }
    } else {
        println!(
            "hint: set FORCE_TEST_ROUTE=1 to bypass Bellman-Ford and test Port ABI \
             with a synthetic 2-hop USDC→SOL→USDC route (may be slightly unprofitable)"
        );
    }

    if let Some((route, err, logs)) = last_failure {
        println!(
            "final_failure borrow_amount={} hops={} gross_profit={} net_profit={} err={}",
            route.borrow_amount,
            route.hops.len(),
            route.gross_profit,
            route.net_profit,
            err
        );
        for log in logs {
            println!("{}", log);
        }
    }

    Err(anyhow!("no Port route candidate simulated successfully"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

enum SimResult {
    Success,
    Failure(RouteParams, String, Vec<String>),
}

fn try_simulate(
    idx: usize,
    borrow_amount: u64,
    route: RouteParams,
    rpc: &RpcClient,
    builder: &TxBuilder,
    alt_accounts: &[AddressLookupTableAccount],
    pool_cache: &Arc<PoolStateCache>,
    payer_pubkey: &Pubkey,
) -> Result<Option<SimResult>> {
    println!(
        "candidate={} borrow_amount={} hops={} gross_profit={} net_profit={}",
        idx + 1,
        route.borrow_amount,
        route.hops.len(),
        route.gross_profit,
        route.net_profit
    );
    for (hop_idx, hop) in route.hops.iter().enumerate() {
        println!(
            "hop={} dex={:?} pool={} token_in={} token_out={} amount_out={}",
            hop_idx + 1,
            DexType::from_program_id(&hop.dex_program),
            hop.pool,
            hop.token_in,
            hop.token_out,
            hop.amount_out
        );
    }

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = match builder.build(
        &route,
        FlashProvider::PortFinance,
        recent_blockhash,
        alt_accounts,
        pool_cache,
    ) {
        Ok(tx) => tx,
        Err(err) => {
            println!("build_error={}", err);
            return Ok(None);
        }
    };

    let sim = rpc.simulate_transaction(&tx)?;
    println!("simulation_err={:?}", sim.value.err);
    println!("units_consumed={:?}", sim.value.units_consumed);

    let logs = sim.value.logs.unwrap_or_default();
    if sim.value.err.is_none() {
        for log in &logs {
            println!("{}", log);
        }
        println!("selected_borrow_amount={}", borrow_amount);
        println!("selected_candidate={}", idx + 1);
        println!("payer={}", payer_pubkey);
        return Ok(Some(SimResult::Success));
    }

    Ok(Some(SimResult::Failure(
        route,
        format!("{:?}", sim.value.err),
        logs,
    )))
}

/// Build a synthetic 2-hop USDC→SOL→USDC route using the best available
/// pool data for ABI testing (ignores profitability).
fn build_force_route(
    borrow_amount: u64,
    usdc_mint: Pubkey,
    sol_mint: Pubkey,
    pool_cache: &Arc<PoolStateCache>,
) -> Option<RouteParams> {
    let orca_program: Pubkey = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc".parse().ok()?;
    let raydium_program: Pubkey = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
        .parse()
        .ok()?;

    // Dynamically pick the best Orca USDC/SOL pool (highest sum of reserves = most liquid).
    // Override via FORCE_ORCA_POOL env var for testing specific pools.
    let force_orca_key: Option<Pubkey> = std::env::var("FORCE_ORCA_POOL")
        .ok()
        .and_then(|s| s.parse().ok());

    let best_orca = if let Some(key) = force_orca_key {
        pool_cache.get(&key)?
    } else {
        pool_cache
            .inner_iter()
            .filter(|p| {
                p.dex_type == DexType::OrcaWhirlpool
                    && (p.token_a == usdc_mint || p.token_b == usdc_mint)
                    && (p.token_a == sol_mint || p.token_b == sol_mint)
                    && p.reserve_a > 0
                    && p.reserve_b > 0
            })
            .max_by_key(|p| p.reserve_a.saturating_add(p.reserve_b))?
    };

    // Dynamically pick best Raydium USDC/SOL pool.
    let best_ray = pool_cache
        .inner_iter()
        .filter(|p| {
            p.dex_type == DexType::RaydiumAmmV4
                && (p.token_a == usdc_mint || p.token_b == usdc_mint)
                && (p.token_a == sol_mint || p.token_b == sol_mint)
                && p.reserve_a > 0
                && p.reserve_b > 0
        })
        .max_by_key(|p| p.reserve_a.saturating_add(p.reserve_b));

    // Prefer Raydium for hop 2 (different DEX = more realistic cross-DEX test),
    // fall back to a second Orca pool if Raydium has no data.
    let hop2_pool = if let Some(ray) = best_ray {
        ray
    } else {
        // Find a different Orca USDC/SOL pool with good reserves.
        pool_cache
            .inner_iter()
            .filter(|p| {
                p.dex_type == DexType::OrcaWhirlpool
                    && p.pool_address != best_orca.pool_address
                    && (p.token_a == usdc_mint || p.token_b == usdc_mint)
                    && (p.token_a == sol_mint || p.token_b == sol_mint)
                    && p.reserve_a > 0
                    && p.reserve_b > 0
            })
            .max_by_key(|p| p.reserve_a.saturating_add(p.reserve_b))?
    };

    let orca_pool_key = best_orca.pool_address;
    let hop2_pool_key = hop2_pool.pool_address;
    let hop2_program = if hop2_pool.dex_type == DexType::RaydiumAmmV4 {
        raydium_program
    } else {
        orca_program
    };

    println!(
        "force_route: hop1=Orca:{} (ra={} rb={}) hop2={:?}:{} (ra={} rb={})",
        orca_pool_key,
        best_orca.reserve_a,
        best_orca.reserve_b,
        hop2_pool.dex_type,
        hop2_pool_key,
        hop2_pool.reserve_a,
        hop2_pool.reserve_b,
    );

    // Quote hop 1: USDC → SOL via Orca
    let sol_out = if best_orca.token_a == usdc_mint {
        best_orca.quote_a_to_b(borrow_amount)
    } else {
        best_orca.quote_b_to_a(borrow_amount)
    };
    let (orca_token_in, orca_token_out) = (usdc_mint, sol_mint);
    if sol_out == 0 {
        println!(
            "force_route_skip: Orca pool {} has zero quote for borrow_amount={}",
            orca_pool_key, borrow_amount
        );
        return None;
    }

    // Quote hop 2: SOL → USDC
    let usdc_out = if hop2_pool.token_a == sol_mint {
        hop2_pool.quote_a_to_b(sol_out)
    } else {
        hop2_pool.quote_b_to_a(sol_out)
    };
    if usdc_out == 0 {
        println!(
            "force_route_skip: hop2 pool {} has zero quote for sol_out={}",
            hop2_pool_key, sol_out
        );
        return None;
    }

    let gross_profit = usdc_out as i64 - borrow_amount as i64;
    // Port fee ≈ 0.09% of borrow
    let port_fee = (borrow_amount as i64 * 9) / 10_000;
    let tx_fees: i64 = 10_000; // 2 hops × 5000 lamports
    let net_profit = gross_profit - port_fee - tx_fees;

    Some(RouteParams {
        borrow_amount,
        hops: vec![
            Hop {
                pool: orca_pool_key,
                dex_program: orca_program,
                token_in: orca_token_in,
                token_out: orca_token_out,
                amount_out: sol_out,
                price_impact: 0.0,
            },
            Hop {
                pool: hop2_pool_key,
                dex_program: hop2_program,
                token_in: sol_mint,
                token_out: usdc_mint,
                amount_out: usdc_out,
                price_impact: 0.0,
            },
        ],
        gross_profit: gross_profit.max(0) as u64,
        net_profit,
        risk_factor: 0.5,
        strategy: "port-probe",
        tier: 0,
    })
}

/// Simulate a direct Orca SwapV2 call (bypassing Port/helios) to verify account layout.
fn direct_orca_simulate(
    rpc: &RpcClient,
    payer: &Pubkey,
    pool_key: Pubkey,
    usdc_mint: Pubkey,
    sol_mint: Pubkey,
    pool_cache: &Arc<PoolStateCache>,
    alt_accounts: &[AddressLookupTableAccount],
) -> Result<Option<Vec<String>>> {
    use solana_sdk::message::{v0, VersionedMessage};
    use solana_sdk::transaction::VersionedTransaction;

    let pool = match pool_cache.get(&pool_key) {
        Some(p) => p,
        None => {
            println!("direct_orca_skip: pool not in cache");
            return Ok(None);
        }
    };
    let meta = match pool.orca_meta.as_ref() {
        Some(m) => m.clone(),
        None => {
            println!("direct_orca_skip: no Orca metadata");
            return Ok(None);
        }
    };
    if meta.tick_spacing == 0 {
        println!("direct_orca_skip: tick_spacing=0 (metadata not loaded)");
        return Ok(None);
    }

    // Build tick arrays and oracle using the same derivation as tx_builder.
    let orca_program: Pubkey = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
        .parse()
        .unwrap();
    let memo_program: Pubkey = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
        .parse()
        .unwrap();
    let token_program: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap();

    // USDC → SOL: if token_b = USDC, we go b→a
    let a_to_b = pool.token_a == usdc_mint;
    let payer_ata_usdc = {
        let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
            .parse()
            .unwrap();
        Pubkey::find_program_address(
            &[payer.as_ref(), token_program.as_ref(), usdc_mint.as_ref()],
            &ata_program,
        )
        .0
    };
    let payer_ata_sol = {
        let ata_program: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
            .parse()
            .unwrap();
        Pubkey::find_program_address(
            &[payer.as_ref(), token_program.as_ref(), sol_mint.as_ref()],
            &ata_program,
        )
        .0
    };

    let tick_array_size: i32 = 88;
    let ticks_in_array = tick_array_size * meta.tick_spacing as i32;
    // Use floor division (div_euclid) + string encoding — same as tx_builder::get_orca_tick_array_start_tick_index
    let start_tick = meta
        .tick_current_index
        .div_euclid(meta.tick_spacing as i32)
        .div_euclid(tick_array_size)
        * meta.tick_spacing as i32
        * tick_array_size;
    let direction: i32 = if a_to_b { -1 } else { 1 };
    let ta_keys: Vec<Pubkey> = (0..3)
        .map(|i| {
            let tick_start = start_tick + i * direction * ticks_in_array;
            // Orca seeds use ASCII string of the tick index, NOT binary bytes
            let tick_str = tick_start.to_string();
            Pubkey::find_program_address(
                &[b"tick_array", pool_key.as_ref(), tick_str.as_bytes()],
                &orca_program,
            )
            .0
        })
        .collect();
    let oracle = Pubkey::find_program_address(&[b"oracle", pool_key.as_ref()], &orca_program).0;
    let whirlpool_authority =
        Pubkey::find_program_address(&[b"whirlpool", pool_key.as_ref()], &orca_program).0;

    // Print what we'll send
    let (ta, tb, oa, ob, va, vb) = if pool.token_a == usdc_mint {
        (
            usdc_mint,
            sol_mint,
            payer_ata_usdc,
            payer_ata_sol,
            meta.token_vault_a,
            meta.token_vault_b,
        )
    } else {
        (
            sol_mint,
            usdc_mint,
            payer_ata_sol,
            payer_ata_usdc,
            meta.token_vault_a,
            meta.token_vault_b,
        )
    };

    println!(
        "direct_orca_test: pool={} tick_spacing={} tick_current={} a_to_b={}",
        pool_key, meta.tick_spacing, meta.tick_current_index, a_to_b
    );
    println!("  token_mint_a={} token_mint_b={}", ta, tb);
    println!("  token_vault_a={} token_vault_b={}", va, vb);
    println!(
        "  oracle={} whirlpool_authority={}",
        oracle, whirlpool_authority
    );
    for (i, ta_key) in ta_keys.iter().enumerate() {
        println!("  tick_array[{}]={}", i, ta_key);
    }

    // Build SwapV2 instruction directly to Orca (no helios)
    let amount: u64 = 1_000_000; // 1 USDC
    let min_out: u64 = 0;
    let mut data = vec![43u8, 4, 237, 11, 26, 201, 30, 98]; // SwapV2 discriminator
    data.extend_from_slice(&amount.to_le_bytes()); // amount
    data.extend_from_slice(&min_out.to_le_bytes()); // other_amount_threshold
    data.extend_from_slice(&0u128.to_le_bytes()); // sqrt_price_limit
    data.push(1u8); // amount_specified_is_input
    data.push(if a_to_b { 1u8 } else { 0u8 }); // a_to_b
    data.push(0u8); // remaining_accounts_info = None

    let ix = Instruction {
        program_id: orca_program,
        accounts: vec![
            AccountMeta::new_readonly(token_program, false), // token_program_a
            AccountMeta::new_readonly(token_program, false), // token_program_b
            AccountMeta::new_readonly(memo_program, false),  // memo_program
            AccountMeta::new_readonly(*payer, true),         // token_authority
            AccountMeta::new(pool_key, false),               // whirlpool
            AccountMeta::new_readonly(ta, false),            // token_mint_a (both mints grouped)
            AccountMeta::new_readonly(tb, false),            // token_mint_b
            AccountMeta::new(oa, false),                     // token_owner_account_a
            AccountMeta::new(va, false),                     // token_vault_a
            AccountMeta::new(ob, false),                     // token_owner_account_b
            AccountMeta::new(vb, false),                     // token_vault_b
            AccountMeta::new(ta_keys[0], false),             // tick_array_0
            AccountMeta::new(ta_keys[1], false),             // tick_array_1
            AccountMeta::new(ta_keys[2], false),             // tick_array_2
            AccountMeta::new(oracle, false),                 // oracle
        ],
        data,
    };

    let blockhash = rpc.get_latest_blockhash()?;
    // Use a fake keypair for signing — just for simulation
    let fake_kp = solana_sdk::signature::Keypair::new();
    let msg = v0::Message::try_compile(payer, &[ix], alt_accounts, blockhash)?;
    // Build an unsigned versioned tx (sigVerify=false in simulation)
    let versioned_msg = VersionedMessage::V0(msg);
    let tx = VersionedTransaction {
        signatures: vec![solana_sdk::signature::Signature::default()],
        message: versioned_msg,
    };
    let _ = fake_kp;

    let sim = rpc.simulate_transaction(&tx)?;
    println!("direct_orca_sim_err={:?}", sim.value.err);
    println!("direct_orca_units={:?}", sim.value.units_consumed);
    Ok(Some(sim.value.logs.unwrap_or_default()))
}

/// Load the project ALT from chain. Falls back to empty vec on any error.
fn load_alt_accounts(rpc: &RpcClient) -> Vec<AddressLookupTableAccount> {
    let alt_path = std::env::var("ALT_ADDRESS_FILE")
        .unwrap_or_else(|_| "/root/solana-bot/alt_address.txt".to_string());
    let alt_addr_str = match std::fs::read_to_string(&alt_path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            println!("alt_load_skip: cannot read {}: {}", alt_path, e);
            return Vec::new();
        }
    };
    let alt_key: Pubkey = match alt_addr_str.parse() {
        Ok(k) => k,
        Err(e) => {
            println!("alt_load_skip: invalid pubkey '{}': {}", alt_addr_str, e);
            return Vec::new();
        }
    };
    match rpc.get_account(&alt_key) {
        Ok(acc) => match AddressLookupTable::deserialize(&acc.data) {
            Ok(table) => {
                println!("alt_loaded={} addresses={}", alt_key, table.addresses.len());
                vec![AddressLookupTableAccount {
                    key: alt_key,
                    addresses: table.addresses.to_vec(),
                }]
            }
            Err(e) => {
                println!("alt_load_skip: deserialize error: {}", e);
                Vec::new()
            }
        },
        Err(e) => {
            println!("alt_load_skip: get_account error: {}", e);
            Vec::new()
        }
    }
}

/// Accept any route that starts/ends with USDC and uses only supported DEXes.
fn route_looks_supported(route: &RouteParams, usdc_mint: Pubkey) -> bool {
    let Some(first) = route.hops.first() else {
        return false;
    };
    let Some(last) = route.hops.last() else {
        return false;
    };
    if first.token_in != usdc_mint || last.token_out != usdc_mint {
        return false;
    }
    // Accept Raydium + Orca (Meteora/PumpSwap excluded until CPIs validated)
    route.hops.iter().all(|hop| {
        matches!(
            DexType::from_program_id(&hop.dex_program),
            DexType::OrcaWhirlpool | DexType::RaydiumAmmV4
        )
    })
}

fn probe_borrow_amounts() -> Vec<u64> {
    if let Ok(raw) = std::env::var("PORT_PROBE_BORROW_AMOUNTS") {
        let parsed: Vec<u64> = raw
            .split(',')
            .filter_map(|part| part.trim().parse::<u64>().ok())
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }

    vec![1_000_000_000, 500_000_000, 100_000_000, 10_000_000]
}

fn probe_max_routes() -> usize {
    std::env::var("PORT_PROBE_MAX_ROUTES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(10)
}

fn initialize_helios_if_missing(
    rpc: &RpcClient,
    payer: &Keypair,
    helios_program: Pubkey,
    helios_config: Pubkey,
) -> Result<()> {
    if rpc.get_account(&helios_config).is_ok() {
        return Ok(());
    }

    let initialize_ix = Instruction {
        program_id: helios_program,
        accounts: vec![
            AccountMeta::new(helios_config, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: vec![175, 175, 109, 31, 13, 152, 155, 237],
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[initialize_ix],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    let sig = rpc.send_and_confirm_transaction_with_spinner(&tx)?;
    println!("helios_initialize_signature={}", sig);

    Ok(())
}
