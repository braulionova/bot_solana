// raydium_cpmm.rs – Raydium CPMM (LaunchLab) adapter (constant product XY=K).

use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

/// Raydium CPMM program ID on Solana mainnet.
pub const RAYDIUM_CPMM_PROGRAM: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");

/// Default fee: 25bps (from amm_config, but 25 is the standard).
pub const RAYDIUM_CPMM_DEFAULT_FEE_BPS: u64 = 25;

/// Decoded Raydium CPMM pool account data.
#[derive(Debug, Clone)]
pub struct CpmmPoolInfo {
    pub token_0: Pubkey,
    pub token_1: Pubkey,
    pub vault_0: Pubkey,
    pub vault_1: Pubkey,
    pub token_0_program: Pubkey,
    pub token_1_program: Pubkey,
    pub fee_bps: u64,
}

/// Decode a Raydium CPMM pool from on-chain account data.
///
/// Layout:
///   Offset   8: amm_config (Pubkey)
///   Offset  40: pool_creator (Pubkey)
///   Offset  72: token_0_vault (Pubkey)
///   Offset 104: token_1_vault (Pubkey)
///   Offset 136: lp_mint (Pubkey)
///   Offset 168: token_0_mint (Pubkey)
///   Offset 200: token_1_mint (Pubkey)
///   Offset 232: token_0_program (Pubkey)
///   Offset 264: token_1_program (Pubkey)
///   Offset 329: status (u8)
pub fn decode_cpmm_pool(data: &[u8]) -> Option<CpmmPoolInfo> {
    // Minimum size: 373 + 8 = 381 bytes (through open_time)
    if data.len() < 381 {
        tracing::warn!(
            "raydium_cpmm: account data too short ({} < 381)",
            data.len()
        );
        return None;
    }

    // Status check – pool should be active.
    // Status byte at offset 329: 0 = uninitialized, 1 = active, 2 = disabled.
    let status = data[329];
    if status != 1 {
        tracing::warn!("raydium_cpmm: pool status {} (not active)", status);
        return None;
    }

    let vault_0 = Pubkey::try_from(&data[72..104]).ok()?;
    let vault_1 = Pubkey::try_from(&data[104..136]).ok()?;
    let token_0 = Pubkey::try_from(&data[168..200]).ok()?;
    let token_1 = Pubkey::try_from(&data[200..232]).ok()?;
    let token_0_program = Pubkey::try_from(&data[232..264]).ok()?;
    let token_1_program = Pubkey::try_from(&data[264..296]).ok()?;

    // Reject pools with default/zero pubkeys in critical fields.
    if token_0 == Pubkey::default()
        || token_1 == Pubkey::default()
        || vault_0 == Pubkey::default()
        || vault_1 == Pubkey::default()
    {
        tracing::warn!("raydium_cpmm: pool has default pubkey in critical field");
        return None;
    }

    Some(CpmmPoolInfo {
        token_0,
        token_1,
        vault_0,
        vault_1,
        token_0_program,
        token_1_program,
        fee_bps: RAYDIUM_CPMM_DEFAULT_FEE_BPS,
    })
}

/// Constant-product (XY=K) quote with fee deduction.
///
/// Identical math to FluxBeam / Raydium V4 — standard AMM formula.
/// Returns the output amount, or 0 on error/overflow.
#[inline]
pub fn quote_cpmm(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_bps: u64) -> u64 {
    if reserve_in == 0 || reserve_out == 0 || amount_in == 0 || fee_bps >= 10_000 {
        return 0;
    }
    let fee_numerator = 10_000u128.checked_sub(fee_bps as u128).unwrap_or(0);
    let amount_in_with_fee = (amount_in as u128).checked_mul(fee_numerator).unwrap_or(0);
    let numerator = amount_in_with_fee
        .checked_mul(reserve_out as u128)
        .unwrap_or(0);
    let denominator = (reserve_in as u128)
        .checked_mul(10_000)
        .unwrap_or(0)
        .checked_add(amount_in_with_fee)
        .unwrap_or(0);
    if denominator == 0 {
        return 0;
    }
    (numerator / denominator) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_basic() {
        // 1000 in, 1M/1M reserves, 25bps fee
        let out = quote_cpmm(1_000_000, 1_000_000, 1_000, 25);
        assert!(out > 0 && out < 1_000);
    }

    #[test]
    fn test_quote_zero_reserve() {
        assert_eq!(quote_cpmm(0, 1_000_000, 1_000, 25), 0);
        assert_eq!(quote_cpmm(1_000_000, 0, 1_000, 25), 0);
    }

    #[test]
    fn test_quote_zero_input() {
        assert_eq!(quote_cpmm(1_000_000, 1_000_000, 0, 25), 0);
    }

    #[test]
    fn test_quote_large_trade() {
        // 50% of reserve — should get significant output.
        let out = quote_cpmm(1_000_000, 1_000_000, 500_000, 25);
        // XY=K: ~332k expected (with fee)
        assert!(out > 300_000 && out < 340_000, "out = {out}");
    }

    #[test]
    fn test_decode_too_short() {
        let data = vec![0u8; 100];
        assert!(decode_cpmm_pool(&data).is_none());
    }
}
