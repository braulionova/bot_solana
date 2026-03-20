//! blockhash_cache.rs – Keeps a recent blockhash updated every 400ms.
//!
//! Uses raw HTTP POST (reqwest) instead of solana RpcClient to avoid
//! the getVersion call that breaks with Agave 3.x version strings.

use parking_lot::RwLock;
use solana_sdk::hash::Hash;
use std::{str::FromStr, sync::Arc, time::Duration};
use tracing::{debug, warn};

pub struct BlockhashCache {
    inner: Arc<RwLock<Hash>>,
}

fn fetch_blockhash(client: &reqwest::blocking::Client, url: &str) -> Option<Hash> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getLatestBlockhash",
        "params": [{"commitment": "confirmed"}]
    });
    let resp = client
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(5))
        .send()
        .ok()?;
    let val: serde_json::Value = resp.json().ok()?;
    let hash_str = val
        .get("result")?
        .get("value")?
        .get("blockhash")?
        .as_str()?;
    Hash::from_str(hash_str).ok()
}

impl BlockhashCache {
    pub fn new(rpc_url: &str, interval: Duration) -> Arc<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client");

        let inner = Arc::new(RwLock::new(Hash::default()));

        // Initial fetch
        match fetch_blockhash(&client, rpc_url) {
            Some(h) => {
                *inner.write() = h;
                debug!(blockhash = %h, "initial blockhash fetched");
            }
            None => warn!("initial blockhash fetch failed"),
        }

        let cache = Arc::new(Self {
            inner: inner.clone(),
        });

        let rpc_url = rpc_url.to_string();
        std::thread::Builder::new()
            .name("blockhash-cache".into())
            .spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .expect("build reqwest client");
                loop {
                    std::thread::sleep(interval);
                    match fetch_blockhash(&client, &rpc_url) {
                        Some(h) => {
                            *inner.write() = h;
                            debug!(blockhash = %h, "blockhash refreshed");
                        }
                        None => warn!("blockhash refresh failed"),
                    }
                }
            })
            .expect("spawn blockhash-cache thread");

        cache
    }

    pub fn get(&self) -> Hash {
        *self.inner.read()
    }

    pub fn inner_arc(&self) -> Arc<RwLock<Hash>> {
        Arc::clone(&self.inner)
    }
}
