//! redis_cache.rs — Sub-millisecond cache for pool metadata, blockhash, and hot data.
//!
//! Redis keys:
//! - `meta:{pool_address}` → JSON of static pool metadata (vaults, market accounts)
//! - `bh:latest` → latest blockhash string
//! - `reserves:{pool_address}` → JSON {reserve_a, reserve_b, ts}

use redis::{Client, Commands, Connection};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{debug, info, warn};

use crate::pool_state::{DexPool, MeteoraMetadata, OrcaMetadata, PoolStateCache, RaydiumMetadata};
use crate::types::DexType;

/// Cached pool metadata (static — doesn't change after pool creation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPoolMeta {
    pub dex_type: String,
    pub token_a: String,
    pub token_b: String,
    pub fee_bps: u64,
    // Raydium V4
    pub vault_a: Option<String>,
    pub vault_b: Option<String>,
    pub market_id: Option<String>,
    pub market_program: Option<String>,
    pub market_bids: Option<String>,
    pub market_asks: Option<String>,
    pub market_event_queue: Option<String>,
    pub market_base_vault: Option<String>,
    pub market_quote_vault: Option<String>,
    pub market_vault_signer: Option<String>,
    pub open_orders: Option<String>,
    // Orca
    pub orca_vault_a: Option<String>,
    pub orca_vault_b: Option<String>,
    pub orca_tick_spacing: Option<u16>,
    // Meteora
    pub meteora_vault_a: Option<String>,
    pub meteora_vault_b: Option<String>,
    pub meteora_bin_step: Option<u16>,
}

pub struct RedisCache {
    conn: Mutex<Option<Connection>>,
}

