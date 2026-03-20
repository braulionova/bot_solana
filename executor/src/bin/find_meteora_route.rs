use anyhow::{Context, Result};
use executor::tx_builder::TxBuilder;
use market_engine::{
    pool_hydrator::{load_mapped_pools, PoolHydrator},
    pool_state::{DexPool, PoolStateCache},
    types::{DexType, FlashProvider, Hop, RouteParams},
};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, pubkey::Pubkey, signature::read_keypair_file,
};
use std::{collections::HashMap, sync::Arc};

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const MAPPED_POOLS_FILE: &str = "/root/solana-bot/mapped_pools.json";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const MAX_RETURN_MULTIPLIER: u64 = 2;

#[derive(Clone)]
struct HopInfo {
    pool: Pubkey,
    dex: DexType,
    token_in: Pubkey,
    token_out: Pubkey,
    amount_out: u64,
}

fn main() -> Result<()> {
    let payer_path = std::env::var("PAYER_KEYPAIR")
        .unwrap_or_else(|_| "/root/solana-bot/wallet.json".to_string());
    let payer = read_keypair_file(&payer_path).expect("failed to read payer keypair");

    let pool_cache = Arc::new(PoolStateCache::new());
    let hydrator = PoolHydrator::new(RPC_URL, pool_cache.clone(), MAPPED_POOLS_FILE);
    let mapped = load_mapped_pools(MAPPED_POOLS_FILE)
        .with_context(|| format!("failed to load {}", MAPPED_POOLS_FILE))?;
    hydrator.load_from_file()?;
    hydrator.refresh_all(&mapped)?;

    let usdc_mint: Pubkey = USDC_MINT.parse().unwrap();
    let borrow_amount: u64 = 1_000_000;

    let liquid_pools: Vec<DexPool> = pool_cache
        .inner_iter()
        .filter(|pool| pool.reserve_a >= 1_000_000 && pool.reserve_b >= 1_000_000)
        .collect();

    let mut adj: HashMap<Pubkey, Vec<(DexPool, Pubkey)>> = HashMap::new();
    for pool in &liquid_pools {
        adj.entry(pool.token_a)
            .or_default()
            .push((pool.clone(), pool.token_b));
        adj.entry(pool.token_b)
            .or_default()
            .push((pool.clone(), pool.token_a));
    }

    let mut meteora_routes = Vec::new();
    if let Some(hop1_edges) = adj.get(&usdc_mint) {
        for (pool1, token1) in hop1_edges {
            let amount1 = quote(pool1, usdc_mint, borrow_amount);
            if amount1 == 0 {
                continue;
            }

            if let Some(hop2_edges) = adj.get(token1) {
                for (pool2, token2) in hop2_edges {
                    if pool2.pool_address == pool1.pool_address {
                        continue;
                    }
                    let amount2 = quote(pool2, *token1, amount1);
                    if amount2 == 0 {
                        continue;
                    }

                    let two_hop = vec![
                        HopInfo {
                            pool: pool1.pool_address,
                            dex: pool1.dex_type,
                            token_in: usdc_mint,
                            token_out: *token1,
                            amount_out: amount1,
                        },
                        HopInfo {
                            pool: pool2.pool_address,
                            dex: pool2.dex_type,
                            token_in: *token1,
                            token_out: *token2,
                            amount_out: amount2,
                        },
                    ];

                    if *token2 == usdc_mint
                        && amount2 <= borrow_amount.saturating_mul(MAX_RETURN_MULTIPLIER)
                        && two_hop.iter().any(|hop| hop.dex == DexType::MeteoraDlmm)
                    {
                        meteora_routes.push((amount2, two_hop));
                        continue;
                    }

                    if let Some(hop3_edges) = adj.get(token2) {
                        for (pool3, token3) in hop3_edges {
                            if pool3.pool_address == pool2.pool_address {
                                continue;
                            }
                            let amount3 = quote(pool3, *token2, amount2);
                            if amount3 == 0 {
                                continue;
                            }

                            let three_hop = vec![
                                HopInfo {
                                    pool: pool1.pool_address,
                                    dex: pool1.dex_type,
                                    token_in: usdc_mint,
                                    token_out: *token1,
                                    amount_out: amount1,
                                },
                                HopInfo {
                                    pool: pool2.pool_address,
                                    dex: pool2.dex_type,
                                    token_in: *token1,
                                    token_out: *token2,
                                    amount_out: amount2,
                                },
                                HopInfo {
                                    pool: pool3.pool_address,
                                    dex: pool3.dex_type,
                                    token_in: *token2,
                                    token_out: *token3,
                                    amount_out: amount3,
                                },
                            ];

                            if *token3 == usdc_mint
                                && amount3 <= borrow_amount.saturating_mul(MAX_RETURN_MULTIPLIER)
                                && three_hop.iter().any(|hop| hop.dex == DexType::MeteoraDlmm)
                            {
                                meteora_routes.push((amount3, three_hop));
                            }
                        }
                    }
                }
            }
        }
    }

    meteora_routes.sort_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| b.0.cmp(&a.0)));

    println!("meteora_routes_found={}", meteora_routes.len());
    let (best_amount, best_path) = match meteora_routes.first() {
        Some(route) => route,
        None => {
            println!("No Meteora routes found.");
            return Ok(());
        }
    };

    println!(
        "selected_route_hops={} end_amount={}",
        best_path.len(),
        *best_amount as f64 / 1e6
    );

    let mut hops = Vec::new();
    for hop in best_path {
        println!(
            "  Hop: dex={:?} pool={} in={} out={} amount={}",
            hop.dex, hop.pool, hop.token_in, hop.token_out, hop.amount_out
        );

        let dex_program = match hop.dex {
            DexType::RaydiumAmmV4 => market_engine::types::dex_programs::raydium_amm_v4(),
            DexType::OrcaWhirlpool => market_engine::types::dex_programs::orca_whirlpool(),
            DexType::MeteoraDlmm => market_engine::types::dex_programs::meteora_dlmm(),
            _ => continue,
        };

        hops.push(Hop {
            pool: hop.pool,
            dex_program,
            token_in: hop.token_in,
            token_out: hop.token_out,
            amount_out: hop.amount_out,
            price_impact: 0.0,
        });
    }

    let route_params = RouteParams {
        borrow_amount,
        hops,
        gross_profit: best_amount.saturating_sub(borrow_amount),
        net_profit: best_amount.saturating_sub(borrow_amount) as i64 - 15_000,
        risk_factor: 0.1,
        strategy: "meteora-probe",
        tier: 0,
    };

    let builder = TxBuilder::new(payer, "https://api.mainnet-beta.solana.com");
    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let blockhash = rpc.get_latest_blockhash()?;
    let tx = match builder.build(
        &route_params,
        FlashProvider::PortFinance,
        blockhash,
        &[],
        &pool_cache,
    ) {
        Ok(tx) => tx,
        Err(err) => {
            println!("Failed to build tx: {:?}", err);
            return Ok(());
        }
    };

    let tx_size = bincode::serialize(&tx)?.len();
    println!("tx_size={}", tx_size);
    if tx_size > 1232 {
        println!("Transaction too large without ALT.");
        return Ok(());
    }

    let sim_result = rpc.simulate_transaction(&tx)?;
    if let Some(err) = &sim_result.value.err {
        println!("Simulation failed: {:?}", err);
    } else {
        println!("Simulation succeeded!");
    }
    if let Some(logs) = sim_result.value.logs {
        for log in logs {
            println!("{}", log);
        }
    }

    Ok(())
}

fn quote(pool: &DexPool, token_in: Pubkey, amount_in: u64) -> u64 {
    if pool.token_a == token_in {
        pool.quote_a_to_b(amount_in)
    } else {
        pool.quote_b_to_a(amount_in)
    }
}
