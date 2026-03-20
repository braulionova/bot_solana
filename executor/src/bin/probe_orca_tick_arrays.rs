/// Directed probe: simulates an Orca SwapV2 for a specific pool using the same
/// traversal-based tick-array selection as tx_builder::derive_orca_tick_arrays.
/// This validates that the new orca.rs quote_exact_in traversal resolves the
/// TickArraySequenceInvalidIndex (6038) error without the old brute-force shift loop.
///
/// Usage:
///   ORCA_POOL=<pubkey> ORCA_A_TO_B=1 ORCA_AMOUNT_IN=918000000 \
///     ./target/release/probe_orca_tick_arrays
use anyhow::{Context, Result};
use borsh::BorshSerialize;
use market_engine::{
    orca::quote_exact_in as quote_orca_exact_in,
    pool_hydrator::{load_mapped_pools, PoolHydrator},
    pool_state::PoolStateCache,
};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};
use std::{str::FromStr, sync::Arc};

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const MAPPED_POOLS_FILE: &str = "/root/solana-bot/mapped_pools.json";
const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const ORCA_WHIRLPOOL_PROGRAM_ID: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const DEFAULT_POOL: &str = "7rDhNbGByYY3rbnJQDFMV9txQYZeBUNQwsMnowDRvw22";
const DEFAULT_PAYER: &str = "Hn87MEK6NLiGquWay8n151xXDN4o3xyvhwuRmge4tS1Y";
const DEFAULT_AMOUNT_IN: u64 = 918_000_000;
const ORCA_TICK_ARRAY_SIZE: i32 = 88;

#[derive(BorshSerialize)]
struct SwapV2Args {
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit: u128,
    amount_specified_is_input: bool,
    a_to_b: bool,
    remaining_accounts_info: Option<RemainingAccountsInfo>,
}

#[derive(BorshSerialize)]
struct RemainingAccountsInfo {
    slices: Vec<RemainingAccountsSlice>,
}

#[derive(BorshSerialize)]
struct RemainingAccountsSlice {
    accounts_type: AccountsType,
    length: u8,
}

#[derive(BorshSerialize)]
enum AccountsType {
    TransferHookA,
    TransferHookB,
    TransferHookReward,
    TransferHookInput,
    TransferHookIntermediate,
    TransferHookOutput,
    SupplementalTickArrays,
    SupplementalTickArraysOne,
    SupplementalTickArraysTwo,
}