impl RedisCache {
    pub fn new(url: &str) -> Self {
        let conn = Client::open(url)
            .ok()
            .and_then(|c| c.get_connection().ok());
        if conn.is_some() {
            info!("redis-cache: connected to {}", url);
        } else {
            warn!("redis-cache: failed to connect to {}, running without cache", url);
        }
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Save pool metadata to Redis (called after successful RPC fetch).
    pub fn save_pool_meta(&self, pool_address: &Pubkey, pool: &DexPool) {
        let meta = self.pool_to_cached_meta(pool);
        let key = format!("meta:{}", pool_address);
        if let Ok(json) = serde_json::to_string(&meta) {
            if let Ok(mut guard) = self.conn.lock() {
                if let Some(conn) = guard.as_mut() {
                    let _: Result<(), _> = conn.set_ex(&key, &json, 86400); // 24h TTL
                }
            }
        }
    }

    /// Load pool metadata from Redis and apply to pool cache.
    /// Returns count of pools with metadata restored.
    pub fn load_all_metadata(&self, cache: &PoolStateCache) -> usize {
        let mut count = 0usize;
        let keys: Vec<String> = {
            let Ok(mut guard) = self.conn.lock() else { return 0 };
            let Some(conn) = guard.as_mut() else { return 0 };
            let result: Result<Vec<String>, _> = redis::cmd("KEYS").arg("meta:*").query(conn);
            result.unwrap_or_default()
        };

        for key in &keys {
            let pool_addr_str = key.strip_prefix("meta:").unwrap_or(key);
            let Ok(pool_addr) = pool_addr_str.parse::<Pubkey>() else { continue };

            let json: Option<String> = {
                let Ok(mut guard) = self.conn.lock() else { continue };
                let Some(conn) = guard.as_mut() else { continue };
                conn.get(key).ok()
            };

            let Some(json) = json else { continue };
            let Ok(meta) = serde_json::from_str::<CachedPoolMeta>(&json) else { continue };

            if let Some(mut pool) = cache.inner.get_mut(&pool_addr) {
                self.apply_cached_meta(&mut pool, &meta);
                count += 1;
            }
        }

        if count > 0 {
            info!(pools = count, "redis-cache: restored pool metadata from cache");
        }
        count
    }

    /// Save blockhash to Redis.
    pub fn save_blockhash(&self, blockhash: &str) {
        if let Ok(mut guard) = self.conn.lock() {
            if let Some(conn) = guard.as_mut() {
                let _: Result<(), _> = conn.set_ex("bh:latest", blockhash, 60); // 60s TTL
            }
        }
    }

    /// Load blockhash from Redis.
    pub fn load_blockhash(&self) -> Option<String> {
        let Ok(mut guard) = self.conn.lock() else { return None };
        let Some(conn) = guard.as_mut() else { return None };
        conn.get("bh:latest").ok()
    }

    /// Bulk save all pools with metadata to Redis.
    pub fn save_all_metadata(&self, cache: &PoolStateCache) -> usize {
        let mut count = 0usize;
        let Ok(mut guard) = self.conn.lock() else { return 0 };
        let Some(conn) = guard.as_mut() else { return 0 };

        let mut pipe = redis::pipe();
        for entry in cache.inner.iter() {
            let pool = entry.value();
            let has_meta = match pool.dex_type {
                DexType::RaydiumAmmV4 => pool.raydium_meta.is_some(),
                DexType::OrcaWhirlpool => pool.orca_meta.as_ref().map_or(false, |m| m.tick_spacing > 0),
                DexType::MeteoraDlmm => pool.meteora_meta.as_ref().map_or(false, |m| m.token_vault_a != Pubkey::default()),
                _ => false,
            };
            if !has_meta { continue; }

            let meta = self.pool_to_cached_meta(pool);
            if let Ok(json) = serde_json::to_string(&meta) {
                let key = format!("meta:{}", pool.pool_address);
                pipe.set_ex(key, json, 86400u64);
                count += 1;
            }
        }

        if count > 0 {
            let _: Result<(), _> = pipe.query(conn);
            info!(pools = count, "redis-cache: saved pool metadata");
        }
        count
    }

    fn pool_to_cached_meta(&self, pool: &DexPool) -> CachedPoolMeta {
        let mut meta = CachedPoolMeta {
            dex_type: format!("{:?}", pool.dex_type),
            token_a: pool.token_a.to_string(),
            token_b: pool.token_b.to_string(),
            fee_bps: pool.fee_bps,
            vault_a: None, vault_b: None,
            market_id: None, market_program: None,
            market_bids: None, market_asks: None,
            market_event_queue: None, market_base_vault: None,
            market_quote_vault: None, market_vault_signer: None,
            open_orders: None,
            orca_vault_a: None, orca_vault_b: None, orca_tick_spacing: None,
            meteora_vault_a: None, meteora_vault_b: None, meteora_bin_step: None,
        };

        if let Some(rm) = &pool.raydium_meta {
            meta.vault_a = Some(rm.vault_a.to_string());
            meta.vault_b = Some(rm.vault_b.to_string());
            meta.market_id = Some(rm.market_id.to_string());
            meta.market_program = Some(rm.market_program.to_string());
            meta.market_bids = Some(rm.market_bids.to_string());
            meta.market_asks = Some(rm.market_asks.to_string());
            meta.market_event_queue = Some(rm.market_event_queue.to_string());
            meta.market_base_vault = Some(rm.market_base_vault.to_string());
            meta.market_quote_vault = Some(rm.market_quote_vault.to_string());
            meta.market_vault_signer = Some(rm.market_vault_signer.to_string());
            meta.open_orders = Some(rm.open_orders.to_string());
        }
        if let Some(om) = &pool.orca_meta {
            meta.orca_vault_a = Some(om.token_vault_a.to_string());
            meta.orca_vault_b = Some(om.token_vault_b.to_string());
            meta.orca_tick_spacing = Some(om.tick_spacing);
        }
        if let Some(mm) = &pool.meteora_meta {
            meta.meteora_vault_a = Some(mm.token_vault_a.to_string());
            meta.meteora_vault_b = Some(mm.token_vault_b.to_string());
            meta.meteora_bin_step = Some(mm.bin_step);
        }
        meta
    }

    /// Fast metadata load: MGET only pool addresses already in cache (no KEYS scan).
    pub fn load_metadata_fast(&self, cache: &PoolStateCache) -> usize {
        let pool_addrs: Vec<Pubkey> = cache.inner.iter().map(|e| *e.key()).collect();
        if pool_addrs.is_empty() { return 0; }

        let keys: Vec<String> = pool_addrs.iter().map(|a| format!("meta:{}", a)).collect();
        let mut count = 0usize;

        for chunk in keys.chunks(500) {
            let values: Vec<Option<String>> = {
                let Ok(mut guard) = self.conn.lock() else { return count };
                let Some(conn) = guard.as_mut() else { return count };
                redis::cmd("MGET").arg(chunk).query(conn).unwrap_or_default()
            };

            for (key, val) in chunk.iter().zip(values.iter()) {
                let Some(json) = val else { continue };
                let addr_str = key.strip_prefix("meta:").unwrap_or(key);
                let Ok(addr) = addr_str.parse::<Pubkey>() else { continue };
                let Ok(meta) = serde_json::from_str::<CachedPoolMeta>(json) else { continue };

                if let Some(mut pool) = cache.inner.get_mut(&addr) {
                    self.apply_cached_meta(&mut pool, &meta);
                    count += 1;
                }
            }
        }

        if count > 0 {
            info!(pools = count, "redis-cache: metadata restored (fast MGET)");
        }
        count
    }

    /// Load vault balances from Redis and apply as pool reserves.
    /// Uses the vault→pool index from PoolStateCache.
    /// Also accepts a supplemental vault map from mapped_pools.json for pools
    /// whose metadata hasn't been loaded yet.
    pub fn load_vault_balances(
        &self,
        cache: &PoolStateCache,
        extra_vault_map: &HashMap<Pubkey, (Pubkey, bool)>,
    ) -> usize {
        // Merge: vault index from cache metadata + extra from mapped_pools
        let mut vault_index = cache.build_vault_index();
        for (vault, mapping) in extra_vault_map {
            vault_index.entry(*vault).or_insert(*mapping);
        }

        if vault_index.is_empty() { return 0; }

        let vault_keys: Vec<String> = vault_index.keys()
            .map(|v| format!("vault:{}", v))
            .collect();

        let mut count = 0usize;

        for chunk in vault_keys.chunks(500) {
            let values: Vec<Option<String>> = {
                let Ok(mut guard) = self.conn.lock() else { return count };
                let Some(conn) = guard.as_mut() else { return count };
                redis::cmd("MGET").arg(chunk).query(conn).unwrap_or_default()
            };

            for (key, val) in chunk.iter().zip(values.iter()) {
                let Some(json) = val else { continue };
                let vault_str = key.strip_prefix("vault:").unwrap_or(key);
                let Ok(vault_addr) = vault_str.parse::<Pubkey>() else { continue };
                let Some(&(pool_addr, is_vault_a)) = vault_index.get(&vault_addr) else { continue };

                let Ok(vb) = serde_json::from_str::<VaultBalance>(json) else { continue };
                if vb.amount == 0 { continue; }

                if cache.update_reserve_by_vault(&pool_addr, is_vault_a, vb.amount) {
                    count += 1;
                }
            }
        }

        if count > 0 {
            info!(reserves = count, "redis-cache: vault balances loaded");
        }
        count
    }

    /// Build supplemental vault→pool map from mapped_pools.json.
    /// This covers pools that don't yet have DEX-specific metadata loaded.
    pub fn build_mapped_pools_vault_index(pools_path: &str) -> HashMap<Pubkey, (Pubkey, bool)> {
        let mut map = HashMap::new();
        let Ok(content) = std::fs::read_to_string(pools_path) else { return map };
        let Ok(pools) = serde_json::from_str::<Vec<MappedPoolEntry>>(&content) else { return map };

        for p in &pools {
            let Ok(pool_id) = p.pool_id.parse::<Pubkey>() else { continue };
            let Ok(vault_a) = p.vault_a.parse::<Pubkey>() else { continue };
            let Ok(vault_b) = p.vault_b.parse::<Pubkey>() else { continue };
            if vault_a != Pubkey::default() {
                map.insert(vault_a, (pool_id, true));
            }
            if vault_b != Pubkey::default() {
                map.insert(vault_b, (pool_id, false));
            }
        }
        map
    }

    fn apply_cached_meta(&self, pool: &mut DexPool, meta: &CachedPoolMeta) {
        let parse = |s: &Option<String>| -> Pubkey {
            s.as_ref().and_then(|s| s.parse().ok()).unwrap_or_default()
        };

        match pool.dex_type {
            DexType::RaydiumAmmV4 => {
                if pool.raydium_meta.is_none() && meta.vault_a.is_some() {
                    pool.raydium_meta = Some(RaydiumMetadata {
                        vault_a: parse(&meta.vault_a),
                        vault_b: parse(&meta.vault_b),
                        market_id: parse(&meta.market_id),
                        market_program: parse(&meta.market_program),
                        market_bids: parse(&meta.market_bids),
                        market_asks: parse(&meta.market_asks),
                        market_event_queue: parse(&meta.market_event_queue),
                        market_base_vault: parse(&meta.market_base_vault),
                        market_quote_vault: parse(&meta.market_quote_vault),
                        market_vault_signer: parse(&meta.market_vault_signer),
                        open_orders: parse(&meta.open_orders),
                        ..Default::default()
                    });
                }
            }
            DexType::OrcaWhirlpool => {
                if let Some(om) = &mut pool.orca_meta {
                    if om.tick_spacing == 0 {
                        om.token_vault_a = parse(&meta.orca_vault_a);
                        om.token_vault_b = parse(&meta.orca_vault_b);
                        om.tick_spacing = meta.orca_tick_spacing.unwrap_or(0);
                    }
                } else if meta.orca_vault_a.is_some() {
                    pool.orca_meta = Some(OrcaMetadata {
                        token_vault_a: parse(&meta.orca_vault_a),
                        token_vault_b: parse(&meta.orca_vault_b),
                        tick_spacing: meta.orca_tick_spacing.unwrap_or(0),
                        ..Default::default()
                    });
                }
            }
            DexType::MeteoraDlmm => {
                if pool.meteora_meta.is_none() && meta.meteora_vault_a.is_some() {
                    pool.meteora_meta = Some(MeteoraMetadata {
                        token_vault_a: parse(&meta.meteora_vault_a),
                        token_vault_b: parse(&meta.meteora_vault_b),
                        bin_step: meta.meteora_bin_step.unwrap_or(0),
                        ..Default::default()
                    });
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Deserialize)]
struct VaultBalance {
    amount: u64,
    #[allow(dead_code)]
    slot: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct MappedPoolEntry {
    pool_id: String,
    vault_a: String,
    vault_b: String,
}
