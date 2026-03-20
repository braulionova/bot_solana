// fluxbeam.rs – FluxBeam AMM adapter (constant product XY=K).

use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

/// FluxBeam AMM program ID on Solana mainnet.
pub const FLUXBEAM_PROGRAM: Pubkey = pubkey!("FLUXubRmkEi2q6K3Y9kBPg9248ggaZVsoSFhtJHSR1X4");

/// Default fee: 30bps (0.3%).
pub const FLUXBEAM_DEFAULT_FEE_BPS: u64 = 30;

/// Decoded FluxBeam pool account data.
#[derive(Debug, Clone)]
pub struct FluxbeamPoolInfo {
    pub token_a: Pubkey,
    pub token_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub fee_bps: u64,
}

/// Decode a FluxBeam pool from on-chain account data.
///
/// Layout:
///   Offset   8: status (u64)
///   Offset 261: token_a_mint (Pubkey)
///   Offset 293: token_b_mint (Pubkey)
///   Offset 325: token_a_vault (Pubkey)
///   Offset 357: token_b_vault (Pubkey)
pub fn decode_fluxbeam_pool(data: &[u8]) -> Option<FluxbeamPoolInfo> {
    // Minimum size: 357 + 32 = 389 bytes
    if data.len() < 389 {
        tracing::warn!(
            "fluxbeam: account data too short ({} < 389)",
            data.len()
        );
        return None;
    }

    // Status check – pool should be active (status == 1 is typical).
    let status = u64::from_le_bytes(data[8..16].try_into().ok()?);
    if status == 0 {
        tracing::warn!("fluxbeam: pool status is 0 (inactive)");
        return None;
    }

    let token_a = Pubkey::try_from(&data[261..293]).ok()?;
    let token_b = Pubkey::try_from(&data[293..325]).ok()?;
    let vault_a = Pubkey::try_from(&data[325..357]).ok()?;
    let vault_b = Pubkey::try_from(&data[357..389]).ok()?;

    // Reject pools with default/zero pubkeys in critical fields.
    if token_a == Pubkey::default()
        || token_b == Pubkey::default()
        || vault_a == Pubkey::default()
        || vault_b == Pubkey::default()
    {
        tracing::warn!("fluxbeam: pool has default pubkey in critical field");
        return None;
    }

    Some(FluxbeamPoolInfo {
        token_a,
        token_b,
        vault_a,
        vault_b,
        fee_bps: FLUXBEAM_DEFAULT_FEE_BPS,
    })
}

/// Constant-product (XY=K) quote with fee deduction.
///
/// Returns the output amount, or 0 on error/overflow.
#[inline]
pub fn quote_fluxbeam(reserve_in: u64, reserve_out: u64, amount_in: u64, fee_bps: u64) -> u64 {
    if reserve_in == 0 || reserve_out == 0 || amount_in == 0 || fee_bps >= 10_000 {
        return 0;
    }
    let fee_numerator = 10_000u128.checked_sub(fee_bps as u128).unwrap_or(0);
    let amount_in_with_fee = (amount_in as u128).checked_mul(fee_numerator).unwrap_or(0);
    let numerator = amount_in_with_fee.checked_mul(reserve_out as u128).unwrap_or(0);
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
        // 1000 in, 1M/1M reserves, 30bps fee
        let out = quote_fluxbeam(1_000_000, 1_000_000, 1_000, 30);
        // Expected: ~997 * 1M / (1M*10000 + 997*10000) ≈ 996
        assert!(out > 0 && out < 1_000);
    }

    #[test]
    fn test_quote_zero_reserve() {
        assert_eq!(quote_fluxbeam(0, 1_000_000, 1_000, 30), 0);
        assert_eq!(quote_fluxbeam(1_000_000, 0, 1_000, 30), 0);
    }

    #[test]
    fn test_quote_zero_input() {
        assert_eq!(quote_fluxbeam(1_000_000, 1_000_000, 0, 30), 0);
    }

    #[test]
    fn test_decode_too_short() {
        let data = vec![0u8; 100];
        assert!(decode_fluxbeam_pool(&data).is_none());
    }
}
