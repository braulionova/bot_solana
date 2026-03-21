use anyhow::{bail, Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    sysvar,
};

use market_engine::types::FlashProvider;

const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";
const PORT_PROGRAM_ID: &str = "Port7uDYB3wk6GJAw4KT1WpTeMtSu9bTcChBHkX2LfR";
const SOLEND_PROGRAM_ID: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
const KAMINO_PROGRAM_ID: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
const PORT_FLASH_RECEIVER_PROGRAM_ID: &str = "Hwrv5nizU3MWRbmDswt1Z2uwbGjhoJbz88chJnsyHDVi";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedMint {
    Sol,
    Usdc,
}

#[derive(Debug, Clone)]
struct SolendConfig {
    reserve: Pubkey,
    liquidity_supply: Pubkey,
    fee_receiver: Pubkey,
    host_fee_receiver: Option<Pubkey>,
    lending_market: Pubkey,
}

#[derive(Debug, Clone)]
struct MarginfiConfig {
    group: Pubkey,
    marginfi_account: Pubkey,
    bank: Pubkey,
    liquidity_vault: Pubkey,
    oracle: Pubkey,
}

#[derive(Debug, Clone)]
struct KaminoConfig {
    reserve: Pubkey,
    liquidity_supply: Pubkey,
    fee_vault: Pubkey,
    lending_market: Pubkey,
}

#[derive(Debug, Clone)]
struct PortConfig {
    reserve: Pubkey,
    liquidity_supply: Pubkey,
    fee_receiver: Pubkey,
    host_fee_receiver: Option<Pubkey>,
    lending_market: Pubkey,
    receiver_program: Pubkey,
}

#[derive(Debug, Clone, BorshDeserialize)]
struct ExecuteFlashArbArgs {
    borrow_amount: u64,
    min_profit: u64,
    hop_account_counts: Vec<u8>,
    hop_data: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, BorshSerialize)]
enum PortFlashReceiverInstruction {
    ReceiveFlashLoan {
        repay_amount: u64,
    },
    CarryPlan {
        min_profit: u64,
        hop_account_counts: Vec<u8>,
        hop_data: Vec<Vec<u8>>,
    },
}

