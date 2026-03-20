//! pool_hydrator.rs – Bootstrap and refresh the PoolStateCache.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::time::{Duration, Instant};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::{debug, error, info, trace, warn};

use crate::meteora::{
    candidate_bin_array_indices, decode_bin_array, decode_lb_pair, derive_bin_array_pda,
};
use crate::orca::{
    decode_tick_array as decode_orca_tick_array,
    tick_array_start_indexes as orca_tick_array_start_indexes,
};
use crate::pool_state::{DexPool, OrcaMetadata, PoolStateCache, RaydiumMetadata};
use crate::types::DexType;

const AUTHORITY_AMM_SEED: &[u8] = b"amm authority";

#[derive(Clone, Copy)]
struct RaydiumAmmLayout {
    coin_vault_offset: usize,
    pc_vault_offset: usize,
    open_orders_offset: usize,
    market_offset: usize,
    market_program_offset: usize,
    target_orders_offset: usize,
}

const RAYDIUM_AMM_LAYOUTS: [RaydiumAmmLayout; 2] = [
    RaydiumAmmLayout {
        coin_vault_offset: 336,
        pc_vault_offset: 368,
        open_orders_offset: 496,
        market_offset: 528,
        market_program_offset: 560,
        target_orders_offset: 592,
    },
    RaydiumAmmLayout {
        coin_vault_offset: 224,
        pc_vault_offset: 256,
        open_orders_offset: 160,
        market_offset: 320,
        market_program_offset: 352,
        target_orders_offset: 384,
    },
];

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MappedPool {
    pub pool_id: String,
    pub dex: String,
    #[serde(default)]
    pub symbol: String,
    pub vault_a: String,
    pub vault_b: String,
    pub mint_a: String,
    pub mint_b: String,
    #[serde(default = "default_decimals")]
    pub decimals_a: u8,
    #[serde(default = "default_decimals")]
    pub decimals_b: u8,
    #[serde(default)]
    pub fee_bps: Option<u64>,
}

fn default_decimals() -> u8 { 9 }

impl MappedPool {
    pub fn to_dex_type(&self) -> DexType {
        let s = self.dex.to_lowercase();
        if s.contains("raydium") && s.contains("clmm") {
            DexType::RaydiumClmm
        } else if s.contains("cpmm") || s.contains("cpswap") {
            DexType::RaydiumCpmm
        } else if s.contains("raydium") {
            DexType::RaydiumAmmV4
        } else if s.contains("orca") || s.contains("whirlpool") {
            DexType::OrcaWhirlpool
        } else if s.contains("meteora") || s.contains("dlmm") {
            DexType::MeteoraDlmm
        } else if s.contains("pump") {
            DexType::PumpSwap
        } else if s.contains("flux") {
            DexType::Fluxbeam
        } else if s.contains("saber") || s.contains("stable") {
            DexType::Saber
        } else {
            DexType::Unknown
        }
    }
}

pub struct PoolHydrator {
    rpc: RpcClient,
    fallback_rpc: Option<RpcClient>,
    cache: Arc<PoolStateCache>,
    pools_file: String,
}

impl PoolHydrator {
    pub fn new(rpc_url: &str, cache: Arc<PoolStateCache>, pools_file: &str) -> Self {
        // Fallback RPC: used when primary (e.g. Agave local) is down.
        // Set POOL_RPC_FALLBACK_URL to sim-server or Helius.
        let fallback_rpc = std::env::var("POOL_RPC_FALLBACK_URL").ok().map(|url| {
            info!(fallback_rpc = %url, "pool hydrator fallback RPC configured");
            RpcClient::new_with_timeout_and_commitment(
                url,
                Duration::from_secs(10),
                CommitmentConfig::processed(),
            )
        });
        Self {
            rpc: RpcClient::new_with_timeout_and_commitment(
                rpc_url.to_string(),
                Duration::from_secs(10),
                CommitmentConfig::processed(),
            ),
            fallback_rpc,
            cache,
            pools_file: pools_file.to_string(),
        }
    }

    /// Fetch multiple accounts with retry on failure (rate limit, transient errors).
    /// Falls back to secondary RPC if primary is unreachable.
    fn get_multiple_accounts_retry(
        &self,
        keys: &[Pubkey],
    ) -> Option<Vec<Option<solana_sdk::account::Account>>> {
        // Try primary with fast-fail on connection errors.
        match self.rpc.get_multiple_accounts(keys) {
            Ok(accs) => return Some(accs),
            Err(e) => {
                let err_str = e.to_string();
                let is_conn_error = err_str.contains("Connection refused")
                    || err_str.contains("connect error")
                    || err_str.contains("connection closed");

                if !is_conn_error {
                    // Transient error (rate limit, timeout) — retry primary once.
                    std::thread::sleep(Duration::from_millis(200));
                    if let Ok(accs) = self.rpc.get_multiple_accounts(keys) {
                        return Some(accs);
                    }
                }
                // else: connection error — skip retries, go straight to fallback.

                // Try fallback RPC if configured.
                if let Some(fallback) = &self.fallback_rpc {
                    match fallback.get_multiple_accounts(keys) {
                        Ok(accs) => return Some(accs),
                        Err(fe) => {
                            // Fallback also failed — retry fallback once for rate limits.
                            std::thread::sleep(Duration::from_millis(300));
                            match fallback.get_multiple_accounts(keys) {
                                Ok(accs) => return Some(accs),
                                Err(fe2) => {
                                    warn!(
                                        primary_error = %e,
                                        fallback_error = %fe2,
                                        keys = keys.len(),
                                        "RPC batch failed on both primary and fallback"
                                    );
                                }
                            }
                        }
                    }
                } else {
                    warn!(error = %e, keys = keys.len(), "RPC batch failed (no fallback)");
                }
            }
        }
        None
    }

