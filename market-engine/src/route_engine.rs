// route_engine.rs – Build a weighted directed graph of token pairs and run
//                    Bellman-Ford negative-cycle detection for cyclic arbitrage.
//
// Each edge in the graph represents a swap on a particular DEX pool.
// Edge weights are -log(price), so a negative cycle corresponds to a
// profitable arbitrage round-trip (product of exchange rates > 1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use petgraph::{
    algo::find_negative_cycle,
    graph::{DiGraph, NodeIndex},
    visit::EdgeRef,
};
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, trace, warn};

use crate::{
    oracle::OracleCache,
    orca::{tick_array_start_index as orca_array_start, ORCA_TICK_ARRAY_SIZE},
    pool_state::{DexPool, PoolStateCache},
    types::{DexType, Hop, RouteParams},
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum net profit in lamports to consider an arb opportunity viable.
const MIN_NET_PROFIT_LAMPORTS: i64 = 1_000;

/// Maximum number of hops in a route.
/// 2 hops (same pair, 2 pools) → TX ~700-900 bytes without ALT.
/// 3 hops (A→B→C→A) → TX ~1200-1400 bytes with ALT (~600-700B compressed).
/// ALT reduces account list from ~2000B to ~500-650B, so 3-hop fits comfortably.
const MAX_HOPS: usize = 5;

/// Cyclic arb on liquid venues should not multiply capital in a single loop.
/// Anything above this cap is treated as a bad quote / stale pool hallucination.
/// Maximum plausible return: 20% profit. 3-hop Meteora routes through exotic
/// tokens can legitimately have higher returns due to low competition.
/// Flash loan atomic = safety net, so allow more attempts.
const MAX_RETURN_BPS: u64 = 2000;

/// Maximum per-hop price impact (%).
/// Raised from 5% → 10%: was rejecting profitable cycles at 5.01% impact.
/// Flash loans are atomic — failed TX = 0 cost. Better to attempt more routes.
const MAX_HOP_IMPACT_PCT: f64 = 10.0;

/// Minimum known-side TVL in USD required to route through a pool.
const MIN_KNOWN_TVL_USD: f64 = 10_000.0;

/// Minimum reserve in either token (raw units) to include a pool when oracle
/// TVL data is unavailable.  Filters out dust / empty pools.
const MIN_RESERVE_WITHOUT_ORACLE: u64 = 1_000_000; // ~0.001 SOL or 0.001 USDC equivalent

/// BF explores from these anchor tokens to find negative cycles.
/// USDT re-added: BF may find cycles passing through USDT, then rotation
/// shifts start to WSOL/USDC for actual flash loan execution.
const ANCHOR_TOKENS: &[&str] = &[
    "So11111111111111111111111111111111111111112",    // WSOL
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",  // USDT (explore only, borrow via WSOL/USDC)
];

/// Mints that flash loan providers actually support for borrowing.
const BORROWABLE_MINTS: &[&str] = &[
    "So11111111111111111111111111111111111111112",    // WSOL
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
];

/// Transaction fee per hop (estimated, in lamports).
const TX_FEE_PER_HOP: u64 = 5_000;

// ---------------------------------------------------------------------------
// Token node map
// ---------------------------------------------------------------------------

struct TokenGraph {
    graph: DiGraph<Pubkey, EdgeData>,
    node_map: HashMap<Pubkey, NodeIndex>,
}

#[derive(Debug, Clone)]
struct EdgeData {
    pool: Pubkey,
    dex_program: Pubkey,
    token_in: Pubkey,
    token_out: Pubkey,
    /// -log(effective_price) – negative means profitable.
    weight: f64,
    /// Effective exchange rate (amount_out / amount_in).
    rate: f64,
    fee_bps: u64,
}

impl TokenGraph {
    fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            node_map: HashMap::new(),
        }
    }

    fn get_or_insert_node(&mut self, token: Pubkey) -> NodeIndex {
        if let Some(&idx) = self.node_map.get(&token) {
            idx
        } else {
            let idx = self.graph.add_node(token);
            self.node_map.insert(token, idx);
            idx
        }
    }

    /// Add a directed edge token_a → token_b representing a swap on `pool`.
    fn add_edge(&mut self, pool: &DexPool) {
        // Ignore extremely low liquidity "dust" pools to avoid math hallucinations.
        if pool.reserve_a < 10_000_000 || pool.reserve_b < 10_000_000 {
            return;
        }
        // Blacklist known phantom pools with stale reserves that generate fake arb cycles.
        {
            static BLACKLIST: std::sync::OnceLock<std::collections::HashSet<Pubkey>> = std::sync::OnceLock::new();
            let bl = BLACKLIST.get_or_init(|| {
                let mut s = std::collections::HashSet::new();
                // mSOL/Raydium: $100K liq, $26/day vol → phantom 670K arb every 2s
                if let Ok(p) = "EGyhb2uLAsRUbRx9dNFBjMVYnFaASWMvD6RE1aEf2LxL".parse() { s.insert(p); }
                // USDT thin pool
                if let Ok(p) = "7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX".parse() { s.insert(p); }
                if let Ok(list) = std::env::var("POOL_BLACKLIST") {
                    for addr in list.split(',') {
                        if let Ok(p) = addr.trim().parse() { s.insert(p); }
                    }
                }
                s
            });
            if bl.contains(&pool.pool_address) { return; }
        }

        // Use 0.1 SOL (100M lamports) as the reference amount for weight calculation.
        // At 1M (0.001 SOL), integer division artifacts and fee dominance distort weights
        // and mask real arb opportunities.
        // For stablecoin pairs (6 decimals), use 100_000 (0.1 USDC/USDT).
        let ref_amount: u64 = if is_stablecoin_mint(&pool.token_a) && is_stablecoin_mint(&pool.token_b) {
            100_000 // 0.1 USDC/USDT (6 decimals)
        } else if is_stablecoin_mint(&pool.token_a) || is_stablecoin_mint(&pool.token_b) {
            // Mixed pair: use the SOL-scale amount; the pool's XY=K math handles decimals
            100_000_000
        } else {
            100_000_000 // 0.1 SOL (9 decimals)
        };
        let dex_program = infer_dex_program(pool);

        // For Orca Whirlpool, check that the tick arrays required for each swap
        // direction are actually initialized on-chain.  If the pool only has
        // arrays on one side of the current tick, adding the opposite direction
        // would cause TickArraySequenceInvalidIndex (6038) at execution time.
        let (allow_a_to_b, allow_b_to_a) = if pool.dex_type == DexType::OrcaWhirlpool {
            match &pool.orca_meta {
                Some(meta) if !meta.tick_arrays.is_empty() && meta.tick_spacing > 0 => {
                    let start = orca_array_start(meta.tick_current_index, meta.tick_spacing);
                    let offset = meta.tick_spacing as i32 * ORCA_TICK_ARRAY_SIZE;
                    let has_current = meta.tick_arrays.contains_key(&start);
                    // a_to_b → price moves down → need array at (start - offset)
                    let a_to_b_ok = has_current && meta.tick_arrays.contains_key(&(start - offset));
                    // b_to_a → price moves up → need array at (start + offset)
                    let b_to_a_ok = has_current && meta.tick_arrays.contains_key(&(start + offset));
                    (a_to_b_ok, b_to_a_ok)
                }
                // No tick-array metadata yet — skip this pool entirely.
                _ => (false, false),
            }
        } else {
            (true, true)
        };

        // A → B
        if allow_a_to_b {
            let out = pool.quote_a_to_b(ref_amount);
            if out > 0 {
                let rate = out as f64 / ref_amount as f64;
                let a_idx = self.get_or_insert_node(pool.token_a);
                let b_idx = self.get_or_insert_node(pool.token_b);
                self.graph.add_edge(
                    a_idx,
                    b_idx,
                    EdgeData {
                        pool: pool.pool_address,
                        dex_program,
                        token_in: pool.token_a,
                        token_out: pool.token_b,
                        weight: -rate.ln(),
                        rate,
                        fee_bps: pool.fee_bps,
                    },
                );
            }
        }

        // B → A
        if allow_b_to_a {
            let out = pool.quote_b_to_a(ref_amount);
            if out > 0 {
                let rate = out as f64 / ref_amount as f64;
                let a_idx = self.get_or_insert_node(pool.token_a);
                let b_idx = self.get_or_insert_node(pool.token_b);
                self.graph.add_edge(
                    b_idx,
                    a_idx,
                    EdgeData {
                        pool: pool.pool_address,
                        dex_program,
                        token_in: pool.token_b,
                        token_out: pool.token_a,
                        weight: -rate.ln(),
                        rate,
                        fee_bps: pool.fee_bps,
                    },
                );
            }
        }
    }
}

