// tx_builder.rs – Build a VersionedTransaction that executes a flash-arb.

use anyhow::{bail, Context, Result};
use borsh::BorshSerialize;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program,
    transaction::VersionedTransaction,
};
use std::collections::{HashMap, HashSet};
use tracing::info;

use market_engine::{
    orca::quote_exact_in as quote_orca_exact_in,
    pool_state::PoolStateCache,
    types::{DexType, FlashProvider, RouteParams},
};

use crate::flash_loans;

pub const HELIOS_ARB_PROGRAM_ID: &str = "DMSsKkNyyxVviDGHJTVpGxmSnzgMiPFrJ2SNvmbjhm64";
const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbEquiGDJ66fqNWi9Ejh2To96pqV9pMBoY";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const ORCA_WHIRLPOOL_PROGRAM_ID: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
const ORCA_TICK_ARRAY_SIZE: i32 = 88;
const METEORA_DLMM_PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const METEORA_EVENT_AUTHORITY: &str = "D8cy77uS2nZBWbS99Z6qc9CDjyYvPbcE969RYdhDkkE";
const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const PUMPSWAP_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const PUMPSWAP_PROTOCOL_FEE_RECIPIENT: &str = "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(BorshSerialize)]
struct MeteoraSwapArgs {
    amount_in: u64,
    min_amount_out: u64,
}

#[derive(BorshSerialize)]
struct ExecuteFlashArbArgs {
    borrow_amount: u64,
    min_profit: u64,
    hop_account_counts: Vec<u8>,
    hop_data: Vec<Vec<u8>>,
}

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
    accounts_type: OrcaAccountsType,
    length: u8,
}

#[derive(BorshSerialize)]
enum OrcaAccountsType {
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

struct OrcaTickArraySet {
    primary: [Pubkey; 3],
    supplemental: Vec<Pubkey>,
}

pub struct TxBuilder {
    pub payer: Keypair,
    helios_program: Pubkey,
    rpc: RpcClient,
    /// Cache: mint → (token_program, cached_at). TTL = 300s.
    mint_program_cache: dashmap::DashMap<Pubkey, (Pubkey, std::time::Instant)>,
}

impl TxBuilder {
    pub fn new(payer: Keypair, rpc_url: &str) -> Self {
        let helios_program = HELIOS_ARB_PROGRAM_ID.parse().unwrap();
        Self {
            payer,
            helios_program,
            rpc: RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::processed()),
            mint_program_cache: dashmap::DashMap::new(),
        }
    }

    /// Batch-fetch the token program (SPL Token or Token-2022) for all mints in a route.
    /// Uses a DashMap cache with 300s TTL to avoid repeated RPC calls.
    /// Public so callers can fetch once and pass to build_with_cached_programs().
    pub fn token_programs_for_route(&self, route: &RouteParams) -> HashMap<Pubkey, Pubkey> {
        const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
        let t22: Pubkey = TOKEN_2022_PROGRAM_ID.parse().unwrap();
        let spl: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
        let all_mints: Vec<Pubkey> = route
            .hops
            .iter()
            .flat_map(|h| [h.token_in, h.token_out])
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let mut result = HashMap::new();
        let mut uncached: Vec<Pubkey> = Vec::new();

        let now = std::time::Instant::now();
        for mint in &all_mints {
            // Well-known SPL Token mints — skip RPC entirely.
            let known_spl = mint.to_string();
            if known_spl.starts_with("So111") // WSOL
                || known_spl.starts_with("EPjFW") // USDC
                || known_spl.starts_with("Es9vM") // USDT
                || known_spl.starts_with("mSoLz") // mSOL
                || known_spl.starts_with("7dHbW") // stSOL
                || known_spl.starts_with("J1tos") // jitoSOL
                || known_spl.starts_with("bSo13") // bSOL
            {
                result.insert(*mint, spl);
                self.mint_program_cache.insert(*mint, (spl, now));
                continue;
            }
            if let Some(entry) = self.mint_program_cache.get(mint) {
                let (prog, cached_at) = entry.value();
                if now.duration_since(*cached_at) < CACHE_TTL {
                    result.insert(*mint, *prog);
                    continue;
                }
            }
            uncached.push(*mint);
        }

        if uncached.is_empty() {
            return result;
        }

        // Non-blocking: if RPC is slow, default to SPL Token to avoid blocking hot path.
        if let Ok(accounts) = self.rpc.get_multiple_accounts(&uncached) {
            for (mint, maybe_acc) in uncached.iter().zip(accounts.iter()) {
                let prog = maybe_acc
                    .as_ref()
                    .map(|a| if a.owner == t22 { t22 } else { spl })
                    .unwrap_or(spl);
                self.mint_program_cache.insert(*mint, (prog, now));
                result.insert(*mint, prog);
            }
        } else {
            for mint in &uncached {
                result.insert(*mint, spl);
            }
        }
        result
    }

    pub fn build(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
    ) -> Result<VersionedTransaction> {
        self.build_inner(route, flash_provider, recent_blockhash, alt_accounts, pool_cache, false, None)
    }

    /// Build with a Jito tip instruction appended (for bundle submission).
    pub fn build_with_tip(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
        tip_ix: Instruction,
    ) -> Result<VersionedTransaction> {
        self.build_inner(route, flash_provider, recent_blockhash, alt_accounts, pool_cache, false, Some(tip_ix))
    }

    /// Build with `min_out=1` for all swaps so simulation never fails on slippage.
    /// Use this to check if the route is structurally valid and profitable
    /// (extract actual output from logs), then rebuild with real slippage for sending.
    pub fn build_for_simulation(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
    ) -> Result<VersionedTransaction> {
        self.build_inner(route, flash_provider, recent_blockhash, alt_accounts, pool_cache, true, None)
    }

    /// Build with pre-fetched token programs (avoids redundant RPC on multi-provider builds).
    pub fn build_for_simulation_cached(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
        token_programs: &HashMap<Pubkey, Pubkey>,
    ) -> Result<VersionedTransaction> {
        self.build_inner_cached(route, flash_provider, recent_blockhash, alt_accounts, pool_cache, true, None, token_programs)
    }

    /// Build with tip + pre-fetched token programs.
    pub fn build_with_tip_cached(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
        tip_ix: Instruction,
        token_programs: &HashMap<Pubkey, Pubkey>,
    ) -> Result<VersionedTransaction> {
        self.build_inner_cached(route, flash_provider, recent_blockhash, alt_accounts, pool_cache, false, Some(tip_ix), token_programs)
    }

    fn build_inner(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
        simulation_mode: bool,
        tip_ix: Option<Instruction>,
    ) -> Result<VersionedTransaction> {
        let token_programs = self.token_programs_for_route(route);
        self.build_inner_cached(route, flash_provider, recent_blockhash, alt_accounts, pool_cache, simulation_mode, tip_ix, &token_programs)
    }

