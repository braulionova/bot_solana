//! pool_loader.rs — Load pool vault pubkeys from mapped_pools.json + on-chain bootstrap.
//!
//! Two-phase vault discovery:
//!   Phase 1: Read JSON → extract pool_id + any non-default vault_a/vault_b
//!   Phase 2: Fetch pool accounts via RPC → decode vault addresses from on-chain data
//!
//! Returns a deduplicated set of all vault pubkeys to subscribe via gRPC.

use std::collections::HashSet;

use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

/// Raydium AMM V4 account layout offsets for vault addresses.
/// Two variants exist; we try both and pick whichever yields valid pubkeys.
const RAY_LAYOUTS: &[(usize, usize)] = &[
    (336, 368), // V1: coin_vault, pc_vault
    (224, 256), // V2: coin_vault, pc_vault
];

/// Orca Whirlpool account offsets for token vaults.
const ORCA_VAULT_A_OFFSET: usize = 133;
const ORCA_VAULT_B_OFFSET: usize = 213;

/// Meteora DLMM account offsets for token vaults (reserve_x, reserve_y pubkeys).
const METEORA_VAULT_A_OFFSET: usize = 72;
const METEORA_VAULT_B_OFFSET: usize = 104;

#[derive(Debug, Clone, serde::Deserialize)]
struct MappedPool {
    pool_id: String,
    dex: String,
    #[serde(default)]
    vault_a: Option<String>,
    #[serde(default)]
    vault_b: Option<String>,
}