/// Compute exchange rate and BF weight from reserves.
fn compute_rate(reserve_in: u64, reserve_out: u64, fee_bps: u64) -> (f64, f64) {
    if reserve_in == 0 || reserve_out == 0 {
        return (0.0, 0.0);
    }
    // Reference amount: 100M lamports (0.1 SOL)
    let amount_in = 100_000_000u64.min(reserve_in / 10);
    if amount_in == 0 { return (0.0, 0.0); }
    let fee = amount_in * fee_bps / 10_000;
    let net_in = amount_in.saturating_sub(fee);
    let amount_out = (reserve_out as u128 * net_in as u128 / (reserve_in as u128 + net_in as u128)) as u64;
    if amount_out == 0 { return (0.0, 0.0); }
    let rate = amount_out as f64 / amount_in as f64;
    let weight = -(rate.ln());
    (rate, weight)
}

fn infer_dex_program(pool: &DexPool) -> Pubkey {
    use crate::types::{dex_programs, DexType};
    match pool.dex_type {
        DexType::RaydiumAmmV4 => dex_programs::raydium_amm_v4(),
        DexType::RaydiumClmm => dex_programs::raydium_clmm(),
        DexType::OrcaWhirlpool => dex_programs::orca_whirlpool(),
        DexType::MeteoraDlmm => dex_programs::meteora_dlmm(),
        DexType::PumpSwap => dex_programs::pumpswap(),
        DexType::Fluxbeam => dex_programs::fluxbeam(),
        DexType::Saber => dex_programs::saber(),
        DexType::RaydiumCpmm => dex_programs::raydium_cpmm(),
        DexType::Unknown => Pubkey::default(),
    }
}

// ---------------------------------------------------------------------------
// Route cache (avoids re-running Bellman-Ford when pool state unchanged)
// ---------------------------------------------------------------------------

struct RouteCache {
    swap_gen: u64,
    borrow_amount: u64,
    routes: Vec<RouteParams>,
    computed_at: Instant,
}