    fn build_inner_cached(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
        simulation_mode: bool,
        tip_ix: Option<Instruction>,
        token_programs: &HashMap<Pubkey, Pubkey>,
    ) -> Result<VersionedTransaction> {
        let borrow_mint = route
            .hops
            .first()
            .map(|hop| hop.token_in)
            .context("route has no hops")?;
        let borrow_token_program = token_programs
            .get(&borrow_mint)
            .copied()
            .unwrap_or_else(|| TOKEN_PROGRAM_ID.parse().unwrap());
        let user_token_account = associated_token_address_with_program(
            &self.payer.pubkey(),
            &borrow_mint,
            &borrow_token_program,
        );

        let helios_ix = self.build_helios_instruction(route, user_token_account, pool_cache, &token_programs, simulation_mode)?;
        let mut instructions = Vec::new();

        // Compute budget: limit CUs + dynamic priority fee scaled to expected profit.
        // Higher profit → higher priority fee → better chance of inclusion.
        // Formula: priority_fee = max(floor, min(ceil, profit_lamports * PRIORITY_PCT / cu_limit))
        // Default: 10% of net_profit allocated to priority fee, min 10K, max 500K µlamports.
        let cu_limit = 400_000u32; // 2-hop flash arb typical: 200-350K CUs
        let priority_pct: f64 = std::env::var("PRIORITY_FEE_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10.0); // 10% of net profit
        // Priority fee: minimal for arb TXs. Jito tip handles block inclusion priority.
        // Failed TXs via TPU pay base_fee + priority_fee, so keep priority LOW.
        // 1000 µlamports × 400K CU = 400 lamports priority → total fee ~5400 lamports.
        let priority_floor: u64 = std::env::var("PRIORITY_FEE_FLOOR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000); // 1K µlamports (was 10K)
        let priority_ceil: u64 = std::env::var("PRIORITY_FEE_CEIL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_000); // 5K µlamports (was 500K) → max ~2K lamports priority
        // Convert: net_profit (lamports) → priority budget (lamports) → µlamports/CU
        // priority_budget = net_profit * pct / 100
        // µlamports_per_cu = priority_budget * 1_000_000 / cu_limit
        let priority_budget_lamports = (route.net_profit.max(0) as f64 * priority_pct / 100.0) as u64;
        let dynamic_fee = if cu_limit > 0 {
            priority_budget_lamports.saturating_mul(1_000_000) / cu_limit as u64
        } else {
            priority_floor
        };
        let priority_fee = dynamic_fee.max(priority_floor).min(priority_ceil);
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee));
        tracing::debug!(
            net_profit = route.net_profit,
            priority_fee,
            priority_budget_lamports,
            "dynamic priority fee"
        );

        instructions.extend(self.build_ata_setup_instructions(route, &token_programs));
        let ata_count = instructions.len(); // includes compute budget IXs — used as instruction index offset
        instructions.extend(flash_loans::build_wrapped_instructions(
            flash_provider,
            &self.payer,
            borrow_mint,
            route.borrow_amount,
            user_token_account,
            helios_ix,
            ata_count,
        )?);

        // Append Jito tip as last instruction (only for real sends, not simulation).
        if let Some(tip) = tip_ix {
            instructions.push(tip);
        }

        let message = v0::Message::try_compile(
            &self.payer.pubkey(),
            &instructions,
            alt_accounts,
            recent_blockhash,
        )?;
        Ok(VersionedTransaction::try_new(
            VersionedMessage::V0(message),
            &[&self.payer],
        )?)
    }

    pub fn build_ata_setup_transaction(
        &self,
        route: &RouteParams,
        recent_blockhash: Hash,
    ) -> Result<Option<VersionedTransaction>> {
        let token_programs = self.token_programs_for_route(route);
        let instructions = self.build_ata_setup_instructions(route, &token_programs);
        if instructions.is_empty() {
            return Ok(None);
        }

        let message =
            v0::Message::try_compile(&self.payer.pubkey(), &instructions, &[], recent_blockhash)?;
        Ok(Some(VersionedTransaction::try_new(
            VersionedMessage::V0(message),
            &[&self.payer],
        )?))
    }

    pub fn required_token_accounts(&self, route: &RouteParams) -> Vec<(Pubkey, Pubkey)> {
        let token_programs = self.token_programs_for_route(route);
        let mut seen_mints = HashSet::new();
        let mut out = Vec::new();

        for mint in route
            .hops
            .iter()
            .flat_map(|hop| [hop.token_in, hop.token_out])
        {
            if seen_mints.insert(mint) {
                let prog = token_programs
                    .get(&mint)
                    .copied()
                    .unwrap_or_else(|| TOKEN_PROGRAM_ID.parse().unwrap());
                out.push((mint, associated_token_address_with_program(&self.payer.pubkey(), &mint, &prog)));
            }
        }

        out
    }

    /// Build a flash-loan-wrapped TX using Jupiter swap instructions.
    /// Jupiter handles the DEX routing (GoonFi, ZeroFi, HumidiFi, etc.).
    /// MarginFi provides the flash loan (borrow SOL → Jupiter swaps → repay).
    /// The entire round-trip is atomic: fail = 0 cost via Jito bundle.
    pub fn build_jupiter_flash_tx(
        &self,
        jupiter_swap_ixs: Vec<Instruction>,
        borrow_amount: u64,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[solana_sdk::address_lookup_table::AddressLookupTableAccount],
        tip_ix: Option<Instruction>,
    ) -> Result<VersionedTransaction> {
        let borrow_mint: Pubkey = "So11111111111111111111111111111111111111112".parse().unwrap();
        let borrow_tp: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
        let user_token_account = associated_token_address_with_program(
            &self.payer.pubkey(), &borrow_mint, &borrow_tp,
        );

        let mut instructions = Vec::new();

        // Compute budget
        let cu_limit = 1_000_000u32; // Jupiter routes can use a lot of CUs
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(1_000));

        let preceding_count = instructions.len();

        // Wrap Jupiter instructions with flash loan
        let wrapped = flash_loans::build_wrapped_instructions_direct(
            flash_provider,
            &self.payer,
            borrow_mint,
            borrow_amount,
            user_token_account,
            jupiter_swap_ixs,
            preceding_count,
        )?;
        instructions.extend(wrapped);

        if let Some(tip) = tip_ix {
            instructions.push(tip);
        }

        let message = v0::Message::try_compile(
            &self.payer.pubkey(),
            &instructions,
            alt_accounts,
            recent_blockhash,
        )?;
        Ok(VersionedTransaction::try_new(
            VersionedMessage::V0(message),
            &[&self.payer],
        )?)
    }

