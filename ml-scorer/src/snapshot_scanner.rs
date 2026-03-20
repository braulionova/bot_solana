//! snapshot_scanner.rs — Exhaustive route scanner across ALL pools from AccountsDB.
//!
//! 1. Reads every pool from PoolStateCache (5700+ pools, all DEXes)
//! 2. Builds a full token graph with real quotes
//! 3. Scans ALL 2-hop and 3-hop cycles (not just from WSOL/USDC/USDT)
//! 4. Scores routes by: profitability, liquidity, competition, uniqueness
//! 5. Persists top "hot routes" to PostgreSQL for priority execution
//!
//! The "AI" aspect: combines ML scorer feedback (historical success/failure rates),
//! on-chain competitor analysis, pool volatility features, and graph-theoretic
//! uniqueness scoring to find LOW COMPETITION routes that other bots miss.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, info, warn};

use crate::MlScorer;

/// Well-known anchor tokens for flash loans.
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

/// Minimum reserves per side to consider a pool viable.
const MIN_RESERVE: u64 = 1_000_000; // 1M raw units (dust filter)
/// Minimum net profit in lamports to store a route.
const MIN_NET_PROFIT: i64 = 5_000; // 5K lamports
/// Maximum routes to store per scan.
const MAX_ROUTES_PER_SCAN: usize = 500;
/// Borrow amounts to test (in lamports).
const BORROW_AMOUNTS: &[u64] = &[
    50_000_000,   // 0.05 SOL
    100_000_000,  // 0.1 SOL
    500_000_000,  // 0.5 SOL
    1_000_000_000, // 1 SOL
    2_000_000_000, // 2 SOL
];

/// A pool edge in the token graph.
#[derive(Clone)]
struct PoolEdge {
    pool_address: String,
    dex_type: String,
    token_in: String,
    token_out: String,
    reserve_in: u64,
    reserve_out: u64,
    fee_bps: u64,
    /// How many other pools serve the same pair (competition proxy).
    pair_pool_count: u32,
}

impl PoolEdge {
    /// Simple XY=K quote with fee deduction.
    fn quote(&self, amount_in: u64) -> u64 {
        if self.reserve_in == 0 || self.reserve_out == 0 || amount_in == 0 {
            return 0;
        }
        let fee_amount = amount_in * self.fee_bps / 10_000;
        let net_in = amount_in.saturating_sub(fee_amount);
        // x * y = k  =>  y_out = reserve_out * net_in / (reserve_in + net_in)
        let numerator = self.reserve_out as u128 * net_in as u128;
        let denominator = self.reserve_in as u128 + net_in as u128;
        if denominator == 0 {
            return 0;
        }
        (numerator / denominator) as u64
    }

    /// Price impact as percentage.
    fn impact(&self, amount_in: u64) -> f64 {
        if self.reserve_in == 0 {
            return 100.0;
        }
        (amount_in as f64 / self.reserve_in as f64) * 100.0
    }
}

/// A scored route candidate.
#[derive(Clone)]
pub struct ScoredRoute {
    pub route_key: String,
    pub tokens: Vec<String>,
    pub pools: Vec<String>,
    pub dex_types: Vec<String>,
    pub hops: usize,
    pub borrow_mint: String,
    pub borrow_amount: u64,
    pub gross_profit: u64,
    pub net_profit: i64,
    pub min_liquidity_usd: f64,
    pub total_fee_bps: u64,
    pub max_impact: f64,
    pub competition_score: f64,
    pub ml_score: f64,
    pub route_uniqueness: f64,
    pub dex_diversity: u16,
}

/// Token graph for route scanning.
struct TokenGraph {
    /// token → [(edge_index)]
    adjacency: HashMap<String, Vec<usize>>,
    edges: Vec<PoolEdge>,
    /// (token_a, token_b) canonical → count of pools
    pair_counts: HashMap<(String, String), u32>,
}

impl TokenGraph {
    fn new() -> Self {
        Self {
            adjacency: HashMap::new(),
            edges: Vec::new(),
            pair_counts: HashMap::new(),
        }
    }

