//! Loads pre-extracted pool account data from snapshot ETL.
//! Format: [count:u64][entries: pubkey(32)+owner(32)+lamports(8)+data_len(4)+data(N)]

use crate::account_cache::{AccountCache, CachedAccount};
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tracing::{info, warn};

pub fn load_pool_accounts(path: &str, cache: &Arc<AccountCache>) -> u64 {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            warn!("Pool data not found: {} ({})", path, e);
            return 0;
        }
    };
    if data.len() < 8 { return 0; }

    let count = u64::from_le_bytes(data[0..8].try_into().unwrap()) as usize;
    let mut offset = 8usize;
    let mut loaded = 0u64;

    for _ in 0..count {
        if offset + 76 > data.len() { break; }
        let pubkey = Pubkey::new_from_array(data[offset..offset+32].try_into().unwrap());
        offset += 32;
        let owner = Pubkey::new_from_array(data[offset..offset+32].try_into().unwrap());
        offset += 32;
        let lamports = u64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
        offset += 8;
        let data_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + data_len > data.len() { break; }
        let account_data = data[offset..offset+data_len].to_vec();
        offset += data_len;

        cache.upsert(pubkey, CachedAccount {
            lamports,
            data: account_data,
            owner,
            slot: 0,
        });
        loaded += 1;
    }

    info!("Pool data loaded: {} accounts from {}", loaded, path);
    loaded
}