    fn build_helios_instruction(
        &self,
        route: &RouteParams,
        user_token_account: Pubkey,
        pool_cache: &PoolStateCache,
        token_programs: &HashMap<Pubkey, Pubkey>,
        simulation_mode: bool,
    ) -> Result<Instruction> {
        let mut remaining_accounts = Vec::new();
        let mut hop_account_counts = Vec::new();
        let mut hop_data = Vec::new();
        let mut current_amount_in = route.borrow_amount;

        for (hop_idx, hop) in route.hops.iter().enumerate() {
            let dex = DexType::from_program_id(&hop.dex_program);
            let pool = pool_cache.get(&hop.pool).context("pool not in cache")?;

            // Validate token direction matches pool layout
            if hop.token_in == hop.token_out {
                bail!("hop {} token_in == token_out ({}), invalid route", hop_idx, hop.token_in);
            }
            if hop.token_in != pool.token_a && hop.token_in != pool.token_b {
                bail!(
                    "hop {} token_in {} not in pool {} (token_a={}, token_b={})",
                    hop_idx, hop.token_in, hop.pool, pool.token_a, pool.token_b
                );
            }
            if hop.token_out != pool.token_a && hop.token_out != pool.token_b {
                bail!(
                    "hop {} token_out {} not in pool {} (token_a={}, token_b={})",
                    hop_idx, hop.token_out, hop.pool, pool.token_a, pool.token_b
                );
            }

            let spl: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
            let prog_in = token_programs.get(&hop.token_in).copied().unwrap_or(spl);
            let prog_out = token_programs.get(&hop.token_out).copied().unwrap_or(spl);
            let user_source = associated_token_address_with_program(&self.payer.pubkey(), &hop.token_in, &prog_in);
            let user_destination = associated_token_address_with_program(&self.payer.pubkey(), &hop.token_out, &prog_out);

            let a_to_b = hop.token_in == pool.token_a;

            let mut accounts = match dex {
                DexType::RaydiumAmmV4 => {
                    let meta = pool.raydium_meta.as_ref().context("Raydium meta missing")?;
                    if meta.market_program == Pubkey::default()
                        || meta.market_event_queue == Pubkey::default()
                        || meta.market_bids == Pubkey::default()
                        || meta.market_asks == Pubkey::default()
                        || meta.market_base_vault == Pubkey::default()
                        || meta.market_quote_vault == Pubkey::default()
                        || meta.market_vault_signer == Pubkey::default()
                    {
                        bail!("Raydium metadata incomplete for pool {}", hop.pool);
                    }

                    vec![
                        AccountMeta::new_readonly(hop.dex_program, false),
                        AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse().unwrap(), false),
                        AccountMeta::new(hop.pool, false),
                        AccountMeta::new_readonly(meta.authority, false),
                        AccountMeta::new(meta.open_orders, false),
                        AccountMeta::new(meta.vault_a, false),
                        AccountMeta::new(meta.vault_b, false),
                        AccountMeta::new_readonly(meta.market_program, false),
                        AccountMeta::new(meta.market_id, false),
                        AccountMeta::new(meta.market_bids, false),
                        AccountMeta::new(meta.market_asks, false),
                        AccountMeta::new(meta.market_event_queue, false),
                        AccountMeta::new(meta.market_base_vault, false),
                        AccountMeta::new(meta.market_quote_vault, false),
                        AccountMeta::new_readonly(meta.market_vault_signer, false),
                        AccountMeta::new(user_source, false),
                        AccountMeta::new(user_destination, false),
                        AccountMeta::new_readonly(self.payer.pubkey(), true),
                    ]
                }
                DexType::OrcaWhirlpool => {
                    let meta = pool.orca_meta.as_ref().context("Orca meta missing")?;
                    if meta.tick_spacing == 0 {
                        bail!("Orca metadata incomplete for pool {}", hop.pool);
                    }
                    let token_owner_account_a =
                        associated_token_address(&self.payer.pubkey(), &pool.token_a);
                    let token_owner_account_b =
                        associated_token_address(&self.payer.pubkey(), &pool.token_b);
                    let tick_arrays = derive_orca_tick_arrays(
                        hop.pool,
                        meta,
                        current_amount_in,
                        pool.fee_bps,
                        a_to_b,
                    )?;
                    let whirlpool_authority = derive_orca_whirlpool_authority(hop.pool)?;
                    let oracle = derive_orca_oracle(hop.pool)?;
                    info!(
                        pool = %hop.pool,
                        token_in = %hop.token_in,
                        token_out = %hop.token_out,
                        pool_token_a = %pool.token_a,
                        pool_token_b = %pool.token_b,
                        a_to_b,
                        tick_current_index = meta.tick_current_index,
                        tick_spacing = meta.tick_spacing,
                        tick_array_0 = %tick_arrays.primary[0],
                        tick_array_1 = %tick_arrays.primary[1],
                        tick_array_2 = %tick_arrays.primary[2],
                        supplemental_tick_array_0 = %tick_arrays.supplemental[0],
                        supplemental_tick_array_1 = %tick_arrays.supplemental[1],
                        oracle = %oracle,
                        "building Orca Whirlpool hop"
                    );
                    // Orca SwapV2 account order (verified against real on-chain txs):
                    // [0] dex_program   — consumed by helios-arb as the CPI target
                    // [1] token_program_a
                    // [2] token_program_b
                    // [3] memo_program
                    // [4] token_authority (signer)
                    // [5] whirlpool
                    // [6] token_mint_a        ← both mints grouped together
                    // [7] token_mint_b        ←
                    // [8] token_owner_account_a
                    // [9] token_vault_a
                    // [10] token_owner_account_b
                    // [11] token_vault_b
                    // [12..14] tick_array_0/1/2
                    // [15] oracle
                    // [16..] remaining_accounts: supplemental tick arrays for swap_v2
                    let _ = whirlpool_authority; // not needed for SwapV2
                    let mut metas = vec![
                        AccountMeta::new_readonly(hop.dex_program, false),
                        AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse().unwrap(), false),
                        AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse().unwrap(), false),
                        AccountMeta::new_readonly(MEMO_PROGRAM_ID.parse().unwrap(), false),
                        AccountMeta::new_readonly(self.payer.pubkey(), true),
                        AccountMeta::new(hop.pool, false),
                        AccountMeta::new_readonly(pool.token_a, false),
                        AccountMeta::new_readonly(pool.token_b, false),
                        AccountMeta::new(token_owner_account_a, false),
                        AccountMeta::new(meta.token_vault_a, false),
                        AccountMeta::new(token_owner_account_b, false),
                        AccountMeta::new(meta.token_vault_b, false),
                        AccountMeta::new(tick_arrays.primary[0], false),
                        AccountMeta::new(tick_arrays.primary[1], false),
                        AccountMeta::new(tick_arrays.primary[2], false),
                        AccountMeta::new(oracle, false),
                    ];
                    metas.extend(
                        tick_arrays
                            .supplemental
                            .into_iter()
                            .map(|tick_array| AccountMeta::new(tick_array, false)),
                    );
                    metas
                }
                DexType::MeteoraDlmm => {
                    let meta = pool.meteora_meta.as_ref().context("Meteora meta missing")?;
                    let bin_arrays = derive_meteora_bin_arrays(hop.pool, meta, a_to_b)?;
                    let event_authority = METEORA_EVENT_AUTHORITY.parse::<Pubkey>().unwrap();
                    let oracle = derive_meteora_oracle(hop.pool)?;

                    // Meteora DLMM swap account order:
                    // [0] dex_program
                    // [1] lb_pair
                    // [2] reserve_x
                    // [3] reserve_y
                    // [4] user_token_in
                    // [5] user_token_out
                    // [6] token_x_mint
                    // [7] token_y_mint
                    // [8] oracle
                    // [9] user_authority
                    // [10] token_program
                    // [11] event_authority
                    // [12] program (same as dex_program)
                    // [13..15] bin_arrays
                    let mut metas = vec![
                        AccountMeta::new_readonly(hop.dex_program, false),
                        AccountMeta::new(hop.pool, false),
                        AccountMeta::new(meta.token_vault_a, false),
                        AccountMeta::new(meta.token_vault_b, false),
                        AccountMeta::new(user_source, false),
                        AccountMeta::new(user_destination, false),
                        AccountMeta::new_readonly(pool.token_a, false),
                        AccountMeta::new_readonly(pool.token_b, false),
                        AccountMeta::new(oracle, false),
                        AccountMeta::new_readonly(self.payer.pubkey(), true),
                        AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse().unwrap(), false),
                        AccountMeta::new_readonly(event_authority, false),
                        AccountMeta::new_readonly(hop.dex_program, false),
                    ];
                    for ba in bin_arrays {
                        metas.push(AccountMeta::new(ba, false));
                    }
                    metas
                }
                DexType::PumpSwap => {
                    let pumpswap_program: Pubkey = PUMPSWAP_PROGRAM_ID.parse().unwrap();
                    let wsol_mint: Pubkey = WSOL_MINT.parse().unwrap();
                    let fee_recipient: Pubkey = PUMPSWAP_PROTOCOL_FEE_RECIPIENT.parse().unwrap();

                    // Determine base/quote: PumpSwap pools always have WSOL as quote.
                    let (base_mint, quote_mint) = if pool.token_a == wsol_mint {
                        (pool.token_b, pool.token_a) // token_b is base, token_a is WSOL quote
                    } else {
                        (pool.token_a, pool.token_b) // token_a is base, token_b is WSOL quote
                    };

                    // PDAs
                    let (global_config, _) = Pubkey::find_program_address(
                        &[b"global_config"],
                        &pumpswap_program,
                    );
                    let (event_authority, _) = Pubkey::find_program_address(
                        &[b"__event_authority"],
                        &pumpswap_program,
                    );

                    // Pool vaults are ATAs of the pool account
                    let pool_base_vault = associated_token_address(&hop.pool, &base_mint);
                    let pool_quote_vault = associated_token_address(&hop.pool, &quote_mint);

                    // Protocol fee token accounts
                    let protocol_fee_base = associated_token_address(&fee_recipient, &base_mint);
                    let protocol_fee_quote = associated_token_address(&fee_recipient, &quote_mint);

                    // User ATAs
                    let user_base_ata = associated_token_address_with_program(
                        &self.payer.pubkey(), &base_mint, &prog_in.min(prog_out),
                    );
                    let user_quote_ata = associated_token_address_with_program(
                        &self.payer.pubkey(), &quote_mint, &prog_in.max(prog_out),
                    );

                    // PumpSwap account layout (17 accounts, matching sniper.rs):
                    vec![
                        AccountMeta::new_readonly(pumpswap_program, false),  // [0] dex_program
                        AccountMeta::new(hop.pool, false),                   // [1] pool
                        AccountMeta::new_readonly(global_config, false),     // [2] global_config
                        AccountMeta::new_readonly(base_mint, false),         // [3] base_mint
                        AccountMeta::new_readonly(quote_mint, false),        // [4] quote_mint
                        AccountMeta::new(pool_base_vault, false),            // [5] pool_base_vault
                        AccountMeta::new(pool_quote_vault, false),           // [6] pool_quote_vault
                        AccountMeta::new_readonly(fee_recipient, false),     // [7] protocol_fee_recipient
                        AccountMeta::new(protocol_fee_base, false),          // [8] protocol_fee_base
                        AccountMeta::new(protocol_fee_quote, false),         // [9] protocol_fee_quote
                        AccountMeta::new(self.payer.pubkey(), true),          // [10] user (signer)
                        AccountMeta::new(user_base_ata, false),              // [11] user_base_account
                        AccountMeta::new(user_quote_ata, false),             // [12] user_quote_account
                        AccountMeta::new_readonly(system_program::id(), false), // [13] system_program
                        AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse::<Pubkey>().unwrap(), false), // [14] token_program
                        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID.parse::<Pubkey>().unwrap(), false), // [15] assoc_token_program
                        AccountMeta::new_readonly(event_authority, false),    // [16] event_authority
                        AccountMeta::new_readonly(pumpswap_program, false),   // [17] program
                    ]
                }
                _ => bail!("DEX {:?} not yet supported by helios tx builder", dex),
            };

            if std::env::var("DEBUG_ACCOUNTS").is_ok() {
                eprintln!("HOP{} dex={:?} n_accounts={}", hop_idx, dex, accounts.len());
                for (ai, am) in accounts.iter().enumerate() {
                    eprintln!(
                        "  HOP{} account[{}] = {} writable={} signer={}",
                        hop_idx, ai, am.pubkey, am.is_writable, am.is_signer
                    );
                }
            }
            hop_account_counts.push(accounts.len() as u8);
            remaining_accounts.append(&mut accounts);

            // min_out=1 for ALL modes (simulation AND production).
            // Safety: flash loan is atomic — if swaps produce less than borrow amount,
            // the repay instruction fails and the entire TX reverts (0 cost with JITO_ONLY).
            // Any per-hop slippage check is COUNTER-PRODUCTIVE: it causes Custom(1) failures
            // when reserves shift even slightly between detection and on-chain execution.
            // The flash loan repay IS the slippage check — it's the ultimate safety net.
            let safe_out = hop.amount_out.saturating_mul(95) / 100;
            let min_out = 1u64;
            let orca_supplemental_tick_arrays = if matches!(dex, DexType::OrcaWhirlpool) {
                2
            } else {
                0
            };
            hop_data.push(build_swap_data(
                dex,
                current_amount_in,
                min_out,
                a_to_b,
                orca_supplemental_tick_arrays,
            )?);
            current_amount_in = safe_out;
        }

        let mut data = vec![55, 189, 43, 168, 168, 89, 43, 57];
        let args = ExecuteFlashArbArgs {
            borrow_amount: route.borrow_amount,
            // On-chain min_profit: just enough to cover costs (not the full expected profit).
            // The flash loan already protects capital (reverts if can't repay).
            // Using full net_profit causes InsufficientProfit reverts when reserves shift
            // by even 0.1% between detection and execution (~875ms).
            // 10_000 lamports ≈ TX fee + minimal profit to not waste Jito tip.
            // min_profit=0: only verify no loss. Flash loan is atomic safety net.
            // The on-chain InsufficientProfit check was killing 100% of landed TXs.
            // With min_profit=0, any profit > 0 is accepted on-chain.
            min_profit: 0,
            hop_account_counts,
            hop_data,
        };
        args.serialize(&mut data)?;

        let (config_pda, _) = Pubkey::find_program_address(&[b"config"], &self.helios_program);
        let accounts = vec![
            AccountMeta::new(self.payer.pubkey(), true),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new_readonly(config_pda, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse().unwrap(), false),
        ];

        Ok(Instruction {
            program_id: self.helios_program,
            accounts: accounts.into_iter().chain(remaining_accounts).collect(),
            data,
        })
    }

    fn build_ata_setup_instructions(&self, route: &RouteParams, token_programs: &HashMap<Pubkey, Pubkey>) -> Vec<Instruction> {
        let spl: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
        let mut seen_mints = HashSet::new();
        let mut instructions = Vec::new();

        for mint in route.hops.iter().flat_map(|h| [h.token_in, h.token_out]) {
            if seen_mints.insert(mint) {
                let prog = token_programs.get(&mint).copied().unwrap_or(spl);
                instructions.push(build_create_ata_idempotent_ix(
                    &self.payer.pubkey(),
                    &self.payer.pubkey(),
                    &mint,
                    &prog,
                ));
            }
        }

        instructions
    }
}

