//! swap_decoder.rs – Decode DEX swap instructions from VersionedTransactions.
//!
//! Ported from solana-bot/src/shred/tx_decoder.rs with real discriminators.
//! Hot-path: called per-transaction in the signal processor.

use solana_sdk::{message::VersionedMessage, pubkey, pubkey::Pubkey, transaction::VersionedTransaction};

// ---------------------------------------------------------------------------
// Program IDs (mainnet) — both &str (for display) and Pubkey (for zero-alloc comparison)
// ---------------------------------------------------------------------------

pub const RAYDIUM_AMM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const RAYDIUM_CLMM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
pub const ORCA_WHIRLPOOL: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
pub const METEORA_DLMM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
/// Meteora Dynamic AMM (non-DLMM, constant-product pools).
pub const METEORA_DYNAMIC_AMM: &str = "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB";
/// PumpSwap AMM (the post-graduation pool, NOT the bonding curve).
pub const PUMPSWAP_AMM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// Pump.fun bonding curve (pre-graduation).
pub const PUMPFUN_BONDING: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const JUPITER_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
/// Raydium LaunchLab (BONK.fun / LetsBONK) — launchpad with migration to Raydium AMM/CPSwap.
pub const RAYDIUM_LAUNCHLAB: &str = "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj";
/// Moonshot launchpad — memecoin launcher with migration to DEX.
pub const MOONSHOT: &str = "MoonCVVNZFSYkqNXP6bxHLPL6QQJiMagDL3qcqUQTrG";

// Compile-time Pubkey constants for zero-alloc matching in hot path.
const PK_RAYDIUM_V4: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
const PK_RAYDIUM_CLMM: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
const PK_ORCA: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
const PK_METEORA: Pubkey = pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const PK_PUMPSWAP: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
const PK_PUMPFUN: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const PK_LAUNCHLAB: Pubkey = pubkey!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
const PK_MOONSHOT: Pubkey = pubkey!("MoonCVVNZFSYkqNXP6bxHLPL6QQJiMagDL3qcqUQTrG");
const PK_FLUXBEAM: Pubkey = pubkey!("FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSR1X4");
const PK_SABER: Pubkey = pubkey!("SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ");
const PK_RAYDIUM_CPMM: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
const PK_JUPITER: Pubkey = pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
const PK_METEORA_DYN: Pubkey = pubkey!("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB");

// ---------------------------------------------------------------------------
// Instruction discriminators
// ---------------------------------------------------------------------------

/// Raydium AMM V4 SwapBaseIn discriminator (single byte 9).
const RAYDIUM_SWAP_DISC: u8 = 9;

/// Orca Whirlpool `swap` discriminator.
const ORCA_SWAP_DISC: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// Meteora DLMM swap discriminator (Anchor sha256 prefix).
const METEORA_SWAP_DISC: [u8; 8] = [248, 198, 244, 145, 90, 24, 30, 227];

/// PumpSwap buy discriminator.
const PUMPSWAP_BUY_DISC: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// PumpSwap sell discriminator.
const PUMPSWAP_SELL_DISC: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
/// PumpSwap create_pool discriminator (graduation event).
const PUMPSWAP_CREATE_POOL_DISC: [u8; 8] = [233, 146, 209, 142, 207, 104, 64, 188];

/// PumpFun migration instruction (bonding curve → PumpSwap).
const PUMPFUN_MIGRATE_DISC: [u8; 8] = [155, 234, 231, 146, 236, 158, 162, 30];

/// Raydium LaunchLab: migrate_to_amm (sha256("global:migrate_to_amm")[..8]).
const LAUNCHLAB_MIGRATE_AMM_DISC: [u8; 8] = [207, 82, 192, 145, 254, 207, 145, 223];
/// Raydium LaunchLab: migrate_to_cpswap (sha256("global:migrate_to_cpswap")[..8]).
const LAUNCHLAB_MIGRATE_CPSWAP_DISC: [u8; 8] = [136, 92, 200, 103, 28, 218, 144, 140];