pub fn fallback_candidates(preferred: FlashProvider) -> Vec<FlashProvider> {
    let ordered = [
        preferred,
        FlashProvider::MarginFi,
        FlashProvider::Solend,
        FlashProvider::Kamino,
        FlashProvider::PortFinance,
    ];
    let mut providers = Vec::with_capacity(ordered.len());
    for provider in ordered {
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    providers
}

/// Build flash loan wrapper instructions.
///
/// `preceding_ix_count` is the number of instructions that appear BEFORE
/// these flash loan instructions in the final transaction (e.g. ATA
/// CreateIdempotent instructions).  Flash loan programs like MarginFi,
/// Solend, and Kamino embed absolute instruction indices that reference
/// other instructions in the same transaction, so we must offset them
/// correctly.
pub fn build_wrapped_instructions(
    provider: FlashProvider,
    payer: &Keypair,
    borrow_mint: Pubkey,
    borrow_amount: u64,
    user_token_account: Pubkey,
    helios_ix: Instruction,
    preceding_ix_count: usize,
) -> Result<Vec<Instruction>> {
    match provider {
        FlashProvider::MarginFi => build_marginfi_wrapped_instructions(
            payer,
            borrow_mint,
            borrow_amount,
            user_token_account,
            helios_ix,
            preceding_ix_count,
        ),
        FlashProvider::Solend => build_solend_wrapped_instructions(
            payer,
            borrow_mint,
            borrow_amount,
            user_token_account,
            helios_ix,
            preceding_ix_count,
        ),
        FlashProvider::Kamino => build_kamino_wrapped_instructions(
            payer,
            borrow_mint,
            borrow_amount,
            user_token_account,
            helios_ix,
            preceding_ix_count,
        ),
        FlashProvider::PortFinance => build_port_wrapped_instructions(
            payer,
            borrow_mint,
            borrow_amount,
            user_token_account,
            helios_ix,
        ),
    }
}

/// Build flash loan wrapper for DIRECT swap instructions (no helios-arb CPI).
/// Takes a Vec<Instruction> of swap IXs instead of a single helios_ix.
///
/// Layout: [begin, borrow, swap1, swap2, ..., swapN, end]
pub fn build_wrapped_instructions_direct(
    provider: FlashProvider,
    payer: &Keypair,
    borrow_mint: Pubkey,
    borrow_amount: u64,
    user_token_account: Pubkey,
    swap_ixs: Vec<Instruction>,
    preceding_ix_count: usize,
) -> Result<Vec<Instruction>> {
    match provider {
        FlashProvider::MarginFi => {
            let cfg = resolve_marginfi_config(borrow_mint)?;
            let program_id: Pubkey = MARGINFI_PROGRAM_ID.parse().unwrap();
            let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
            let (liquidity_vault_authority, _) =
                Pubkey::find_program_address(&[b"liquidity_vault_auth", cfg.bank.as_ref()], &program_id);

            // Layout: [begin, borrow, swap0..swapN, end]
            // end_index = preceding + 2 + swap_count
            let end_index = (preceding_ix_count + 2 + swap_ixs.len()) as u64;

            let begin_ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new(cfg.marginfi_account, false),
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new_readonly(sysvar::instructions::id(), false),
                ],
                data: marginfi_start_flashloan_data(end_index),
            };

            let borrow_ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new_readonly(cfg.group, false),
                    AccountMeta::new(cfg.marginfi_account, false),
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new(cfg.bank, false),
                    AccountMeta::new(user_token_account, false),
                    AccountMeta::new_readonly(liquidity_vault_authority, false),
                    AccountMeta::new(cfg.liquidity_vault, false),
                    AccountMeta::new_readonly(token_program, false),
                ],
                data: marginfi_borrow_data(borrow_amount),
            };

            let end_ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new(cfg.marginfi_account, false),
                    AccountMeta::new_readonly(payer.pubkey(), true),
                    AccountMeta::new_readonly(sysvar::instructions::id(), false),
                ],
                data: marginfi_end_flashloan_data(),
            };

            let mut result = vec![begin_ix, borrow_ix];
            result.extend(swap_ixs);
            result.push(end_ix);
            Ok(result)
        }
        // For other providers, combine swap IXs into a single "helios_ix" equivalent
        // by building them sequentially. Only MarginFi supports multi-IX flash loans natively.
        FlashProvider::Kamino => {
            // Kamino flash loan: special flash_borrow + flash_repay instructions
            // Layout from on-chain TX: 6 accounts each, simple disc + amount
            let cfg = resolve_kamino_config(borrow_mint)?;
            let program_id: Pubkey = KAMINO_PROGRAM_ID.parse().unwrap();

            // Flash borrow discriminator (from on-chain: 02da8aeb4fc91966)
            let mut borrow_data = vec![2, 218, 138, 235, 79, 201, 25, 102];
            borrow_data.extend_from_slice(&borrow_amount.to_le_bytes());

            // 6 accounts from real on-chain TX:
            // [reserve, lending_market, liquidity_supply, fee_vault, user_dest_ata, sysvar_instructions]
            let borrow_ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new(cfg.reserve, false),
                    AccountMeta::new_readonly(cfg.lending_market, false),
                    AccountMeta::new(cfg.liquidity_supply, false),
                    AccountMeta::new(cfg.fee_vault, false),
                    AccountMeta::new(user_token_account, false),
                    AccountMeta::new_readonly(sysvar::instructions::id(), false),
                ],
                data: borrow_data,
            };

            // Flash repay: same disc pattern, amount + borrow_ix_index
            let borrow_ix_index = preceding_ix_count as u8;
            let mut repay_data = vec![185, 117, 0, 203, 96, 245, 180, 186];
            repay_data.extend_from_slice(&borrow_amount.to_le_bytes());
            repay_data.push(borrow_ix_index);

            let repay_ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new(cfg.reserve, false),
                    AccountMeta::new_readonly(cfg.lending_market, false),
                    AccountMeta::new(cfg.liquidity_supply, false),
                    AccountMeta::new(cfg.fee_vault, false),
                    AccountMeta::new(user_token_account, false),
                    AccountMeta::new_readonly(sysvar::instructions::id(), false),
                ],
                data: repay_data,
            };

            let mut ixs = vec![borrow_ix];
            ixs.extend(swap_ixs);
            ixs.push(repay_ix);
            Ok(ixs)
        }
        _ => {
            bail!("Direct swap mode not supported for {:?}", provider);
        }
    }
}

