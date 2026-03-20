//! Account feed: populates the AccountCache from RPC polling.
//!
//! Runs background threads that fetch account data and write
//! directly to the shared DashMap.

use crate::account_cache::{AccountCache, CachedAccount};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Feed configuration.
pub struct FeedConfig {
    pub rpc_url: String,
    pub vault_pubkeys: Vec<Pubkey>,
    pub poll_interval_secs: u64,
    pub batch_size: usize,
    pub batch_sleep_ms: u64,
}

/// Blockhash cache — updated by polling, read by executor.
pub struct BlockhashCache {
    pub hash: parking_lot::RwLock<Hash>,
    pub slot: AtomicU64,
    pub updates: AtomicU64,
}

impl BlockhashCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            hash: parking_lot::RwLock::new(Hash::default()),
            slot: AtomicU64::new(0),
            updates: AtomicU64::new(0),
        })
    }

    pub fn get(&self) -> (Hash, u64) {
        let h = *self.hash.read();
        let s = self.slot.load(Ordering::Relaxed);
        (h, s)
    }
}

/// Start all feed threads. Returns immediately.
pub fn spawn_feed(
    config: FeedConfig,
    cache: Arc<AccountCache>,
    blockhash: Arc<BlockhashCache>,
) {
    // Blockhash poller (400ms).
    {
        let bh = blockhash.clone();
        let url = config.rpc_url.clone();
        std::thread::Builder::new()
            .name("al-bh-poll".into())
            .spawn(move || blockhash_poller(bh, &url))
            .expect("spawn al-bh-poll");
    }

    // Account poller.
    if !config.vault_pubkeys.is_empty() {
        let c = cache.clone();
        let cfg = config;
        std::thread::Builder::new()
            .name("al-acc-poll".into())
            .spawn(move || account_poller(c, &cfg))
            .expect("spawn al-acc-poll");
    }

    info!("agave-lite-core feed started");
}

fn blockhash_poller(bh: Arc<BlockhashCache>, rpc_url: &str) {
    let rpc = solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
        rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    );
    let interval = Duration::from_millis(400);
    let mut errors = 0u32;

    loop {
        std::thread::sleep(interval);
        match rpc.get_latest_blockhash_with_commitment(CommitmentConfig::confirmed()) {
            Ok((hash, slot)) => {
                let old = bh.slot.load(Ordering::Relaxed);
                if slot > old {
                    *bh.hash.write() = hash;
                    bh.slot.store(slot, Ordering::Relaxed);
                    bh.updates.fetch_add(1, Ordering::Relaxed);
                }
                if errors > 0 {
                    info!("blockhash poller recovered after {} errors", errors);
                    errors = 0;
                }
            }
            Err(e) => {
                errors += 1;
                if errors <= 3 || errors % 20 == 0 {
                    warn!("blockhash poll error ({}x): {}", errors, e);
                }
            }
        }
    }
}

fn account_poller(cache: Arc<AccountCache>, config: &FeedConfig) {
    let rpc = solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        CommitmentConfig::confirmed(),
    );
    let interval = Duration::from_secs(config.poll_interval_secs);
    let batch_sleep = Duration::from_millis(config.batch_sleep_ms);
    let keys = &config.vault_pubkeys;

    info!(
        "account poller: {} keys, {}s interval, batch={}",
        keys.len(), config.poll_interval_secs, config.batch_size
    );

    loop {
        let start = std::time::Instant::now();
        let mut updated = 0u64;

        for chunk in keys.chunks(config.batch_size) {
            match rpc.get_multiple_accounts(chunk) {
                Ok(accounts) => {
                    let slot = cache.current_slot();
                    for (pk, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                        if let Some(acc) = maybe_acc {
                            cache.upsert(
                                *pk,
                                CachedAccount {
                                    lamports: acc.lamports,
                                    data: acc.data.clone(),
                                    owner: acc.owner,
                                    slot,
                                },
                            );
                            updated += 1;
                        }
                    }
                }
                Err(e) => {
                    debug!("account poll batch error: {}", e);
                }
            }
            if !batch_sleep.is_zero() {
                std::thread::sleep(batch_sleep);
            }
        }

        debug!(
            "account poll: {} updated in {:.1}s, {} cached",
            updated,
            start.elapsed().as_secs_f64(),
            cache.len()
        );

        let remaining = interval.saturating_sub(start.elapsed());
        if !remaining.is_zero() {
            std::thread::sleep(remaining);
        }
    }
}