/// Moonshot: migrateFunds — camelCase variant (sha256("global:migrateFunds")[..8]).
const MOONSHOT_MIGRATE_CAMEL_DISC: [u8; 8] = [252, 23, 149, 132, 146, 87, 69, 198];
/// Moonshot: migrate_funds — snake_case variant (sha256("global:migrate_funds")[..8]).
const MOONSHOT_MIGRATE_SNAKE_DISC: [u8; 8] = [42, 229, 10, 231, 189, 62, 193, 174];

/// FluxBeam swap: standard AMM swap instruction (discriminator byte 1).
const FLUXBEAM_SWAP_DISC: u8 = 1;

/// Saber swap: standard stable-swap instruction (discriminator byte 1).
const SABER_SWAP_DISC: u8 = 1;

/// Raydium CPMM swap_base_input discriminator.
const RAYDIUM_CPMM_SWAP_BASE_INPUT_DISC: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];
/// Raydium CPMM swap_base_output discriminator.
const RAYDIUM_CPMM_SWAP_BASE_OUTPUT_DISC: [u8; 8] = [55, 217, 98, 86, 163, 74, 180, 173];

/// PumpFun bonding curve buy discriminator (sha256("global:buy")[..8]).
const PUMPFUN_BUY_DISC: [u8; 8] = [102, 6, 61, 18, 5, 205, 228, 178];
/// PumpFun bonding curve sell discriminator (sha256("global:sell")[..8]).
const PUMPFUN_SELL_DISC: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
/// PumpFun bonding curve create discriminator (sha256("global:create")[..8]).
const PUMPFUN_CREATE_DISC: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];

/// Raydium CLMM swap discriminator (sha256("global:swap")[..8]).
const RAYDIUM_CLMM_SWAP_DISC: [u8; 8] = [43, 4, 237, 11, 26, 201, 106, 217];
/// Raydium CLMM swap_v2 discriminator (sha256("global:swap_v2")[..8]).
const RAYDIUM_CLMM_SWAP_V2_DISC: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];

/// Jupiter V6 sharedAccountsRoute discriminator.
const JUPITER_ROUTE_DISC: [u8; 8] = [229, 23, 203, 151, 122, 227, 173, 42];
/// Jupiter V6 route discriminator (sha256("global:route")[..8]).
const JUPITER_ROUTE2_DISC: [u8; 8] = [193, 32, 155, 51, 65, 214, 156, 129];

/// Meteora Dynamic AMM swap discriminator (same Anchor prefix as DLMM swap).
const METEORA_DYN_SWAP_DISC: [u8; 8] = [248, 198, 244, 145, 90, 24, 30, 227];

// ---------------------------------------------------------------------------
// Pool creation discriminators (for new_pool_monitor)
// ---------------------------------------------------------------------------

/// Raydium AMM V4 Initialize2: single-byte discriminator 1.
const RAYDIUM_V4_INITIALIZE2_DISC: u8 = 1;

/// Orca Whirlpool initializePool discriminator (sha256("global:initialize_pool")[..8]).
const ORCA_INITIALIZE_POOL_DISC: [u8; 8] = [95, 180, 10, 172, 84, 174, 232, 40];

/// Meteora DLMM initializeLbPair discriminator (sha256("global:initialize_lb_pair")[..8]).
const METEORA_INITIALIZE_LB_PAIR_DISC: [u8; 8] = [45, 154, 220, 170, 219, 89, 235, 234];

/// Raydium CPMM initialize discriminator (sha256("global:initialize")[..8]).
const RAYDIUM_CPMM_INITIALIZE_DISC: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];

// ---------------------------------------------------------------------------
// Detected swap
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapDex {
    RaydiumAmmV4,
    RaydiumClmm,
    OrcaWhirlpool,
    MeteoraDlmm,
    MeteoraDynamicAmm,
    PumpSwap,
    PumpFun,
    Jupiter,
    Fluxbeam,
    Saber,
    RaydiumCpmm,
}

#[derive(Debug, Clone)]
pub struct DetectedSwap {
    pub dex: SwapDex,
    /// Pool / pair account.
    pub pool: Pubkey,
    /// Estimated input amount (0 if unparseable).
    pub amount_in: u64,
    /// Minimum expected output (0 if unparseable).
    pub amount_out_min: u64,
    /// A→B direction (true) or B→A (false).
    pub a_to_b: bool,
}

