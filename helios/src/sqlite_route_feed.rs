use crossbeam_channel::Sender;
use market_engine::{
    flash_selector::FlashSelector,
    pool_state::{DexPool, PoolStateCache},
    scoring::Scorer,
    types::{FlashProvider, Hop, OpportunityBundle, RouteParams},
};
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::HashMap,
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tracing::{info, warn};

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const ORCA_PROGRAM_ID: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
const RAYDIUM_AMM_V4_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

#[derive(Debug, Clone, Deserialize)]
struct SqliteRouteCandidate {
    route_key: String,
    mint: String,
    base_symbol: String,
    dex_buy: String,
    dex_sell: String,
    pool_buy: String,
    pool_sell: String,
    amount_in_lamports: u64,
    estimated_profit_lamports: i64,
    validated_profit_lamports: Option<i64>,
    validation_status: String,
}

pub fn spawn_sqlite_route_feed(
    db_path: String,
    poll_interval: Duration,
    top_n: usize,
    cooldown: Duration,
    min_profit: i64,
    pool_cache: Arc<PoolStateCache>,
    opp_tx: Sender<OpportunityBundle>,
) {
    thread::Builder::new()
        .name("sqlite-route-feed".into())
        .spawn(move || {
            let selector = FlashSelector::new();
            let scorer = Scorer::new();
            let mut next_id = 10_000_000u64;
            let mut last_sent_at: HashMap<String, Instant> = HashMap::new();

            loop {
                match fetch_candidates(&db_path, top_n) {
                    Ok(candidates) => {
                        for candidate in candidates {
                            let effective_profit = effective_profit(&candidate);
                            if effective_profit < min_profit {
                                continue;
                            }

                            let now = Instant::now();
                            if let Some(prev) = last_sent_at.get(&candidate.route_key) {
                                if now.duration_since(*prev) < cooldown {
                                    continue;
                                }
                            }

                            let route = match build_route(&candidate, &pool_cache) {
                                Some(route) => route,
                                None => continue,
                            };

                            let flash_provider = match selector.select(route.borrow_amount) {
                                Ok(provider) => provider,
                                Err(_) => fallback_provider(route.hops.first().map(|h| h.token_in)),
                            };

                            let mut bundle = OpportunityBundle {
                                id: next_id,
                                slot: 0,
                                detected_at: Instant::now(),
                                route,
                                flash_provider,
                                score: 0.0,
                            };
                            bundle.score = scorer.score(&bundle);

                            info!(
                                route_key = %candidate.route_key,
                                validation_status = %candidate.validation_status,
                                effective_profit,
                                flash_provider = ?bundle.flash_provider,
                                "sqlite route candidate forwarded to executor"
                            );

                            next_id = next_id.saturating_add(1);
                            last_sent_at.insert(candidate.route_key, now);
                            let _ = opp_tx.send(bundle);
                            break;
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, db_path = %db_path, "sqlite route feed poll failed");
                    }
                }

                thread::sleep(poll_interval);
            }
        })
        .expect("spawn sqlite-route-feed");
}

fn effective_profit(candidate: &SqliteRouteCandidate) -> i64 {
    candidate
        .validated_profit_lamports
        .unwrap_or(candidate.estimated_profit_lamports)
}

fn fetch_candidates(db_path: &str, top_n: usize) -> anyhow::Result<Vec<SqliteRouteCandidate>> {
    let sql = format!(
        "select route_key, mint, base_symbol, dex_buy, dex_sell, pool_buy, pool_sell, \
         amount_in_lamports, estimated_profit_lamports, validated_profit_lamports, validation_status \
         from route_candidates \
         order by coalesce(validated_profit_lamports, estimated_profit_lamports) desc \
         limit {}",
        top_n.max(1)
    );

    let output = Command::new("sqlite3")
        .arg("-json")
        .arg(db_path)
        .arg(sql)
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let candidates = serde_json::from_slice::<Vec<SqliteRouteCandidate>>(&output.stdout)?;
    Ok(candidates)
}

fn build_route(
    candidate: &SqliteRouteCandidate,
    pool_cache: &PoolStateCache,
) -> Option<RouteParams> {
    let base_mint = parse_base_mint(&candidate.base_symbol)?;
    let mid_mint = candidate.mint.parse::<Pubkey>().ok()?;
    let pool_buy = candidate.pool_buy.parse::<Pubkey>().ok()?;
    let pool_sell = candidate.pool_sell.parse::<Pubkey>().ok()?;
    let dex_buy = dex_program_id(&candidate.dex_buy)?;
    let dex_sell = dex_program_id(&candidate.dex_sell)?;
    let buy_pool = pool_cache.get(&pool_buy)?;
    let sell_pool = pool_cache.get(&pool_sell)?;

    let mid_amount = quote_pool(&buy_pool, base_mint, mid_mint, candidate.amount_in_lamports)?;
    let final_amount = quote_pool(&sell_pool, mid_mint, base_mint, mid_amount)?;
    let gross_profit = final_amount.saturating_sub(candidate.amount_in_lamports);

    let hops = vec![
        Hop {
            pool: pool_buy,
            dex_program: dex_buy,
            token_in: base_mint,
            token_out: mid_mint,
            amount_out: mid_amount,
            price_impact: price_impact(&buy_pool, candidate.amount_in_lamports),
        },
        Hop {
            pool: pool_sell,
            dex_program: dex_sell,
            token_in: mid_mint,
            token_out: base_mint,
            amount_out: final_amount,
            price_impact: price_impact(&sell_pool, mid_amount),
        },
    ];

    Some(RouteParams {
        hops,
        borrow_amount: candidate.amount_in_lamports,
        gross_profit,
        net_profit: effective_profit(candidate),
        risk_factor: 0.15,
        strategy: "sqlite-feed",
        tier: 0,
    })
}

fn parse_base_mint(symbol: &str) -> Option<Pubkey> {
    match symbol {
        "SOL" => WSOL_MINT.parse().ok(),
        "USDC" => USDC_MINT.parse().ok(),
        _ => None,
    }
}

fn dex_program_id(label: &str) -> Option<Pubkey> {
    match label {
        "Orca" => ORCA_PROGRAM_ID.parse().ok(),
        "Raydium" => RAYDIUM_AMM_V4_PROGRAM_ID.parse().ok(),
        _ => None,
    }
}

fn quote_pool(pool: &DexPool, token_in: Pubkey, token_out: Pubkey, amount_in: u64) -> Option<u64> {
    if token_in == pool.token_a && token_out == pool.token_b {
        Some(pool.quote_a_to_b(amount_in))
    } else if token_in == pool.token_b && token_out == pool.token_a {
        Some(pool.quote_b_to_a(amount_in))
    } else {
        None
    }
}

fn price_impact(pool: &DexPool, amount_in: u64) -> f64 {
    let base = pool.reserve_a.max(1) as f64;
    (amount_in as f64 / base) * 100.0
}

fn fallback_provider(borrow_mint: Option<Pubkey>) -> FlashProvider {
    let Some(mint) = borrow_mint else {
        return FlashProvider::MarginFi;
    };

    match mint.to_string().as_str() {
        USDC_MINT => FlashProvider::Solend,
        _ => FlashProvider::MarginFi,
    }
}