impl RouteCache {
    fn empty() -> Self {
        Self {
            swap_gen: u64::MAX,
            borrow_amount: 0,
            routes: Vec::new(),
            computed_at: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// RouteEngine
// ---------------------------------------------------------------------------

pub struct RouteEngine {
    pool_cache: Arc<PoolStateCache>,
    oracle_cache: Option<Arc<OracleCache>>,
    route_cache: Mutex<RouteCache>,
    /// Persistent graph — rebuilt only when pool set changes, weights updated incrementally.
    persistent_graph: Mutex<Option<PersistentGraph>>,
    /// Last observed total pools (for diagnostics).
    last_total_pools: std::sync::atomic::AtomicUsize,
    /// Last observed pools added to graph (for diagnostics).
    last_pools_in_graph: std::sync::atomic::AtomicUsize,
    /// Blacklist for BF cycles that failed re-quote. Keyed by sorted pool set.
    /// Prevents re-evaluating the same phantom cycle every second.
    rejected_cycles: Mutex<HashMap<Vec<Pubkey>, Instant>>,
}

/// Persistent graph that avoids full rebuild every BF run.
/// Nodes (tokens) are stable. Edge weights are updated in-place from fresh reserves.
struct PersistentGraph {
    graph: TokenGraph,
    /// pool_address → Vec<EdgeIndex> for fast weight updates.
    pool_edges: HashMap<Pubkey, Vec<petgraph::graph::EdgeIndex>>,
    /// Number of pools when graph was built. If pool count changes, full rebuild.
    n_pools: usize,
    built_at: Instant,
}

impl RouteEngine {
    pub fn new(pool_cache: Arc<PoolStateCache>, oracle_cache: Option<Arc<OracleCache>>) -> Self {
        Self {
            persistent_graph: Mutex::new(None),
            pool_cache,
            oracle_cache,
            route_cache: Mutex::new(RouteCache::empty()),
            last_total_pools: std::sync::atomic::AtomicUsize::new(0),
            last_pools_in_graph: std::sync::atomic::AtomicUsize::new(0),
            rejected_cycles: Mutex::new(HashMap::new()),
        }
    }

    pub fn last_total_pools(&self) -> usize {
        self.last_total_pools.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn last_pools_in_graph(&self) -> usize {
        self.last_pools_in_graph.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Rebuild the token graph from the current pool cache and run
    /// Bellman-Ford to discover negative cycles.
    ///
    /// Returns a list of profitable `RouteParams` sorted by net profit (desc).
    /// Find arb opportunities trying multiple borrow amounts per cycle.
    /// Bellman-Ford runs once; `cycle_to_route` is tried with each amount.
    pub fn find_opportunities_multi(
        &self,
        borrow_amounts: &[u64],
    ) -> Vec<RouteParams> {
        let current_swap_gen = self.pool_cache.swap_generation();

        // Fast path: return cached result if pool state unchanged and cache is fresh (< 500ms).
        // Reduced from 5s to 500ms for competitive arb — stale routes lose to faster bots.
        if let Ok(cache) = self.route_cache.lock() {
            if cache.swap_gen == current_swap_gen
                && cache.computed_at.elapsed().as_millis() < 500
            {
                trace!(
                    swap_gen = current_swap_gen,
                    "route cache hit – skipping Bellman-Ford"
                );
                return cache.routes.clone();
            }
        }

        let routes = self.find_opportunities_inner(current_swap_gen, borrow_amounts);

        // Update cache (borrow_amount no longer part of key — cycles are amount-independent).
        if let Ok(mut cache) = self.route_cache.lock() {
            *cache = RouteCache {
                swap_gen: current_swap_gen,
                borrow_amount: 0,
                routes: routes.clone(),
                computed_at: Instant::now(),
            };
        }
        routes
    }

    /// Single borrow amount (backward compat).
    pub fn find_opportunities(&self, borrow_amount: u64) -> Vec<RouteParams> {
        self.find_opportunities_multi(&[borrow_amount])
    }

    fn find_opportunities_inner(
        &self,
        current_swap_gen: u64,
        borrow_amounts: &[u64],
    ) -> Vec<RouteParams> {
        let oracle_prices = self.oracle_cache.as_ref().map(|cache| cache.snapshot());
        let pools: Vec<DexPool> = self.pool_cache.inner_iter().collect();
        let total_pools = pools.len();
        let mut pools_with_reserves = 0usize;

        for pool in &pools {
            if pool.reserve_a > 0 && pool.reserve_b > 0 {
                pools_with_reserves += 1;
            }
        }

        // Persistent graph: full rebuild only when pool count changes or >60s old.
        // Otherwise: update edge weights in-place from fresh reserves (~1-5ms vs ~200ms).
        let mut pg_lock = self.persistent_graph.lock().unwrap();
        let need_rebuild = pg_lock.as_ref().map_or(true, |pg| {
            pg.n_pools != total_pools || pg.built_at.elapsed().as_secs() > 60
        });

        if need_rebuild {
            let t_build = Instant::now();
            let mut tg = TokenGraph::new();
            let mut pool_edges: HashMap<Pubkey, Vec<petgraph::graph::EdgeIndex>> = HashMap::new();
            let mut pools_added = 0usize;

            for pool in &pools {
                if !pool_is_eligible(pool, oracle_prices.as_ref()) {
                    continue;
                }
                let edges_before = tg.graph.edge_count();
                tg.add_edge(pool);
                let edges_after = tg.graph.edge_count();
                if edges_after > edges_before {
                    pools_added += 1;
                    // Track which edges belong to this pool for incremental updates
                    let mut edge_indices = Vec::new();
                    for idx in edges_before..edges_after {
                        edge_indices.push(petgraph::graph::EdgeIndex::new(idx));
                    }
                    pool_edges.insert(pool.pool_address, edge_indices);
                }
            }

            self.last_total_pools.store(total_pools, std::sync::atomic::Ordering::Relaxed);
            self.last_pools_in_graph.store(pools_added, std::sync::atomic::Ordering::Relaxed);

            info!(
                total_pools,
                pools_with_reserves,
                pools_in_graph = pools_added,
                nodes = tg.node_map.len(),
                edges = tg.graph.edge_count(),
                build_ms = t_build.elapsed().as_millis(),
                "graph REBUILT (full)"
            );

            *pg_lock = Some(PersistentGraph {
                graph: tg,
                pool_edges,
                n_pools: total_pools,
                built_at: Instant::now(),
            });
        } else {
            // FAST PATH: update edge weights in-place from fresh reserves (~1-5ms)
            let t_update = Instant::now();
            let pg = pg_lock.as_mut().unwrap();
            let mut updated = 0u32;

            for pool in &pools {
                if pool.reserve_a == 0 || pool.reserve_b == 0 { continue; }
                if let Some(edge_indices) = pg.pool_edges.get(&pool.pool_address) {
                    for &eidx in edge_indices {
                        if let Some(edge) = pg.graph.graph.edge_weight_mut(eidx) {
                            // Recompute rate and weight from fresh reserves
                            let (new_rate, new_weight) = if edge.token_in == pool.token_a {
                                compute_rate(pool.reserve_a, pool.reserve_b, pool.fee_bps)
                            } else {
                                compute_rate(pool.reserve_b, pool.reserve_a, pool.fee_bps)
                            };
                            if new_rate > 0.0 {
                                edge.rate = new_rate;
                                edge.weight = new_weight;
                                updated += 1;
                            }
                        }
                    }
                }
            }

            self.last_total_pools.store(total_pools, std::sync::atomic::Ordering::Relaxed);

            trace!(
                updated,
                update_us = t_update.elapsed().as_micros(),
                "graph weights updated (incremental)"
            );
        }

        let pg = pg_lock.as_ref().unwrap();
        let tg = &pg.graph;
        let pools_added = pg.pool_edges.len();

        // Connectivity diagnostics: check that anchor tokens are present in the graph.
        let anchor_connected: Vec<&str> = ANCHOR_TOKENS
            .iter()
            .filter(|s| {
                s.parse::<Pubkey>()
                    .ok()
                    .and_then(|pk| tg.node_map.get(&pk).copied())
                    .map_or(false, |idx| tg.graph.edges(idx).next().is_some())
            })
            .copied()
            .collect();

        info!(
            total_pools,
            pools_with_reserves,
            pools_in_graph = pools_added,
            nodes = tg.graph.node_count(),
            edges = tg.graph.edge_count(),
            anchor_connected = anchor_connected.len(),
            "token graph built"
        );
        if anchor_connected.len() < ANCHOR_TOKENS.len() {
            let missing: Vec<&str> = ANCHOR_TOKENS
                .iter()
                .filter(|s| !anchor_connected.contains(s))
                .copied()
                .collect();
            warn!(
                connected = anchor_connected.len(),
                missing = ?missing,
                "anchor tokens NOT all connected — BF will miss cycles"
            );
        }

        if tg.graph.node_count() < 2 {
            trace!("token graph too small, skipping Bellman-Ford");
            return Vec::new();
        }

        debug!(
            nodes = tg.graph.node_count(),
            edges = tg.graph.edge_count(),
            "running Bellman-Ford on token graph"
        );

        // Build a f64-weighted graph for Bellman-Ford (FloatMeasure is required).
        // The EdgeData graph is kept for route reconstruction.
        let weight_graph = tg.graph.map(|_, n| *n, |_, e| e.weight);

        // Run negative-cycle detection from anchor tokens only.
        // Running from all nodes is O(V²·E) which takes minutes on large graphs.
        let mut routes: Vec<RouteParams> = Vec::new();
        let anchor_pubkeys: Vec<Pubkey> = ANCHOR_TOKENS
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();
        let start_nodes: Vec<NodeIndex> = anchor_pubkeys
            .iter()
            .filter_map(|pk| tg.node_map.get(pk).copied())
            .collect();

        let mut bf_raw_cycles = 0u32;
        let mut bf_rejected_requote = 0u32;
        let mut bf_rejected_profit = 0u32;
        let mut bf_rejected_maxreturn = 0u32;

        // Prune expired blacklist entries (>10s old).
        {
            let mut bl = self.rejected_cycles.lock().unwrap();
            bl.retain(|_, t| t.elapsed().as_secs() < 10);
        }

        for &start in &start_nodes {
            if let Some(cycle_nodes) = find_negative_cycle(&weight_graph, start) {
                bf_raw_cycles += 1;

                // Build a canonical key for this cycle (sorted pool set from edges).
                let cycle_key: Vec<Pubkey> = {
                    let mut pools: Vec<Pubkey> = Vec::new();
                    for w in cycle_nodes.windows(2) {
                        if let Some(edge) = tg.graph.find_edge(w[0], w[1]) {
                            pools.push(tg.graph[edge].pool);
                        }
                    }
                    // Close the cycle
                    if cycle_nodes.len() >= 2 {
                        if let Some(edge) = tg.graph.find_edge(*cycle_nodes.last().unwrap(), cycle_nodes[0]) {
                            pools.push(tg.graph[edge].pool);
                        }
                    }
                    pools.sort();
                    pools
                };

                // Skip if this cycle was recently rejected (blacklisted for 10s).
                if !cycle_key.is_empty() {
                    let bl = self.rejected_cycles.lock().unwrap();
                    if bl.contains_key(&cycle_key) {
                        trace!("skipping blacklisted BF cycle ({} pools)", cycle_key.len());
                        continue;
                    }
                }

                // Try each borrow amount — different scales can be profitable.
                let mut best_route: Option<RouteParams> = None;
                let mut any_passed_requote = false;
                for &borrow_amount in borrow_amounts {
                    if let Some(route) =
                        self.cycle_to_route(&tg, &cycle_nodes, borrow_amount)
                    {
                        any_passed_requote = true;
                        if route.net_profit >= MIN_NET_PROFIT_LAMPORTS {
                            if best_route
                                .as_ref()
                                .map_or(true, |b| route.net_profit > b.net_profit)
                            {
                                best_route = Some(route);
                            }
                        } else {
                            bf_rejected_profit += 1;
                        }
                    }
                }
                if !any_passed_requote {
                    bf_rejected_requote += 1;
                    // Blacklist this cycle for 10 seconds to stop repeated re-evaluation.
                    if !cycle_key.is_empty() {
                        self.rejected_cycles.lock().unwrap().insert(cycle_key, Instant::now());
                    }
                }
                if let Some(route) = best_route {
                    info!(
                        net_profit = route.net_profit,
                        borrow = route.borrow_amount,
                        hops = route.hops.len(),
                        "arbitrage opportunity found"
                    );
                    routes.push(route);
                }
            }
        }
        if !routes.is_empty() {
            info!(
                bf_raw_cycles,
                bf_rejected_requote,
                bf_rejected_profit,
                accepted = routes.len(),
                "bellman-ford cycle diagnostics"
            );
        } else if bf_raw_cycles > 0 {
            info!(
                bf_raw_cycles,
                bf_rejected_requote,
                bf_rejected_profit,
                bf_rejected_maxreturn,
                "bellman-ford cycles found but all rejected"
            );
        }

        routes.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
        routes
    }

    /// Convert a detected negative cycle into a concrete `RouteParams` with
    /// simulated amounts. The cycle order comes from petgraph; we only choose
    /// among multiple parallel pools for the same token pair.
    fn cycle_to_route(
        &self,
        tg: &TokenGraph,
        cycle_nodes: &[NodeIndex],
        borrow_amount: u64,
    ) -> Option<RouteParams> {
        if cycle_nodes.len() < 2 || cycle_nodes.len() > MAX_HOPS {
            info!(
                cycle_len = cycle_nodes.len(),
                max_hops = MAX_HOPS,
                tokens = ?cycle_nodes.iter().map(|n| tg.graph[*n].to_string()[..8].to_string()).collect::<Vec<_>>(),
                "cycle_to_route: rejected (len out of range)"
            );
            return None;
        }

        let mut current_amount = borrow_amount;
        let mut hops: Vec<Hop> = Vec::new();

        for i in 0..cycle_nodes.len() {
            let from = cycle_nodes[i];
            let to = cycle_nodes[(i + 1) % cycle_nodes.len()];
            let result = self.best_edge_for_pair(tg, from, to, current_amount);
            let (edge, amount_out, impact) = match result {
                Some(r) => r,
                None => {
                    info!(hop = i, from = %tg.graph[from], to = %tg.graph[to], current_amount, "cycle_to_route: no edge for pair");
                    return None;
                }
            };

            if impact > MAX_HOP_IMPACT_PCT {
                info!(hop = i, impact, pool = %edge.pool, "cycle_to_route: pre-rotation high impact");
                return None;
            }

            hops.push(Hop {
                pool: edge.pool,
                dex_program: edge.dex_program,
                token_in: edge.token_in,
                token_out: edge.token_out,
                amount_out,
                price_impact: impact,
            });

            current_amount = amount_out;
        }

        if hops.len() < 2 {
            return None;
        }

        let pre_rotation_return = current_amount;

        // Ensure the cycle starts at a borrowable token (for flash loan).
        let anchor_set: std::collections::HashSet<Pubkey> = ANCHOR_TOKENS
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();
        let borrowable_set: std::collections::HashSet<Pubkey> = BORROWABLE_MINTS
            .iter()
            .filter_map(|s| s.parse::<Pubkey>().ok())
            .collect();

        // First try: rotate to a borrowable anchor
        let start_borrowable = hops.iter().position(|h| borrowable_set.contains(&h.token_in));
        if let Some(pos) = start_borrowable {
            hops.rotate_left(pos);
        } else {
            // No borrowable token in cycle. Try to bridge: find a hop whose token_in
            // has a pool connecting it to WSOL, then prepend WSOL→token_in and append
            // token_in→WSOL to make a longer cycle starting from WSOL.
            let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
            let first_token = hops[0].token_in;
            if let Some((_, bridge_out, bridge_impact)) = self.best_edge_for_pair(tg,
                *tg.node_map.get(&wsol).unwrap_or(&petgraph::graph::NodeIndex::new(0)),
                *tg.node_map.get(&first_token).unwrap_or(&petgraph::graph::NodeIndex::new(0)),
                borrow_amount)
            {
                let bridge_pool = self.pool_cache.inner.iter()
                    .find(|e| {
                        let p = e.value();
                        (p.token_a == wsol && p.token_b == first_token) ||
                        (p.token_b == wsol && p.token_a == first_token)
                    })
                    .map(|e| *e.key());
                let return_pool = self.pool_cache.inner.iter()
                    .find(|e| {
                        let p = e.value();
                        (p.token_a == first_token && p.token_b == wsol) ||
                        (p.token_b == first_token && p.token_a == wsol)
                    })
                    .map(|e| *e.key());

                if let (Some(bp), Some(rp)) = (bridge_pool, return_pool) {
                    if let Some(bp_entry) = self.pool_cache.get(&bp) {
                        let dex_prog = infer_dex_program(&bp_entry);
                        // Prepend WSOL→first_token bridge hop
                        let bridge_hop = Hop {
                            pool: bp,
                            dex_program: dex_prog,
                            token_in: wsol,
                            token_out: first_token,
                            amount_out: bridge_out,
                            price_impact: bridge_impact,
                        };
                        hops.insert(0, bridge_hop);
                        // Append first_token→WSOL return hop (quote later in re-quote)
                        if let Some(rp_entry) = self.pool_cache.get(&rp) {
                            let rp_dex = infer_dex_program(&rp_entry);
                            hops.push(Hop {
                                pool: rp,
                                dex_program: rp_dex,
                                token_in: first_token,
                                token_out: wsol,
                                amount_out: 0, // re-quoted below
                                price_impact: 0.0,
                            });
                        }
                        info!(
                            original_tokens = ?vec![first_token.to_string()[..8].to_string()],
                            bridged_hops = hops.len(),
                            "cycle_to_route: bridged non-anchor cycle via WSOL"
                        );
                    }
                } else {
                    debug!(tokens = ?hops.iter().map(|h| h.token_in.to_string()[..8].to_string()).collect::<Vec<_>>(), "cycle_to_route: no anchor and no bridge available");
                    return None;
                }
            } else {
                debug!(tokens = ?hops.iter().map(|h| h.token_in.to_string()[..8].to_string()).collect::<Vec<_>>(), "cycle_to_route: no anchor and no WSOL bridge");
                return None;
            }
        }

        // Final check: first hop must be borrowable
        let borrow_mint = hops[0].token_in;
        if !borrowable_set.contains(&borrow_mint) {
            debug!(mint = %borrow_mint, "cycle_to_route: borrow mint not borrowable after rotation/bridge");
            return None;
        }

        // Re-quote the rotated cycle from borrow_amount.
        current_amount = borrow_amount;
        for (hi, hop) in hops.iter_mut().enumerate() {
            let pool = match self.pool_cache.get(&hop.pool) {
                Some(p) => p,
                None => {
                    debug!(hop = hi, pool = %hop.pool, "cycle_to_route: pool not in cache");
                    return None;
                }
            };
            let amount_out = if hop.token_in == pool.token_a {
                pool.quote_a_to_b(current_amount)
            } else {
                pool.quote_b_to_a(current_amount)
            };
            if amount_out == 0 {
                info!(
                    hop = hi,
                    pool = %hop.pool,
                    token_in = %hop.token_in,
                    current_amount,
                    reserve_a = pool.reserve_a,
                    reserve_b = pool.reserve_b,
                    dex = ?pool.dex_type,
                    "cycle_to_route: zero quote after rotation"
                );
                return None;
            }
            let reserve_in = if hop.token_in == pool.token_a {
                pool.reserve_a
            } else {
                pool.reserve_b
            };
            // Reserve must be >2x input. Lowered from 5x: flash loans are atomic,
            // prefer more attempts. With min_profit=0 on-chain, the TX reverts if unprofitable.
            if reserve_in < current_amount.saturating_mul(2) {
                info!(hop = hi, pool = %hop.pool, reserve_in, amount_in = current_amount,
                    "cycle_to_route: reserve too thin vs input (<2x)");
                return None;
            }
            let impact = (current_amount as f64 / reserve_in.max(1) as f64) * 100.0;
            if impact > MAX_HOP_IMPACT_PCT {
                info!(hop = hi, impact, pool = %hop.pool, "cycle_to_route: post-rotation high impact");
                return None;
            }
            hop.amount_out = amount_out;
            hop.price_impact = impact;
            current_amount = amount_out;
        }

        let gross_profit_signed = current_amount as i64 - borrow_amount as i64;
        let tx_fee = TX_FEE_PER_HOP * hops.len() as u64;
        let net_profit_signed = gross_profit_signed - tx_fee as i64;

        // Log re-quoted cycles for diagnostics.
        {
            info!(
                borrow_amount,
                final_amount = current_amount,
                pre_rotation_return,
                gross_profit = gross_profit_signed,
                net_profit = net_profit_signed,
                hops = hops.len(),
                "cycle_to_route: re-quoted result"
            );
        }

        let max_return = borrow_amount.saturating_add(borrow_amount / 10_000 * MAX_RETURN_BPS);
        if current_amount > max_return {
            // Log phantom pool details to identify quoting bugs
            let hop_details: Vec<String> = hops.iter().enumerate().map(|(i, h)| {
                let dex = self.pool_cache.get(&h.pool)
                    .map(|p| format!("{:?}", p.dex_type))
                    .unwrap_or_else(|| "?".to_string());
                format!("hop{}:{}({})→impact={:.1}%", i, &h.pool.to_string()[..8], dex, h.price_impact)
            }).collect();
            let return_pct = (current_amount as f64 / borrow_amount as f64 - 1.0) * 100.0;
            debug!(
                borrow_amount,
                current_amount,
                return_pct = format!("{:.1}%", return_pct),
                hops = ?hop_details,
                "PHANTOM ROUTE: implausible return — likely quoting bug in one of these pools"
            );
            return None;
        }

        let gross_profit = current_amount.saturating_sub(borrow_amount);
        let tx_fee = TX_FEE_PER_HOP * hops.len() as u64;
        let net_profit = gross_profit as i64 - tx_fee as i64;

        // Diagnostic: log routes with high predicted profit to debug quoting accuracy.
        if net_profit > 50_000_000 {
            for (i, hop) in hops.iter().enumerate() {
                if let Some(pool) = self.pool_cache.get(&hop.pool) {
                    let has_tick_arrays = pool.orca_meta.as_ref().map_or(false, |m| !m.tick_arrays.is_empty());
                    let sqrt_price = pool.orca_meta.as_ref().map_or(0u128, |m| m.sqrt_price);
                    let liquidity = pool.orca_meta.as_ref().map_or(0u128, |m| m.liquidity);
                    tracing::debug!(
                        hop = i,
                        pool = %hop.pool,
                        dex = ?pool.dex_type,
                        token_in = %hop.token_in,
                        token_out = %hop.token_out,
                        amount_out = hop.amount_out,
                        impact = hop.price_impact,
                        reserve_a = pool.reserve_a,
                        reserve_b = pool.reserve_b,
                        fee_bps = pool.fee_bps,
                        has_tick_arrays,
                        sqrt_price,
                        liquidity,
                        "high-profit route hop detail"
                    );
                }
            }
            tracing::warn!(
                borrow_amount,
                final_amount = current_amount,
                net_profit,
                "high-profit route summary"
            );
        }
        let risk_factor = compute_risk_factor(&hops);

        Some(RouteParams {
            hops,
            borrow_amount,
            gross_profit,
            net_profit,
            risk_factor,
            strategy: "bellman-ford",
            tier: 0,
        })
    }

    fn best_edge_for_pair(
        &self,
        tg: &TokenGraph,
        from: NodeIndex,
        to: NodeIndex,
        amount_in: u64,
    ) -> Option<(EdgeData, u64, f64)> {
        let mut best: Option<(EdgeData, u64, f64)> = None;

        for edge_ref in tg.graph.edges(from).filter(|edge| edge.target() == to) {
            let edge = edge_ref.weight().clone();
            let pool = match self.pool_cache.get(&edge.pool) {
                Some(pool) => pool,
                None => continue,
            };

            let amount_out = if edge.token_in == pool.token_a {
                pool.quote_a_to_b(amount_in)
            } else {
                pool.quote_b_to_a(amount_in)
            };
            if amount_out == 0 {
                continue;
            }

            let reserve_in = if edge.token_in == pool.token_a {
                pool.reserve_a
            } else {
                pool.reserve_b
            };
            let impact = (amount_in as f64 / reserve_in.max(1) as f64) * 100.0;

            match &best {
                Some((_, best_out, best_impact))
                    if amount_out < *best_out
                        || (amount_out == *best_out && impact >= *best_impact) => {}
                _ => best = Some((edge, amount_out, impact)),
            }
        }

        best
    }
}

/// Returns true if the mint is USDC or USDT (6-decimal stablecoins).
fn is_stablecoin_mint(mint: &Pubkey) -> bool {
    let s = mint.to_string();
    s == "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" // USDC
        || s == "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" // USDT
}

fn pool_is_eligible(pool: &DexPool, oracle_prices: Option<&HashMap<Pubkey, f64>>) -> bool {
    // RaydiumClmm: allow pools that have valid orca_meta with tick_spacing > 0.
    // These use the concentrated-liquidity quote engine (not XY=K), so quotes are accurate.
    // Pools without orca_meta still produce phantom cycles — reject those.
    if pool.dex_type == crate::types::DexType::RaydiumClmm {
        match &pool.orca_meta {
            Some(m) if m.tick_spacing > 0 && m.liquidity > 0 && m.sqrt_price > 0 => {
                // Valid CLMM metadata — allow through to normal reserve/TVL checks below.
            }
            _ => return false,
        }
    }

    // RaydiumCpmm: TX builder doesn't support this DEX yet.
    // Routes with CPMM pools always fail at execution ("DEX RaydiumCpmm not yet supported").
    if pool.dex_type == crate::types::DexType::RaydiumCpmm {
        return false;
    }

    // Raydium AMM V4 pools require COMPLETE OpenBook metadata to build TX.
    // Skip pools with missing or incomplete metadata (default pubkeys in market fields).
    if pool.dex_type == crate::types::DexType::RaydiumAmmV4 {
        match &pool.raydium_meta {
            None => return false,
            Some(meta) => {
                let default = solana_sdk::pubkey::Pubkey::default();
                // Reject pools with unresolved vault addresses (SystemProgram placeholder).
                // These come from mapped_pools.json entries where vault_a/vault_b were
                // never populated from on-chain state — the TX builder would use wrong accounts.
                if meta.vault_a == default || meta.vault_b == default {
                    return false;
                }
                if meta.market_program == default
                    || meta.market_event_queue == default
                    || meta.market_bids == default
                    || meta.market_asks == default
                    || meta.market_base_vault == default
                    || meta.market_quote_vault == default
                    || meta.market_vault_signer == default
                {
                    return false;
                }
            }
        }
    }

    if let Some(oracle_prices) = oracle_prices {
        if let Some(known_tvl_usd) = pool.known_tvl_usd_lower_bound(oracle_prices) {
            return known_tvl_usd >= MIN_KNOWN_TVL_USD;
        }
    }
    // No oracle data: require minimum absolute reserves to filter out dust pools.
    if pool.reserve_a < MIN_RESERVE_WITHOUT_ORACLE || pool.reserve_b < MIN_RESERVE_WITHOUT_ORACLE {
        return false;
    }

    // Ratio sanity check: reject pools with extreme reserve imbalance.
    // A pool with 1,352 SOL and 15.2M USDC (ratio 11,265:1) is broken.
    // Normal SOL/USDC pools have ratio ~130:1 (at $130/SOL).
    // Allow up to 10x the expected ratio to handle volatile tokens.
    let ratio = if pool.reserve_a > pool.reserve_b {
        pool.reserve_a as f64 / pool.reserve_b.max(1) as f64
    } else {
        pool.reserve_b as f64 / pool.reserve_a.max(1) as f64
    };
    // Max ratio 100,000:1 — any higher is a broken/exploited pool.
    if ratio > 100_000.0 {
        return false;
    }

    true
}

/// Compute a composite risk factor based on price impact across hops.
fn compute_risk_factor(hops: &[Hop]) -> f64 {
    let max_impact = hops.iter().map(|h| h.price_impact).fold(0.0_f64, f64::max);
    // Clamp to [0, 1].
    (max_impact / 100.0).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pool_state::{DexPool, PoolStateCache},
        types::DexType,
    };
    use std::time::Instant;

    fn pk(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    fn pool(address: u8, token_a: u8, token_b: u8, reserve_a: u64, reserve_b: u64) -> DexPool {
        DexPool {
            pool_address: pk(address),
            dex_type: DexType::RaydiumAmmV4,
            token_a: pk(token_a),
            token_b: pk(token_b),
            decimals_a: 6,
            decimals_b: 6,
            reserve_a,
            reserve_b,
            pool_vault_a_balance: reserve_a,
            pool_vault_b_balance: reserve_b,
            market_vault_a_balance: 0,
            market_vault_b_balance: 0,
            last_updated: Instant::now(),
            fee_bps: 0,
            raydium_meta: None,
            orca_meta: None,
            meteora_meta: None,
        }
    }

    #[test]
    fn cycle_to_route_uses_detected_cycle_order_instead_of_greedy_edge() {
        let cache = Arc::new(PoolStateCache::new());
        let ab = pool(11, 1, 2, 1_000_000_000, 1_100_000_000);
        let bc = pool(12, 2, 3, 1_000_000_000, 1_100_000_000);
        let ca = pool(13, 3, 1, 1_000_000_000, 1_100_000_000);
        let bd = pool(14, 2, 4, 1_000_000_000, 2_000_000_000);

        for p in [&ab, &bc, &ca, &bd] {
            cache.upsert(p.clone());
        }

        let engine = RouteEngine::new(cache, None);
        let mut tg = TokenGraph::new();
        for p in [&ab, &bc, &ca, &bd] {
            tg.add_edge(p);
        }

        let route = engine
            .cycle_to_route(
                &tg,
                &[
                    tg.node_map[&pk(1)],
                    tg.node_map[&pk(2)],
                    tg.node_map[&pk(3)],
                ],
                1_000_000,
            )
            .expect("route should be reconstructed");

        assert_eq!(route.hops.len(), 3);
        assert_eq!(route.hops[0].token_in, pk(1));
        assert_eq!(route.hops[0].token_out, pk(2));
        assert_eq!(route.hops[1].token_in, pk(2));
        assert_eq!(route.hops[1].token_out, pk(3));
        assert_eq!(route.hops[2].token_in, pk(3));
        assert_eq!(route.hops[2].token_out, pk(1));
    }

    #[test]
    fn pool_is_eligible_rejects_low_known_tvl_pool() {
        let pool = pool(21, 1, 2, 1_000_000, 1_000_000);
        let mut oracle_prices = HashMap::new();
        oracle_prices.insert(pk(1), 1.0);

        assert!(!pool_is_eligible(&pool, Some(&oracle_prices)));
    }

    #[test]
    fn pool_is_eligible_accepts_partial_oracle_coverage_when_known_side_is_large_enough() {
        let pool = pool(22, 1, 2, 6_000_000_000, 100);
        let mut oracle_prices = HashMap::new();
        oracle_prices.insert(pk(1), 1.0);

        assert!(pool_is_eligible(&pool, Some(&oracle_prices)));
    }
}