    pub fn load_from_file(&self) -> Result<usize> {
        let content = std::fs::read_to_string(&self.pools_file)
            .with_context(|| format!("read {}", self.pools_file))?;
        let mapped: Vec<MappedPool> =
            serde_json::from_str(&content).context("parse mapped_pools.json")?;

        for m in &mapped {
            let Ok(pool_id) = m.pool_id.parse::<Pubkey>() else {
                continue;
            };
            let Ok(token_a) = m.mint_a.parse::<Pubkey>() else {
                continue;
            };
            let Ok(token_b) = m.mint_b.parse::<Pubkey>() else {
                continue;
            };

            let dex_type = m.to_dex_type();
            let mut orca_meta = None;
            if dex_type == DexType::OrcaWhirlpool {
                if let (Ok(va), Ok(vb)) = (m.vault_a.parse::<Pubkey>(), m.vault_b.parse::<Pubkey>())
                {
                    orca_meta = Some(crate::pool_state::OrcaMetadata {
                        token_vault_a: va,
                        token_vault_b: vb,
                        liquidity: 0,
                        sqrt_price: 0,
                        tick_spacing: 0,
                        tick_current_index: 0,
                        tick_arrays: Arc::new(HashMap::new()),
                    });
                }
            }

            let pool = DexPool {
                pool_address: pool_id,
                dex_type,
                token_a,
                token_b,
                decimals_a: m.decimals_a,
                decimals_b: m.decimals_b,
                reserve_a: 0,
                reserve_b: 0,
                reserve_a_optimistic: 0,
                reserve_b_optimistic: 0,
                last_shred_update: Instant::now(),
                pool_vault_a_balance: 0,
                pool_vault_b_balance: 0,
                market_vault_a_balance: 0,
                market_vault_b_balance: 0,
                last_updated: Instant::now(),
                fee_bps: m.fee_bps.unwrap_or(30),
                raydium_meta: None,
                orca_meta,
                meteora_meta: None,
            };
            self.cache.upsert(pool);
        }
        Ok(mapped.len())
    }

