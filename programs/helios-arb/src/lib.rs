// lib.rs – Helios Arb on-chain Anchor program.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke,
};
use anchor_spl::token::{Token, TokenAccount};

declare_id!("8Hi69VoPFTufCZd1Ht2XHHt5mDxrGCMedea1WfCLwE9c");

const MAX_DEX_WHITELIST: usize = 10;
const CONFIG_SPACE: usize = 8 + 32 + 4 + (32 * MAX_DEX_WHITELIST);

#[program]
pub mod helios_arb {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.authority = ctx.accounts.authority.key();
        let whitelist = vec![
            pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"), // Raydium V4
            pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"), // Raydium CLMM
            pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),  // Orca Whirlpool
            pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),  // Meteora DLMM
            pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),  // PumpSwap
        ];
        require!(
            whitelist.len() <= MAX_DEX_WHITELIST,
            HeliosError::WhitelistTooLarge
        );
        config.dex_whitelist = whitelist;
        Ok(())
    }

    pub fn update_config(
        ctx: Context<UpdateConfig>,
        new_authority: Option<Pubkey>,
        new_whitelist: Option<Vec<Pubkey>>,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        if let Some(auth) = new_authority {
            config.authority = auth;
        }
        if let Some(whitelist) = new_whitelist {
            require!(
                whitelist.len() <= MAX_DEX_WHITELIST,
                HeliosError::WhitelistTooLarge
            );
            config.dex_whitelist = whitelist;
        }
        Ok(())
    }

    pub fn execute_flash_arb<'info>(
        ctx: Context<'_, '_, '_, 'info, ExecuteFlashArb<'info>>,
        _borrow_amount: u64,
        min_profit: u64,
        hop_account_counts: Vec<u8>,
        hop_data: Vec<Vec<u8>>,
    ) -> Result<()> {
        let balance_before = ctx.accounts.user_token_account.amount;
        execute_hops(
            &ctx.accounts.config,
            ctx.remaining_accounts,
            &hop_account_counts,
            &hop_data,
        )?;

        // Profit check is enforced on the flash-loan destination account.
        ctx.accounts.user_token_account.reload()?;
        let balance_after = ctx.accounts.user_token_account.amount;
        // balance_before already includes borrow_amount (flash-loan deposited
        // tokens before this instruction runs).  We only need to verify that
        // the swap hops produced at least min_profit on top.
        let required_after = balance_before
            .checked_add(min_profit)
            .ok_or(HeliosError::ArithmeticOverflow)?;
        require!(
            balance_after >= required_after,
            HeliosError::InsufficientProfit
        );

        let profit = balance_after.saturating_sub(balance_before);
        msg!("profit: {}", profit);
        Ok(())
    }

    pub fn execute_route_only<'info>(
        ctx: Context<'_, '_, '_, 'info, ExecuteFlashArb<'info>>,
        required_balance_increase: u64,
        hop_account_counts: Vec<u8>,
        hop_data: Vec<Vec<u8>>,
    ) -> Result<()> {
        let balance_before = ctx.accounts.user_token_account.amount;
        msg!(
            "route_only user_token_account={} payer={} token_owner={} required_balance_increase={} balance_before={}",
            ctx.accounts.user_token_account.key(),
            ctx.accounts.payer.key(),
            ctx.accounts.user_token_account.owner,
            required_balance_increase,
            balance_before
        );
        execute_hops(
            &ctx.accounts.config,
            ctx.remaining_accounts,
            &hop_account_counts,
            &hop_data,
        )?;

        ctx.accounts.user_token_account.reload()?;
        let balance_after = ctx.accounts.user_token_account.amount;
        let required_after = balance_before
            .checked_add(required_balance_increase)
            .ok_or(HeliosError::ArithmeticOverflow)?;
        msg!(
            "route_only balance_after={} required_after={}",
            balance_after,
            required_after
        );
        require!(
            balance_after >= required_after,
            HeliosError::InsufficientProfit
        );

        let profit = balance_after.saturating_sub(balance_before);
        msg!("route_only_profit: {}", profit);
        Ok(())
    }
}

fn execute_hops<'info>(
    config: &Account<'info, HeliosConfig>,
    remaining_accounts: &[AccountInfo<'info>],
    hop_account_counts: &[u8],
    hop_data: &[Vec<u8>],
) -> Result<()> {
    require!(
        hop_account_counts.len() == hop_data.len(),
        HeliosError::InvalidRoutePlan
    );

    // Flash-loan begin/end happens at the transaction level, outside this
    // program. Here we only execute the routed hops atomically.
    let mut current_idx = 0;
    for (i, &count) in hop_account_counts.iter().enumerate() {
        require!(count > 0, HeliosError::InvalidRoutePlan);
        let next_idx = current_idx + count as usize;
        require!(
            next_idx <= remaining_accounts.len(),
            HeliosError::InvalidRoutePlan
        );
        let accounts = &remaining_accounts[current_idx..next_idx];
        let data = &hop_data[i];

        // Security: first account in remaining_accounts for each hop MUST
        // be the DEX program account itself.
        let program_id = accounts[0].key;
        require!(
            config.dex_whitelist.contains(program_id),
            HeliosError::UnauthorizedDex
        );
        require!(accounts[0].executable, HeliosError::InvalidDexProgram);

        let metas: Vec<AccountMeta> = accounts[1..]
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
            .collect();

        invoke(
            &Instruction {
                program_id: *program_id,
                accounts: metas,
                data: data.clone(),
            },
            accounts,
        )?;
        current_idx = next_idx;
    }
    require!(
        current_idx == remaining_accounts.len(),
        HeliosError::InvalidRoutePlan
    );

    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = authority, space = CONFIG_SPACE, seeds = [b"config"], bump)]
    pub config: Account<'info, HeliosConfig>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(mut, has_one = authority)]
    pub config: Account<'info, HeliosConfig>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct ExecuteFlashArb<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, constraint = user_token_account.owner == payer.key())]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(seeds = [b"config"], bump)]
    pub config: Account<'info, HeliosConfig>,
    pub token_program: Program<'info, Token>,
}

#[account]
pub struct HeliosConfig {
    pub authority: Pubkey,
    pub dex_whitelist: Vec<Pubkey>,
}

#[error_code]
pub enum HeliosError {
    UnauthorizedDex,
    InsufficientProfit,
    InvalidRoutePlan,
    InvalidDexProgram,
    WhitelistTooLarge,
    ArithmeticOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_space_matches_limit() {
        let expected = 8 + 32 + 4 + (32 * MAX_DEX_WHITELIST);
        assert_eq!(CONFIG_SPACE, expected);
    }
}