/// Load pool vault pubkeys from JSON file + on-chain bootstrap.
/// Returns (pool_pubkeys, vault_pubkeys) both deduplicated.
pub fn load_pool_vaults(
    pools_file: &str,
    bootstrap_rpc: &str,
) -> Result<Vec<Pubkey>> {
    let data = std::fs::read_to_string(pools_file)
        .with_context(|| format!("read {pools_file}"))?;
    let pools: Vec<MappedPool> = serde_json::from_str(&data)
        .with_context(|| "parse mapped_pools.json")?;

    info!(pools = pools.len(), "loaded mapped_pools.json");

    let default_pk = Pubkey::default();
    let mut all_accounts: HashSet<Pubkey> = HashSet::new();
    let mut pool_pubkeys: Vec<Pubkey> = Vec::new();

    // Phase 1: Extract from JSON (non-default vaults)
    for p in &pools {
        let pool_pk: Pubkey = match p.pool_id.parse() {
            Ok(pk) => pk,
            Err(_) => continue,
        };
        pool_pubkeys.push(pool_pk);
        all_accounts.insert(pool_pk);

        // Add vaults from JSON if they're not placeholder defaults
        if let Some(ref va) = p.vault_a {
            if let Ok(pk) = va.parse::<Pubkey>() {
                if pk != default_pk {
                    all_accounts.insert(pk);
                }
            }
        }
        if let Some(ref vb) = p.vault_b {
            if let Ok(pk) = vb.parse::<Pubkey>() {
                if pk != default_pk {
                    all_accounts.insert(pk);
                }
            }
        }
    }

    let json_vaults = all_accounts.len();
    info!(vaults_from_json = json_vaults, "phase 1 complete");

    // Phase 2: Fetch pool accounts via RPC and decode vault addresses
    let rpc = solana_rpc_client::rpc_client::RpcClient::new_with_commitment(
        bootstrap_rpc.to_string(),
        solana_sdk::commitment_config::CommitmentConfig::confirmed(),
    );

    let batch_size = 100;
    let mut decoded = 0usize;
    let mut failed_batches = 0usize;

    for chunk in pool_pubkeys.chunks(batch_size) {
        match rpc.get_multiple_accounts(chunk) {
            Ok(accounts) => {
                for (pk, maybe_acc) in chunk.iter().zip(accounts.iter()) {
                    if let Some(acc) = maybe_acc {
                        let dex = pools.iter().find(|p| {
                            p.pool_id.parse::<Pubkey>().ok().as_ref() == Some(pk)
                        });
                        let dex_type = dex.map(|d| d.dex.as_str()).unwrap_or("");

                        let vaults = decode_vaults(dex_type, &acc.data);
                        for v in vaults {
                            if v != default_pk {
                                all_accounts.insert(v);
                            }
                        }
                        decoded += 1;
                    }
                }
            }
            Err(e) => {
                failed_batches += 1;
                if failed_batches <= 3 {
                    warn!(error = %e, "RPC batch fetch failed");
                }
            }
        }

        // Delay between batches to avoid rate limiting during bootstrap
        let delay_ms: u64 = std::env::var("BOOTSTRAP_BATCH_SLEEP_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }

    let total = all_accounts.len();
    info!(
        decoded_pools = decoded,
        total_accounts = total,
        failed_batches,
        "phase 2 complete — vault discovery finished"
    );

    Ok(all_accounts.into_iter().collect())
}

/// Decode vault pubkeys from on-chain pool account data based on DEX type.
fn decode_vaults(dex_type: &str, data: &[u8]) -> Vec<Pubkey> {
    let mut vaults = Vec::new();

    if dex_type.contains("Raydium") {
        // Try both Raydium V4 layouts
        for &(va_off, vb_off) in RAY_LAYOUTS {
            if data.len() >= vb_off + 32 {
                if let (Ok(va), Ok(vb)) = (
                    Pubkey::try_from(&data[va_off..va_off + 32]),
                    Pubkey::try_from(&data[vb_off..vb_off + 32]),
                ) {
                    if va != Pubkey::default() && vb != Pubkey::default() {
                        vaults.push(va);
                        vaults.push(vb);
                        break; // Found valid layout
                    }
                }
            }
        }
        // Also try to get open_orders, market accounts for Raydium
        // Layout V1: open_orders=496, market=528
        // Layout V2: open_orders=160, market=320
        for &(oo_off, mkt_off) in &[(496usize, 528usize), (160, 320)] {
            if data.len() >= mkt_off + 32 {
                if let Ok(mkt) = Pubkey::try_from(&data[mkt_off..mkt_off + 32]) {
                    if mkt != Pubkey::default() {
                        vaults.push(mkt); // market account (for serum vault decode)
                    }
                }
                if let Ok(oo) = Pubkey::try_from(&data[oo_off..oo_off + 32]) {
                    if oo != Pubkey::default() {
                        vaults.push(oo);
                    }
                }
            }
        }
    } else if dex_type.contains("Orca") || dex_type.contains("Whirlpool") {
        if data.len() >= ORCA_VAULT_B_OFFSET + 32 {
            if let (Ok(va), Ok(vb)) = (
                Pubkey::try_from(&data[ORCA_VAULT_A_OFFSET..ORCA_VAULT_A_OFFSET + 32]),
                Pubkey::try_from(&data[ORCA_VAULT_B_OFFSET..ORCA_VAULT_B_OFFSET + 32]),
            ) {
                vaults.push(va);
                vaults.push(vb);
            }
        }
    } else if dex_type.contains("Meteora") {
        if data.len() >= METEORA_VAULT_B_OFFSET + 32 {
            if let (Ok(va), Ok(vb)) = (
                Pubkey::try_from(&data[METEORA_VAULT_A_OFFSET..METEORA_VAULT_A_OFFSET + 32]),
                Pubkey::try_from(&data[METEORA_VAULT_B_OFFSET..METEORA_VAULT_B_OFFSET + 32]),
            ) {
                vaults.push(va);
                vaults.push(vb);
            }
        }
    } else if dex_type.contains("PumpSwap") {
        // PumpSwap pool vaults at offsets 139 and 171
        if data.len() >= 171 + 32 {
            if let (Ok(va), Ok(vb)) = (
                Pubkey::try_from(&data[139..171]),
                Pubkey::try_from(&data[171..203]),
            ) {
                vaults.push(va);
                vaults.push(vb);
            }
        }
    }

    vaults
}