/// Enriched swap info for wallet tracking. Combines DetectedSwap with signer
/// and token/program context extracted from the transaction.
#[derive(Debug, Clone)]
pub struct SwapInfo {
    pub signer: Pubkey,
    pub program_id: Pubkey,
    pub pool: Pubkey,
    pub token_in: Pubkey,
    pub token_out: Pubkey,
    pub amount_in: u64,
    pub amount_out: u64,
    pub a_to_b: bool,
}

/// Build SwapInfo list from a transaction's DetectedSwaps + pool state for token resolution.
/// `resolve_tokens` is called per pool to get (token_a, token_b).
pub fn enrich_swaps(
    tx: &VersionedTransaction,
    swaps: &[DetectedSwap],
    resolve_tokens: impl Fn(&Pubkey) -> Option<(Pubkey, Pubkey)>,
    quote_fn: impl Fn(&Pubkey, u64, bool) -> u64,
) -> Vec<SwapInfo> {
    let signer = tx_signer(tx);
    swaps
        .iter()
        .filter_map(|ds| {
            let (token_a, token_b) = resolve_tokens(&ds.pool)?;
            let (token_in, token_out) = if ds.a_to_b {
                (token_a, token_b)
            } else {
                (token_b, token_a)
            };
            let amount_out = quote_fn(&ds.pool, ds.amount_in, ds.a_to_b);
            let program_id = dex_to_program_id(&ds.dex);
            Some(SwapInfo {
                signer,
                program_id,
                pool: ds.pool,
                token_in,
                token_out,
                amount_in: ds.amount_in,
                amount_out,
                a_to_b: ds.a_to_b,
            })
        })
        .collect()
}

fn tx_signer(tx: &VersionedTransaction) -> Pubkey {
    match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.first().copied().unwrap_or_default(),
        VersionedMessage::V0(m) => m.account_keys.first().copied().unwrap_or_default(),
    }
}

fn dex_to_program_id(dex: &SwapDex) -> Pubkey {
    match dex {
        SwapDex::RaydiumAmmV4 => PK_RAYDIUM_V4,
        SwapDex::RaydiumClmm => PK_RAYDIUM_CLMM,
        SwapDex::OrcaWhirlpool => PK_ORCA,
        SwapDex::MeteoraDlmm => PK_METEORA,
        SwapDex::MeteoraDynamicAmm => PK_METEORA_DYN,
        SwapDex::PumpSwap => PK_PUMPSWAP,
        SwapDex::PumpFun => PK_PUMPFUN,
        SwapDex::Jupiter => PK_JUPITER,
        SwapDex::Fluxbeam => PK_FLUXBEAM,
        SwapDex::Saber => PK_SABER,
        SwapDex::RaydiumCpmm => PK_RAYDIUM_CPMM,
    }
}

#[derive(Debug, Clone)]
pub struct GraduationEvent {
    pub token_mint: Pubkey,
    pub pump_pool: Pubkey,
    /// Creator pubkey from create_pool (needed for PumpSwap creator fee accounts).
    pub creator: Option<Pubkey>,
    /// The pool's base_mint as defined in create_pool (may be WSOL or the token).
    pub pool_base_mint: Option<Pubkey>,
    /// The pool's quote_mint as defined in create_pool (may be WSOL or the token).
    pub pool_quote_mint: Option<Pubkey>,
}

// ---------------------------------------------------------------------------
// Decode swaps from a transaction
// ---------------------------------------------------------------------------

