//! parse_snapshot_pools — Scan Agave append-vec files to extract DEX pool metadata.
//!
//! Reads the accounts directory produced by Agave bootstrap/replay, finds accounts
//! owned by Raydium V4, Orca Whirlpool, Meteora DLMM, and PumpSwap, extracts static
//! pool metadata, and saves to Redis for instant loading by helios-bot.
//!
//! Usage:
//!   parse_snapshot_pools --accounts-dir /root/solana-spy-ledger/accounts/run/
//!   REDIS_URL=redis://127.0.0.1:6379 parse_snapshot_pools --accounts-dir ...

use anyhow::{Context, Result};
use memmap2::Mmap;
use rayon::prelude::*;
use redis::{Client, Connection};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

// ── Append-vec entry layout ────────────────────────────────────────────────
// StoredMeta:   write_version(u64=8) + data_len(u64=8) + pubkey(32) = 48
// AccountMeta:  lamports(u64=8) + rent_epoch(u64=8) + owner(32) + executable(u8=1) + padding(7) = 56
// Hash:         32 bytes
// Total header: 136 bytes, then data_len bytes of account data.
const HEADER_SIZE: usize = 136;
const OFFSET_DATA_LEN: usize = 8;
const OFFSET_PUBKEY: usize = 16;
const OFFSET_LAMPORTS: usize = 48;
const OFFSET_OWNER: usize = 64;

// Meteora LB Pair discriminator (Anchor)
const LB_PAIR_DISC: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];

// ── CachedPoolMeta — matches redis_cache.rs format exactly ─────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedPoolMeta {
    dex_type: String,
    token_a: String,
    token_b: String,
    fee_bps: u64,
    // Raydium V4
    vault_a: Option<String>,
    vault_b: Option<String>,
    market_id: Option<String>,
    market_program: Option<String>,
    market_bids: Option<String>,
    market_asks: Option<String>,
    market_event_queue: Option<String>,
    market_base_vault: Option<String>,
    market_quote_vault: Option<String>,
    market_vault_signer: Option<String>,
    open_orders: Option<String>,
    // Orca
    orca_vault_a: Option<String>,
    orca_vault_b: Option<String>,
    orca_tick_spacing: Option<u16>,
    // Meteora
    meteora_vault_a: Option<String>,
    meteora_vault_b: Option<String>,
    meteora_bin_step: Option<u16>,
}

/// Decoded program ID bytes for owner comparison.
struct TargetPrograms {
    raydium: [u8; 32],
    orca: [u8; 32],
    meteora: [u8; 32],
    pumpswap: [u8; 32],
    openbook: [u8; 32],
    spl_token: [u8; 32],
}