    /// Refresh reserves AND metadata for Raydium pools.
    pub fn refresh_all(&self, mapped: &[MappedPool]) -> Result<()> {
        let pool_keys: Vec<Pubkey> = mapped
            .iter()
            .filter_map(|m| m.pool_id.parse().ok())
            .collect();
        let mut vault_to_pool = std::collections::HashMap::new();
        let mut raydium_markets = HashMap::<Pubkey, Pubkey>::new();
        let mut pending_raydium_metas = HashMap::<Pubkey, RaydiumMetadata>::new();
        let mut expected_vaults = HashMap::<Pubkey, (Pubkey, Pubkey)>::new();
        let mut meteora_bin_array_requests = HashMap::<Pubkey, Vec<(i64, Pubkey)>>::new();
        let mut orca_tick_array_requests = HashMap::<Pubkey, Vec<(i32, Pubkey)>>::new();
        for m in mapped {
            if let (Ok(p), Ok(va), Ok(vb)) = (
                m.pool_id.parse::<Pubkey>(),
                m.vault_a.parse::<Pubkey>(),
                m.vault_b.parse::<Pubkey>(),
            ) {
                expected_vaults.insert(p, (va, vb));
                vault_to_pool.insert(va, (p, true));
                vault_to_pool.insert(vb, (p, false));
            }
        }

        // Inter-batch sleep: 0 for local RPC (no rate limits), higher for external.
        let batch_sleep_ms: u64 = std::env::var("POOL_BATCH_SLEEP_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20); // 20ms default (safe for public mainnet, fast for local RPC)
        let batch_sleep = Duration::from_millis(batch_sleep_ms);

        // 1. Refresh pool metadata (Raydium specific)
        for chunk in pool_keys.chunks(100) {
            std::thread::sleep(batch_sleep);
            if let Some(accounts) = self.get_multiple_accounts_retry(chunk) {
                for (pubkey, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                    let Some(acc) = maybe_acc else { continue };
                    if let Some(pool) = self.cache.get(pubkey) {
                        if pool.dex_type == DexType::RaydiumAmmV4 && acc.data.len() >= 752 {
                            let Some(&(expected_a, expected_b)) = expected_vaults.get(pubkey)
                            else {
                                continue;
                            };
                            let Some(meta) = decode_raydium_amm(
                                acc.data.as_slice(),
                                &crate::types::dex_programs::raydium_amm_v4(),
                                expected_a,
                                expected_b,
                            ) else {
                                trace!(pool = %pubkey, "failed to decode Raydium AMM metadata");
                                continue;
                            };
                            raydium_markets.insert(*pubkey, meta.market_id);
                            pending_raydium_metas.insert(*pubkey, meta);
                        } else if pool.dex_type == DexType::OrcaWhirlpool {
                            let Some(meta) = decode_orca_whirlpool(acc.data.as_slice()) else {
                                warn!(pool = %pubkey, "failed to decode Orca Whirlpool metadata");
                                continue;
                            };
                            if meta.tick_spacing == 0 {
                                warn!(pool = %pubkey, "Orca pool has tick_spacing=0, skipping");
                                continue;
                            }
                            let requests = orca_tick_array_start_indexes(
                                meta.tick_current_index,
                                meta.tick_spacing,
                                8,
                            )
                            .into_iter()
                            .map(|start_tick_index| {
                                (
                                    start_tick_index,
                                    derive_orca_tick_array_pda(*pubkey, start_tick_index),
                                )
                            })
                            .collect::<Vec<_>>();
                            orca_tick_array_requests.insert(*pubkey, requests);
                            if let Some(mut entry) = self.cache.inner.get_mut(pubkey) {
                                entry.orca_meta = Some(meta);
                            }
                        } else if pool.dex_type == DexType::Fluxbeam
                            || pool.dex_type == DexType::Saber
                            || pool.dex_type == DexType::RaydiumCpmm
                        {
                            // Simple AMM pools: vaults come from mapped_pools.json.
                            // No special on-chain metadata decoding needed — vault balances
                            // are fetched in step 2 via the expected_vaults map.
                            trace!(pool = %pubkey, dex = ?pool.dex_type, "simple AMM pool, vaults from JSON");
                        } else if pool.dex_type == DexType::MeteoraDlmm {
                            let Some(meta) = decode_meteora_dlmm(acc.data.as_slice()) else {
                                warn!(pool = %pubkey, "failed to decode Meteora DLMM metadata");
                                continue;
                            };
                            let requests =
                                candidate_bin_array_indices(meta.active_id, &meta.bin_array_bitmap)
                                    .into_iter()
                                    .map(|index| (index, derive_bin_array_pda(*pubkey, index)))
                                    .collect::<Vec<_>>();
                            meteora_bin_array_requests.insert(*pubkey, requests);
                            if let Some(mut entry) = self.cache.inner.get_mut(pubkey) {
                                entry.meteora_meta = Some(meta);
                            }
                        }
                    }
                }
            }
        }

        let mut meteora_pda_to_pool = HashMap::<Pubkey, (Pubkey, i64)>::new();
        let mut meteora_pdas = Vec::new();
        let mut seen_meteora_pdas = HashSet::new();
        for (pool, requests) in &meteora_bin_array_requests {
            for (index, pda) in requests {
                if seen_meteora_pdas.insert(*pda) {
                    meteora_pdas.push(*pda);
                }
                meteora_pda_to_pool.insert(*pda, (*pool, *index));
            }
        }
        let mut meteora_bin_arrays_by_pool =
            HashMap::<Pubkey, HashMap<i64, crate::meteora::MeteoraBinArray>>::new();
        for chunk in meteora_pdas.chunks(100) {
            std::thread::sleep(batch_sleep);
            if let Some(accounts) = self.get_multiple_accounts_retry(chunk) {
                for (pda, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                    let Some(acc) = maybe_acc else { continue };
                    let Some((pool, index)) = meteora_pda_to_pool.get(pda).copied() else {
                        continue;
                    };
                    let Some(bin_array) = decode_bin_array(acc.data.as_slice(), &pool) else {
                        continue;
                    };
                    meteora_bin_arrays_by_pool
                        .entry(pool)
                        .or_default()
                        .insert(index, bin_array);
                }
            }
        }
        for (pool, bin_arrays) in meteora_bin_arrays_by_pool {
            if let Some(mut entry) = self.cache.inner.get_mut(&pool) {
                if let Some(meta) = entry.meteora_meta.as_mut() {
                    meta.bin_arrays = Arc::new(bin_arrays);
                }
            }
        }

        let mut orca_pda_to_pool = HashMap::<Pubkey, (Pubkey, i32)>::new();
        let mut orca_pdas = Vec::new();
        let mut seen_orca_pdas = HashSet::new();
        for (pool, requests) in &orca_tick_array_requests {
            for (start_tick_index, pda) in requests {
                if seen_orca_pdas.insert(*pda) {
                    orca_pdas.push(*pda);
                }
                orca_pda_to_pool.insert(*pda, (*pool, *start_tick_index));
            }
        }
        let mut orca_tick_arrays_by_pool =
            HashMap::<Pubkey, HashMap<i32, crate::orca::OrcaTickArray>>::new();
        for chunk in orca_pdas.chunks(100) {
            std::thread::sleep(batch_sleep);
            if let Some(accounts) = self.get_multiple_accounts_retry(chunk) {
                for (pda, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                    let Some(acc) = maybe_acc else { continue };
                    let Some((pool, start_tick_index)) = orca_pda_to_pool.get(pda).copied() else {
                        continue;
                    };
                    let Some(tick_array) = decode_orca_tick_array(acc.data.as_slice(), &pool)
                    else {
                        continue;
                    };
                    orca_tick_arrays_by_pool
                        .entry(pool)
                        .or_default()
                        .insert(start_tick_index, tick_array);
                }
            }
        }
        for (pool, tick_arrays) in orca_tick_arrays_by_pool {
            if let Some(mut entry) = self.cache.inner.get_mut(&pool) {
                if let Some(meta) = entry.orca_meta.as_mut() {
                    meta.tick_arrays = Arc::new(tick_arrays);
                }
            }
        }

        // 1b. Resolve Serum market vaults/authority for Raydium pools.
        let market_keys: Vec<Pubkey> = raydium_markets.values().copied().collect();
        for chunk in market_keys.chunks(100) {
            std::thread::sleep(batch_sleep);
            if let Some(accounts) = self.get_multiple_accounts_retry(chunk) {
                for (market_id, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                    let Some(acc) = maybe_acc else { continue };
                    for (pool_id, candidate_market) in &raydium_markets {
                        if candidate_market != market_id {
                            continue;
                        }
                        if let Some(meta) = pending_raydium_metas.get_mut(pool_id) {
                            let Some((
                                market_bids,
                                market_asks,
                                market_event_queue,
                                market_base_vault,
                                market_quote_vault,
                                market_vault_signer,
                            )) = decode_serum_market(
                                acc.data.as_slice(),
                                market_id,
                                &meta.market_program,
                            )
                            else {
                                continue;
                            };
                            meta.market_bids = market_bids;
                            meta.market_asks = market_asks;
                            meta.market_event_queue = market_event_queue;
                            meta.market_base_vault = market_base_vault;
                            meta.market_quote_vault = market_quote_vault;
                            meta.market_vault_signer = market_vault_signer;
                        }
                    }
                }
            }
        }

        // Flush complete Raydium metas to cache (atomic: either absent or fully resolved).
        for (pubkey, meta) in pending_raydium_metas {
            self.cache.update_raydium_meta(&pubkey, meta);
        }

        enum VaultType {
            PoolA(Pubkey),
            PoolB(Pubkey),
            MarketA(Pubkey),
            MarketB(Pubkey),
        }
        let mut all_vaults = HashMap::<Pubkey, VaultType>::new();

        // 1c. Add pool vaults (skip placeholder 11111... = Pubkey::default())
        for m in mapped {
            if let (Ok(p), Ok(va), Ok(vb)) = (
                m.pool_id.parse::<Pubkey>(),
                m.vault_a.parse::<Pubkey>(),
                m.vault_b.parse::<Pubkey>(),
            ) {
                if va != Pubkey::default() {
                    all_vaults.insert(va, VaultType::PoolA(p));
                }
                if vb != Pubkey::default() {
                    all_vaults.insert(vb, VaultType::PoolB(p));
                }
            }
        }

        // 1d. Add market vaults and dynamically discovered pool vaults
        for e in self.cache.inner.iter() {
            if let Some(meta) = &e.raydium_meta {
                // Pool vaults (decoded from chain data — not the JSON placeholders)
                if meta.vault_a != Pubkey::default() {
                    all_vaults.insert(meta.vault_a, VaultType::PoolA(e.pool_address));
                }
                if meta.vault_b != Pubkey::default() {
                    all_vaults.insert(meta.vault_b, VaultType::PoolB(e.pool_address));
                }
                // Market (OpenBook) vaults
                if meta.market_base_vault != Pubkey::default() {
                    all_vaults.insert(meta.market_base_vault, VaultType::MarketA(e.pool_address));
                }
                if meta.market_quote_vault != Pubkey::default() {
                    all_vaults.insert(meta.market_quote_vault, VaultType::MarketB(e.pool_address));
                }
            }
            if let Some(meta) = &e.orca_meta {
                if meta.token_vault_a != Pubkey::default() {
                    all_vaults.insert(meta.token_vault_a, VaultType::PoolA(e.pool_address));
                }
                if meta.token_vault_b != Pubkey::default() {
                    all_vaults.insert(meta.token_vault_b, VaultType::PoolB(e.pool_address));
                }
            }
            if let Some(meta) = &e.meteora_meta {
                if meta.token_vault_a != Pubkey::default() {
                    all_vaults.insert(meta.token_vault_a, VaultType::PoolA(e.pool_address));
                }
                if meta.token_vault_b != Pubkey::default() {
                    all_vaults.insert(meta.token_vault_b, VaultType::PoolB(e.pool_address));
                }
            }
        }

        // 2. Refresh vault balances (reserves)
        let vault_keys: Vec<Pubkey> = all_vaults.keys().copied().collect();
        for chunk in vault_keys.chunks(100) {
            std::thread::sleep(batch_sleep);
            if let Some(accounts) = self.get_multiple_accounts_retry(chunk) {
                for (pubkey, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                    let Some(acc) = maybe_acc else { continue };
                    if acc.data.len() >= 72 {
                        let amount =
                            u64::from_le_bytes(acc.data[64..72].try_into().unwrap_or([0u8; 8]));
                        if let Some(vtype) = all_vaults.get(pubkey) {
                            match vtype {
                                VaultType::PoolA(pool_id) => {
                                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                                        p.pool_vault_a_balance = amount;
                                        p.reserve_a =
                                            p.pool_vault_a_balance + p.market_vault_a_balance;
                                        p.last_updated = Instant::now();
                                    }
                                }
                                VaultType::PoolB(pool_id) => {
                                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                                        p.pool_vault_b_balance = amount;
                                        p.reserve_b =
                                            p.pool_vault_b_balance + p.market_vault_b_balance;
                                        p.last_updated = Instant::now();
                                    }
                                }
                                VaultType::MarketA(pool_id) => {
                                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                                        p.market_vault_a_balance = amount;
                                        p.reserve_a =
                                            p.pool_vault_a_balance + p.market_vault_a_balance;
                                        p.last_updated = Instant::now();
                                    }
                                }
                                VaultType::MarketB(pool_id) => {
                                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                                        p.market_vault_b_balance = amount;
                                        p.reserve_b =
                                            p.pool_vault_b_balance + p.market_vault_b_balance;
                                        p.last_updated = Instant::now();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Diagnostic: count pools with non-zero reserves.
        let mut pools_with_reserves = 0usize;
        for entry in self.cache.inner.iter() {
            if entry.reserve_a > 0 && entry.reserve_b > 0 {
                pools_with_reserves += 1;
            }
        }
        info!(
            vaults_fetched = vault_keys.len(),
            pools_with_reserves,
            total_pools = self.cache.inner.len(),
            "vault balance refresh completed"
        );

        Ok(())
    }

    /// Refresh vault balances directly from a shared AccountCache (0ms reads).
    /// This bypasses RPC entirely — reads from the same DashMap that the
    /// agave-lite-core feed writes to.
    pub fn refresh_vaults_from_cache(
        &self,
        account_cache: &agave_lite_core::account_cache::AccountCache,
        mapped: &[MappedPool],
    ) -> usize {
        #[derive(Clone)]
        enum VT { PoolA(Pubkey), PoolB(Pubkey), MarketA(Pubkey), MarketB(Pubkey) }
        let mut all_vaults = HashMap::<Pubkey, VT>::new();

        // Collect vault pubkeys (same logic as refresh_vaults).
        for m in mapped {
            let Ok(p) = m.pool_id.parse::<Pubkey>() else { continue };
            if let Ok(va) = m.vault_a.parse::<Pubkey>() {
                if va != Pubkey::default() { all_vaults.insert(va, VT::PoolA(p)); }
            }
            if let Ok(vb) = m.vault_b.parse::<Pubkey>() {
                if vb != Pubkey::default() { all_vaults.insert(vb, VT::PoolB(p)); }
            }
        }
        // Add dynamically discovered vaults from metadata.
        for e in self.cache.inner.iter() {
            let pool = e.value();
            if let Some(ref meta) = pool.raydium_meta {
                if meta.vault_a != Pubkey::default() { all_vaults.insert(meta.vault_a, VT::PoolA(pool.pool_address)); }
                if meta.vault_b != Pubkey::default() { all_vaults.insert(meta.vault_b, VT::PoolB(pool.pool_address)); }
                if meta.market_base_vault != Pubkey::default() { all_vaults.insert(meta.market_base_vault, VT::MarketA(pool.pool_address)); }
                if meta.market_quote_vault != Pubkey::default() { all_vaults.insert(meta.market_quote_vault, VT::MarketB(pool.pool_address)); }
            }
            if let Some(ref meta) = pool.orca_meta {
                if meta.token_vault_a != Pubkey::default() { all_vaults.insert(meta.token_vault_a, VT::PoolA(pool.pool_address)); }
                if meta.token_vault_b != Pubkey::default() { all_vaults.insert(meta.token_vault_b, VT::PoolB(pool.pool_address)); }
            }
            if let Some(ref meta) = pool.meteora_meta {
                if meta.token_vault_a != Pubkey::default() { all_vaults.insert(meta.token_vault_a, VT::PoolA(pool.pool_address)); }
                if meta.token_vault_b != Pubkey::default() { all_vaults.insert(meta.token_vault_b, VT::PoolB(pool.pool_address)); }
            }
        }

        // Read balances directly from AccountCache (0ms per read).
        let mut updated = 0usize;
        for (vault_pk, vtype) in &all_vaults {
            let balance = account_cache.vault_balance(vault_pk);
            if balance == 0 { continue; }

            match vtype {
                VT::PoolA(pool_id) => {
                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                        p.pool_vault_a_balance = balance;
                        p.reserve_a = p.pool_vault_a_balance + p.market_vault_a_balance;
                        p.last_updated = Instant::now();
                        updated += 1;
                    }
                }
                VT::PoolB(pool_id) => {
                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                        p.pool_vault_b_balance = balance;
                        p.reserve_b = p.pool_vault_b_balance + p.market_vault_b_balance;
                        p.last_updated = Instant::now();
                        updated += 1;
                    }
                }
                VT::MarketA(pool_id) => {
                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                        p.market_vault_a_balance = balance;
                        p.reserve_a = p.pool_vault_a_balance + p.market_vault_a_balance;
                        p.last_updated = Instant::now();
                        updated += 1;
                    }
                }
                VT::MarketB(pool_id) => {
                    if let Some(mut p) = self.cache.inner.get_mut(pool_id) {
                        p.market_vault_b_balance = balance;
                        p.reserve_b = p.pool_vault_b_balance + p.market_vault_b_balance;
                        p.last_updated = Instant::now();
                        updated += 1;
                    }
                }
            }
        }

        // Bump swap generation so route engine re-evaluates.
        if updated > 0 {
            self.cache.bump_swap_generation();
        }

        updated
    }

    /// Refresh vault balances for a single pool immediately (targeted hot-path update).
    /// Called from signal_processor after detecting a swap on a known pool.
    /// Fetches vault balances + Orca whirlpool state (sqrt_price/liquidity) from RPC.
    pub fn refresh_single_pool(&self, pool_addr: &Pubkey) {
        enum SingleVaultType {
            PoolA,
            PoolB,
            MarketA,
            MarketB,
        }

        let pool = match self.cache.get(pool_addr) {
            Some(p) => p,
            None => return,
        };

        let is_orca = pool.orca_meta.is_some();

        // Collect vault pubkeys from metadata.
        let mut vaults: Vec<(Pubkey, SingleVaultType)> = Vec::new();
        if let Some(meta) = &pool.raydium_meta {
            if meta.vault_a != Pubkey::default() {
                vaults.push((meta.vault_a, SingleVaultType::PoolA));
            }
            if meta.vault_b != Pubkey::default() {
                vaults.push((meta.vault_b, SingleVaultType::PoolB));
            }
            if meta.market_base_vault != Pubkey::default() {
                vaults.push((meta.market_base_vault, SingleVaultType::MarketA));
            }
            if meta.market_quote_vault != Pubkey::default() {
                vaults.push((meta.market_quote_vault, SingleVaultType::MarketB));
            }
        }
        if let Some(meta) = &pool.orca_meta {
            if meta.token_vault_a != Pubkey::default() {
                vaults.push((meta.token_vault_a, SingleVaultType::PoolA));
            }
            if meta.token_vault_b != Pubkey::default() {
                vaults.push((meta.token_vault_b, SingleVaultType::PoolB));
            }
        }
        if let Some(meta) = &pool.meteora_meta {
            if meta.token_vault_a != Pubkey::default() {
                vaults.push((meta.token_vault_a, SingleVaultType::PoolA));
            }
            if meta.token_vault_b != Pubkey::default() {
                vaults.push((meta.token_vault_b, SingleVaultType::PoolB));
            }
        }
        // PumpSwap: read vault addresses from pool account data, then fetch vault balances.
        let is_pumpswap = pool.dex_type == DexType::PumpSwap;
        if vaults.is_empty() && !is_orca && !is_pumpswap {
            return;
        }

        if is_pumpswap {
            // Fetch pool account to read vault pubkeys at offsets 139 and 171.
            if let Some(accounts) = self.get_multiple_accounts_retry(&[*pool_addr]) {
                if let Some(Some(acc)) = accounts.first() {
                    if acc.data.len() >= 203 {
                        let vault_a = Pubkey::try_from(&acc.data[139..171]).unwrap_or_default();
                        let vault_b = Pubkey::try_from(&acc.data[171..203]).unwrap_or_default();
                        if vault_a != Pubkey::default() && vault_b != Pubkey::default() {
                            // Now fetch vault balances.
                            if let Some(vault_accs) = self.get_multiple_accounts_retry(&[vault_a, vault_b]) {
                                for (i, maybe_acc) in vault_accs.iter().enumerate() {
                                    if let Some(vacc) = maybe_acc {
                                        if vacc.data.len() >= 72 {
                                            let amount = u64::from_le_bytes(
                                                vacc.data[64..72].try_into().unwrap_or([0u8; 8]),
                                            );
                                            if let Some(mut p) = self.cache.inner.get_mut(pool_addr) {
                                                if i == 0 {
                                                    p.reserve_a = amount;
                                                } else {
                                                    p.reserve_b = amount;
                                                }
                                                p.last_updated = std::time::Instant::now();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            return;
        }

        // For Orca: also fetch the whirlpool account itself (sqrt_price, liquidity, tick).
        // We prepend pool_addr to the batch so it's fetched in one RPC call.
        let mut keys: Vec<Pubkey> = Vec::new();
        let orca_pool_idx = if is_orca {
            keys.push(*pool_addr);
            Some(0)
        } else {
            None
        };
        let vault_start = keys.len();
        keys.extend(vaults.iter().map(|(k, _)| *k));

        if keys.is_empty() {
            return;
        }

        if let Some(accounts) = self.get_multiple_accounts_retry(&keys) {
            // Update Orca whirlpool state (sqrt_price, liquidity, tick_current_index).
            if let Some(idx) = orca_pool_idx {
                if let Some(Some(acc)) = accounts.get(idx) {
                    if acc.data.len() >= 85 {
                        let liquidity =
                            u128::from_le_bytes(acc.data[49..65].try_into().unwrap_or([0u8; 16]));
                        let sqrt_price =
                            u128::from_le_bytes(acc.data[65..81].try_into().unwrap_or([0u8; 16]));
                        let tick_current_index =
                            i32::from_le_bytes(acc.data[81..85].try_into().unwrap_or([0u8; 4]));
                        if let Some(mut p) = self.cache.inner.get_mut(pool_addr) {
                            if let Some(meta) = p.orca_meta.as_mut() {
                                meta.sqrt_price = sqrt_price;
                                meta.liquidity = liquidity;
                                meta.tick_current_index = tick_current_index;
                            }
                        }
                    }
                }
            }

            // Update vault balances.
            for (i, maybe_acc) in accounts[vault_start..].iter().enumerate() {
                let Some(acc) = maybe_acc else { continue };
                let Some((_, vault_type)) = vaults.get(i) else {
                    continue;
                };
                if acc.data.len() >= 72 {
                    let amount =
                        u64::from_le_bytes(acc.data[64..72].try_into().unwrap_or([0u8; 8]));
                    if let Some(mut p) = self.cache.inner.get_mut(pool_addr) {
                        match vault_type {
                            SingleVaultType::PoolA => {
                                p.pool_vault_a_balance = amount;
                                p.reserve_a = p.pool_vault_a_balance + p.market_vault_a_balance;
                            }
                            SingleVaultType::PoolB => {
                                p.pool_vault_b_balance = amount;
                                p.reserve_b = p.pool_vault_b_balance + p.market_vault_b_balance;
                            }
                            SingleVaultType::MarketA => {
                                p.market_vault_a_balance = amount;
                                p.reserve_a = p.pool_vault_a_balance + p.market_vault_a_balance;
                            }
                            SingleVaultType::MarketB => {
                                p.market_vault_b_balance = amount;
                                p.reserve_b = p.pool_vault_b_balance + p.market_vault_b_balance;
                            }
                        }
                        p.last_updated = std::time::Instant::now();
                    }
                }
            }
        }
    }

    pub fn spawn_refresh_loop(
        rpc_url: String,
        cache: Arc<PoolStateCache>,
        pools_file: String,
        interval: Duration,
    ) {
        let no_agave = std::env::var("NO_AGAVE_MODE")
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
            .unwrap_or(false);

        std::thread::Builder::new()
            .name("pool-hydrator".into())
            .spawn(move || {
                // Wait for initial pool-init-refresh to complete before starting the loop.
                // Avoids double-hitting the RPC during startup.
                std::thread::sleep(interval);
                let hydrator = PoolHydrator::new(&rpc_url, cache.clone(), &pools_file);
                let all_mapped = load_mapped_pools(&pools_file).unwrap_or_default();

                if no_agave {
                    tracing::info!(
                        total_pools = all_mapped.len(),
                        "pool-hydrator: NO_AGAVE_MODE — hydrating only recently active pools to reduce RPC pressure"
                    );
                }

                loop {
                    let mapped = if no_agave {
                        // In no-agave mode, only hydrate pools with recent activity
                        // (last_shred_update < 30s or last_updated < 60s with non-zero reserves).
                        // This reduces Helius RPC calls from ~7700 pools to ~500-2000 active ones.
                        let active_pools: std::collections::HashSet<String> = cache.inner.iter()
                            .filter(|e| {
                                let pool = e.value();
                                // Pool had shred delta in last 30s
                                let shred_active = pool.last_shred_update.elapsed().as_secs() < 30;
                                // Pool has real reserves (was hydrated before)
                                let has_reserves = pool.reserve_a > 0 && pool.reserve_b > 0;
                                shred_active || has_reserves
                            })
                            .map(|e| e.key().to_string())
                            .collect();
                        let filtered: Vec<MappedPool> = all_mapped.iter()
                            .filter(|m| active_pools.contains(&m.pool_id))
                            .cloned()
                            .collect();
                        tracing::debug!(
                            active = filtered.len(),
                            total = all_mapped.len(),
                            "pool-hydrator: no-agave active filter"
                        );
                        filtered
                    } else {
                        all_mapped.clone()
                    };
                    let _ = hydrator.refresh_all(&mapped);
                    std::thread::sleep(interval);
                }
            })
            .expect("spawn hydrator");
    }
}

fn decode_raydium_amm(
    data: &[u8],
    dex_program: &Pubkey,
    expected_vault_a: Pubkey,
    expected_vault_b: Pubkey,
) -> Option<RaydiumMetadata> {
    if data.len() < 752 {
        return None;
    }

    let nonce = *data.get(8)?;
    let authority =
        Pubkey::create_program_address(&[AUTHORITY_AMM_SEED, &[nonce]], dex_program).ok()?;

    for layout in RAYDIUM_AMM_LAYOUTS {
        let vault_a =
            pubkey_from_bytes(&data[layout.coin_vault_offset..layout.coin_vault_offset + 32])?;
        let vault_b =
            pubkey_from_bytes(&data[layout.pc_vault_offset..layout.pc_vault_offset + 32])?;

        // Validate against expected vaults only if they are real (not placeholder).
        // JSON mapped_pools.json may have "11111..." (Pubkey::default) as placeholder.
        let placeholder = Pubkey::default();
        if expected_vault_a != placeholder && expected_vault_b != placeholder {
            let actual = [vault_a, vault_b];
            let expected = [expected_vault_a, expected_vault_b];
            if !same_pubkey_set(actual, expected) {
                continue;
            }
        }

        return Some(RaydiumMetadata {
            authority,
            nonce,
            open_orders: pubkey_from_bytes(
                &data[layout.open_orders_offset..layout.open_orders_offset + 32],
            )?,
            target_orders: pubkey_from_bytes(
                &data[layout.target_orders_offset..layout.target_orders_offset + 32],
            )?,
            vault_a,
            vault_b,
            market_id: pubkey_from_bytes(&data[layout.market_offset..layout.market_offset + 32])?,
            market_program: pubkey_from_bytes(
                &data[layout.market_program_offset..layout.market_program_offset + 32],
            )?,
            market_event_queue: Pubkey::default(),
            market_bids: Pubkey::default(),
            market_asks: Pubkey::default(),
            market_base_vault: Pubkey::default(),
            market_quote_vault: Pubkey::default(),
            market_vault_signer: Pubkey::default(),
        });
    }

    None
}

fn decode_serum_market(
    data: &[u8],
    market_id: &Pubkey,
    market_program: &Pubkey,
) -> Option<(Pubkey, Pubkey, Pubkey, Pubkey, Pubkey, Pubkey)> {
    const HEAD_PADDING: &[u8; 5] = b"serum";
    const TAIL_PADDING: &[u8; 7] = b"padding";
    const PADDED_MARKET_LEN: usize = 5 + (47 * 8) + 7;

    if data.len() < PADDED_MARKET_LEN || &data[..5] != HEAD_PADDING {
        return None;
    }
    if &data[PADDED_MARKET_LEN - 7..PADDED_MARKET_LEN] != TAIL_PADDING {
        return None;
    }

    let nonce_offset = 5 + (5 * 8);
    let coin_vault_offset = 5 + (14 * 8);
    let pc_vault_offset = 5 + (20 * 8);
    let event_q_offset = 5 + (31 * 8);
    let bids_offset = 5 + (35 * 8);
    let asks_offset = 5 + (39 * 8);

    let mut nonce_bytes = [0u8; 8];
    nonce_bytes.copy_from_slice(&data[nonce_offset..nonce_offset + 8]);
    let vault_signer_nonce = u64::from_le_bytes(nonce_bytes);

    let market_event_queue = pubkey_from_bytes(&data[event_q_offset..event_q_offset + 32])?;
    let market_bids = pubkey_from_bytes(&data[bids_offset..bids_offset + 32])?;
    let market_asks = pubkey_from_bytes(&data[asks_offset..asks_offset + 32])?;
    let market_base_vault = pubkey_from_bytes(&data[coin_vault_offset..coin_vault_offset + 32])?;
    let market_quote_vault = pubkey_from_bytes(&data[pc_vault_offset..pc_vault_offset + 32])?;
    let market_vault_signer = Pubkey::create_program_address(
        &[market_id.as_ref(), &vault_signer_nonce.to_le_bytes()],
        market_program,
    )
    .ok()?;

    Some((
        market_bids,
        market_asks,
        market_event_queue,
        market_base_vault,
        market_quote_vault,
        market_vault_signer,
    ))
}

fn decode_orca_whirlpool(data: &[u8]) -> Option<OrcaMetadata> {
    if data.len() < 237 {
        return None;
    }

    Some(OrcaMetadata {
        // Whirlpool account layout from Orca docs/client:
        // tick_spacing@41, liquidity@49, sqrt_price@65, tick_current_index@81,
        // token_vault_a@133, token_vault_b@213.
        tick_spacing: u16::from_le_bytes(data[41..43].try_into().ok()?),
        liquidity: u128::from_le_bytes(data[49..65].try_into().ok()?),
        sqrt_price: u128::from_le_bytes(data[65..81].try_into().ok()?),
        tick_current_index: i32::from_le_bytes(data[81..85].try_into().ok()?),
        token_vault_a: pubkey_from_bytes(&data[133..165])?,
        token_vault_b: pubkey_from_bytes(&data[213..245])?,
        tick_arrays: Arc::new(HashMap::new()),
    })
}

fn derive_orca_tick_array_pda(whirlpool: Pubkey, start_tick_index: i32) -> Pubkey {
    let program_id = crate::types::dex_programs::orca_whirlpool();
    let start_tick_index = start_tick_index.to_string();
    Pubkey::find_program_address(
        &[
            b"tick_array",
            whirlpool.as_ref(),
            start_tick_index.as_bytes(),
        ],
        &program_id,
    )
    .0
}

fn decode_meteora_dlmm(data: &[u8]) -> Option<crate::pool_state::MeteoraMetadata> {
    let decoded = decode_lb_pair(data)?;
    Some(crate::pool_state::MeteoraMetadata {
        active_id: decoded.active_id,
        bin_step: decoded.bin_step,
        token_vault_a: decoded.reserve_x,
        token_vault_b: decoded.reserve_y,
        parameters: decoded.parameters,
        v_parameters: decoded.v_parameters,
        bin_array_bitmap: decoded.bin_array_bitmap,
        bin_arrays: Arc::new(HashMap::new()),
    })
}

fn same_pubkey_set(actual: [Pubkey; 2], expected: [Pubkey; 2]) -> bool {
    (actual[0] == expected[0] && actual[1] == expected[1])
        || (actual[0] == expected[1] && actual[1] == expected[0])
}

fn pubkey_from_bytes(bytes: &[u8]) -> Option<Pubkey> {
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Some(Pubkey::new_from_array(out))
}

pub fn load_mapped_pools(path: &str) -> Result<Vec<MappedPool>> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str::<Vec<MappedPool>>(&content).map_err(|e| e.into())
}

pub fn bootstrap_cache(cache: &Arc<PoolStateCache>, rpc_url: &str) -> usize {
    let path = "/root/solana-bot/mapped_pools.json";
    let hydrator = PoolHydrator::new(rpc_url, cache.clone(), path);
    match hydrator.load_from_file() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(error = %e, path = path, "bootstrap_cache: failed to load mapped_pools.json");
            0
        }
    }
}