/// Extract all recognisable swap events from a transaction.
pub fn decode_swaps(tx: &VersionedTransaction) -> Vec<DetectedSwap> {
    let accounts = static_accounts(tx);
    let instructions = instructions_with_data(tx);
    let mut out = Vec::new();

    for (prog_idx, data, acc_indices) in &instructions {
        let Some(&prog) = accounts.get(*prog_idx) else {
            continue;
        };

        // Zero-alloc Pubkey comparison (no .to_string() heap allocation).
        let swap = if prog == PK_RAYDIUM_V4 {
            decode_raydium_v4(data, acc_indices, &accounts)
        } else if prog == PK_RAYDIUM_CLMM {
            decode_raydium_clmm(data, acc_indices, &accounts)
        } else if prog == PK_ORCA {
            decode_orca(data, acc_indices, &accounts)
        } else if prog == PK_METEORA {
            decode_meteora(data, acc_indices, &accounts)
        } else if prog == PK_PUMPSWAP {
            decode_pumpswap(data, acc_indices, &accounts)
        } else if prog == PK_FLUXBEAM {
            decode_fluxbeam(data, acc_indices, &accounts)
        } else if prog == PK_SABER {
            decode_saber(data, acc_indices, &accounts)
        } else if prog == PK_RAYDIUM_CPMM {
            decode_raydium_cpmm(data, acc_indices, &accounts)
        } else if prog == PK_PUMPFUN {
            decode_pumpfun(data, acc_indices, &accounts)
        } else if prog == PK_JUPITER {
            decode_jupiter(data, acc_indices, &accounts)
        } else if prog == PK_METEORA_DYN {
            decode_meteora_dynamic(data, acc_indices, &accounts)
        } else {
            None
        };

        if let Some(s) = swap {
            out.push(s);
        }
    }
    out
}