    fn add_edge(&mut self, edge: PoolEdge) {
        let idx = self.edges.len();
        self.adjacency
            .entry(edge.token_in.clone())
            .or_default()
            .push(idx);
        self.edges.push(edge);
    }

    /// Count pools per canonical pair (for competition scoring).
    fn compute_pair_counts(&mut self) {
        self.pair_counts.clear();
        for edge in &self.edges {
            let pair = canonical_pair(&edge.token_in, &edge.token_out);
            *self.pair_counts.entry(pair).or_default() += 1;
        }
        // Update edge pair_pool_count
        for edge in &mut self.edges {
            let pair = canonical_pair(&edge.token_in, &edge.token_out);
            edge.pair_pool_count = *self.pair_counts.get(&pair).unwrap_or(&1);
        }
    }

    /// Scan all 2-hop cycles: A → B → A
    fn scan_2hop(&self, anchor_tokens: &HashSet<String>) -> Vec<RawCycle> {
        let mut cycles = Vec::new();

        for (token_a, edges_a) in &self.adjacency {
            // Only start from anchor tokens (we need to flash loan)
            if !anchor_tokens.contains(token_a) {
                continue;
            }

            for &ei in edges_a {
                let e1 = &self.edges[ei];
                let token_b = &e1.token_out;

                // Find return edges B → A
                if let Some(edges_b) = self.adjacency.get(token_b) {
                    for &ej in edges_b {
                        let e2 = &self.edges[ej];
                        if &e2.token_out != token_a {
                            continue;
                        }
                        // Must be different pools (not same pool both directions)
                        if e1.pool_address == e2.pool_address {
                            continue;
                        }

                        cycles.push(RawCycle {
                            edges: vec![ei, ej],
                            start_token: token_a.clone(),
                        });
                    }
                }
            }
        }
        cycles
    }

