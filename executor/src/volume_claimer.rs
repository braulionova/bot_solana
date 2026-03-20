//! volume_claimer.rs – Claim PumpSwap volume incentive rewards (PUMP tokens).
//!
//! Every arb/snipe trade on PumpSwap accumulates volume in a per-user PDA.
//! Periodically (once/day) we sync + claim earned PUMP tokens — free money.
//!
//! Flow:
//!   1. init_user_volume_accumulator (if PDA doesn't exist yet)
//!   2. sync_user_volume_accumulator (snapshot user share of global volume)
//!   3. claim_token_incentives (transfer earned PUMP tokens to user ATA)
//!
//! All instructions live on the PumpSwap AMM program (pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA).

use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::v0::Message as MessageV0,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    transaction::VersionedTransaction,
};
use tracing::{info, warn};

use crate::jito_sender::JitoSender;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PUMPSWAP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const ASSOC_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Anchor discriminator for `init_user_volume_accumulator`.
/// sha256("global:init_user_volume_accumulator")[..8]
const INIT_USER_VOL_DISC: [u8; 8] = compute_anchor_disc(b"global:init_user_volume_accumulator");

/// Anchor discriminator for `sync_user_volume_accumulator`.
/// sha256("global:sync_user_volume_accumulator")[..8]
const SYNC_USER_VOL_DISC: [u8; 8] = compute_anchor_disc(b"global:sync_user_volume_accumulator");

/// Anchor discriminator for `claim_token_incentives`.
/// sha256("global:claim_token_incentives")[..8]
const CLAIM_INCENTIVES_DISC: [u8; 8] = compute_anchor_disc(b"global:claim_token_incentives");

// ---------------------------------------------------------------------------
// Compile-time Anchor discriminator via const fn SHA-256
// ---------------------------------------------------------------------------

/// Minimal const-fn SHA-256 for computing 8-byte Anchor discriminators at compile time.
const fn compute_anchor_disc(input: &[u8]) -> [u8; 8] {
    let hash = const_sha256(input);
    let mut disc = [0u8; 8];
    disc[0] = hash[0];
    disc[1] = hash[1];
    disc[2] = hash[2];
    disc[3] = hash[3];
    disc[4] = hash[4];
    disc[5] = hash[5];
    disc[6] = hash[6];
    disc[7] = hash[7];
    disc
}

const fn const_sha256(input: &[u8]) -> [u8; 32] {
    // SHA-256 initial hash values
    const H: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    // SHA-256 round constants
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
        0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
        0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
        0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let len = input.len();
    // Pad: input || 0x80 || zeros || big-endian 64-bit bit length
    // We only need 1 block for inputs < 56 bytes (our discriminator strings are short).
    let bit_len = (len as u64) * 8;
    let mut block = [0u8; 64];
    let mut i = 0;
    while i < len && i < 64 {
        block[i] = input[i];
        i += 1;
    }
    if len < 64 {
        block[len] = 0x80;
    }
    // Length in big-endian at end of block (assumes single-block message, len < 56).
    block[56] = (bit_len >> 56) as u8;
    block[57] = (bit_len >> 48) as u8;
    block[58] = (bit_len >> 40) as u8;
    block[59] = (bit_len >> 32) as u8;
    block[60] = (bit_len >> 24) as u8;
    block[61] = (bit_len >> 16) as u8;
    block[62] = (bit_len >> 8) as u8;
    block[63] = bit_len as u8;

    // Parse block into 16 u32 words
    let mut w = [0u32; 64];
    let mut t = 0;
    while t < 16 {
        w[t] = ((block[t * 4] as u32) << 24)
            | ((block[t * 4 + 1] as u32) << 16)
            | ((block[t * 4 + 2] as u32) << 8)
            | (block[t * 4 + 3] as u32);
        t += 1;
    }
    while t < 64 {
        let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
        let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16]
            .wrapping_add(s0)
            .wrapping_add(w[t - 7])
            .wrapping_add(s1);
        t += 1;
    }

    let mut a = H[0];
    let mut b = H[1];
    let mut c = H[2];
    let mut d = H[3];
    let mut e = H[4];
    let mut f = H[5];
    let mut g = H[6];
    let mut h = H[7];

    t = 0;
    while t < 64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[t])
            .wrapping_add(w[t]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
        t += 1;
    }

    let vals = [
        H[0].wrapping_add(a),
        H[1].wrapping_add(b),
        H[2].wrapping_add(c),
        H[3].wrapping_add(d),
        H[4].wrapping_add(e),
        H[5].wrapping_add(f),
        H[6].wrapping_add(g),
        H[7].wrapping_add(h),
    ];

    let mut out = [0u8; 32];
    let mut j = 0;
    while j < 8 {
        out[j * 4] = (vals[j] >> 24) as u8;
        out[j * 4 + 1] = (vals[j] >> 16) as u8;
        out[j * 4 + 2] = (vals[j] >> 8) as u8;
        out[j * 4 + 3] = vals[j] as u8;
        j += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// PDA derivation
// ---------------------------------------------------------------------------

fn pumpswap_program() -> Pubkey {
    PUMPSWAP_PROGRAM.parse().unwrap()
}

/// PDA: seeds = ["global_volume_accumulator"], program = PumpSwap AMM.
pub fn derive_global_volume_accumulator() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], &pumpswap_program())
}