/// Detect a PumpSwap graduation event (create_pool or migrate instruction).
pub fn detect_graduation(tx: &VersionedTransaction) -> Option<GraduationEvent> {
    let accounts = static_accounts(tx);
    let instructions = instructions_with_data(tx);

    for (prog_idx, data, acc_indices) in &instructions {
        let Some(&prog) = accounts.get(*prog_idx) else {
            continue;
        };
        // PumpSwap create_pool: graduation to AMM pool.
        // Account layout: [0]=pool, [1]=global_config, [2]=creator, [3]=base_mint, [4]=quote_mint, ...
        // base/quote ordering is NOT fixed — some pools have SOL as base. Pick the non-WSOL mint.
        if prog == PK_PUMPSWAP && data.len() >= 8 && data[..8] == PUMPSWAP_CREATE_POOL_DISC {
            let pump_pool = accounts.get(*acc_indices.first()? as usize).copied()?;
            let creator = accounts.get(*acc_indices.get(2)? as usize).copied();
            let mint_a = accounts.get(*acc_indices.get(3)? as usize).copied()?;
            let mint_b = accounts.get(*acc_indices.get(4)? as usize).copied()?;
            let wsol: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
            let token_mint = if mint_a == wsol { mint_b } else { mint_a };
            // Skip if both are WSOL or neither is WSOL (malformed)
            if token_mint == wsol { continue; }
            return Some(GraduationEvent {
                token_mint,
                pump_pool,
                creator,
                pool_base_mint: Some(mint_a),
                pool_quote_mint: Some(mint_b),
            });
        }

        // PumpFun migration instruction.
        if prog == PK_PUMPFUN && data.len() >= 8 && data[..8] == PUMPFUN_MIGRATE_DISC {
            let token_mint = accounts.get(*acc_indices.first()? as usize).copied()?;
            let pump_pool = accounts.get(*acc_indices.get(2)? as usize).copied()?;
            return Some(GraduationEvent {
                token_mint,
                pump_pool,
                creator: None,
                pool_base_mint: None,
                pool_quote_mint: None,
            });
        }

        // Raydium LaunchLab (BONK.fun) migration → Raydium AMM or CPSwap.
        if prog == PK_LAUNCHLAB && data.len() >= 8 {
            let disc = &data[..8];
            if disc == LAUNCHLAB_MIGRATE_AMM_DISC || disc == LAUNCHLAB_MIGRATE_CPSWAP_DISC {
                // LaunchLab migrate accounts layout (Anchor):
                // [0] = authority, [1] = pool_state / amm, [2] = token_mint, ...
                // The newly created AMM pool is accounts[1], token mint is accounts[2].
                let pump_pool = accounts.get(*acc_indices.get(1)? as usize).copied()?;
                let token_mint = accounts.get(*acc_indices.get(2)? as usize).copied()?;
                return Some(GraduationEvent {
                    token_mint,
                    pump_pool,
                    creator: None,
                    pool_base_mint: None,
                    pool_quote_mint: None,
                });
            }
        }

        // Moonshot migration → DEX pool.
        if prog == PK_MOONSHOT && data.len() >= 8
            && (data[..8] == MOONSHOT_MIGRATE_CAMEL_DISC || data[..8] == MOONSHOT_MIGRATE_SNAKE_DISC)
        {
            // Moonshot migrateFunds accounts layout:
            // [0] = sender/authority, [1] = token_mint, [2] = curve_account, ...
            // The destination pool address depends on which DEX Moonshot migrates to;
            // we use curve_account as a proxy (the pool gets created in CPI).
            let token_mint = accounts.get(*acc_indices.get(1)? as usize).copied()?;
            let pump_pool = accounts.get(*acc_indices.get(2)? as usize).copied()?;
            return Some(GraduationEvent {
                token_mint,
                pump_pool,
                creator: None,
                pool_base_mint: None,
                pool_quote_mint: None,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Per-DEX decoders
// ---------------------------------------------------------------------------

fn decode_raydium_v4(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // SwapBaseIn: [9, amount_in(8), min_out(8)]  accounts[1]=amm
    if data.len() < 17 || data[0] != RAYDIUM_SWAP_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[1..9].try_into().ok()?);
    let amount_out_min = u64::from_le_bytes(data[9..17].try_into().ok()?);
    let pool = accounts.get(*accs.get(1)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::RaydiumAmmV4,
        pool,
        amount_in,
        amount_out_min,
        a_to_b: true,
    })
}

fn decode_raydium_clmm(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // Anchor swap/swap_v2: [disc(8), amount(8), other_amount_threshold(8), sqrt_price_limit(16), is_base_input(1), a_to_b(... varies)]
    // accounts[2]=pool_state
    if data.len() < 16 {
        return None;
    }
    let disc = &data[..8];
    if disc != RAYDIUM_CLMM_SWAP_DISC && disc != RAYDIUM_CLMM_SWAP_V2_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let amount_out_min = if data.len() >= 24 {
        u64::from_le_bytes(data[16..24].try_into().ok()?)
    } else {
        0
    };
    let pool = accounts.get(*accs.get(2)? as usize).copied()?;
    // sqrt_price_limit is at bytes 24..40 (u128), then is_base_input at 40, but
    // a simpler heuristic: if sqrt_price_limit == MIN (a_to_b) vs MAX (b_to_a)
    // For now use byte 40 (is_base_input) presence as indicator.
    let a_to_b = data.get(24).copied().unwrap_or(1) != 0;
    Some(DetectedSwap {
        dex: SwapDex::RaydiumClmm,
        pool,
        amount_in,
        amount_out_min,
        a_to_b,
    })
}

fn decode_orca(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // [disc(8), amount(8), other_threshold(8), sqrt_price_limit(16), exact_input(1), a_to_b(1)]
    // Accounts: [token_program, token_authority, whirlpool, ...]
    if data.len() < 42 || data[..8] != ORCA_SWAP_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let amount_out_min = u64::from_le_bytes(data[16..24].try_into().ok()?);
    let pool = accounts.get(*accs.get(2)? as usize).copied()?;
    let a_to_b = data.get(41).copied().unwrap_or(1) != 0;
    Some(DetectedSwap {
        dex: SwapDex::OrcaWhirlpool,
        pool,
        amount_in,
        amount_out_min,
        a_to_b,
    })
}

fn decode_meteora(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // [disc(8), amount_in(8), min_amount_out(8)]  accounts[0]=lb_pair
    if data.len() < 24 || data[..8] != METEORA_SWAP_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let amount_out_min = u64::from_le_bytes(data[16..24].try_into().ok()?);
    let pool = accounts.get(*accs.get(0)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::MeteoraDlmm,
        pool,
        amount_in,
        amount_out_min,
        a_to_b: true,
    })
}

fn decode_pumpswap(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    if data.len() < 16 {
        return None;
    }
    let disc = &data[..8];
    let (amount_in, amount_out_min, a_to_b) = if disc == PUMPSWAP_BUY_DISC {
        let amt = u64::from_le_bytes(data[8..16].try_into().ok()?);
        let min_out = if data.len() >= 24 {
            u64::from_le_bytes(data[16..24].try_into().ok()?)
        } else { 0 };
        (amt, min_out, true)
    } else if disc == PUMPSWAP_SELL_DISC {
        let amt = u64::from_le_bytes(data[8..16].try_into().ok()?);
        let min_out = if data.len() >= 24 {
            u64::from_le_bytes(data[16..24].try_into().ok()?)
        } else { 0 };
        (amt, min_out, false)
    } else {
        return None;
    };
    // accounts[0]=pool
    let pool = accounts.get(*accs.get(0)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::PumpSwap,
        pool,
        amount_in,
        amount_out_min,
        a_to_b,
    })
}

fn decode_fluxbeam(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // Standard AMM swap: [1, amount_in(8), min_out(8)]  accounts[1]=amm/pool
    if data.len() < 17 || data[0] != FLUXBEAM_SWAP_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[1..9].try_into().ok()?);
    let amount_out_min = u64::from_le_bytes(data[9..17].try_into().ok()?);
    let pool = accounts.get(*accs.get(1)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::Fluxbeam,
        pool,
        amount_in,
        amount_out_min,
        a_to_b: true,
    })
}