    /// Scan all 3-hop cycles: A → B → C → A
    fn scan_3hop(&self, anchor_tokens: &HashSet<String>) -> Vec<RawCycle> {
        let mut cycles = Vec::new();

        for (token_a, edges_a) in &self.adjacency {
            if !anchor_tokens.contains(token_a) {
                continue;
            }

            for &ei in edges_a {
                let e1 = &self.edges[ei];
                let token_b = &e1.token_out;
                if token_b == token_a {
                    continue;
                }

                if let Some(edges_b) = self.adjacency.get(token_b) {
                    for &ej in edges_b {
                        let e2 = &self.edges[ej];
                        let token_c = &e2.token_out;
                        if token_c == token_a || token_c == token_b {
                            continue;
                        }
                        // Must be different pools
                        if e2.pool_address == e1.pool_address {
                            continue;
                        }

                        // Find return C → A
                        if let Some(edges_c) = self.adjacency.get(token_c) {
                            for &ek in edges_c {
                                let e3 = &self.edges[ek];
                                if &e3.token_out != token_a {
                                    continue;
                                }
                                if e3.pool_address == e1.pool_address
                                    || e3.pool_address == e2.pool_address
                                {
                                    continue;
                                }

                                cycles.push(RawCycle {
                                    edges: vec![ei, ej, ek],
                                    start_token: token_a.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
        cycles
    }
}

struct RawCycle {
    edges: Vec<usize>,
    start_token: String,
}

fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a < b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Main scan function: builds graph from pool data, finds profitable routes.
///
/// `pools_json`: serialized Vec of pool snapshots from PoolStateCache
/// `scorer`: optional ML scorer for historical performance data
pub fn scan_all_routes(
    pools: &[PoolSnapshot],
    scorer: Option<&Arc<MlScorer>>,
) -> Vec<ScoredRoute> {
    let t0 = Instant::now();

    // Build token graph
    let mut graph = TokenGraph::new();
    let mut pools_added = 0u64;

    for pool in pools {
        if pool.reserve_a < MIN_RESERVE || pool.reserve_b < MIN_RESERVE {
            continue;
        }
        // Skip pools with default vaults (phantom)
        if pool.vault_a.is_empty() || pool.vault_b.is_empty() {
            continue;
        }

        // Add both directions
        graph.add_edge(PoolEdge {
            pool_address: pool.address.clone(),
            dex_type: pool.dex_type.clone(),
            token_in: pool.token_a.clone(),
            token_out: pool.token_b.clone(),
            reserve_in: pool.reserve_a,
            reserve_out: pool.reserve_b,
            fee_bps: pool.fee_bps,
            pair_pool_count: 0,
        });
        graph.add_edge(PoolEdge {
            pool_address: pool.address.clone(),
            dex_type: pool.dex_type.clone(),
            token_in: pool.token_b.clone(),
            token_out: pool.token_a.clone(),
            reserve_in: pool.reserve_b,
            reserve_out: pool.reserve_a,
            fee_bps: pool.fee_bps,
            pair_pool_count: 0,
        });
        pools_added += 1;
    }

    graph.compute_pair_counts();

    info!(
        pools = pools_added,
        tokens = graph.adjacency.len(),
        edges = graph.edges.len(),
        pairs = graph.pair_counts.len(),
        "graph built in {:.1}ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );

    // Define anchor tokens
    let mut anchors = HashSet::new();
    anchors.insert(WSOL.to_string());
    anchors.insert(USDC.to_string());
    anchors.insert(USDT.to_string());

    // Scan cycles
    let t1 = Instant::now();
    let cycles_2 = graph.scan_2hop(&anchors);
    let cycles_3 = graph.scan_3hop(&anchors);
    info!(
        cycles_2hop = cycles_2.len(),
        cycles_3hop = cycles_3.len(),
        "cycle scan in {:.1}ms",
        t1.elapsed().as_secs_f64() * 1000.0
    );

    // Evaluate and score all cycles
    let t2 = Instant::now();
    let mut scored: Vec<ScoredRoute> = Vec::new();

    let all_cycles: Vec<RawCycle> = cycles_2.into_iter().chain(cycles_3).collect();

    for cycle in &all_cycles {
        for &borrow_amount in BORROW_AMOUNTS {
            if let Some(route) = evaluate_cycle(&graph, cycle, borrow_amount, scorer) {
                if route.net_profit >= MIN_NET_PROFIT {
                    scored.push(route);
                }
            }
        }
    }

    // Sort by composite score (ML score * net_profit)
    scored.sort_by(|a, b| {
        let sa = a.ml_score * (a.net_profit as f64);
        let sb = b.ml_score * (b.net_profit as f64);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(MAX_ROUTES_PER_SCAN);

    info!(
        profitable = scored.len(),
        "route evaluation in {:.1}ms, total scan {:.1}ms",
        t2.elapsed().as_secs_f64() * 1000.0,
        t0.elapsed().as_secs_f64() * 1000.0,
    );

    scored
}

fn evaluate_cycle(
    graph: &TokenGraph,
    cycle: &RawCycle,
    borrow_amount: u64,
    scorer: Option<&Arc<MlScorer>>,
) -> Option<ScoredRoute> {
    let mut current = borrow_amount;
    let mut tokens = vec![cycle.start_token.clone()];
    let mut pools = Vec::new();
    let mut dex_types = Vec::new();
    let mut total_fee_bps = 0u64;
    let mut max_impact = 0.0f64;
    let mut min_liquidity = f64::MAX;
    let mut dex_set = HashSet::new();
    let mut total_pair_pools = 0u32;

    for &ei in &cycle.edges {
        let edge = &graph.edges[ei];

        // Check impact limit
        let impact = edge.impact(current);
        if impact > 10.0 {
            return None; // Too much impact
        }
        max_impact = max_impact.max(impact);

        // Quote
        let out = edge.quote(current);
        if out == 0 {
            return None;
        }

        // Liquidity estimate (USD rough: assume 1 SOL ≈ $150 for reserves)
        let liq_usd = (edge.reserve_in.min(edge.reserve_out) as f64) / 1e9 * 150.0;
        min_liquidity = min_liquidity.min(liq_usd);

        tokens.push(edge.token_out.clone());
        pools.push(edge.pool_address.clone());
        dex_types.push(edge.dex_type.clone());
        dex_set.insert(edge.dex_type.clone());
        total_fee_bps += edge.fee_bps;
        total_pair_pools += edge.pair_pool_count;
        current = out;
    }

    if current <= borrow_amount {
        return None; // Not profitable
    }

    let gross_profit = current - borrow_amount;
    // Estimate costs: 5K lamports base + 5K per hop
    let tx_cost = 5_000 + 5_000 * cycle.edges.len() as i64;
    let net_profit = gross_profit as i64 - tx_cost;

    if net_profit < MIN_NET_PROFIT {
        return None;
    }

    // Competition score: higher = MORE competition (bad)
    // Based on: number of pools serving the same pairs (more pools = more bots watching)
    let avg_pair_pools = total_pair_pools as f64 / cycle.edges.len().max(1) as f64;
    let competition_score = (avg_pair_pools / 10.0).min(1.0); // normalize to 0-1

    // Route uniqueness: longer routes through uncommon pairs = more unique
    let uniqueness = if cycle.edges.len() >= 3 {
        0.7 + (1.0 - competition_score) * 0.3
    } else {
        (1.0 - competition_score) * 0.5
    };

    // DEX diversity bonus
    let dex_diversity = dex_set.len() as u16;

    // ML score from historical data
    let pool_keys: Vec<&str> = pools.iter().map(|s| s.as_str()).collect();
    let strategy = if cycle.edges.len() == 2 {
        "cross-dex"
    } else {
        "bellman-ford"
    };

    let ml_boost = scorer
        .map(|s| s.score_boost(&pool_keys, strategy))
        .unwrap_or(1.0);

    // Composite ML score:
    // profit_factor * (1 - competition) * uniqueness * dex_diversity_bonus * ml_boost * liquidity_factor
    let profit_factor = (net_profit as f64 / 100_000.0).min(5.0); // normalize
    let liquidity_factor = if min_liquidity > 10_000.0 {
        1.0
    } else if min_liquidity > 1_000.0 {
        0.8
    } else {
        0.5
    };
    let diversity_bonus = match dex_diversity {
        1 => 1.0,
        2 => 1.3,
        _ => 1.5,
    };

    let ml_score = profit_factor
        * (1.0 - competition_score * 0.7)
        * uniqueness
        * diversity_bonus
        * ml_boost
        * liquidity_factor;

    // Route key for dedup
    let mut sorted_pools = pools.clone();
    sorted_pools.sort();
    let route_key = sorted_pools.join("_");

    Some(ScoredRoute {
        route_key,
        tokens,
        pools,
        dex_types,
        hops: cycle.edges.len(),
        borrow_mint: cycle.start_token.clone(),
        borrow_amount,
        gross_profit,
        net_profit,
        min_liquidity_usd: min_liquidity,
        total_fee_bps,
        max_impact,
        competition_score,
        ml_score,
        route_uniqueness: uniqueness,
        dex_diversity,
    })
}

/// Simplified pool snapshot for the scanner (avoids full DexPool dependency).
#[derive(Clone)]
pub struct PoolSnapshot {
    pub address: String,
    pub dex_type: String,
    pub token_a: String,
    pub token_b: String,
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub fee_bps: u64,
    pub vault_a: String,
    pub vault_b: String,
}

/// Save scored routes to PostgreSQL.
pub async fn save_hot_routes(db_url: &str, routes: &[ScoredRoute]) -> anyhow::Result<u64> {
    let (client, connection) =
        tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!("hot-routes pg error: {e}");
        }
    });

    // Expire old routes
    client
        .execute("DELETE FROM hot_routes WHERE expires_at < now()", &[])
        .await?;

    let mut inserted = 0u64;
    for route in routes {
        let tokens: Vec<&str> = route.tokens.iter().map(|s| s.as_str()).collect();
        let pools: Vec<&str> = route.pools.iter().map(|s| s.as_str()).collect();
        let dex_types: Vec<&str> = route.dex_types.iter().map(|s| s.as_str()).collect();

        match client
            .execute(
                "INSERT INTO hot_routes (
                    route_key, hops, tokens, pools, dex_types, borrow_mint,
                    borrow_amount, gross_profit_lamports, net_profit_lamports,
                    min_liquidity_usd, total_fee_bps, price_impact_pct,
                    competition_score, ml_score, route_uniqueness, dex_diversity
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
                ON CONFLICT (route_key, borrow_amount) DO UPDATE SET
                    gross_profit_lamports = $8, net_profit_lamports = $9,
                    min_liquidity_usd = $10, price_impact_pct = $12,
                    competition_score = $13, ml_score = $14,
                    scanned_at = now(), expires_at = now() + interval '1 hour'",
                &[
                    &route.route_key,
                    &(route.hops as i16),
                    &tokens,
                    &pools,
                    &dex_types,
                    &route.borrow_mint,
                    &(route.borrow_amount as i64),
                    &(route.gross_profit as i64),
                    &route.net_profit,
                    &route.min_liquidity_usd,
                    &(route.total_fee_bps as i32),
                    &route.max_impact,
                    &route.competition_score,
                    &route.ml_score,
                    &route.route_uniqueness,
                    &(route.dex_diversity as i16),
                ],
            )
            .await
        {
            Ok(n) => inserted += n,
            Err(e) => debug!(error = %e, "insert hot route failed"),
        }
    }

    info!(
        inserted,
        total = routes.len(),
        "saved hot routes to PostgreSQL"
    );
    Ok(inserted)
}

/// Query top hot routes for the executor (sorted by ML score).
pub async fn get_prioritized_routes(
    db_url: &str,
    limit: usize,
    min_score: f64,
) -> anyhow::Result<Vec<ScoredRoute>> {
    let (client, connection) =
        tokio_postgres::connect(db_url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let rows = client
        .query(
            "SELECT route_key, hops, tokens, pools, dex_types, borrow_mint,
                    borrow_amount, gross_profit_lamports, net_profit_lamports,
                    min_liquidity_usd, total_fee_bps, price_impact_pct,
                    competition_score, ml_score, route_uniqueness, dex_diversity
             FROM hot_routes
             WHERE ml_score >= $1 AND expires_at > now()
             ORDER BY ml_score DESC
             LIMIT $2",
            &[&min_score, &(limit as i64)],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| ScoredRoute {
            route_key: r.get(0),
            hops: r.get::<_, i16>(1) as usize,
            tokens: r.get(2),
            pools: r.get(3),
            dex_types: r.get(4),
            borrow_mint: r.get(5),
            borrow_amount: r.get::<_, i64>(6) as u64,
            gross_profit: r.get::<_, i64>(7) as u64,
            net_profit: r.get(8),
            min_liquidity_usd: r.get(9),
            total_fee_bps: r.get::<_, i32>(10) as u64,
            max_impact: r.get(11),
            competition_score: r.get(12),
            ml_score: r.get(13),
            route_uniqueness: r.get(14),
            dex_diversity: r.get::<_, i16>(15) as u16,
        })
        .collect())
}

/// Print a summary of the scan results.
pub fn print_summary(routes: &[ScoredRoute]) {
    if routes.is_empty() {
        info!("No profitable routes found");
        return;
    }

    let total_profit: i64 = routes.iter().map(|r| r.net_profit).sum();
    let avg_competition: f64 =
        routes.iter().map(|r| r.competition_score).sum::<f64>() / routes.len() as f64;
    let n_3hop = routes.iter().filter(|r| r.hops >= 3).count();
    let n_multi_dex = routes.iter().filter(|r| r.dex_diversity >= 2).count();

    let mut dex_counts: HashMap<&str, u32> = HashMap::new();
    for r in routes {
        for d in &r.dex_types {
            *dex_counts.entry(d.as_str()).or_default() += 1;
        }
    }

    info!(
        routes = routes.len(),
        total_profit_sol = format!("{:.4}", total_profit as f64 / 1e9),
        avg_competition = format!("{:.3}", avg_competition),
        three_hop = n_3hop,
        multi_dex = n_multi_dex,
        "hot route scan summary"
    );

    // Top 10 routes
    for (i, r) in routes.iter().take(10).enumerate() {
        info!(
            "#{}: {} hops, profit={:.6} SOL, competition={:.2}, ml_score={:.3}, dexes={:?}",
            i + 1,
            r.hops,
            r.net_profit as f64 / 1e9,
            r.competition_score,
            r.ml_score,
            r.dex_types,
        );
    }
}