impl TargetPrograms {
    fn new() -> Self {
        Self {
            raydium: bs58_decode_32("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"),
            orca: bs58_decode_32("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
            meteora: bs58_decode_32("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),
            pumpswap: bs58_decode_32("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
            openbook: bs58_decode_32("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX"),
            spl_token: bs58_decode_32("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        }
    }
}

/// Decoded OpenBook/Serum market account fields.
#[derive(Debug, Clone)]
struct MarketAccountData {
    bids: Pubkey,
    asks: Pubkey,
    event_queue: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    vault_signer: Pubkey,
}

/// SPL Token account vault balance.
#[derive(Debug, Clone, Serialize)]
struct VaultBalance {
    amount: u64,
    slot: u64,
}

// ── Raydium AMM V4 layout variants ─────────────────────────────────────────
struct RaydiumLayout {
    coin_vault: usize,
    pc_vault: usize,
    coin_mint: usize,
    pc_mint: usize,
    open_orders: usize,
    market: usize,
    market_program: usize,
}

const RAYDIUM_LAYOUTS: [RaydiumLayout; 2] = [
    // Standard layout (most common)
    RaydiumLayout {
        coin_vault: 336,
        pc_vault: 368,
        coin_mint: 432,
        pc_mint: 464,
        open_orders: 496,
        market: 528,
        market_program: 560,
    },
    // Alternate layout (older pools)
    RaydiumLayout {
        coin_vault: 224,
        pc_vault: 256,
        coin_mint: 288,
        pc_mint: 320,
        open_orders: 160,
        market: 320,
        market_program: 352,
    },
];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let accounts_dir = if let Some(pos) = args.iter().position(|a| a == "--accounts-dir") {
        args.get(pos + 1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/root/solana-spy-ledger/accounts/run"))
    } else {
        PathBuf::from("/root/solana-spy-ledger/accounts/run")
    };

    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let targets = TargetPrograms::new();

    eprintln!(
        "parse_snapshot_pools: scanning {} for DEX pool accounts",
        accounts_dir.display()
    );
    eprintln!("  Redis: {}", redis_url);
    eprintln!(
        "  Raydium V4: {}",
        Pubkey::new_from_array(targets.raydium)
    );
    eprintln!(
        "  Orca Whirlpool: {}",
        Pubkey::new_from_array(targets.orca)
    );
    eprintln!(
        "  Meteora DLMM: {}",
        Pubkey::new_from_array(targets.meteora)
    );
    eprintln!(
        "  PumpSwap AMM: {}",
        Pubkey::new_from_array(targets.pumpswap)
    );

    // Collect all append-vec files
    let start = Instant::now();
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&accounts_dir).context("read accounts dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            files.push(path);
        }
    }
    let total_files = files.len();
    eprintln!("  Found {} append-vec files", total_files);

    // Counters
    let files_processed = AtomicU64::new(0);
    let accounts_scanned = AtomicU64::new(0);
    let raydium_count = AtomicU64::new(0);
    let orca_count = AtomicU64::new(0);
    let meteora_count = AtomicU64::new(0);
    let pumpswap_count = AtomicU64::new(0);
    let errors = AtomicU64::new(0);

    // Process files in parallel, collect results
    let results: Vec<(String, CachedPoolMeta)> = files
        .par_iter()
        .flat_map(|path| {
            let mut local_results: Vec<(String, CachedPoolMeta)> = Vec::new();

            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    return local_results;
                }
            };

            let file_len = match file.metadata() {
                Ok(m) => m.len() as usize,
                Err(_) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    return local_results;
                }
            };

            if file_len < HEADER_SIZE {
                files_processed.fetch_add(1, Ordering::Relaxed);
                return local_results;
            }

            // Safety: read-only mmap. Concurrent Agave writes may cause stale reads
            // but not UB — we tolerate garbage by validating all offsets.
            let mmap = match unsafe { Mmap::map(&file) } {
                Ok(m) => m,
                Err(_) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    return local_results;
                }
            };

            let data = &mmap[..];
            let mut offset = 0usize;

            while offset + HEADER_SIZE <= data.len() {
                let data_len = u64::from_le_bytes(
                    match data[offset + OFFSET_DATA_LEN..offset + OFFSET_DATA_LEN + 8].try_into()
                    {
                        Ok(b) => b,
                        Err(_) => break,
                    },
                ) as usize;

                // Sanity: data_len must be reasonable and fit within the file
                if data_len > 100_000_000 || offset + HEADER_SIZE + data_len > data.len() {
                    break;
                }

                let lamports = u64::from_le_bytes(
                    data[offset + OFFSET_LAMPORTS..offset + OFFSET_LAMPORTS + 8]
                        .try_into()
                        .unwrap_or([0u8; 8]),
                );

                accounts_scanned.fetch_add(1, Ordering::Relaxed);

                // Skip closed accounts
                if lamports > 0 && data_len > 0 {
                    let owner = &data[offset + OFFSET_OWNER..offset + OFFSET_OWNER + 32];
                    let account_data = &data[offset + HEADER_SIZE..offset + HEADER_SIZE + data_len];
                    let pubkey_bytes: [u8; 32] =
                        data[offset + OFFSET_PUBKEY..offset + OFFSET_PUBKEY + 32]
                            .try_into()
                            .unwrap_or([0u8; 32]);
                    let pubkey = Pubkey::new_from_array(pubkey_bytes);

                    if owner == &targets.raydium {
                        if let Some(meta) = decode_raydium(account_data) {
                            local_results.push((pubkey.to_string(), meta));
                            raydium_count.fetch_add(1, Ordering::Relaxed);
                        }
                    } else if owner == &targets.orca {
                        if let Some(meta) = decode_orca(account_data) {
                            local_results.push((pubkey.to_string(), meta));
                            orca_count.fetch_add(1, Ordering::Relaxed);
                        }
                    } else if owner == &targets.meteora {
                        if let Some(meta) = decode_meteora(account_data) {
                            local_results.push((pubkey.to_string(), meta));
                            meteora_count.fetch_add(1, Ordering::Relaxed);
                        }
                    } else if owner == &targets.pumpswap {
                        if let Some(meta) = decode_pumpswap(account_data) {
                            local_results.push((pubkey.to_string(), meta));
                            pumpswap_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }

                // Advance to next entry (aligned to 8 bytes)
                let entry_size = HEADER_SIZE + data_len;
                offset += (entry_size + 7) & !7;
            }

            let n = files_processed.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 5000 == 0 {
                eprintln!(
                    "  progress: {}/{} files, {} accounts, {} pools",
                    n,
                    total_files,
                    accounts_scanned.load(Ordering::Relaxed),
                    raydium_count.load(Ordering::Relaxed)
                        + orca_count.load(Ordering::Relaxed)
                        + meteora_count.load(Ordering::Relaxed)
                        + pumpswap_count.load(Ordering::Relaxed),
                );
            }

            local_results
        })
        .collect();

    let scan_elapsed = start.elapsed();
    let total_pools = results.len();

    eprintln!("\n=== Scan complete in {:.1}s ===", scan_elapsed.as_secs_f64());
    eprintln!(
        "  Files: {}  Accounts: {}  Errors: {}",
        files_processed.load(Ordering::Relaxed),
        accounts_scanned.load(Ordering::Relaxed),
        errors.load(Ordering::Relaxed),
    );
    eprintln!(
        "  Raydium: {}  Orca: {}  Meteora: {}  PumpSwap: {}",
        raydium_count.load(Ordering::Relaxed),
        orca_count.load(Ordering::Relaxed),
        meteora_count.load(Ordering::Relaxed),
        pumpswap_count.load(Ordering::Relaxed),
    );
    eprintln!("  Total pool accounts (pre-dedup): {}", total_pools);

    // Deduplicate: keep latest per pubkey (last write wins in append-vec)
    let mut deduped: HashMap<String, CachedPoolMeta> = HashMap::with_capacity(total_pools);
    for (pubkey, meta) in results {
        deduped.insert(pubkey, meta);
    }
    eprintln!("  Unique pools after dedup: {}", deduped.len());

    // Save to Redis
    let redis_start = Instant::now();
    match Client::open(redis_url.as_str()) {
        Ok(client) => match client.get_connection() {
            Ok(mut conn) => {
                let saved = save_to_redis(&mut conn, &deduped)?;
                eprintln!(
                    "  Saved {} pools to Redis in {:.1}s",
                    saved,
                    redis_start.elapsed().as_secs_f64()
                );
            }
            Err(e) => {
                eprintln!(
                    "  WARNING: Redis connection failed: {}. Dumping to stdout as JSON.",
                    e
                );
                dump_json(&deduped);
            }
        },
        Err(e) => {
            eprintln!(
                "  WARNING: Redis client error: {}. Dumping to stdout as JSON.",
                e
            );
            dump_json(&deduped);
        }
    }

    // ── Phase 2: OpenBook market accounts + SPL Token vault balances ────────
    eprintln!("\n=== Phase 2: OpenBook market resolution + vault balances ===");

    // 2a. Collect unique market_ids from Raydium pools, and all known vault pubkeys.
    let mut market_ids: HashSet<[u8; 32]> = HashSet::new();
    let mut market_id_to_program: HashMap<[u8; 32], [u8; 32]> = HashMap::new();
    // Map market_id -> list of pool pubkeys referencing it
    let mut market_to_pools: HashMap<[u8; 32], Vec<String>> = HashMap::new();
    // All vault pubkeys we want balances for
    let mut vault_pubkeys: HashSet<[u8; 32]> = HashSet::new();

    for (pool_key, meta) in &deduped {
        if let Some(ref mid) = meta.market_id {
            if let Ok(market_pk) = mid.parse::<Pubkey>() {
                let bytes = market_pk.to_bytes();
                market_ids.insert(bytes);
                market_to_pools
                    .entry(bytes)
                    .or_default()
                    .push(pool_key.clone());
                if let Some(ref mp) = meta.market_program {
                    if let Ok(mp_pk) = mp.parse::<Pubkey>() {
                        market_id_to_program.insert(bytes, mp_pk.to_bytes());
                    }
                }
            }
        }
        // Collect all vault pubkeys for balance extraction
        for vault_str in [
            meta.vault_a.as_deref(),
            meta.vault_b.as_deref(),
            meta.orca_vault_a.as_deref(),
            meta.orca_vault_b.as_deref(),
            meta.meteora_vault_a.as_deref(),
            meta.meteora_vault_b.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(pk) = vault_str.parse::<Pubkey>() {
                vault_pubkeys.insert(pk.to_bytes());
            }
        }
    }

    eprintln!(
        "  Unique market_ids to resolve: {}",
        market_ids.len()
    );
    eprintln!(
        "  Vault pubkeys to scan balances: {}",
        vault_pubkeys.len()
    );

    let phase2_start = Instant::now();
    let markets_found = AtomicU64::new(0);
    let vaults_found = AtomicU64::new(0);
    let phase2_accounts = AtomicU64::new(0);
    let phase2_files_done = AtomicU64::new(0);

    // Thread-safe collectors for Phase 2 results
    let market_results: Mutex<HashMap<[u8; 32], MarketAccountData>> =
        Mutex::new(HashMap::with_capacity(market_ids.len()));
    let vault_results: Mutex<HashMap<[u8; 32], VaultBalance>> =
        Mutex::new(HashMap::with_capacity(vault_pubkeys.len()));

    // Snapshot slot: read from the first append-vec filename (format: <slot>.<id>)
    let snapshot_slot: u64 = files
        .first()
        .and_then(|p| {
            p.file_name()?
                .to_str()?
                .split('.')
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0);

    files.par_iter().for_each(|path| {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let file_len = match file.metadata() {
            Ok(m) => m.len() as usize,
            Err(_) => return,
        };
        if file_len < HEADER_SIZE {
            phase2_files_done.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mmap = match unsafe { Mmap::map(&file) } {
            Ok(m) => m,
            Err(_) => return,
        };

        let data = &mmap[..];
        let mut offset = 0usize;
        let mut local_markets: Vec<([u8; 32], MarketAccountData)> = Vec::new();
        let mut local_vaults: Vec<([u8; 32], VaultBalance)> = Vec::new();

        while offset + HEADER_SIZE <= data.len() {
            let data_len = match data[offset + OFFSET_DATA_LEN..offset + OFFSET_DATA_LEN + 8]
                .try_into()
            {
                Ok(b) => u64::from_le_bytes(b) as usize,
                Err(_) => break,
            };

            if data_len > 100_000_000 || offset + HEADER_SIZE + data_len > data.len() {
                break;
            }

            let lamports = u64::from_le_bytes(
                data[offset + OFFSET_LAMPORTS..offset + OFFSET_LAMPORTS + 8]
                    .try_into()
                    .unwrap_or([0u8; 8]),
            );

            phase2_accounts.fetch_add(1, Ordering::Relaxed);

            if lamports > 0 && data_len > 0 {
                let owner = &data[offset + OFFSET_OWNER..offset + OFFSET_OWNER + 32];
                let pubkey_bytes: [u8; 32] =
                    data[offset + OFFSET_PUBKEY..offset + OFFSET_PUBKEY + 32]
                        .try_into()
                        .unwrap_or([0u8; 32]);
                let account_data =
                    &data[offset + HEADER_SIZE..offset + HEADER_SIZE + data_len];

                // OpenBook market account
                if owner == &targets.openbook && market_ids.contains(&pubkey_bytes) {
                    let market_pk = Pubkey::new_from_array(pubkey_bytes);
                    let market_program_bytes = market_id_to_program
                        .get(&pubkey_bytes)
                        .copied()
                        .unwrap_or(targets.openbook);
                    let market_program = Pubkey::new_from_array(market_program_bytes);

                    if let Some(mkt) =
                        decode_openbook_market(account_data, &market_pk, &market_program)
                    {
                        // Also add discovered market vaults to vault_pubkeys for balance scan
                        // (they're in this same pass so we check them too)
                        local_markets.push((pubkey_bytes, mkt));
                        markets_found.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // SPL Token account — check if it's a vault we care about
                if owner == &targets.spl_token
                    && data_len >= 72
                    && vault_pubkeys.contains(&pubkey_bytes)
                {
                    let amount = u64::from_le_bytes(
                        account_data[64..72].try_into().unwrap_or([0u8; 8]),
                    );
                    local_vaults.push((
                        pubkey_bytes,
                        VaultBalance {
                            amount,
                            slot: snapshot_slot,
                        },
                    ));
                    vaults_found.fetch_add(1, Ordering::Relaxed);
                }
            }

            let entry_size = HEADER_SIZE + data_len;
            offset += (entry_size + 7) & !7;
        }

        // Merge local results into global maps
        if !local_markets.is_empty() {
            let mut guard = market_results.lock().unwrap();
            for (k, v) in local_markets {
                guard.insert(k, v);
            }
        }
        if !local_vaults.is_empty() {
            let mut guard = vault_results.lock().unwrap();
            for (k, v) in local_vaults {
                guard.insert(k, v);
            }
        }

        let n = phase2_files_done.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 5000 == 0 {
            eprintln!(
                "  phase2 progress: {}/{} files, {} markets, {} vaults",
                n,
                total_files,
                markets_found.load(Ordering::Relaxed),
                vaults_found.load(Ordering::Relaxed),
            );
        }
    });

    let market_map = market_results.into_inner().unwrap();
    let vault_map = vault_results.into_inner().unwrap();

    eprintln!(
        "\n  Phase 2 scan complete in {:.1}s",
        phase2_start.elapsed().as_secs_f64()
    );
    eprintln!(
        "  Accounts scanned: {}  Markets found: {}  Vaults found: {}",
        phase2_accounts.load(Ordering::Relaxed),
        market_map.len(),
        vault_map.len(),
    );

    // 2b. Update Raydium pool entries in Redis with market sub-accounts
    let redis_phase2_start = Instant::now();
    let mut pools_updated = 0u64;
    let mut market_vaults_for_balance: Vec<[u8; 32]> = Vec::new();

    for (market_bytes, mkt) in &market_map {
        // Collect market vaults we discovered — they need balance scans too
        market_vaults_for_balance.push(mkt.base_vault.to_bytes());
        market_vaults_for_balance.push(mkt.quote_vault.to_bytes());

        if let Some(pool_keys) = market_to_pools.get(market_bytes) {
            for pool_key in pool_keys {
                if let Some(meta) = deduped.get_mut(pool_key) {
                    meta.market_bids = Some(mkt.bids.to_string());
                    meta.market_asks = Some(mkt.asks.to_string());
                    meta.market_event_queue = Some(mkt.event_queue.to_string());
                    meta.market_base_vault = Some(mkt.base_vault.to_string());
                    meta.market_quote_vault = Some(mkt.quote_vault.to_string());
                    meta.market_vault_signer = Some(mkt.vault_signer.to_string());
                    pools_updated += 1;
                }
            }
        }
    }

    eprintln!(
        "  Updated {} Raydium pools with market sub-accounts ({} unique markets)",
        pools_updated,
        market_map.len()
    );

    // 2c. Save updated pools + vault balances to Redis
    match Client::open(redis_url.as_str()) {
        Ok(client) => match client.get_connection() {
            Ok(mut conn) => {
                // Re-save updated Raydium pools
                let resaved = save_to_redis(&mut conn, &deduped)?;
                eprintln!(
                    "  Re-saved {} pools to Redis (with market data) in {:.1}s",
                    resaved,
                    redis_phase2_start.elapsed().as_secs_f64()
                );

                // Save vault balances
                let vaults_saved = save_vault_balances(&mut conn, &vault_map)?;
                eprintln!("  Saved {} vault balances to Redis", vaults_saved);
            }
            Err(e) => {
                eprintln!("  WARNING: Redis connection failed for phase 2: {}", e);
            }
        },
        Err(e) => {
            eprintln!("  WARNING: Redis client error for phase 2: {}", e);
        }
    }

    // 2d. If we discovered new market vaults, do a third pass for their balances
    let new_market_vaults: Vec<[u8; 32]> = market_vaults_for_balance
        .into_iter()
        .filter(|v| !vault_map.contains_key(v))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    if !new_market_vaults.is_empty() {
        eprintln!(
            "\n=== Phase 2b: Scanning {} newly-discovered market vault balances ===",
            new_market_vaults.len()
        );
        let market_vault_set: HashSet<[u8; 32]> = new_market_vaults.into_iter().collect();
        let extra_vaults_found = AtomicU64::new(0);
        let extra_vault_results: Mutex<HashMap<[u8; 32], VaultBalance>> =
            Mutex::new(HashMap::new());

        files.par_iter().for_each(|path| {
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return,
            };
            let file_len = match file.metadata() {
                Ok(m) => m.len() as usize,
                Err(_) => return,
            };
            if file_len < HEADER_SIZE {
                return;
            }
            let mmap = match unsafe { Mmap::map(&file) } {
                Ok(m) => m,
                Err(_) => return,
            };

            let data = &mmap[..];
            let mut offset = 0usize;
            let mut local_vaults: Vec<([u8; 32], VaultBalance)> = Vec::new();

            while offset + HEADER_SIZE <= data.len() {
                let data_len = match data
                    [offset + OFFSET_DATA_LEN..offset + OFFSET_DATA_LEN + 8]
                    .try_into()
                {
                    Ok(b) => u64::from_le_bytes(b) as usize,
                    Err(_) => break,
                };
                if data_len > 100_000_000 || offset + HEADER_SIZE + data_len > data.len() {
                    break;
                }
                let lamports = u64::from_le_bytes(
                    data[offset + OFFSET_LAMPORTS..offset + OFFSET_LAMPORTS + 8]
                        .try_into()
                        .unwrap_or([0u8; 8]),
                );
                if lamports > 0 && data_len >= 72 {
                    let owner = &data[offset + OFFSET_OWNER..offset + OFFSET_OWNER + 32];
                    let pubkey_bytes: [u8; 32] =
                        data[offset + OFFSET_PUBKEY..offset + OFFSET_PUBKEY + 32]
                            .try_into()
                            .unwrap_or([0u8; 32]);
                    if owner == &targets.spl_token && market_vault_set.contains(&pubkey_bytes)
                    {
                        let account_data =
                            &data[offset + HEADER_SIZE..offset + HEADER_SIZE + data_len];
                        let amount = u64::from_le_bytes(
                            account_data[64..72].try_into().unwrap_or([0u8; 8]),
                        );
                        local_vaults.push((
                            pubkey_bytes,
                            VaultBalance {
                                amount,
                                slot: snapshot_slot,
                            },
                        ));
                        extra_vaults_found.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let entry_size = HEADER_SIZE + data_len;
                offset += (entry_size + 7) & !7;
            }

            if !local_vaults.is_empty() {
                let mut guard = extra_vault_results.lock().unwrap();
                for (k, v) in local_vaults {
                    guard.insert(k, v);
                }
            }
        });

        let extra_map = extra_vault_results.into_inner().unwrap();
        eprintln!(
            "  Found {} market vault balances",
            extra_map.len()
        );

        if !extra_map.is_empty() {
            if let Ok(client) = Client::open(redis_url.as_str()) {
                if let Ok(mut conn) = client.get_connection() {
                    let saved = save_vault_balances(&mut conn, &extra_map)?;
                    eprintln!("  Saved {} market vault balances to Redis", saved);
                }
            }
        }
    }

    let total_elapsed = start.elapsed();
    eprintln!("\nDone in {:.1}s total.", total_elapsed.as_secs_f64());

    Ok(())
}

// ── Decoders ───────────────────────────────────────────────────────────────

fn decode_raydium(data: &[u8]) -> Option<CachedPoolMeta> {
    if data.len() < 624 {
        return None;
    }

    for layout in &RAYDIUM_LAYOUTS {
        let max_needed = *[
            layout.coin_vault + 32,
            layout.pc_vault + 32,
            layout.open_orders + 32,
            layout.market + 32,
            layout.market_program + 32,
        ]
        .iter()
        .max()?;

        if data.len() < max_needed {
            continue;
        }

        let vault_a = read_pubkey(data, layout.coin_vault)?;
        let vault_b = read_pubkey(data, layout.pc_vault)?;

        // Skip if vaults are default (placeholder/uninitialized)
        if vault_a == Pubkey::default() || vault_b == Pubkey::default() {
            continue;
        }

        let open_orders = read_pubkey(data, layout.open_orders)?;
        let market_id = read_pubkey(data, layout.market)?;
        let market_program = read_pubkey(data, layout.market_program)?;

        // Extract mints if within bounds
        let (token_a, token_b) = if data.len() >= layout.pc_mint + 32 {
            let mint_a = read_pubkey(data, layout.coin_mint)?;
            let mint_b = read_pubkey(data, layout.pc_mint)?;
            if mint_a != Pubkey::default() && mint_b != Pubkey::default() {
                (mint_a.to_string(), mint_b.to_string())
            } else {
                (String::new(), String::new())
            }
        } else {
            (String::new(), String::new())
        };

        return Some(CachedPoolMeta {
            dex_type: "RaydiumAmmV4".into(),
            token_a,
            token_b,
            fee_bps: 25,
            vault_a: Some(vault_a.to_string()),
            vault_b: Some(vault_b.to_string()),
            market_id: Some(market_id.to_string()),
            market_program: Some(market_program.to_string()),
            market_bids: None,
            market_asks: None,
            market_event_queue: None,
            market_base_vault: None,
            market_quote_vault: None,
            market_vault_signer: None,
            open_orders: Some(open_orders.to_string()),
            orca_vault_a: None,
            orca_vault_b: None,
            orca_tick_spacing: None,
            meteora_vault_a: None,
            meteora_vault_b: None,
            meteora_bin_step: None,
        });
    }

    None
}

fn decode_orca(data: &[u8]) -> Option<CachedPoolMeta> {
    // Orca Whirlpool layout (production offsets from pool_hydrator.rs):
    //   tick_spacing@41, liquidity@49, sqrt_price@65, tick_current_index@81
    //   token_mint_a@101, token_vault_a@133
    //   token_mint_b@181, token_vault_b@213
    //   fee_rate@60 (u16)
    if data.len() < 245 {
        return None;
    }

    let tick_spacing = u16::from_le_bytes(data[41..43].try_into().ok()?);
    if tick_spacing == 0 || tick_spacing > 32768 {
        return None;
    }

    let vault_a = read_pubkey(data, 133)?;
    let vault_b = read_pubkey(data, 213)?;
    if vault_a == Pubkey::default() || vault_b == Pubkey::default() {
        return None;
    }

    let mint_a = read_pubkey(data, 101)?;
    let mint_b = read_pubkey(data, 181)?;

    // fee_rate at offset 60: hundredths of a basis point. 3000 = 30 bps.
    let fee_rate = u16::from_le_bytes(data[60..62].try_into().ok()?) as u64;
    let fee_bps = fee_rate / 100;

    Some(CachedPoolMeta {
        dex_type: "OrcaWhirlpool".into(),
        token_a: mint_a.to_string(),
        token_b: mint_b.to_string(),
        fee_bps,
        vault_a: None,
        vault_b: None,
        market_id: None,
        market_program: None,
        market_bids: None,
        market_asks: None,
        market_event_queue: None,
        market_base_vault: None,
        market_quote_vault: None,
        market_vault_signer: None,
        open_orders: None,
        orca_vault_a: Some(vault_a.to_string()),
        orca_vault_b: Some(vault_b.to_string()),
        orca_tick_spacing: Some(tick_spacing),
        meteora_vault_a: None,
        meteora_vault_b: None,
        meteora_bin_step: None,
    })
}

fn decode_meteora(data: &[u8]) -> Option<CachedPoolMeta> {
    // Meteora DLMM LB Pair layout (production offsets from meteora.rs):
    //   discriminator@0-8, active_id@76, bin_step@80
    //   token_x_mint@88, token_y_mint@120
    //   reserve_x(vault_a)@152, reserve_y(vault_b)@184
    if data.len() < 216 {
        return None;
    }

    // Verify Anchor discriminator
    if data[..8] != LB_PAIR_DISC {
        return None;
    }

    let bin_step = u16::from_le_bytes(data[80..82].try_into().ok()?);
    if bin_step == 0 || bin_step > 1000 {
        return None;
    }

    let mint_a = read_pubkey(data, 88)?;
    let mint_b = read_pubkey(data, 120)?;
    let vault_a = read_pubkey(data, 152)?;
    let vault_b = read_pubkey(data, 184)?;

    if vault_a == Pubkey::default() || vault_b == Pubkey::default() {
        return None;
    }

    let fee_bps = (bin_step as u64).max(1);

    Some(CachedPoolMeta {
        dex_type: "MeteoraDlmm".into(),
        token_a: mint_a.to_string(),
        token_b: mint_b.to_string(),
        fee_bps,
        vault_a: None,
        vault_b: None,
        market_id: None,
        market_program: None,
        market_bids: None,
        market_asks: None,
        market_event_queue: None,
        market_base_vault: None,
        market_quote_vault: None,
        market_vault_signer: None,
        open_orders: None,
        orca_vault_a: None,
        orca_vault_b: None,
        orca_tick_spacing: None,
        meteora_vault_a: Some(vault_a.to_string()),
        meteora_vault_b: Some(vault_b.to_string()),
        meteora_bin_step: Some(bin_step),
    })
}

fn decode_pumpswap(data: &[u8]) -> Option<CachedPoolMeta> {
    // PumpSwap AMM layout (from CLAUDE.md verified):
    //   base_mint@43, quote_mint@75
    //   pool_base_vault@139, pool_quote_vault@171
    if data.len() < 203 {
        return None;
    }

    let base_mint = read_pubkey(data, 43)?;
    let quote_mint = read_pubkey(data, 75)?;
    let vault_a = read_pubkey(data, 139)?;
    let vault_b = read_pubkey(data, 171)?;

    if base_mint == Pubkey::default()
        || quote_mint == Pubkey::default()
        || vault_a == Pubkey::default()
        || vault_b == Pubkey::default()
    {
        return None;
    }

    Some(CachedPoolMeta {
        dex_type: "PumpSwap".into(),
        token_a: base_mint.to_string(),
        token_b: quote_mint.to_string(),
        fee_bps: 25,
        vault_a: Some(vault_a.to_string()),
        vault_b: Some(vault_b.to_string()),
        market_id: None,
        market_program: None,
        market_bids: None,
        market_asks: None,
        market_event_queue: None,
        market_base_vault: None,
        market_quote_vault: None,
        market_vault_signer: None,
        open_orders: None,
        orca_vault_a: None,
        orca_vault_b: None,
        orca_tick_spacing: None,
        meteora_vault_a: None,
        meteora_vault_b: None,
        meteora_bin_step: None,
    })
}

// ── Redis save ─────────────────────────────────────────────────────────────

fn save_to_redis(conn: &mut Connection, pools: &HashMap<String, CachedPoolMeta>) -> Result<usize> {
    let mut count = 0usize;
    let batch_size = 500;
    let entries: Vec<_> = pools.iter().collect();

    for chunk in entries.chunks(batch_size) {
        let mut pipe = redis::pipe();
        for (pubkey, meta) in chunk {
            if let Ok(json) = serde_json::to_string(meta) {
                let key = format!("meta:{}", pubkey);
                pipe.set_ex(key, json, 86400u64);
                count += 1;
            }
        }
        pipe.query::<()>(conn).context("redis pipeline exec")?;
    }

    Ok(count)
}

fn dump_json(pools: &HashMap<String, CachedPoolMeta>) {
    let json = serde_json::to_string_pretty(pools).unwrap_or_default();
    println!("{}", json);
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn read_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    if data.len() < offset + 32 {
        return None;
    }
    let bytes: [u8; 32] = data[offset..offset + 32].try_into().ok()?;
    Some(Pubkey::new_from_array(bytes))
}

fn bs58_decode_32(s: &str) -> [u8; 32] {
    let bytes = bs58::decode(s).into_vec().expect("valid base58");
    assert_eq!(bytes.len(), 32, "expected 32 bytes from base58 decode");
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

// ── Phase 2 decoders ─────────────────────────────────────────────────────

/// Decode an OpenBook/Serum market account to extract bids, asks, event_queue,
/// base_vault, quote_vault, and derive vault_signer.
/// Layout matches pool_hydrator.rs decode_serum_market exactly.
fn decode_openbook_market(
    data: &[u8],
    market_id: &Pubkey,
    market_program: &Pubkey,
) -> Option<MarketAccountData> {
    const HEAD_PADDING: &[u8; 5] = b"serum";
    const TAIL_PADDING: &[u8; 7] = b"padding";
    const PADDED_MARKET_LEN: usize = 5 + (47 * 8) + 7; // 388

    if data.len() < PADDED_MARKET_LEN || &data[..5] != HEAD_PADDING {
        return None;
    }
    if &data[PADDED_MARKET_LEN - 7..PADDED_MARKET_LEN] != TAIL_PADDING {
        return None;
    }

    let nonce_offset = 5 + (5 * 8);        // 45
    let coin_vault_offset = 5 + (14 * 8);  // 117  (but actually used as base_vault)
    let pc_vault_offset = 5 + (20 * 8);    // 165  (quote_vault)
    let event_q_offset = 5 + (31 * 8);     // 253
    let bids_offset = 5 + (35 * 8);        // 285
    let asks_offset = 5 + (39 * 8);        // 317

    let mut nonce_bytes = [0u8; 8];
    nonce_bytes.copy_from_slice(&data[nonce_offset..nonce_offset + 8]);
    let vault_signer_nonce = u64::from_le_bytes(nonce_bytes);

    let bids = read_pubkey(data, bids_offset)?;
    let asks = read_pubkey(data, asks_offset)?;
    let event_queue = read_pubkey(data, event_q_offset)?;
    let base_vault = read_pubkey(data, coin_vault_offset)?;
    let quote_vault = read_pubkey(data, pc_vault_offset)?;

    let vault_signer = Pubkey::create_program_address(
        &[market_id.as_ref(), &vault_signer_nonce.to_le_bytes()],
        market_program,
    )
    .ok()?;

    Some(MarketAccountData {
        bids,
        asks,
        event_queue,
        base_vault,
        quote_vault,
        vault_signer,
    })
}

/// Save vault balances to Redis as `vault:<pubkey>` -> JSON.
fn save_vault_balances(
    conn: &mut Connection,
    vaults: &HashMap<[u8; 32], VaultBalance>,
) -> Result<usize> {
    let mut count = 0usize;
    let batch_size = 500;
    let entries: Vec<_> = vaults.iter().collect();

    for chunk in entries.chunks(batch_size) {
        let mut pipe = redis::pipe();
        for (pubkey_bytes, balance) in chunk {
            let pubkey = Pubkey::new_from_array(**pubkey_bytes);
            if let Ok(json) = serde_json::to_string(balance) {
                let key = format!("vault:{}", pubkey);
                pipe.set_ex(key, json, 86400u64);
                count += 1;
            }
        }
        pipe.query::<()>(conn).context("redis vault pipeline exec")?;
    }

    Ok(count)
}
