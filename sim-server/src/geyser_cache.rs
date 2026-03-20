//! geyser_cache.rs – Real-time blockhash + account data cache via Yellowstone gRPC.
//!
//! Two improvements over the polling-based BlockhashCache:
//!
//! 1. **Blockhash**: subscribes to `blocks_meta` updates → blockhash arrives within
//!    ~50ms of block confirmation instead of the 400ms polling interval.
//!
//! 2. **Account cache**: subscribes to any account updates for a configurable list
//!    of pubkeys (pool vaults, global configs, etc.). Serves `getMultipleAccounts`
//!    from local DashMap at 0ms instead of 80-150ms upstream RPC.
//!
//! Environment variables (all optional):
//!   GEYSER_ENDPOINT  – Yellowstone gRPC URL, e.g. "https://mainnet.helius-rpc.com"
//!   GEYSER_X_TOKEN   – API key / x-token header value
//!
//! If GEYSER_ENDPOINT is unset or the connection fails, this module silently
//! falls back to the polling-based BlockhashCache and proxied getMultipleAccounts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use futures::StreamExt;
use parking_lot::RwLock;
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use tracing::{debug, info, warn};

use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterBlocksMeta,
};

/// Shared Geyser cache state.
pub struct GeyserCache {
    /// Latest confirmed blockhash.
    pub blockhash: Arc<RwLock<Hash>>,
    /// Account data by pubkey. Value is serialized `Account` (lamports, data, owner, etc.).
    pub accounts: Arc<DashMap<Pubkey, CachedAccount>>,
    /// Whether the Geyser connection is currently active.
    pub connected: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone, Debug)]
pub struct CachedAccount {
    pub lamports: u64,
    pub data: Vec<u8>,
    pub owner: Pubkey,
    pub executable: bool,
    pub rent_epoch: u64,
    pub slot: u64,
}

impl GeyserCache {
    /// Create and start the cache. Returns `None` if `GEYSER_ENDPOINT` is not set.
    pub fn try_start(
        account_keys: Vec<Pubkey>,
        fallback_blockhash: Arc<RwLock<Hash>>,
    ) -> Option<Arc<Self>> {
        let endpoint = std::env::var("GEYSER_ENDPOINT").ok()?;
        let x_token = std::env::var("GEYSER_X_TOKEN").unwrap_or_default();

        info!(endpoint = %endpoint, accounts = account_keys.len(), "starting Geyser cache");

        let cache = Arc::new(Self {
            blockhash: fallback_blockhash,
            accounts: Arc::new(DashMap::new()),
            connected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        let cache_clone = Arc::clone(&cache);
        std::thread::Builder::new()
            .name("geyser-cache".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio rt geyser-cache");
                rt.block_on(run_loop(cache_clone, endpoint, x_token, account_keys));
            })
            .expect("spawn geyser-cache");

        Some(cache)
    }

    pub fn get_blockhash(&self) -> Hash {
        *self.blockhash.read()
    }

    /// Return cached account data or None if not subscribed/not yet received.
    pub fn get_account(&self, key: &Pubkey) -> Option<CachedAccount> {
        self.accounts.get(key).map(|r| r.clone())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Background loop
// ---------------------------------------------------------------------------

async fn run_loop(
    cache: Arc<GeyserCache>,
    endpoint: String,
    x_token: String,
    account_keys: Vec<Pubkey>,
) {
    let mut backoff = Duration::from_secs(2);
    loop {
        cache
            .connected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        match run_once(&cache, &endpoint, &x_token, &account_keys).await {
            Ok(()) => {
                warn!("geyser-cache stream ended — reconnecting");
                backoff = Duration::from_secs(2);
            }
            Err(e) => {
                warn!(error = %e, ?backoff, "geyser-cache error — reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn run_once(
    cache: &Arc<GeyserCache>,
    endpoint: &str,
    x_token: &str,
    account_keys: &[Pubkey],
) -> anyhow::Result<()> {
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.to_string())?;
    if !x_token.is_empty() {
        builder = builder.x_token(Some(x_token.to_string()))?;
    }
    let mut client = builder
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("geyser-cache connect: {e}"))?;

    cache
        .connected
        .store(true, std::sync::atomic::Ordering::Relaxed);
    info!(endpoint, accounts = account_keys.len(), "geyser-cache connected");

    // Build subscription request.
    let mut blocks_meta_filter = HashMap::new();
    blocks_meta_filter.insert(
        "bh".to_string(),
        SubscribeRequestFilterBlocksMeta {},
    );

    let mut accounts_filter = HashMap::new();
    if !account_keys.is_empty() {
        let pubkey_strings: Vec<String> = account_keys
            .iter()
            .map(|k| k.to_string())
            .collect();
        accounts_filter.insert(
            "vault_cache".to_string(),
            SubscribeRequestFilterAccounts {
                account: pubkey_strings,
                owner: vec![],
                filters: vec![],
            },
        );
    }

    let request = SubscribeRequest {
        accounts: accounts_filter,
        slots: HashMap::new(),
        transactions: HashMap::new(),
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: blocks_meta_filter,
        commitment: Some(CommitmentLevel::Confirmed as i32),
        accounts_data_slice: vec![],
        ping: None,
        entry: HashMap::new(),
    };

    let (_sub_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;

    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(|e| anyhow::anyhow!("stream: {e}"))?;

        match msg.update_oneof {
            Some(UpdateOneof::BlockMeta(bm)) => {
                if let Ok(bh) = bm.blockhash.parse::<Hash>() {
                    *cache.blockhash.write() = bh;
                    debug!(slot = bm.slot, blockhash = %bh, "geyser blockhash updated");
                }
            }
            Some(UpdateOneof::Account(acc_update)) => {
                let slot = acc_update.slot;
                if let Some(acc) = acc_update.account {
                    if acc.pubkey.len() == 32 {
                        let key = Pubkey::try_from(acc.pubkey.as_slice()).unwrap_or_default();
                        let owner = if acc.owner.len() == 32 {
                            Pubkey::try_from(acc.owner.as_slice()).unwrap_or_default()
                        } else {
                            Pubkey::default()
                        };
                        let cached = CachedAccount {
                            lamports: acc.lamports,
                            data: acc.data,
                            owner,
                            executable: acc.executable,
                            rent_epoch: acc.rent_epoch,
                            slot,
                        };
                        debug!(%key, slot, lamports = cached.lamports, "geyser account updated");
                        cache.accounts.insert(key, cached);
                    }
                }
            }
            Some(UpdateOneof::Ping(_)) => {}
            _ => {}
        }
    }

    Err(anyhow::anyhow!("geyser stream closed"))
}
