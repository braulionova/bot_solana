// rpc_pool.rs — Round-robin RPC pool with fallback for vault refresh.
//
// Rotates across multiple free RPC endpoints to avoid 429 rate limits.
// Each call tries the next endpoint in rotation. If it fails, tries the next.
// Timeout per call: 400ms (fast fail, don't block executor hot path).

use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{account::Account, commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{debug, warn};

pub struct RpcPool {
    clients: Vec<RpcClient>,
    labels: Vec<String>,
    next: AtomicUsize,
}

impl RpcPool {
    pub fn new(urls: &[&str]) -> Self {
        let clients: Vec<RpcClient> = urls
            .iter()
            .map(|url| {
                RpcClient::new_with_timeout_and_commitment(
                    url.to_string(),
                    Duration::from_millis(400), // fast fail
                    CommitmentConfig::processed(), // freshest
                )
            })
            .collect();
        let labels: Vec<String> = urls.iter().map(|u| {
            // Extract hostname for logging
            u.split("//").nth(1).unwrap_or(u).split('/').next().unwrap_or(u).to_string()
        }).collect();
        Self {
            clients,
            labels,
            next: AtomicUsize::new(0),
        }
    }

    /// Fetch multiple accounts, rotating across endpoints.
    /// Tries each endpoint once until one succeeds.
    pub fn get_multiple_accounts(
        &self,
        keys: &[Pubkey],
    ) -> Option<Vec<Option<Account>>> {
        if self.clients.is_empty() || keys.is_empty() {
            return None;
        }

        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let n = self.clients.len();

        for i in 0..n {
            let idx = (start + i) % n;
            match self.clients[idx].get_multiple_accounts(keys) {
                Ok(accounts) => {
                    debug!(
                        endpoint = %self.labels[idx],
                        keys = keys.len(),
                        "rpc-pool: vault fetch OK"
                    );
                    return Some(accounts);
                }
                Err(e) => {
                    debug!(
                        endpoint = %self.labels[idx],
                        error = %e,
                        "rpc-pool: endpoint failed, trying next"
                    );
                }
            }
        }

        warn!(keys = keys.len(), "rpc-pool: all endpoints failed");
        None
    }

    /// Get latest blockhash from pool.
    pub fn get_latest_blockhash(&self) -> Option<solana_sdk::hash::Hash> {
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let n = self.clients.len();

        for i in 0..n {
            let idx = (start + i) % n;
            match self.clients[idx].get_latest_blockhash() {
                Ok(bh) => return Some(bh),
                Err(_) => continue,
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }
}