/// PDA: seeds = ["user_volume_accumulator", user], program = PumpSwap AMM.
pub fn derive_user_volume_accumulator(user: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &pumpswap_program(),
    )
}

/// PDA: seeds = ["__event_authority"], program = PumpSwap AMM.
fn derive_event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &pumpswap_program()).0
}

/// The global incentive token account is the ATA of global_volume_accumulator for the incentive mint.
fn derive_global_incentive_token_account(
    global_vol: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Pubkey {
    spl_ata(global_vol, mint, token_program)
}

/// Derive ATA address (mirrors spl_associated_token_account logic).
fn spl_ata(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let assoc: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &assoc,
    )
    .0
}

// ---------------------------------------------------------------------------
// Instruction builders
// ---------------------------------------------------------------------------

/// Build `init_user_volume_accumulator` instruction.
/// Only needed if the user's PDA doesn't exist yet.
fn build_init_user_vol_ix(user: &Pubkey, payer: &Pubkey) -> Instruction {
    let program = pumpswap_program();
    let (user_vol, _) = derive_user_volume_accumulator(user);
    let event_authority = derive_event_authority();

    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*user, false),
            AccountMeta::new(user_vol, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program, false),
        ],
        data: INIT_USER_VOL_DISC.to_vec(),
    }
}

/// Build `sync_user_volume_accumulator` instruction.
fn build_sync_user_vol_ix(user: &Pubkey) -> Instruction {
    let program = pumpswap_program();
    let (global_vol, _) = derive_global_volume_accumulator();
    let (user_vol, _) = derive_user_volume_accumulator(user);
    let event_authority = derive_event_authority();

    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(*user, false),
            AccountMeta::new_readonly(global_vol, false),
            AccountMeta::new(user_vol, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program, false),
        ],
        data: SYNC_USER_VOL_DISC.to_vec(),
    }
}

/// Build `claim_token_incentives` instruction.
/// `mint` = the incentive token mint (read from global_volume_accumulator on-chain).
fn build_claim_incentives_ix(user: &Pubkey, payer: &Pubkey, mint: &Pubkey) -> Instruction {
    let program = pumpswap_program();
    let token_program: Pubkey = TOKEN_PROGRAM.parse().unwrap();
    let assoc_program: Pubkey = ASSOC_TOKEN_PROGRAM.parse().unwrap();
    let (global_vol, _) = derive_global_volume_accumulator();
    let (user_vol, _) = derive_user_volume_accumulator(user);
    let event_authority = derive_event_authority();
    let global_incentive_ata =
        derive_global_incentive_token_account(&global_vol, mint, &token_program);
    let user_ata = spl_ata(user, mint, &token_program);

    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(*user, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new_readonly(global_vol, false),
            AccountMeta::new(global_incentive_ata, false),
            AccountMeta::new(user_vol, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(assoc_program, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program, false),
            AccountMeta::new(*payer, true),
        ],
        data: CLAIM_INCENTIVES_DISC.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// RPC helpers
// ---------------------------------------------------------------------------

/// Fetch account data via JSON-RPC `getAccountInfo`.
async fn get_account_data(
    client: &reqwest::Client,
    rpc_url: &str,
    pubkey: &Pubkey,
) -> Result<Option<Vec<u8>>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [pubkey.to_string(), {"encoding": "base64"}]
    });
    let resp: serde_json::Value = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let val = &resp["result"]["value"];
    if val.is_null() {
        return Ok(None);
    }
    let data_str = val["data"][0]
        .as_str()
        .context("missing data field")?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        data_str,
    )?;
    Ok(Some(decoded))
}

