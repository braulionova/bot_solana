#![allow(unexpected_cfgs)]

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    hash::hash,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::instructions::{load_current_index_checked, load_instruction_at_checked},
};

entrypoint!(process_instruction);

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub enum PortFlashReceiverInstruction {
    ReceiveFlashLoan {
        repay_amount: u64,
    },
    CarryPlan {
        min_profit: u64,
        hop_account_counts: Vec<u8>,
        hop_data: Vec<Vec<u8>>,
    },
}

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    match PortFlashReceiverInstruction::try_from_slice(instruction_data)
        .map_err(|_| ProgramError::InvalidInstructionData)?
    {
        PortFlashReceiverInstruction::ReceiveFlashLoan { repay_amount } => {
            process_receive_flash_loan(program_id, accounts, repay_amount)
        }
        PortFlashReceiverInstruction::CarryPlan { .. } => Ok(()),
    }
}

fn process_receive_flash_loan(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    repay_amount: u64,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let user_token_account = next_account_info(account_iter)?;
    let reserve_liquidity_supply = next_account_info(account_iter)?;
    let token_program = next_account_info(account_iter)?;
    let payer = next_account_info(account_iter)?;
    let helios_program = next_account_info(account_iter)?;
    let helios_config = next_account_info(account_iter)?;
    let instructions_sysvar = next_account_info(account_iter)?;
    let remaining: Vec<AccountInfo> = account_iter.cloned().collect();

    if *instructions_sysvar.key != solana_program::sysvar::instructions::id() {
        return Err(ProgramError::InvalidAccountData);
    }
    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let current_index = load_current_index_checked(instructions_sysvar)? as usize;
    let carrier_ix = load_instruction_at_checked(current_index + 1, instructions_sysvar)?;
    if carrier_ix.program_id != *program_id {
        msg!("next instruction is not the Port receiver carrier");
        return Err(ProgramError::InvalidInstructionData);
    }

    let plan = match PortFlashReceiverInstruction::try_from_slice(&carrier_ix.data)
        .map_err(|_| ProgramError::InvalidInstructionData)?
    {
        PortFlashReceiverInstruction::CarryPlan {
            min_profit,
            hop_account_counts,
            hop_data,
        } => CarryPlan {
            min_profit,
            hop_account_counts,
            hop_data,
        },
        _ => return Err(ProgramError::InvalidInstructionData),
    };

    msg!(
        "receiver user_token_account={} reserve_liquidity_supply={} payer={} helios_program={} helios_config={} min_profit={}",
        user_token_account.key,
        reserve_liquidity_supply.key,
        payer.key,
        helios_program.key,
        helios_config.key,
        plan.min_profit
    );
    let receiver_balance_before = read_token_amount(user_token_account)?;
    msg!(
        "receiver balance_before_helios={} repay_amount={}",
        receiver_balance_before,
        repay_amount
    );

    let helios_ix = build_helios_route_only_ix(
        helios_program.key,
        payer.key,
        user_token_account.key,
        helios_config.key,
        token_program.key,
        plan.min_profit,
        &plan.hop_account_counts,
        &plan.hop_data,
        &remaining,
    )?;

    let mut helios_infos = Vec::with_capacity(5 + remaining.len());
    helios_infos.push(helios_program.clone());
    helios_infos.push(payer.clone());
    helios_infos.push(user_token_account.clone());
    helios_infos.push(helios_config.clone());
    helios_infos.push(token_program.clone());
    helios_infos.extend(remaining.iter().cloned());
    invoke(&helios_ix, &helios_infos)?;

    let receiver_balance_after_helios = read_token_amount(user_token_account)?;
    msg!(
        "receiver balance_after_helios={}",
        receiver_balance_after_helios
    );

    let repay_ix = build_spl_token_transfer_ix(
        token_program.key,
        user_token_account.key,
        reserve_liquidity_supply.key,
        payer.key,
        repay_amount,
    );
    invoke(
        &repay_ix,
        &[
            user_token_account.clone(),
            reserve_liquidity_supply.clone(),
            payer.clone(),
            token_program.clone(),
        ],
    )?;

    let receiver_balance_after_repay = read_token_amount(user_token_account)?;
    msg!(
        "receiver balance_after_repay={}",
        receiver_balance_after_repay
    );

    Ok(())
}

#[derive(Debug, Clone)]
struct CarryPlan {
    min_profit: u64,
    hop_account_counts: Vec<u8>,
    hop_data: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, BorshSerialize)]
struct ExecuteRouteOnlyArgs {
    required_balance_increase: u64,
    hop_account_counts: Vec<u8>,
    hop_data: Vec<Vec<u8>>,
}

fn build_helios_route_only_ix(
    helios_program: &Pubkey,
    payer: &Pubkey,
    user_token_account: &Pubkey,
    helios_config: &Pubkey,
    token_program: &Pubkey,
    required_balance_increase: u64,
    hop_account_counts: &[u8],
    hop_data: &[Vec<u8>],
    remaining: &[AccountInfo],
) -> Result<Instruction, ProgramError> {
    let mut data = anchor_discriminator("execute_route_only").to_vec();
    ExecuteRouteOnlyArgs {
        required_balance_increase,
        hop_account_counts: hop_account_counts.to_vec(),
        hop_data: hop_data.to_vec(),
    }
    .serialize(&mut data)
    .map_err(|_| ProgramError::InvalidInstructionData)?;

    let accounts = vec![
        AccountMeta::new(*payer, true),
        AccountMeta::new(*user_token_account, false),
        AccountMeta::new_readonly(*helios_config, false),
        AccountMeta::new_readonly(*token_program, false),
    ]
    .into_iter()
    .chain(remaining.iter().map(|account| AccountMeta {
        pubkey: *account.key,
        is_signer: account.is_signer,
        is_writable: account.is_writable,
    }))
    .collect();

    Ok(Instruction {
        program_id: *helios_program,
        accounts,
        data,
    })
}

fn build_spl_token_transfer_ix(
    token_program: &Pubkey,
    source: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3);
    data.extend_from_slice(&amount.to_le_bytes());

    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

fn anchor_discriminator(name: &str) -> [u8; 8] {
    let mut seed = b"global:".to_vec();
    seed.extend_from_slice(name.as_bytes());
    let hash = hash(&seed);
    let mut discriminator = [0u8; 8];
    discriminator.copy_from_slice(&hash.to_bytes()[..8]);
    discriminator
}

fn read_token_amount(account: &AccountInfo) -> Result<u64, ProgramError> {
    let data = account.try_borrow_data()?;
    if data.len() < 72 {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut amount_bytes = [0u8; 8];
    amount_bytes.copy_from_slice(&data[64..72]);
    Ok(u64::from_le_bytes(amount_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_instruction_round_trips() {
        let original = PortFlashReceiverInstruction::CarryPlan {
            min_profit: 42,
            hop_account_counts: vec![2, 3],
            hop_data: vec![vec![1, 2], vec![3, 4, 5]],
        };
        let data = borsh::to_vec(&original).unwrap();
        let decoded = PortFlashReceiverInstruction::try_from_slice(&data).unwrap();
        match decoded {
            PortFlashReceiverInstruction::CarryPlan {
                min_profit,
                hop_account_counts,
                hop_data,
            } => {
                assert_eq!(min_profit, 42);
                assert_eq!(hop_account_counts, vec![2, 3]);
                assert_eq!(hop_data, vec![vec![1, 2], vec![3, 4, 5]]);
            }
            _ => panic!("unexpected variant"),
        }
    }
}