fn decode_saber(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // Standard stable-swap: [1, amount_in(8), min_out(8)]  accounts[1]=swap_pool
    if data.len() < 17 || data[0] != SABER_SWAP_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[1..9].try_into().ok()?);
    let amount_out_min = u64::from_le_bytes(data[9..17].try_into().ok()?);
    let pool = accounts.get(*accs.get(1)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::Saber,
        pool,
        amount_in,
        amount_out_min,
        a_to_b: true,
    })
}

fn decode_raydium_cpmm(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // Anchor: [disc(8), amount_in(8), min_amount_out(8)]  accounts[2]=pool_state
    if data.len() < 24 {
        return None;
    }
    let disc = &data[..8];
    if disc != RAYDIUM_CPMM_SWAP_BASE_INPUT_DISC && disc != RAYDIUM_CPMM_SWAP_BASE_OUTPUT_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let amount_out_min = u64::from_le_bytes(data[16..24].try_into().ok()?);
    let pool = accounts.get(*accs.get(2)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::RaydiumCpmm,
        pool,
        amount_in,
        amount_out_min,
        a_to_b: true,
    })
}

// ---------------------------------------------------------------------------
// New protocol decoders
// ---------------------------------------------------------------------------

fn decode_pumpfun(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // PumpFun bonding curve buy/sell:
    // Buy:  [disc(8), amount(8), max_sol_cost(8)]  accounts[2]=bonding_curve
    // Sell: [disc(8), amount(8), min_sol_output(8)] accounts[2]=bonding_curve
    if data.len() < 16 {
        return None;
    }
    let disc = &data[..8];
    let (amount_in, amount_out_min, a_to_b) = if disc == PUMPFUN_BUY_DISC {
        let amt = u64::from_le_bytes(data[8..16].try_into().ok()?);
        let max_cost = if data.len() >= 24 {
            u64::from_le_bytes(data[16..24].try_into().ok()?)
        } else { 0 };
        (amt, max_cost, true)
    } else if disc == PUMPFUN_SELL_DISC {
        let amt = u64::from_le_bytes(data[8..16].try_into().ok()?);
        let min_out = if data.len() >= 24 {
            u64::from_le_bytes(data[16..24].try_into().ok()?)
        } else { 0 };
        (amt, min_out, false)
    } else {
        return None;
    };
    // accounts[2]=bonding_curve (the pool-equivalent for PumpFun)
    let pool = accounts.get(*accs.get(2)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::PumpFun,
        pool,
        amount_in,
        amount_out_min,
        a_to_b,
    })
}