fn build_swap_data(
    dex: DexType,
    amount_in: u64,
    min_out: u64,
    a_to_b: bool,
    orca_supplemental_tick_arrays: usize,
) -> Result<Vec<u8>> {
    match dex {
        DexType::RaydiumAmmV4 => {
            let mut data = vec![9u8];
            data.extend_from_slice(&amount_in.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            Ok(data)
        }
        DexType::OrcaWhirlpool => {
            let mut data = vec![43, 4, 237, 11, 26, 201, 30, 98];
            // sqrt_price_limit: MIN for a_to_b, MAX for b_to_a (no restriction).
            // 0 is NOT valid — Orca treats it as "no swap" producing 0 output.
            let sqrt_price_limit: u128 = if a_to_b {
                4295048016u128 // MIN_SQRT_PRICE_X64
            } else {
                79226673515401279992447579055u128 // MAX_SQRT_PRICE_X64
            };
            let args = SwapV2Args {
                amount: amount_in,
                other_amount_threshold: min_out,
                sqrt_price_limit,
                amount_specified_is_input: true,
                a_to_b,
                remaining_accounts_info: (orca_supplemental_tick_arrays > 0).then(|| {
                    RemainingAccountsInfo {
                        slices: vec![RemainingAccountsSlice {
                            accounts_type: OrcaAccountsType::SupplementalTickArrays,
                            length: orca_supplemental_tick_arrays as u8,
                        }],
                    }
                }),
            };
            args.serialize(&mut data)?;
            Ok(data)
        }
        DexType::MeteoraDlmm => {
            let mut data = vec![248, 198, 244, 145, 90, 24, 30, 227];
            let args = MeteoraSwapArgs {
                amount_in,
                min_amount_out: min_out,
            };
            args.serialize(&mut data)?;
            Ok(data)
        }
        DexType::PumpSwap => {
            // PumpSwap uses Anchor-style discriminators.
            // buy  = sha256("global:buy")[..8]  = [102, 6, 61, 18, 1, 218, 235, 234]
            // sell = sha256("global:sell")[..8] = [51, 230, 133, 164, 1, 127, 131, 173]
            // a_to_b: if selling the base token (token_a) for WSOL (token_b) → sell
            //         if buying the base token with WSOL → buy
            // Convention: in PumpSwap, "buy" means WSOL→token, "sell" means token→WSOL.
            // The direction depends on which token is WSOL.
            let wsol: Pubkey = WSOL_MINT.parse().unwrap();
            let is_buy = Pubkey::from(wsol.to_bytes()) == Pubkey::default() || {
                // token_in is WSOL → buying the other token
                let token_in_bytes = if a_to_b {
                    // a_to_b means token_a is input; but we need to check if input is WSOL
                    // The caller sets a_to_b based on pool.token_a, not on base/quote
                    // So we just check: is the input token WSOL?
                    false // placeholder, handled below
                } else {
                    false
                };
                token_in_bytes
            };
            // Simpler approach: the data format is the same for buy/sell — just the disc differs.
            // If input is WSOL, it's a buy. If input is the token, it's a sell.
            let disc = if a_to_b {
                // a_to_b: token_a → token_b. We don't know which is WSOL here,
                // but the discriminator determines direction.
                // PumpSwap pools: token_a is base, token_b is quote (WSOL) typically,
                // but not guaranteed. Use the wsol check.
                // Fallback: just use the buy/sell based on whether we're sending WSOL in.
                // Since we can't check mints here, use a_to_b as proxy:
                // In PumpSwap, selling base for quote = sell, buying base with quote = buy.
                [51u8, 230, 133, 164, 1, 127, 131, 173] // sell (base→quote)
            } else {
                [102u8, 6, 61, 18, 1, 218, 235, 234] // buy (quote→base)
            };
            let mut data = disc.to_vec();
            data.extend_from_slice(&amount_in.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            Ok(data)
        }
        _ => bail!("swap data for {:?} not implemented", dex),
    }
}

/// Returns the Associated Token Address for `owner` and `mint`.
/// Checks whether the mint is Token-2022 (owner = TokenzQd...) or SPL Token.
/// Falls back to SPL Token if the RPC call fails.
pub fn resolve_token_program_for_mint(
    rpc: &solana_rpc_client::rpc_client::RpcClient,
    mint: &Pubkey,
) -> Pubkey {
    use solana_sdk::commitment_config::CommitmentConfig;
    let t22: Pubkey = TOKEN_2022_PROGRAM_ID.parse().unwrap();
    if let Ok(Some(acc)) = rpc.get_account_with_commitment(mint, CommitmentConfig::processed())
        .map(|r| r.value)
    {
        if acc.owner == t22 {
            return t22;
        }
    }
    TOKEN_PROGRAM_ID.parse().unwrap()
}

fn associated_token_address(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    associated_token_address_with_program(owner, mint, &TOKEN_PROGRAM_ID.parse().unwrap())
}

fn associated_token_address_with_program(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
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
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

fn derive_orca_oracle(whirlpool: Pubkey) -> Result<Pubkey> {
    let program_id: Pubkey = ORCA_WHIRLPOOL_PROGRAM_ID.parse().unwrap();
    Pubkey::try_find_program_address(&[b"oracle", whirlpool.as_ref()], &program_id)
        .map(|(pubkey, _)| pubkey)
        .context("derive Orca oracle PDA")
}

fn derive_orca_whirlpool_authority(whirlpool: Pubkey) -> Result<Pubkey> {
    let program_id: Pubkey = ORCA_WHIRLPOOL_PROGRAM_ID.parse().unwrap();
    Pubkey::try_find_program_address(&[b"whirlpool", whirlpool.as_ref()], &program_id)
        .map(|(pubkey, _)| pubkey)
        .context("derive Whirlpool authority PDA")
}

fn derive_meteora_oracle(lb_pair: Pubkey) -> Result<Pubkey> {
    let program_id: Pubkey = METEORA_DLMM_PROGRAM_ID.parse().unwrap();
    Pubkey::try_find_program_address(&[b"oracle", lb_pair.as_ref()], &program_id)
        .map(|(pubkey, _)| pubkey)
        .context("derive Meteora oracle PDA")
}

fn derive_meteora_bin_arrays(
    lb_pair: Pubkey,
    meta: &market_engine::pool_state::MeteoraMetadata,
    a_to_b: bool,
) -> Result<Vec<Pubkey>> {
    let current_bin_array_idx = meteora_bin_id_to_array_index(meta.active_id as i64);
    let direction = if a_to_b { -1 } else { 1 };
    let mut indices: Vec<i64> = meta
        .bin_arrays
        .keys()
        .copied()
        .filter(|idx| {
            if a_to_b {
                *idx <= current_bin_array_idx
            } else {
                *idx >= current_bin_array_idx
            }
        })
        .collect();

    indices.sort_unstable();
    if a_to_b {
        indices.reverse();
    }

    if indices.is_empty() {
        indices = vec![
            current_bin_array_idx,
            current_bin_array_idx + direction,
            current_bin_array_idx + direction * 2,
        ];
    }

    indices
        .into_iter()
        .map(|index| derive_meteora_bin_array(lb_pair, index))
        .collect()
}

fn derive_meteora_bin_array(lb_pair: Pubkey, bin_array_index: i64) -> Result<Pubkey> {
    let program_id: Pubkey = METEORA_DLMM_PROGRAM_ID.parse().unwrap();
    Pubkey::try_find_program_address(
        &[
            b"bin_array",
            lb_pair.as_ref(),
            &bin_array_index.to_le_bytes(),
        ],
        &program_id,
    )
    .map(|(pubkey, _)| pubkey)
    .context("derive Meteora bin array PDA")
}

fn meteora_bin_id_to_array_index(bin_id: i64) -> i64 {
    let idx = bin_id / 70;
    let rem = bin_id % 70;
    if bin_id < 0 && rem != 0 {
        idx - 1
    } else {
        idx
    }
}

fn derive_orca_tick_arrays(
    whirlpool: Pubkey,
    meta: &market_engine::pool_state::OrcaMetadata,
    amount_in: u64,
    fee_bps: u64,
    a_to_b: bool,
) -> Result<OrcaTickArraySet> {
    let start_tick_index =
        get_orca_tick_array_start_tick_index(meta.tick_current_index, meta.tick_spacing);
    let offset = meta.tick_spacing as i32 * ORCA_TICK_ARRAY_SIZE;
    let direction = if a_to_b { -1 } else { 1 };

    // Step 1: Use traversal-based quote to determine which arrays the swap actually crosses.
    let mut ordered_starts = if !meta.tick_arrays.is_empty() {
        quote_orca_exact_in(
            amount_in,
            a_to_b,
            meta.sqrt_price,
            meta.liquidity,
            meta.tick_current_index,
            meta.tick_spacing,
            fee_bps,
            &meta.tick_arrays,
        )
        .traversed_arrays
    } else {
        vec![start_tick_index]
    };

    if ordered_starts.is_empty() {
        ordered_starts.push(start_tick_index);
    }
    // Ensure current tick array is always first.
    if ordered_starts[0] != start_tick_index {
        ordered_starts.insert(0, start_tick_index);
    }
    // Dedup.
    let mut deduped = Vec::with_capacity(ordered_starts.len());
    for start in ordered_starts {
        if !deduped.contains(&start) {
            deduped.push(start);
        }
    }
    let mut ordered_starts = deduped;

    // Step 2: ALWAYS extend to 5 sequential arrays using deterministic start indices.
    // Orca on-chain validates that tick_arrays form a contiguous sequence:
    //   ta[1].start = ta[0].start + direction * offset
    //   ta[2].start = ta[1].start + direction * offset
    // Padding with the SAME array causes 6038 because the start indices don't form
    // a valid sequence. Instead, derive the correct sequential start indices.
    // If the underlying account doesn't exist on-chain, the TX will fail with
    // AccountNotInitialized (a correct, expected failure) instead of 6038.
    while ordered_starts.len() < 5 {
        let base = *ordered_starts.last().unwrap();
        let next = base + direction * offset;
        if !ordered_starts.contains(&next) {
            ordered_starts.push(next);
        } else {
            // Should never happen, but avoid infinite loop.
            break;
        }
    }

    let primary_starts = [
        ordered_starts[0],
        ordered_starts[1],
        ordered_starts[2],
    ];
    let supplemental_starts = vec![
        ordered_starts[3],
        ordered_starts[4],
    ];

    tracing::debug!(
        %whirlpool,
        a_to_b,
        tick_current = meta.tick_current_index,
        tick_spacing = meta.tick_spacing,
        ?primary_starts,
        ?supplemental_starts,
        cached_arrays = meta.tick_arrays.len(),
        "orca tick array sequence"
    );

    Ok(OrcaTickArraySet {
        primary: [
            derive_orca_tick_array(whirlpool, primary_starts[0])?,
            derive_orca_tick_array(whirlpool, primary_starts[1])?,
            derive_orca_tick_array(whirlpool, primary_starts[2])?,
        ],
        supplemental: supplemental_starts
            .into_iter()
            .map(|start| derive_orca_tick_array(whirlpool, start))
            .collect::<Result<Vec<_>>>()?,
    })
}

fn derive_orca_tick_array(whirlpool: Pubkey, start_tick_index: i32) -> Result<Pubkey> {
    let program_id: Pubkey = ORCA_WHIRLPOOL_PROGRAM_ID.parse().unwrap();
    let start_tick_index = start_tick_index.to_string();
    Pubkey::try_find_program_address(
        &[
            b"tick_array",
            whirlpool.as_ref(),
            start_tick_index.as_bytes(),
        ],
        &program_id,
    )
    .map(|(pubkey, _)| pubkey)
    .context("derive Orca tick array PDA")
}

fn get_orca_tick_array_start_tick_index(tick_index: i32, tick_spacing: u16) -> i32 {
    let tick_spacing = tick_spacing as i32;
    tick_index
        .div_euclid(tick_spacing)
        .div_euclid(ORCA_TICK_ARRAY_SIZE)
        * tick_spacing
        * ORCA_TICK_ARRAY_SIZE
}

impl TxBuilder {
    /// Build a TX using DIRECT DEX swap instructions (no helios-arb CPI).
    ///
    /// Saves ~30-50% CUs. Only works with MarginFi flash loans.
    /// Falls back to helios-arb if DEX type not supported for direct swap.
    pub fn build_direct_swap_tx(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
        tip_ix: Option<Instruction>,
    ) -> Result<VersionedTransaction> {
        let token_programs = self.token_programs_for_route(route);
        let borrow_mint = route.hops.first()
            .map(|h| h.token_in)
            .context("route has no hops")?;
        let spl: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
        let borrow_tp = token_programs.get(&borrow_mint).copied().unwrap_or(spl);
        let user_borrow_ata = associated_token_address_with_program(
            &self.payer.pubkey(), &borrow_mint, &borrow_tp,
        );

        let mut instructions = Vec::new();

        // Lower CU limit for direct swaps (no on-chain program overhead)
        let cu_limit = 250_000u32;
        let priority_floor: u64 = std::env::var("PRIORITY_FEE_FLOOR")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000);
        let priority_ceil: u64 = std::env::var("PRIORITY_FEE_CEIL")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(5_000);
        let priority_pct: f64 = std::env::var("PRIORITY_FEE_PCT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(10.0);
        let priority_budget = (route.net_profit.max(0) as f64 * priority_pct / 100.0) as u64;
        let dynamic_fee = if cu_limit > 0 {
            priority_budget.saturating_mul(1_000_000) / cu_limit as u64
        } else { priority_floor };
        let priority_fee = dynamic_fee.max(priority_floor).min(priority_ceil);
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee));

        instructions.extend(self.build_ata_setup_instructions(route, &token_programs));

        // Build direct DEX swap instructions
        let mut swap_ixs = Vec::new();
        let mut current_amount = route.borrow_amount;

        for (hop_idx, hop) in route.hops.iter().enumerate() {
            let dex = DexType::from_program_id(&hop.dex_program);
            let pool = pool_cache.get(&hop.pool).context("pool not in cache")?;
            let a_to_b = hop.token_in == pool.token_a;
            let prog_in = token_programs.get(&hop.token_in).copied().unwrap_or(spl);
            let prog_out = token_programs.get(&hop.token_out).copied().unwrap_or(spl);
            let user_source = associated_token_address_with_program(&self.payer.pubkey(), &hop.token_in, &prog_in);
            let user_dest = associated_token_address_with_program(&self.payer.pubkey(), &hop.token_out, &prog_out);
            // min_out=1: flash loan repay is the safety net (direct swap path).
            let min_out = 1u64;

            let ix = match dex {
                DexType::RaydiumAmmV4 => {
                    let meta = pool.raydium_meta.as_ref().context("Raydium meta")?;
                    let mut data = vec![9u8, 0, 0, 0, 0, 0, 0, 0]; // swapBaseIn disc
                    data.extend_from_slice(&current_amount.to_le_bytes());
                    data.extend_from_slice(&min_out.to_le_bytes());
                    Instruction {
                        program_id: hop.dex_program,
                        accounts: vec![
                            AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse().unwrap(), false),
                            AccountMeta::new(hop.pool, false),
                            AccountMeta::new_readonly(meta.authority, false),
                            AccountMeta::new(meta.open_orders, false),
                            AccountMeta::new(meta.target_orders, false),
                            AccountMeta::new(meta.vault_a, false),
                            AccountMeta::new(meta.vault_b, false),
                            AccountMeta::new_readonly(meta.market_program, false),
                            AccountMeta::new(meta.market_id, false),
                            AccountMeta::new(meta.market_bids, false),
                            AccountMeta::new(meta.market_asks, false),
                            AccountMeta::new(meta.market_event_queue, false),
                            AccountMeta::new(meta.market_base_vault, false),
                            AccountMeta::new(meta.market_quote_vault, false),
                            AccountMeta::new_readonly(meta.market_vault_signer, false),
                            AccountMeta::new(user_source, false),
                            AccountMeta::new(user_dest, false),
                            AccountMeta::new_readonly(self.payer.pubkey(), true),
                        ],
                        data,
                    }
                }
                DexType::PumpSwap => {
                    let mut data = vec![0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]; // sell disc
                    data.extend_from_slice(&current_amount.to_le_bytes());
                    data.extend_from_slice(&min_out.to_le_bytes());
                    let global_config: Pubkey = "ADyA8hdefbFUHRNUTC6V2JNsFp6WqXyKcWVr2AfjFLaT".parse().unwrap();
                    let fee_recipient: Pubkey = PUMPSWAP_PROTOCOL_FEE_RECIPIENT.parse().unwrap();
                    Instruction {
                        program_id: hop.dex_program,
                        accounts: vec![
                            AccountMeta::new(hop.pool, false),
                            AccountMeta::new(self.payer.pubkey(), true),
                            AccountMeta::new_readonly(global_config, false),
                            AccountMeta::new_readonly(pool.token_a, false),
                            AccountMeta::new_readonly(pool.token_b, false),
                            AccountMeta::new(user_source, false),
                            AccountMeta::new(user_dest, false),
                            AccountMeta::new(associated_token_address(&hop.pool, &pool.token_a), false),
                            AccountMeta::new(associated_token_address(&hop.pool, &pool.token_b), false),
                            AccountMeta::new(fee_recipient, false),
                            AccountMeta::new(associated_token_address(&fee_recipient, &pool.token_b), false),
                            AccountMeta::new_readonly(prog_in, false),
                            AccountMeta::new_readonly(prog_out, false),
                            AccountMeta::new_readonly(system_program::id(), false),
                            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID.parse().unwrap(), false),
                            AccountMeta::new_readonly("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1".parse::<Pubkey>().unwrap(), false),
                            AccountMeta::new_readonly(hop.dex_program, false),
                        ],
                        data,
                    }
                }
                DexType::MeteoraDlmm => {
                    let meta = pool.meteora_meta.as_ref().context("Meteora meta")?;
                    let disc: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8]; // swap
                    let mut data = disc.to_vec();
                    borsh::BorshSerialize::serialize(
                        &MeteoraSwapArgs { amount_in: current_amount, min_amount_out: min_out },
                        &mut data,
                    ).unwrap();

                    let event_authority: Pubkey = METEORA_EVENT_AUTHORITY.parse().unwrap();
                    let dlmm_program: Pubkey = METEORA_DLMM_PROGRAM_ID.parse().unwrap();

                    let mut accounts = vec![
                        AccountMeta::new(hop.pool, false),
                        AccountMeta::new_readonly(dlmm_program, false),
                        AccountMeta::new_readonly(self.payer.pubkey(), true),
                        AccountMeta::new(meta.token_vault_a, false),
                        AccountMeta::new(meta.token_vault_b, false),
                        AccountMeta::new(user_source, false),
                        AccountMeta::new(user_dest, false),
                        AccountMeta::new_readonly(prog_in, false),
                        AccountMeta::new_readonly(prog_out, false),
                        AccountMeta::new_readonly(event_authority, false),
                        AccountMeta::new_readonly(dlmm_program, false),
                    ];

                    // Add bin arrays from metadata (derive PDAs from index)
                    for (idx, _arr) in meta.bin_arrays.iter() {
                        let (bin_array_pda, _) = Pubkey::find_program_address(
                            &[b"bin_array", hop.pool.as_ref(), &idx.to_le_bytes()],
                            &dlmm_program,
                        );
                        accounts.push(AccountMeta::new(bin_array_pda, false));
                    }

                    Instruction {
                        program_id: hop.dex_program,
                        accounts,
                        data,
                    }
                }
                DexType::OrcaWhirlpool => {
                    let meta = pool.orca_meta.as_ref().context("Orca meta for direct swap")?;
                    if meta.tick_spacing == 0 {
                        bail!("Orca metadata incomplete for direct swap pool {}", hop.pool);
                    }
                    let token_owner_a = associated_token_address_with_program(&self.payer.pubkey(), &pool.token_a, &prog_in);
                    let token_owner_b = associated_token_address_with_program(&self.payer.pubkey(), &pool.token_b, &prog_out);
                    let oracle = derive_orca_oracle(hop.pool)?;
                    let spl_token: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
                    let memo: Pubkey = MEMO_PROGRAM_ID.parse().unwrap();

                    // Derive 3 contiguous tick arrays from current tick (simple, no quoting).
                    // Direction: a_to_b scans lower ticks, b_to_a scans higher.
                    let offset = meta.tick_spacing as i32 * ORCA_TICK_ARRAY_SIZE;
                    let dir = if a_to_b { -1i32 } else { 1i32 };
                    let ta0_start = get_orca_tick_array_start_tick_index(meta.tick_current_index, meta.tick_spacing);
                    let ta1_start = ta0_start + dir * offset;
                    let ta2_start = ta1_start + dir * offset;
                    let ta0 = derive_orca_tick_array(hop.pool, ta0_start)?;
                    let ta1 = derive_orca_tick_array(hop.pool, ta1_start)?;
                    let ta2 = derive_orca_tick_array(hop.pool, ta2_start)?;

                    // No supplemental tick arrays for direct swap (keep it simple).
                    let data = build_swap_data(
                        DexType::OrcaWhirlpool,
                        current_amount,
                        min_out,
                        a_to_b,
                        0, // no supplemental
                    )?;

                    let accounts = vec![
                        AccountMeta::new_readonly(spl_token, false),  // token_program_a
                        AccountMeta::new_readonly(spl_token, false),  // token_program_b
                        AccountMeta::new_readonly(memo, false),        // memo_program
                        AccountMeta::new_readonly(self.payer.pubkey(), true), // token_authority
                        AccountMeta::new(hop.pool, false),             // whirlpool
                        AccountMeta::new_readonly(pool.token_a, false), // token_mint_a
                        AccountMeta::new_readonly(pool.token_b, false), // token_mint_b
                        AccountMeta::new(token_owner_a, false),        // token_owner_account_a
                        AccountMeta::new(meta.token_vault_a, false),   // token_vault_a
                        AccountMeta::new(token_owner_b, false),        // token_owner_account_b
                        AccountMeta::new(meta.token_vault_b, false),   // token_vault_b
                        AccountMeta::new(ta0, false),                  // tick_array_0
                        AccountMeta::new(ta1, false),                  // tick_array_1
                        AccountMeta::new(ta2, false),                  // tick_array_2
                        AccountMeta::new(oracle, false),               // oracle
                    ];

                    Instruction {
                        program_id: hop.dex_program, // whirlpool program
                        accounts,
                        data,
                    }
                }
                _ => bail!("Direct swap not supported for {:?}, use helios-arb CPI", dex),
            };

            swap_ixs.push(ix);
            current_amount = hop.amount_out;
        }

        // Wrap in flash loan
        let ata_count = instructions.len();
        let wrapped = flash_loans::build_wrapped_instructions_direct(
            flash_provider, &self.payer, borrow_mint, route.borrow_amount,
            user_borrow_ata, swap_ixs, ata_count,
        )?;
        instructions.extend(wrapped);

        if let Some(tip) = tip_ix {
            instructions.push(tip);
        }

        let message = v0::Message::try_compile(
            &self.payer.pubkey(), &instructions, alt_accounts, recent_blockhash,
        )?;
        Ok(VersionedTransaction::try_new(VersionedMessage::V0(message), &[&self.payer])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ata_derivation_is_deterministic() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        assert_eq!(
            associated_token_address(&owner, &mint),
            associated_token_address(&owner, &mint)
        );
    }

    #[test]
    fn orca_swap_v2_serializes_supplemental_tick_arrays() {
        let data = build_swap_data(DexType::OrcaWhirlpool, 1, 2, true, 2).unwrap();
        assert_eq!(&data[..8], &[43, 4, 237, 11, 26, 201, 30, 98]);
        assert_eq!(data.last().copied(), Some(2));
    }

    #[allow(dead_code)]
    fn _removed_duplicate(
        &self,
        route: &RouteParams,
        flash_provider: FlashProvider,
        recent_blockhash: Hash,
        alt_accounts: &[AddressLookupTableAccount],
        pool_cache: &PoolStateCache,
        tip_ix: Option<Instruction>,
    ) -> Result<VersionedTransaction> {
        let token_programs = self.token_programs_for_route(route);
        let borrow_mint = route.hops.first()
            .map(|h| h.token_in)
            .context("route has no hops")?;
        let spl: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
        let borrow_tp = token_programs.get(&borrow_mint).copied().unwrap_or(spl);
        let user_borrow_ata = associated_token_address_with_program(
            &self.payer.pubkey(), &borrow_mint, &borrow_tp,
        );

        let mut instructions = Vec::new();

        // Compute budget: lower CU limit since no on-chain program overhead
        let cu_limit = 250_000u32; // Direct swaps: ~120-200K CUs typical
        let priority_floor: u64 = std::env::var("PRIORITY_FEE_FLOOR")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000);
        let priority_ceil: u64 = std::env::var("PRIORITY_FEE_CEIL")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(5_000);
        let priority_pct: f64 = std::env::var("PRIORITY_FEE_PCT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(10.0);
        let priority_budget = (route.net_profit.max(0) as f64 * priority_pct / 100.0) as u64;
        let dynamic_fee = if cu_limit > 0 {
            priority_budget.saturating_mul(1_000_000) / cu_limit as u64
        } else {
            priority_floor
        };
        let priority_fee = dynamic_fee.max(priority_floor).min(priority_ceil);
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
        instructions.push(solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee));

        // ATA setup for intermediate tokens
        instructions.extend(self.build_ata_setup_instructions(route, &token_programs));

        // Build individual swap instructions for each hop
        let mut swap_ixs = Vec::new();
        let mut current_amount = route.borrow_amount;

        for (hop_idx, hop) in route.hops.iter().enumerate() {
            let dex = DexType::from_program_id(&hop.dex_program);
            let pool = pool_cache.get(&hop.pool).context("pool not in cache")?;
            let a_to_b = hop.token_in == pool.token_a;
            let prog_in = token_programs.get(&hop.token_in).copied().unwrap_or(spl);
            let prog_out = token_programs.get(&hop.token_out).copied().unwrap_or(spl);
            let user_source = associated_token_address_with_program(&self.payer.pubkey(), &hop.token_in, &prog_in);
            let user_dest = associated_token_address_with_program(&self.payer.pubkey(), &hop.token_out, &prog_out);

            // min_out=1: flash loan repay is the safety net (slow build path).
            let min_out = 1u64;

            let ix = match dex {
                DexType::RaydiumAmmV4 => {
                    let meta = pool.raydium_meta.as_ref().context("Raydium meta")?;
                    // Raydium swap_base_in: amount_in, min_amount_out
                    let disc: [u8; 8] = [0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]; // swapBaseIn = 9
                    let mut data = disc.to_vec();
                    data.extend_from_slice(&current_amount.to_le_bytes());
                    data.extend_from_slice(&min_out.to_le_bytes());

                    Instruction {
                        program_id: hop.dex_program,
                        accounts: vec![
                            AccountMeta::new_readonly(TOKEN_PROGRAM_ID.parse().unwrap(), false),
                            AccountMeta::new(hop.pool, false),
                            AccountMeta::new_readonly(meta.authority, false),
                            AccountMeta::new(meta.open_orders, false),
                            AccountMeta::new(meta.target_orders, false),
                            AccountMeta::new(meta.vault_a, false),
                            AccountMeta::new(meta.vault_b, false),
                            AccountMeta::new_readonly(meta.market_program, false),
                            AccountMeta::new(meta.market_id, false),
                            AccountMeta::new(meta.market_bids, false),
                            AccountMeta::new(meta.market_asks, false),
                            AccountMeta::new(meta.market_event_queue, false),
                            AccountMeta::new(meta.market_base_vault, false),
                            AccountMeta::new(meta.market_quote_vault, false),
                            AccountMeta::new_readonly(meta.market_vault_signer, false),
                            AccountMeta::new(user_source, false),
                            AccountMeta::new(user_dest, false),
                            AccountMeta::new_readonly(self.payer.pubkey(), true),
                        ],
                        data,
                    }
                }
                DexType::PumpSwap => {
                    // PumpSwap swap (sell if WSOL is base, buy otherwise)
                    let wsol: Pubkey = WSOL_MINT.parse().unwrap();
                    let is_sell = pool.token_a == wsol || (a_to_b && pool.token_b != wsol);

                    let disc = if is_sell {
                        [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad] // sell
                    } else {
                        [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea] // buy
                    };
                    let mut data = disc.to_vec();
                    data.extend_from_slice(&current_amount.to_le_bytes());
                    data.extend_from_slice(&min_out.to_le_bytes());

                    let global_config: Pubkey = "ADyA8hdefbFUHRNUTC6V2JNsFp6WqXyKcWVr2AfjFLaT".parse().unwrap();
                    let fee_recipient: Pubkey = PUMPSWAP_PROTOCOL_FEE_RECIPIENT.parse().unwrap();

                    Instruction {
                        program_id: hop.dex_program,
                        accounts: vec![
                            AccountMeta::new(hop.pool, false),
                            AccountMeta::new(self.payer.pubkey(), true),
                            AccountMeta::new_readonly(global_config, false),
                            AccountMeta::new_readonly(pool.token_a, false), // base_mint
                            AccountMeta::new_readonly(pool.token_b, false), // quote_mint
                            AccountMeta::new(user_source, false),
                            AccountMeta::new(user_dest, false),
                            // Pool vaults from metadata or derived
                            AccountMeta::new(
                                pool.raydium_meta.as_ref().map(|m| m.vault_a).unwrap_or(
                                    associated_token_address(&hop.pool, &pool.token_a)
                                ), false),
                            AccountMeta::new(
                                pool.raydium_meta.as_ref().map(|m| m.vault_b).unwrap_or(
                                    associated_token_address(&hop.pool, &pool.token_b)
                                ), false),
                            AccountMeta::new(fee_recipient, false),
                            AccountMeta::new(
                                associated_token_address(&fee_recipient, &pool.token_b),
                                false,
                            ),
                            AccountMeta::new_readonly(prog_in, false),
                            AccountMeta::new_readonly(prog_out, false),
                            AccountMeta::new_readonly(system_program::id(), false),
                            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID.parse().unwrap(), false),
                            AccountMeta::new_readonly(
                                "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1".parse().unwrap(), false), // event authority
                            AccountMeta::new_readonly(hop.dex_program, false),
                        ],
                        data,
                    }
                }
                DexType::MeteoraDlmm => {
                    let meta = pool.meteora_meta.as_ref().context("Meteora meta")?;
                    let disc: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8]; // swap
                    let args = MeteoraSwapArgs { amount_in: current_amount, min_amount_out: min_out };
                    let mut data = disc.to_vec();
                    args.serialize(&mut data).unwrap();

                    let event_authority: Pubkey = METEORA_EVENT_AUTHORITY.parse().unwrap();

                    let mut accounts = vec![
                        AccountMeta::new(hop.pool, false),
                        AccountMeta::new_readonly(
                            "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo".parse::<Pubkey>().unwrap(), false), // lb_pair but is actually the program (account for swap)
                        AccountMeta::new_readonly(self.payer.pubkey(), true),
                        AccountMeta::new(meta.token_vault_a, false),
                        AccountMeta::new(meta.token_vault_b, false),
                        AccountMeta::new(user_source, false),
                        AccountMeta::new(user_dest, false),
                        AccountMeta::new_readonly(prog_in, false),
                        AccountMeta::new_readonly(prog_out, false),
                        AccountMeta::new_readonly(event_authority, false),
                        AccountMeta::new_readonly(hop.dex_program, false),
                    ];

                    // Add bin arrays
                    for (_idx, arr) in meta.bin_arrays.iter() {
                        accounts.push(AccountMeta::new(arr.key, false));
                    }

                    Instruction {
                        program_id: hop.dex_program,
                        accounts,
                        data,
                    }
                }
                _ => {
                    // Fallback: use helios-arb CPI for unsupported DEX types
                    bail!("Direct swap not supported for {:?}, use helios-arb", dex);
                }
            };

            swap_ixs.push(ix);
            current_amount = hop.amount_out;
        }

        // Wrap swap IXs in flash loan (borrow → swaps → repay)
        let ata_count = instructions.len();
        let wrapped = flash_loans::build_wrapped_instructions_direct(
            flash_provider,
            &self.payer,
            borrow_mint,
            route.borrow_amount,
            user_borrow_ata,
            swap_ixs,
            ata_count,
        )?;
        instructions.extend(wrapped);

        // Jito tip as last IX
        if let Some(tip) = tip_ix {
            instructions.push(tip);
        }

        let message = v0::Message::try_compile(
            &self.payer.pubkey(),
            &instructions,
            alt_accounts,
            recent_blockhash,
        )?;
        Ok(VersionedTransaction::try_new(
            VersionedMessage::V0(message),
            &[&self.payer],
        )?)
    }

    #[test]
    fn orca_tick_arrays_follow_official_order() {
        let whirlpool = Pubkey::new_unique();
        let meta = market_engine::pool_state::OrcaMetadata {
            token_vault_a: Pubkey::new_unique(),
            token_vault_b: Pubkey::new_unique(),
            liquidity: 1_000_000,
            sqrt_price: (1u128 << 64),
            tick_spacing: 128,
            tick_current_index: -62_080,
            tick_arrays: std::sync::Arc::new(std::collections::HashMap::new()),
        };
        let set = derive_orca_tick_arrays(whirlpool, &meta, 1_000_000, 30, true).unwrap();
        assert_eq!(set.primary.len(), 3);
        assert_eq!(set.supplemental.len(), 2);
        assert_ne!(set.primary[0], set.primary[1]);
        assert_ne!(set.primary[1], set.primary[2]);
    }
}