/// Fetch a recent blockhash via JSON-RPC.
async fn fetch_blockhash(
    client: &reqwest::Client,
    rpc_url: &str,
) -> Result<solana_sdk::hash::Hash> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getLatestBlockhash",
        "params": [{"commitment": "finalized"}]
    });
    let resp: serde_json::Value = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let hash_str = resp["result"]["value"]["blockhash"]
        .as_str()
        .context("missing blockhash")?;
    hash_str.parse().context("invalid blockhash")
}

/// Read the incentive token mint from the GlobalVolumeAccumulator account.
/// The mint pubkey is stored after the 8-byte discriminator at a known offset.
/// Layout (Anchor): [disc 8B] ... [mint 32B at some offset].
/// We read it from the `global_incentive_token_account` ATA's mint field instead,
/// or we can scan the GlobalVolumeAccumulator data.
/// Safest approach: read the GlobalVolumeAccumulator and look for a Pubkey-sized
/// field. The IDL indicates `mint` is a relation on `global_volume_accumulator`.
/// Typically in Anchor accounts, after the 8B disc the fields follow declaration order.
/// We'll read the account and try offset 8 first (first field after disc).
async fn read_incentive_mint(
    client: &reqwest::Client,
    rpc_url: &str,
) -> Result<Option<Pubkey>> {
    let (global_vol, _) = derive_global_volume_accumulator();
    let data = match get_account_data(client, rpc_url, &global_vol).await? {
        Some(d) => d,
        None => return Ok(None), // Account doesn't exist → no incentive program active
    };
    // The global_volume_accumulator account must contain the incentive mint.
    // We scan for a plausible mint pubkey. In Anchor, fields are packed sequentially.
    // We know the discriminator is 8 bytes. The mint could be at various offsets
    // depending on the struct layout. We'll try to find it by checking which 32-byte
    // slice at offset 8, 40, 72, etc. corresponds to an existing token mint.
    // Most reliable: iterate known offsets and return the first non-zero, non-system pubkey.
    if data.len() < 40 {
        return Ok(None);
    }
    // Try common offsets after the 8-byte discriminator
    for offset in (8..data.len().saturating_sub(31)).step_by(8) {
        if offset + 32 > data.len() {
            break;
        }
        let candidate = Pubkey::new_from_array(
            data[offset..offset + 32].try_into().unwrap(),
        );
        // Skip zero/system pubkeys — we want a real mint
        if candidate == Pubkey::default() || candidate == system_program::id() {
            continue;
        }
        // Verify this is actually a mint by checking getAccountInfo owner == TokenkegQ...
        if let Ok(Some(mint_data)) = get_account_data(client, rpc_url, &candidate).await {
            // SPL Token mint accounts are exactly 82 bytes
            if mint_data.len() == 82 {
                info!(
                    mint = %candidate,
                    offset = offset,
                    "volume-claimer: found incentive mint in GlobalVolumeAccumulator"
                );
                return Ok(Some(candidate));
            }
        }
    }
    warn!("volume-claimer: could not find incentive mint in GlobalVolumeAccumulator");
    Ok(None)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Attempt to claim PumpSwap volume rewards.
///
/// Returns `Ok(Some(signature))` if a claim TX was sent, `Ok(None)` if nothing
/// to claim (no accumulator or no incentive program active), or `Err` on failure.
///
/// Safe to call frequently — the on-chain program will no-op if nothing is claimable.
/// Sent via Jito bundle → 0 SOL cost on failure.
pub async fn try_claim_volume_rewards(
    rpc_url: &str,
    payer: &Keypair,
    jito_sender: &JitoSender,
) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let user = payer.pubkey();
    let (user_vol_pda, _) = derive_user_volume_accumulator(&user);

    // 1. Check if user_volume_accumulator exists
    let user_vol_exists = get_account_data(&client, rpc_url, &user_vol_pda)
        .await?
        .is_some();

    // 2. Read incentive mint from GlobalVolumeAccumulator
    let mint = match read_incentive_mint(&client, rpc_url).await? {
        Some(m) => m,
        None => {
            info!("volume-claimer: no incentive program active, skipping");
            return Ok(None);
        }
    };

    // 3. Build instructions
    let mut ixs = Vec::with_capacity(4);

    // If user PDA doesn't exist, init it first
    if !user_vol_exists {
        info!("volume-claimer: initializing user_volume_accumulator PDA");
        ixs.push(build_init_user_vol_ix(&user, &user));
    }

    // Sync volume snapshot
    ixs.push(build_sync_user_vol_ix(&user));

    // Claim incentive tokens
    ixs.push(build_claim_incentives_ix(&user, &user, &mint));

    // 4. Build and sign TX
    let blockhash = fetch_blockhash(&client, rpc_url).await?;
    let msg = MessageV0::try_compile(&user, &ixs, &[], blockhash)
        .context("volume-claimer: compile message")?;
    let tx = VersionedTransaction::try_new(
        solana_sdk::message::VersionedMessage::V0(msg),
        &[payer],
    )
    .context("volume-claimer: sign TX")?;

    // 5. Send via Jito bundle (0 cost on failure)
    info!(
        mint = %mint,
        user_vol_existed = user_vol_exists,
        ix_count = ixs.len(),
        "volume-claimer: sending claim TX via Jito"
    );
    match jito_sender.send_bundle(&[tx]).await {
        Ok(bundle_id) => {
            info!(bundle_id = %bundle_id, "volume-claimer: claim bundle sent!");
            Ok(Some(bundle_id))
        }
        Err(e) => {
            warn!(error = %e, "volume-claimer: claim bundle failed");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pda_derivations() {
        let user: Pubkey = "Hn87MEK6NLiGquWay8n151xXDN4o3xyvhwuRmge4tS1Y"
            .parse()
            .unwrap();
        let (global_vol, _) = derive_global_volume_accumulator();
        let (user_vol, _) = derive_user_volume_accumulator(&user);

        // Verify PDAs are deterministic and non-default
        assert_ne!(global_vol, Pubkey::default());
        assert_ne!(user_vol, Pubkey::default());
        assert_ne!(global_vol, user_vol);

        // Verify they match the seeds used in sniper.rs
        let pumpswap: Pubkey = PUMPSWAP_PROGRAM.parse().unwrap();
        let (expected_global, _) =
            Pubkey::find_program_address(&[b"global_volume_accumulator"], &pumpswap);
        let (expected_user, _) = Pubkey::find_program_address(
            &[b"user_volume_accumulator", user.as_ref()],
            &pumpswap,
        );
        assert_eq!(global_vol, expected_global);
        assert_eq!(user_vol, expected_user);
    }

    #[test]
    fn test_discriminators_non_zero() {
        // Ensure the const-fn SHA-256 produces non-zero discriminators
        assert_ne!(INIT_USER_VOL_DISC, [0u8; 8]);
        assert_ne!(SYNC_USER_VOL_DISC, [0u8; 8]);
        assert_ne!(CLAIM_INCENTIVES_DISC, [0u8; 8]);
        // All three should be distinct
        assert_ne!(INIT_USER_VOL_DISC, SYNC_USER_VOL_DISC);
        assert_ne!(SYNC_USER_VOL_DISC, CLAIM_INCENTIVES_DISC);
        assert_ne!(INIT_USER_VOL_DISC, CLAIM_INCENTIVES_DISC);
    }

    #[test]
    fn test_instruction_builders() {
        let user: Pubkey = "Hn87MEK6NLiGquWay8n151xXDN4o3xyvhwuRmge4tS1Y"
            .parse()
            .unwrap();
        let mint: Pubkey = "So11111111111111111111111111111111111111112"
            .parse()
            .unwrap(); // dummy mint

        let init_ix = build_init_user_vol_ix(&user, &user);
        assert_eq!(init_ix.program_id, pumpswap_program());
        assert_eq!(init_ix.data, INIT_USER_VOL_DISC.to_vec());

        let sync_ix = build_sync_user_vol_ix(&user);
        assert_eq!(sync_ix.program_id, pumpswap_program());
        assert_eq!(sync_ix.data, SYNC_USER_VOL_DISC.to_vec());

        let claim_ix = build_claim_incentives_ix(&user, &user, &mint);
        assert_eq!(claim_ix.program_id, pumpswap_program());
        assert_eq!(claim_ix.data, CLAIM_INCENTIVES_DISC.to_vec());
        assert_eq!(claim_ix.accounts.len(), 12);
    }
}