fn decode_jupiter(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // Jupiter V6 route/sharedAccountsRoute: detect as swap signal.
    // [disc(8), ...routing_data]
    // We only detect presence — exact route parsing is not needed for signal generation.
    if data.len() < 8 {
        return None;
    }
    let disc = &data[..8];
    if disc != JUPITER_ROUTE_DISC && disc != JUPITER_ROUTE2_DISC {
        return None;
    }
    // Jupiter accounts vary by route; accounts[1] is typically the user transfer authority,
    // and the actual pools are in inner CPIs. Use accounts[0] (the payer) as a placeholder
    // pool address — the signal processor will treat Jupiter swaps as market activity signals.
    let pool = accounts.get(*accs.get(0)? as usize).copied()?;
    // Try to extract in_amount from sharedAccountsRoute layout: disc(8) + id(1) + route_plan_len(4)?
    // Too variable — just report 0 amount.
    Some(DetectedSwap {
        dex: SwapDex::Jupiter,
        pool,
        amount_in: 0,
        amount_out_min: 0,
        a_to_b: true,
    })
}

fn decode_meteora_dynamic(data: &[u8], accs: &[u8], accounts: &[Pubkey]) -> Option<DetectedSwap> {
    // Meteora Dynamic AMM swap: [disc(8), amount_in(8), min_amount_out(8)]
    // Account layout: [0]=pool, [1]=..., similar to DLMM but constant-product.
    if data.len() < 24 || data[..8] != METEORA_DYN_SWAP_DISC {
        return None;
    }
    let amount_in = u64::from_le_bytes(data[8..16].try_into().ok()?);
    let amount_out_min = u64::from_le_bytes(data[16..24].try_into().ok()?);
    let pool = accounts.get(*accs.get(0)? as usize).copied()?;
    Some(DetectedSwap {
        dex: SwapDex::MeteoraDynamicAmm,
        pool,
        amount_in,
        amount_out_min,
        a_to_b: true,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn static_accounts(tx: &VersionedTransaction) -> Vec<Pubkey> {
    match &tx.message {
        VersionedMessage::Legacy(m) => m.account_keys.clone(),
        VersionedMessage::V0(m) => m.account_keys.clone(),
    }
}

/// Returns (program_id_index, data, accounts) for each instruction.
fn instructions_with_data(tx: &VersionedTransaction) -> Vec<(usize, Vec<u8>, Vec<u8>)> {
    match &tx.message {
        VersionedMessage::Legacy(m) => m
            .instructions
            .iter()
            .map(|ix| {
                (
                    ix.program_id_index as usize,
                    ix.data.clone(),
                    ix.accounts.clone(),
                )
            })
            .collect(),
        VersionedMessage::V0(m) => m
            .instructions
            .iter()
            .map(|ix| {
                (
                    ix.program_id_index as usize,
                    ix.data.clone(),
                    ix.accounts.clone(),
                )
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Pool creation detection (for new_pool_monitor)
// ---------------------------------------------------------------------------

/// A pool creation event detected from transaction instructions.
#[derive(Debug, Clone)]
pub struct PoolCreationEvent {
    /// The newly created pool address.
    pub pool: Pubkey,
    /// Token A mint (or token mint for the non-WSOL side).
    pub token_a: Pubkey,
    /// Token B mint (typically WSOL or USDC).
    pub token_b: Pubkey,
    /// Which DEX the pool was created on.
    pub dex: PoolCreationDex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolCreationDex {
    RaydiumAmmV4,
    RaydiumCpmm,
    OrcaWhirlpool,
    MeteoraDlmm,
}

/// Detect pool creation events in a transaction.
/// Returns all pool initialization instructions found.
pub fn detect_pool_creations(tx: &VersionedTransaction) -> Vec<PoolCreationEvent> {
    let accounts = static_accounts(tx);
    let instructions = instructions_with_data(tx);
    let mut events = Vec::new();

    for (prog_idx, data, acc_indices) in &instructions {
        let Some(&prog) = accounts.get(*prog_idx) else {
            continue;
        };

        // Raydium AMM V4 Initialize2: data[0]=1, accounts layout:
        // [0]=nonce?, [1]=..., [4]=amm (pool), [8]=coin_mint, [9]=pc_mint
        // The exact layout has: accounts[4]=amm, accounts[8]=coinMint, accounts[9]=pcMint
        if prog == PK_RAYDIUM_V4 && !data.is_empty() && data[0] == RAYDIUM_V4_INITIALIZE2_DISC {
            // Raydium V4 initialize2 has >15 accounts. Pool is at index 4.
            if acc_indices.len() > 9 {
                let pool = accounts.get(*acc_indices.get(4).unwrap_or(&0) as usize).copied();
                let mint_a = accounts.get(*acc_indices.get(8).unwrap_or(&0) as usize).copied();
                let mint_b = accounts.get(*acc_indices.get(9).unwrap_or(&0) as usize).copied();
                if let (Some(pool), Some(ma), Some(mb)) = (pool, mint_a, mint_b) {
                    events.push(PoolCreationEvent {
                        pool,
                        token_a: ma,
                        token_b: mb,
                        dex: PoolCreationDex::RaydiumAmmV4,
                    });
                }
            }
        }

        // Orca Whirlpool initializePool: disc(8), accounts layout:
        // [0]=whirlpools_config, [1]=token_mint_a, [2]=token_mint_b, ..., [4]=whirlpool (pool)
        if prog == PK_ORCA && data.len() >= 8 && data[..8] == ORCA_INITIALIZE_POOL_DISC {
            if acc_indices.len() > 4 {
                let mint_a = accounts.get(*acc_indices.get(1).unwrap_or(&0) as usize).copied();
                let mint_b = accounts.get(*acc_indices.get(2).unwrap_or(&0) as usize).copied();
                let pool = accounts.get(*acc_indices.get(4).unwrap_or(&0) as usize).copied();
                if let (Some(pool), Some(ma), Some(mb)) = (pool, mint_a, mint_b) {
                    events.push(PoolCreationEvent {
                        pool,
                        token_a: ma,
                        token_b: mb,
                        dex: PoolCreationDex::OrcaWhirlpool,
                    });
                }
            }
        }

        // Meteora DLMM initializeLbPair: disc(8), accounts layout:
        // [0]=lb_pair (pool), [1]=bin_array_bitmap, [2]=token_mint_x, [3]=token_mint_y
        if prog == PK_METEORA && data.len() >= 8 && data[..8] == METEORA_INITIALIZE_LB_PAIR_DISC {
            if acc_indices.len() > 3 {
                let pool = accounts.get(*acc_indices.get(0).unwrap_or(&0) as usize).copied();
                let mint_a = accounts.get(*acc_indices.get(2).unwrap_or(&0) as usize).copied();
                let mint_b = accounts.get(*acc_indices.get(3).unwrap_or(&0) as usize).copied();
                if let (Some(pool), Some(ma), Some(mb)) = (pool, mint_a, mint_b) {
                    events.push(PoolCreationEvent {
                        pool,
                        token_a: ma,
                        token_b: mb,
                        dex: PoolCreationDex::MeteoraDlmm,
                    });
                }
            }
        }

        // Raydium CPMM initialize: disc(8), accounts layout:
        // [0]=creator, [1]=amm_config, [2]=authority, [3]=pool_state, ..., [5]=token_0_mint, [6]=token_1_mint
        if prog == PK_RAYDIUM_CPMM && data.len() >= 8 && data[..8] == RAYDIUM_CPMM_INITIALIZE_DISC {
            if acc_indices.len() > 6 {
                let pool = accounts.get(*acc_indices.get(3).unwrap_or(&0) as usize).copied();
                let mint_a = accounts.get(*acc_indices.get(5).unwrap_or(&0) as usize).copied();
                let mint_b = accounts.get(*acc_indices.get(6).unwrap_or(&0) as usize).copied();
                if let (Some(pool), Some(ma), Some(mb)) = (pool, mint_a, mint_b) {
                    events.push(PoolCreationEvent {
                        pool,
                        token_a: ma,
                        token_b: mb,
                        dex: PoolCreationDex::RaydiumCpmm,
                    });
                }
            }
        }
    }

    events
}
