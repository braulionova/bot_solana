// rpc_pool_util.rs — Lightweight RPC pool for background validation.

use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{account::Account, commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub struct SimpleRpcPool {
    clients: Vec<RpcClient>,
    next: AtomicUsize,
}

impl SimpleRpcPool {
    pub fn new(urls: &[String]) -> Self {
        let clients = urls.iter().map(|u| {
            RpcClient::new_with_timeout_and_commitment(
                u.clone(), Duration::from_secs(5), CommitmentConfig::processed(),
            )
        }).collect();
        Self { clients, next: AtomicUsize::new(0) }
    }

    pub fn get_multiple_accounts(&self, keys: &[Pubkey]) -> Option<Vec<Option<Account>>> {
        if self.clients.is_empty() { return None; }
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for i in 0..self.clients.len() {
            let idx = (start + i) % self.clients.len();
            if let Ok(accs) = self.clients[idx].get_multiple_accounts(keys) {
                return Some(accs);
            }
        }
        None
    }
}