fn main() -> Result<()> {
    let pool_key =
        Pubkey::from_str(&std::env::var("ORCA_POOL").unwrap_or_else(|_| DEFAULT_POOL.to_string()))?;
    let payer = Pubkey::from_str(
        &std::env::var("ORCA_PAYER").unwrap_or_else(|_| DEFAULT_PAYER.to_string()),
    )?;
    let amount_in = std::env::var("ORCA_AMOUNT_IN")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_AMOUNT_IN);
    let a_to_b = std::env::var("ORCA_A_TO_B")
        .ok()
        .map(|raw| raw == "1" || raw.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // ── 1. Hydrate pool (downloads Whirlpool account + surrounding tick arrays) ──
    let pool_cache = Arc::new(PoolStateCache::new());
    let hydrator = PoolHydrator::new(RPC_URL, pool_cache.clone(), MAPPED_POOLS_FILE);
    let mapped = load_mapped_pools(MAPPED_POOLS_FILE)
        .with_context(|| format!("failed to load {}", MAPPED_POOLS_FILE))?;
    hydrator.load_from_file()?;
    let selected_mapped: Vec<_> = mapped
        .into_iter()
        .filter(|pool| pool.pool_id.parse::<Pubkey>().ok() == Some(pool_key))
        .collect();
    if selected_mapped.is_empty() {
        anyhow::bail!(
            "Pool {} not found in {}. Check MAPPED_POOLS_FILE or ORCA_POOL.",
            pool_key,
            MAPPED_POOLS_FILE
        );
    }
    hydrator.refresh_all(&selected_mapped)?;

    let pool = pool_cache
        .get(&pool_key)
        .with_context(|| format!("pool {} not in cache after hydration", pool_key))?;
    let meta = pool
        .orca_meta
        .as_ref()
        .cloned()
        .context("orca metadata missing — pool may not be an Orca Whirlpool")?;

    println!(
        "probe_orca_tick_arrays (traversal mode): pool={} amount_in={} a_to_b={}",
        pool_key, amount_in, a_to_b
    );
    println!(
        "  tick_current_index={} tick_spacing={} liquidity={} sqrt_price={}",
        meta.tick_current_index, meta.tick_spacing, meta.liquidity, meta.sqrt_price
    );
    println!("  token_a={} token_b={}", pool.token_a, pool.token_b);
    println!("  cached tick arrays: {}", meta.tick_arrays.len());

    // ── 2. Derive tick array sequence via traversal (mirrors tx_builder logic) ──
    let start_tick = tick_array_start_index(meta.tick_current_index, meta.tick_spacing);
    let offset = meta.tick_spacing as i32 * ORCA_TICK_ARRAY_SIZE;
    let direction = if a_to_b { -1 } else { 1 };

    let mut ordered_starts = if !meta.tick_arrays.is_empty() {
        let quote = quote_orca_exact_in(
            amount_in,
            a_to_b,
            meta.sqrt_price,
            meta.liquidity,
            meta.tick_current_index,
            meta.tick_spacing,
            pool.fee_bps,
            &meta.tick_arrays,
        );
        println!(
            "  traversal quote: amount_out={} traversed_arrays={:?}",
            quote.amount_out, quote.traversed_arrays
        );
        quote.traversed_arrays
    } else {
        println!("  WARNING: no tick arrays in cache — falling back to sequential start");
        vec![start_tick]
    };

    if ordered_starts.is_empty() {
        ordered_starts.push(start_tick);
    }
    // Ensure the current array is always first
    if ordered_starts[0] != start_tick {
        ordered_starts.insert(0, start_tick);
    }
    // Deduplicate while preserving order
    let mut deduped = Vec::with_capacity(ordered_starts.len());
    for s in ordered_starts {
        if !deduped.contains(&s) {
            deduped.push(s);
        }
    }
    let mut ordered_starts = deduped;
    // Extend only with tick arrays that exist in the on-chain cache.
    while ordered_starts.len() < 5 {
        let base = *ordered_starts.last().unwrap_or(&start_tick);
        let next = base + direction * offset;
        if meta.tick_arrays.contains_key(&next) && !ordered_starts.contains(&next) {
            ordered_starts.push(next);
        } else {
            break;
        }
    }

    // Pad by repeating the last valid entry (official Orca SDK behavior).
    let fallback = *ordered_starts.last().unwrap_or(&start_tick);
    let primary_starts = [
        *ordered_starts.get(0).unwrap_or(&fallback),
        *ordered_starts.get(1).unwrap_or(&fallback),
        *ordered_starts.get(2).unwrap_or(&fallback),
    ];
    let supplemental_starts = vec![
        *ordered_starts.get(3).unwrap_or(&fallback),
        *ordered_starts.get(4).unwrap_or(&fallback),
    ];

    let primary = [
        derive_orca_tick_array(pool_key, primary_starts[0])?,
        derive_orca_tick_array(pool_key, primary_starts[1])?,
        derive_orca_tick_array(pool_key, primary_starts[2])?,
    ];
    let supplemental: Vec<Pubkey> = supplemental_starts
        .iter()
        .map(|&s| derive_orca_tick_array(pool_key, s))
        .collect::<Result<_>>()?;

    println!("  primary tick array starts:      {:?}", primary_starts);
    println!(
        "  supplemental tick array starts: {:?}",
        supplemental_starts
    );
    for (i, k) in primary.iter().enumerate() {
        println!("  primary[{}]={}", i, k);
    }
    for (i, k) in supplemental.iter().enumerate() {
        println!("  supplemental[{}]={}", i, k);
    }

    // ── 3. Simulate ──
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
    let memo_program: Pubkey = MEMO_PROGRAM_ID.parse().unwrap();
    let oracle = Pubkey::find_program_address(
        &[b"oracle", pool_key.as_ref()],
        &ORCA_WHIRLPOOL_PROGRAM_ID.parse::<Pubkey>().unwrap(),
    )
    .0;

    let instructions = vec![
        build_create_ata_idempotent_ix(&payer, &payer, &pool.token_a, &token_program),
        build_create_ata_idempotent_ix(&payer, &payer, &pool.token_b, &token_program),
        build_orca_swap_ix(
            &payer,
            &pool_key,
            &pool.token_a,
            &pool.token_b,
            &meta.token_vault_a,
            &meta.token_vault_b,
            amount_in,
            a_to_b,
            primary,
            &supplemental,
            token_program,
            memo_program,
            oracle,
        )?,
    ];

    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let blockhash = rpc.get_latest_blockhash()?;
    let msg = v0::Message::try_compile(&payer, &instructions, &[], blockhash)?;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(msg),
    };

    let sim = rpc.simulate_transaction_with_config(
        &tx,
        RpcSimulateTransactionConfig {
            sig_verify: false,
            replace_recent_blockhash: true,
            ..RpcSimulateTransactionConfig::default()
        },
    )?;

    match sim.value.err {
        None => {
            println!("\nSUCCESS — simulation passed with traversal-derived tick arrays");
            if let Some(logs) = sim.value.logs {
                for log in logs {
                    println!("  {}", log);
                }
            }
        }
        Some(err) => {
            println!("\nFAILED — err={:?}", err);
            if let Some(logs) = sim.value.logs {
                for log in logs {
                    println!("  {}", log);
                }
            }
        }
    }

    Ok(())
}

