//! Shared account cache — the single source of truth for account state.
//!
//! Both the feed (writer) and the pool hydrator (reader) access this
//! DashMap directly. No RPC overhead, no serialization.

use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Cached account data for a single Solana account.
#[derive(Debug, Clone)]
pub struct CachedAccount {
    pub lamports: u64,
    pub data: Vec<u8>,
    pub owner: Pubkey,
    pub slot: u64,
}

impl CachedAccount {
    /// Read token balance from SPL Token account data (offset 64-72).
    pub fn token_balance(&self) -> u64 {
        if self.data.len() >= 72 {
            u64::from_le_bytes(self.data[64..72].try_into().unwrap_or([0; 8]))
        } else {
            0
        }
    }
}

/// The shared account cache.
pub struct AccountCache {
    pub accounts: Arc<DashMap<Pubkey, CachedAccount>>,
    pub updates: AtomicU64,
    pub slot: AtomicU64,
}

impl AccountCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            accounts: Arc::new(DashMap::with_capacity(25_000)),
            updates: AtomicU64::new(0),
            slot: AtomicU64::new(0),
        })
    }

    /// Insert or update an account. Latest slot wins.
    pub fn upsert(&self, pubkey: Pubkey, account: CachedAccount) {
        let slot = account.slot;
        self.accounts
            .entry(pubkey)
            .and_modify(|existing| {
                if slot >= existing.slot {
                    *existing = account.clone();
                }
            })
            .or_insert(account);
        self.updates.fetch_add(1, Ordering::Relaxed);
        let _ = self.slot.fetch_max(slot, Ordering::Relaxed);
    }

    /// Get an account's cached data.
    pub fn get(&self, pubkey: &Pubkey) -> Option<CachedAccount> {
        self.accounts.get(pubkey).map(|r| r.value().clone())
    }

    /// Get token balance for a vault account (0ms).
    pub fn vault_balance(&self, pubkey: &Pubkey) -> u64 {
        self.accounts
            .get(pubkey)
            .map(|r| r.value().token_balance())
            .unwrap_or(0)
    }

    /// Batch get multiple accounts.
    pub fn get_multiple(&self, pubkeys: &[Pubkey]) -> Vec<Option<CachedAccount>> {
        pubkeys
            .iter()
            .map(|pk| self.get(pk))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn total_updates(&self) -> u64 {
        self.updates.load(Ordering::Relaxed)
    }

    pub fn current_slot(&self) -> u64 {
        self.slot.load(Ordering::Relaxed)
    }
}