fn build_marginfi_wrapped_instructions(
    payer: &Keypair,
    borrow_mint: Pubkey,
    borrow_amount: u64,
    user_token_account: Pubkey,
    helios_ix: Instruction,
    preceding_ix_count: usize,
) -> Result<Vec<Instruction>> {
    let cfg = resolve_marginfi_config(borrow_mint)?;
    let program_id: Pubkey = MARGINFI_PROGRAM_ID.parse().unwrap();
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
    let (liquidity_vault_authority, _) =
        Pubkey::find_program_address(&[b"liquidity_vault_auth", cfg.bank.as_ref()], &program_id);

    // MarginFi layout: [begin, borrow, helios, end] = 4 instructions.
    // end_flashloan is at position 3 relative to begin (0-indexed within this group).
    // But the absolute TX index = preceding_ix_count + 3.
    let end_index = (preceding_ix_count + 3) as u64;
    let begin_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(cfg.marginfi_account, false),
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
        ],
        data: marginfi_start_flashloan_data(end_index),
    };

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(cfg.group, false),
            AccountMeta::new(cfg.marginfi_account, false),
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new(cfg.bank, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new_readonly(liquidity_vault_authority, false),
            AccountMeta::new(cfg.liquidity_vault, false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: marginfi_borrow_data(borrow_amount),
    };

    let projected_banks = marginfi_projected_active_banks(cfg.bank, cfg.oracle)?;
    let end_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(cfg.marginfi_account, false),
            AccountMeta::new_readonly(payer.pubkey(), true),
        ]
        .into_iter()
        .chain(
            compose_marginfi_health_accounts(&projected_banks)
                .into_iter()
                .map(|pubkey| AccountMeta::new_readonly(pubkey, false)),
        )
        .collect(),
        data: marginfi_end_flashloan_data(),
    };

    Ok(vec![begin_ix, borrow_ix, helios_ix, end_ix])
}