fn tick_array_start_index(tick_index: i32, tick_spacing: u16) -> i32 {
    let tick_spacing = tick_spacing as i32;
    tick_index
        .div_euclid(tick_spacing)
        .div_euclid(ORCA_TICK_ARRAY_SIZE)
        * tick_spacing
        * ORCA_TICK_ARRAY_SIZE
}

fn derive_orca_tick_array(whirlpool: Pubkey, start_tick_index: i32) -> Result<Pubkey> {
    let program_id: Pubkey = ORCA_WHIRLPOOL_PROGRAM_ID.parse().unwrap();
    let start_tick_str = start_tick_index.to_string();
    Ok(Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), start_tick_str.as_bytes()],
        &program_id,
    )
    .0)
}

fn build_orca_swap_ix(
    payer: &Pubkey,
    whirlpool: &Pubkey,
    token_mint_a: &Pubkey,
    token_mint_b: &Pubkey,
    token_vault_a: &Pubkey,
    token_vault_b: &Pubkey,
    amount_in: u64,
    a_to_b: bool,
    primary: [Pubkey; 3],
    supplemental: &[Pubkey],
    token_program: Pubkey,
    memo_program: Pubkey,
    oracle: Pubkey,
) -> Result<Instruction> {
    let args = SwapV2Args {
        amount: amount_in,
        other_amount_threshold: 0,
        sqrt_price_limit: 0,
        amount_specified_is_input: true,
        a_to_b,
        remaining_accounts_info: (!supplemental.is_empty()).then(|| RemainingAccountsInfo {
            slices: vec![RemainingAccountsSlice {
                accounts_type: AccountsType::SupplementalTickArrays,
                length: supplemental.len() as u8,
            }],
        }),
    };
    let mut data = vec![43, 4, 237, 11, 26, 201, 30, 98];
    args.serialize(&mut data)?;

    let owner_a = associated_token_address(payer, token_mint_a);
    let owner_b = associated_token_address(payer, token_mint_b);

    let mut accounts = vec![
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(memo_program, false),
        AccountMeta::new_readonly(*payer, true),
        AccountMeta::new(*whirlpool, false),
        AccountMeta::new_readonly(*token_mint_a, false),
        AccountMeta::new_readonly(*token_mint_b, false),
        AccountMeta::new(owner_a, false),
        AccountMeta::new(*token_vault_a, false),
        AccountMeta::new(owner_b, false),
        AccountMeta::new(*token_vault_b, false),
        AccountMeta::new(primary[0], false),
        AccountMeta::new(primary[1], false),
        AccountMeta::new(primary[2], false),
        AccountMeta::new(oracle, false),
    ];
    accounts.extend(supplemental.iter().map(|k| AccountMeta::new(*k, false)));

    Ok(Instruction {
        program_id: ORCA_WHIRLPOOL_PROGRAM_ID.parse().unwrap(),
        accounts,
        data,
    })
}

fn associated_token_address(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
    let associated_program: Pubkey = ASSOCIATED_TOKEN_PROGRAM_ID.parse().unwrap();
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_program,
    )
    .0
}

fn build_create_ata_idempotent_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let ata = associated_token_address(owner, mint);
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID.parse().unwrap(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}
