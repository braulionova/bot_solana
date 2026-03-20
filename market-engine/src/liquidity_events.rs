// liquidity_events.rs — Detect LP add/remove events from shred-decoded TXs.
//
// When a large LP removes liquidity, the pool's reserves drop → price impact
// on subsequent swaps is amplified → arb opportunities appear.
//
// Also detects: addLiquidity (less interesting but useful for pool tracking).

use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::VersionedTransaction;
use tracing::info;

// Raydium AMM V4 instruction discriminators
const RAYDIUM_ADD_LIQ: u8 = 3;    // addLiquidity
const RAYDIUM_REMOVE_LIQ: u8 = 4; // removeLiquidity

// PumpSwap discriminators (Anchor: sha256("global:deposit")[..8], sha256("global:withdraw")[..8])
const PUMPSWAP_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const PUMPSWAP_WITHDRAW: [u8; 8] = [183, 18, 70, 156, 148, 109, 161, 34];

const PK_RAYDIUM_V4: Pubkey = solana_sdk::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
const PK_PUMPSWAP: Pubkey = solana_sdk::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");

#[derive(Debug, Clone)]
pub struct LpEvent {
    pub pool: Pubkey,
    pub is_remove: bool,
    /// Estimated amount of liquidity added/removed (in token A units).
    pub estimated_amount: u64,
}

/// Detect LP add/remove events from a transaction.
pub fn detect_lp_events(tx: &VersionedTransaction) -> Vec<LpEvent> {
    let msg = &tx.message;
    let keys = msg.static_account_keys();
    let mut events = Vec::new();

    for ix in msg.instructions() {
        let pid = match keys.get(ix.program_id_index as usize) {
            Some(pk) => *pk,
            None => continue,
        };

        // Raydium AMM V4: addLiquidity (disc=3) / removeLiquidity (disc=4)
        if pid == PK_RAYDIUM_V4 && !ix.data.is_empty() {
            let disc = ix.data[0];
            if disc == RAYDIUM_ADD_LIQ || disc == RAYDIUM_REMOVE_LIQ {
                // Pool account is at index 1 in the accounts list
                if let Some(&pool_idx) = ix.accounts.get(1) {
                    if let Some(&pool) = keys.get(pool_idx as usize) {
                        let amount = if ix.data.len() >= 9 {
                            u64::from_le_bytes(ix.data[1..9].try_into().unwrap_or([0; 8]))
                        } else {
                            0
                        };
                        events.push(LpEvent {
                            pool,
                            is_remove: disc == RAYDIUM_REMOVE_LIQ,
                            estimated_amount: amount,
                        });
                    }
                }
            }
        }

        // PumpSwap: deposit / withdraw
        if pid == PK_PUMPSWAP && ix.data.len() >= 8 {
            let disc = &ix.data[..8];
            let is_deposit = disc == PUMPSWAP_DEPOSIT;
            let is_withdraw = disc == PUMPSWAP_WITHDRAW;
            if is_deposit || is_withdraw {
                // Pool is at accounts[0]
                if let Some(&pool_idx) = ix.accounts.first() {
                    if let Some(&pool) = keys.get(pool_idx as usize) {
                        let amount = if ix.data.len() >= 16 {
                            u64::from_le_bytes(ix.data[8..16].try_into().unwrap_or([0; 8]))
                        } else {
                            0
                        };
                        events.push(LpEvent {
                            pool,
                            is_remove: is_withdraw,
                            estimated_amount: amount,
                        });
                    }
                }
            }
        }
    }

    events
}