fn build_port_wrapped_instructions(
    payer: &Keypair,
    borrow_mint: Pubkey,
    borrow_amount: u64,
    user_token_account: Pubkey,
    helios_ix: Instruction,
) -> Result<Vec<Instruction>> {
    let cfg = resolve_port_config(borrow_mint)?;
    let port_program: Pubkey = PORT_PROGRAM_ID.parse().unwrap();
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
    let host_fee_receiver = cfg.host_fee_receiver.unwrap_or(user_token_account);
    let (market_authority, _) =
        Pubkey::find_program_address(&[&cfg.lending_market.to_bytes()[..32]], &port_program);
    let helios_plan = decode_helios_flash_arb_args(&helios_ix)?;

    let mut flash_data = Vec::with_capacity(9);
    flash_data.push(13);
    flash_data.extend_from_slice(&borrow_amount.to_le_bytes());

    let mut callback_accounts = vec![
        AccountMeta::new_readonly(payer.pubkey(), true),
        AccountMeta::new_readonly(helios_ix.program_id, false),
        AccountMeta::new_readonly(
            helios_ix
                .accounts
                .get(2)
                .context("helios config account missing")?
                .pubkey,
            false,
        ),
        AccountMeta::new_readonly(sysvar::instructions::id(), false),
    ];
    callback_accounts.extend(helios_ix.accounts.iter().skip(4).cloned());

    let flash_ix = Instruction {
        program_id: port_program,
        accounts: vec![
            AccountMeta::new(cfg.liquidity_supply, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(cfg.reserve, false),
            AccountMeta::new(cfg.fee_receiver, false),
            AccountMeta::new(host_fee_receiver, false),
            AccountMeta::new_readonly(cfg.lending_market, false),
            AccountMeta::new_readonly(market_authority, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(cfg.receiver_program, false),
        ]
        .into_iter()
        .chain(callback_accounts)
        .collect(),
        data: flash_data,
    };

    let carrier_ix = Instruction {
        program_id: cfg.receiver_program,
        accounts: Vec::new(),
        data: borsh::to_vec(&PortFlashReceiverInstruction::CarryPlan {
            min_profit: helios_plan.min_profit,
            hop_account_counts: helios_plan.hop_account_counts,
            hop_data: helios_plan.hop_data,
        })?,
    };

    Ok(vec![flash_ix, carrier_ix])
}

fn build_solend_wrapped_instructions(
    payer: &Keypair,
    borrow_mint: Pubkey,
    borrow_amount: u64,
    user_token_account: Pubkey,
    helios_ix: Instruction,
    preceding_ix_count: usize,
) -> Result<Vec<Instruction>> {
    let cfg = resolve_solend_config(borrow_mint)?;
    let program_id: Pubkey = SOLEND_PROGRAM_ID.parse().unwrap();
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
    let host_fee_receiver = cfg.host_fee_receiver.unwrap_or(user_token_account);
    let (market_authority, _) =
        Pubkey::find_program_address(&[&cfg.lending_market.to_bytes()[..32]], &program_id);

    let mut borrow_data = Vec::with_capacity(9);
    borrow_data.push(19);
    borrow_data.extend_from_slice(&borrow_amount.to_le_bytes());

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(cfg.liquidity_supply, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(cfg.reserve, false),
            AccountMeta::new_readonly(cfg.lending_market, false),
            AccountMeta::new_readonly(market_authority, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: borrow_data,
    };

    // Solend repay references the absolute TX index of the borrow instruction.
    // Borrow is the first instruction in this group → absolute index = preceding_ix_count.
    let borrow_ix_index = preceding_ix_count as u8;
    let mut repay_data = Vec::with_capacity(10);
    repay_data.push(20);
    repay_data.extend_from_slice(&borrow_amount.to_le_bytes());
    repay_data.push(borrow_ix_index);

    let repay_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(cfg.liquidity_supply, false),
            AccountMeta::new(cfg.fee_receiver, false),
            AccountMeta::new(host_fee_receiver, false),
            AccountMeta::new(cfg.reserve, false),
            AccountMeta::new_readonly(cfg.lending_market, false),
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: repay_data,
    };

    Ok(vec![borrow_ix, helios_ix, repay_ix])
}

fn build_kamino_wrapped_instructions(
    payer: &Keypair,
    borrow_mint: Pubkey,
    borrow_amount: u64,
    user_token_account: Pubkey,
    helios_ix: Instruction,
    preceding_ix_count: usize,
) -> Result<Vec<Instruction>> {
    let cfg = resolve_kamino_config(borrow_mint)?;
    let program_id: Pubkey = KAMINO_PROGRAM_ID.parse().unwrap();
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().unwrap();
    let (market_authority, _) =
        Pubkey::find_program_address(&[b"lma", cfg.lending_market.as_ref()], &program_id);

    let mut borrow_data = vec![135, 231, 52, 167, 7, 52, 212, 193];
    borrow_data.extend_from_slice(&borrow_amount.to_le_bytes());

    let borrow_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new_readonly(market_authority, false),
            AccountMeta::new_readonly(cfg.lending_market, false),
            AccountMeta::new(cfg.reserve, false),
            AccountMeta::new_readonly(borrow_mint, false),
            AccountMeta::new(cfg.liquidity_supply, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(cfg.fee_vault, false),
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: borrow_data,
    };

    // Kamino repay references the absolute TX index of the borrow instruction.
    let borrow_ix_index = preceding_ix_count as u8;
    let mut repay_data = vec![185, 117, 0, 203, 96, 245, 180, 186];
    repay_data.extend_from_slice(&borrow_amount.to_le_bytes());
    repay_data.push(borrow_ix_index);

    let repay_ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new_readonly(market_authority, false),
            AccountMeta::new_readonly(cfg.lending_market, false),
            AccountMeta::new(cfg.reserve, false),
            AccountMeta::new_readonly(borrow_mint, false),
            AccountMeta::new(cfg.liquidity_supply, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(cfg.fee_vault, false),
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data: repay_data,
    };

    Ok(vec![borrow_ix, helios_ix, repay_ix])
}

fn resolve_marginfi_config(mint: Pubkey) -> Result<MarginfiConfig> {
    let suffix = mint_env_suffix(mint)?;
    Ok(MarginfiConfig {
        group: parse_env_pubkey_aliases(&["MARGINFI_GROUP", "MARGINFI_MAIN_GROUP"])?,
        marginfi_account: parse_env_pubkey("MARGINFI_ACCOUNT")?,
        bank: parse_env_pubkey(&format!("MARGINFI_{}_BANK", suffix))?,
        liquidity_vault: parse_env_pubkey(&format!("MARGINFI_{}_LIQUIDITY_VAULT", suffix))?,
        oracle: parse_env_pubkey(&format!("MARGINFI_{}_ORACLE", suffix))?,
    })
}

fn resolve_solend_config(mint: Pubkey) -> Result<SolendConfig> {
    let suffix = mint_env_suffix(mint)?;
    Ok(SolendConfig {
        reserve: parse_env_pubkey(&format!("SOLEND_{}_RESERVE", suffix))?,
        liquidity_supply: parse_env_pubkey(&format!("SOLEND_{}_LIQUIDITY_SUPPLY", suffix))?,
        fee_receiver: parse_env_pubkey(&format!("SOLEND_{}_FEE_RECEIVER", suffix))?,
        host_fee_receiver: parse_optional_env_pubkey(&format!(
            "SOLEND_{}_HOST_FEE_RECEIVER",
            suffix
        ))?,
        lending_market: parse_env_pubkey(&format!("SOLEND_{}_LENDING_MARKET", suffix))?,
    })
}

fn resolve_kamino_config(mint: Pubkey) -> Result<KaminoConfig> {
    let suffix = mint_env_suffix(mint)?;
    Ok(KaminoConfig {
        reserve: parse_env_pubkey(&format!("KAMINO_{}_RESERVE", suffix))?,
        liquidity_supply: parse_env_pubkey_aliases(&[
            &format!("KAMINO_{}_LIQUIDITY_SUPPLY", suffix),
            &format!("KAMINO_{}_LIQUIDITY_VAULT", suffix),
        ])?,
        fee_vault: parse_env_pubkey(&format!("KAMINO_{}_FEE_VAULT", suffix))?,
        lending_market: parse_env_pubkey_aliases(&[
            &format!("KAMINO_{}_LENDING_MARKET", suffix),
            "KAMINO_MAIN_MARKET",
        ])?,
    })
}

fn resolve_port_config(mint: Pubkey) -> Result<PortConfig> {
    let suffix = mint_env_suffix(mint)?;
    Ok(PortConfig {
        reserve: parse_env_pubkey(&format!("PORT_{}_RESERVE", suffix))?,
        liquidity_supply: parse_env_pubkey(&format!("PORT_{}_LIQUIDITY_SUPPLY", suffix))?,
        fee_receiver: parse_env_pubkey(&format!("PORT_{}_FEE_RECEIVER", suffix))?,
        host_fee_receiver: parse_optional_env_pubkey(&format!(
            "PORT_{}_HOST_FEE_RECEIVER",
            suffix
        ))?,
        lending_market: parse_env_pubkey_aliases(&[
            &format!("PORT_{}_LENDING_MARKET", suffix),
            "PORT_MAIN_MARKET",
        ])?,
        receiver_program: parse_env_pubkey_aliases(&[
            "PORT_FLASH_RECEIVER_PROGRAM",
            "PORT_RECEIVER_PROGRAM",
        ])
        .or_else(|_| {
            PORT_FLASH_RECEIVER_PROGRAM_ID
                .parse::<Pubkey>()
                .context("invalid built-in Port flash receiver program id")
        })?,
    })
}

fn mint_env_suffix(mint: Pubkey) -> Result<&'static str> {
    match classify_mint(mint)? {
        SupportedMint::Sol => Ok("SOL"),
        SupportedMint::Usdc => Ok("USDC"),
    }
}

fn classify_mint(mint: Pubkey) -> Result<SupportedMint> {
    match mint.to_string().as_str() {
        SOL_MINT => Ok(SupportedMint::Sol),
        USDC_MINT => Ok(SupportedMint::Usdc),
        _ => bail!("unsupported flash-loan mint: {}", mint),
    }
}

fn parse_env_pubkey(key: &str) -> Result<Pubkey> {
    let raw = lookup_config_value(key).with_context(|| format!("missing env var {}", key))?;
    raw.parse::<Pubkey>()
        .with_context(|| format!("invalid pubkey in {}", key))
}

fn parse_env_pubkey_aliases(keys: &[&str]) -> Result<Pubkey> {
    for key in keys {
        if let Some(raw) = lookup_config_value(key) {
            return raw
                .parse::<Pubkey>()
                .with_context(|| format!("invalid pubkey in {}", key));
        }
    }
    bail!("missing env var {}", keys.join(" or "))
}

fn parse_optional_env_pubkey(key: &str) -> Result<Option<Pubkey>> {
    lookup_config_value(key)
        .map(|raw| {
            raw.parse::<Pubkey>()
                .with_context(|| format!("invalid pubkey in {}", key))
        })
        .transpose()
}

fn parse_env_pubkeys_csv(key: &str) -> Result<Vec<Pubkey>> {
    let Some(raw) = lookup_config_value(key) else {
        return Ok(Vec::new());
    };

    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<Pubkey>()
                .with_context(|| format!("invalid pubkey in {}", key))
        })
        .collect()
}

fn marginfi_start_flashloan_data(end_index: u64) -> Vec<u8> {
    let mut data = vec![14, 131, 33, 220, 81, 186, 180, 107];
    data.extend_from_slice(&end_index.to_le_bytes());
    data
}

fn marginfi_borrow_data(amount: u64) -> Vec<u8> {
    let mut data = vec![4, 126, 116, 53, 48, 5, 212, 31];
    data.extend_from_slice(&amount.to_le_bytes());
    data
}

fn marginfi_end_flashloan_data() -> Vec<u8> {
    vec![105, 124, 201, 106, 153, 2, 8, 156]
}

fn marginfi_projected_active_banks(
    borrow_bank: Pubkey,
    borrow_oracle: Pubkey,
) -> Result<Vec<MarginfiHealthBank>> {
    let mut banks = parse_marginfi_active_banks()?;

    if !banks.iter().any(|bank| bank.bank == borrow_bank) {
        banks.push(MarginfiHealthBank {
            bank: borrow_bank,
            oracle: borrow_oracle,
        });
    }

    Ok(banks)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MarginfiHealthBank {
    bank: Pubkey,
    oracle: Pubkey,
}

fn parse_marginfi_active_banks() -> Result<Vec<MarginfiHealthBank>> {
    let banks = parse_env_pubkeys_csv("MARGINFI_ACTIVE_BANKS")?;
    let mut entries = Vec::with_capacity(banks.len());
    for bank in banks {
        entries.push(MarginfiHealthBank {
            bank,
            oracle: resolve_marginfi_oracle(bank)?,
        });
    }
    Ok(entries)
}

fn resolve_marginfi_oracle(bank: Pubkey) -> Result<Pubkey> {
    let bank_str = bank.to_string();
    let sol_bank = parse_env_pubkey("MARGINFI_SOL_BANK")?;
    if bank == sol_bank {
        return parse_env_pubkey("MARGINFI_SOL_ORACLE");
    }

    let usdc_bank = parse_env_pubkey("MARGINFI_USDC_BANK")?;
    if bank == usdc_bank {
        return parse_env_pubkey("MARGINFI_USDC_ORACLE");
    }

    bail!("missing oracle mapping for MarginFi bank {}", bank_str)
}

fn compose_marginfi_health_accounts(banks: &[MarginfiHealthBank]) -> Vec<Pubkey> {
    let mut groups: Vec<[Pubkey; 2]> = banks.iter().map(|bank| [bank.bank, bank.oracle]).collect();
    groups.sort_by(|a, b| b[0].to_bytes().cmp(&a[0].to_bytes()));
    groups.into_iter().flatten().collect()
}

fn lookup_config_value(key: &str) -> Option<String> {
    if let Ok(value) = std::env::var(key) {
        if !value.trim().is_empty() {
            return Some(value);
        }
    }

    for path in [
        std::env::var("FLASH_ACCOUNT_ENV_FILE").ok(),
        Some("/root/spy_node/flash_accounts.env".to_string()),
        Some("/root/spy_node/.env.flash_loans".to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            if k.trim() == key {
                let value = v.trim().trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
}

fn decode_helios_flash_arb_args(helios_ix: &Instruction) -> Result<ExecuteFlashArbArgs> {
    if helios_ix.data.len() < 8 {
        bail!("helios instruction payload too short");
    }
    let args = ExecuteFlashArbArgs::try_from_slice(&helios_ix.data[8..])
        .context("failed to decode helios execute_flash_arb args")?;
    if args.borrow_amount == 0 {
        bail!("helios borrow amount cannot be zero");
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_list_prefers_requested_provider_once() {
        let providers = fallback_candidates(FlashProvider::Kamino);
        assert_eq!(providers[0], FlashProvider::Kamino);
        assert_eq!(
            providers
                .iter()
                .filter(|&&p| p == FlashProvider::Kamino)
                .count(),
            1
        );
    }

    #[test]
    fn unsupported_mint_rejected() {
        let mint = Pubkey::new_unique();
        assert!(classify_mint(mint).is_err());
    }

    #[test]
    fn marginfi_health_accounts_sorted_descending_by_bank() {
        let low_bank = Pubkey::new_from_array([1; 32]);
        let high_bank = Pubkey::new_from_array([9; 32]);
        let low_oracle = Pubkey::new_unique();
        let high_oracle = Pubkey::new_unique();

        let accounts = compose_marginfi_health_accounts(&[
            MarginfiHealthBank {
                bank: low_bank,
                oracle: low_oracle,
            },
            MarginfiHealthBank {
                bank: high_bank,
                oracle: high_oracle,
            },
        ]);

        assert_eq!(accounts, vec![high_bank, high_oracle, low_bank, low_oracle]);
    }

    #[test]
    fn decode_helios_payload_round_trips() {
        let args = ExecuteFlashArbArgs {
            borrow_amount: 123,
            min_profit: 7,
            hop_account_counts: vec![4],
            hop_data: vec![vec![1, 2, 3]],
        };
        let mut data = vec![0; 8];
        data.extend_from_slice(&borsh::to_vec(&args).unwrap());
        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data,
        };

        let decoded = decode_helios_flash_arb_args(&ix).unwrap();
        assert_eq!(decoded.borrow_amount, 123);
        assert_eq!(decoded.min_profit, 7);
        assert_eq!(decoded.hop_account_counts, vec![4]);
        assert_eq!(decoded.hop_data, vec![vec![1, 2, 3]]);
    }
}
