// saber.rs – Saber StableSwap AMM adapter (Curve-style invariant).

use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

/// Saber Stable AMM program ID on Solana mainnet.
pub const SABER_PROGRAM: Pubkey = pubkey!("SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ");

/// Default fee: 4bps (0.04%) for stable pairs.
pub const SABER_DEFAULT_FEE_NUMERATOR: u64 = 4;
pub const SABER_DEFAULT_FEE_DENOMINATOR: u64 = 10_000;

/// Decoded Saber pool account data.
#[derive(Debug, Clone)]
pub struct SaberPoolInfo {
    pub token_a: Pubkey,
    pub token_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub amp_factor: u64,
    pub fee_numerator: u64,
    pub fee_denominator: u64,
}

/// Decode a Saber StableSwap pool from on-chain account data.
///
/// Layout:
///   Offset   1: is_initialized (bool)
///   Offset   2: is_paused (bool)
///   Offset   4: initial_amp_factor (u64)
///   Offset  12: target_amp_factor (u64)
///   Offset 108: token_a_account / vault_a (Pubkey)
///   Offset 140: token_b_account / vault_b (Pubkey)
///   Offset 204: token_a_mint (Pubkey)
///   Offset 236: token_b_mint (Pubkey)
///   Offset 332: trade_fee_numerator (u64)
///   Offset 340: trade_fee_denominator (u64)
pub fn decode_saber_pool(data: &[u8]) -> Option<SaberPoolInfo> {
    // Minimum size: 340 + 8 = 348 bytes
    if data.len() < 348 {
        tracing::warn!("saber: account data too short ({} < 348)", data.len());
        return None;
    }

    let is_initialized = data[1] != 0;
    if !is_initialized {
        tracing::warn!("saber: pool not initialized");
        return None;
    }

    let is_paused = data[2] != 0;
    if is_paused {
        tracing::warn!("saber: pool is paused");
        return None;
    }

    let initial_amp = u64::from_le_bytes(data[4..12].try_into().ok()?);
    let target_amp = u64::from_le_bytes(data[12..20].try_into().ok()?);
    // Use target_amp_factor as the current amplification (ramp may be complete).
    let amp_factor = if target_amp > 0 { target_amp } else { initial_amp };

    let vault_a = Pubkey::try_from(&data[108..140]).ok()?;
    let vault_b = Pubkey::try_from(&data[140..172]).ok()?;
    let token_a = Pubkey::try_from(&data[204..236]).ok()?;
    let token_b = Pubkey::try_from(&data[236..268]).ok()?;

    let fee_numerator = u64::from_le_bytes(data[332..340].try_into().ok()?);
    let fee_denominator = u64::from_le_bytes(data[340..348].try_into().ok()?);

    // Reject pools with default/zero pubkeys.
    if token_a == Pubkey::default()
        || token_b == Pubkey::default()
        || vault_a == Pubkey::default()
        || vault_b == Pubkey::default()
    {
        tracing::warn!("saber: pool has default pubkey in critical field");
        return None;
    }

    if amp_factor == 0 {
        tracing::warn!("saber: amp_factor is 0");
        return None;
    }

    Some(SaberPoolInfo {
        token_a,
        token_b,
        vault_a,
        vault_b,
        amp_factor,
        fee_numerator: if fee_denominator > 0 { fee_numerator } else { SABER_DEFAULT_FEE_NUMERATOR },
        fee_denominator: if fee_denominator > 0 { fee_denominator } else { SABER_DEFAULT_FEE_DENOMINATOR },
    })
}

/// StableSwap invariant: compute D (the invariant) given reserves and amp.
///
/// Solves: A*n^n * sum(x_i) + D = A*n^n*D + D^(n+1) / (n^n * prod(x_i))
/// For n=2: A*4*(x+y) + D = A*4*D + D^3 / (4*x*y)
///
/// Uses Newton's method. Returns 0 on failure.
fn compute_d(reserve_a: u128, reserve_b: u128, amp: u128) -> u128 {
    let sum = reserve_a.checked_add(reserve_b).unwrap_or(0);
    if sum == 0 {
        return 0;
    }

    // ann = A * n^n = A * 4 (for n=2)
    let ann = amp.checked_mul(4).unwrap_or(0);
    if ann == 0 {
        return 0;
    }

    let mut d = sum;
    // Newton iterations
    for _ in 0..32 {
        // d_p = D^3 / (4 * x * y)  =  D * D / (2*x) * D / (2*y) / ...
        // More precisely: d_p = D^(n+1) / (n^n * prod(x_i))
        // For n=2: d_p = D^3 / (4 * reserve_a * reserve_b)
        // We compute iteratively to avoid overflow:
        //   d_p = D
        //   d_p = d_p * D / (2 * reserve_a)
        //   d_p = d_p * D / (2 * reserve_b)
        let mut d_p = d;
        d_p = d_p
            .checked_mul(d)
            .unwrap_or(u128::MAX)
            .checked_div(reserve_a.checked_mul(2).unwrap_or(1))
            .unwrap_or(u128::MAX);
        d_p = d_p
            .checked_mul(d)
            .unwrap_or(u128::MAX)
            .checked_div(reserve_b.checked_mul(2).unwrap_or(1))
            .unwrap_or(u128::MAX);

        let d_prev = d;
        // numerator = (ann * sum + d_p * n) * d
        // denominator = (ann - 1) * d + (n + 1) * d_p
        // For n=2:
        //   numerator = (ann * sum + d_p * 2) * d
        //   denominator = (ann - 1) * d + 3 * d_p
        let num_part = ann
            .checked_mul(sum)
            .unwrap_or(0)
            .checked_add(d_p.checked_mul(2).unwrap_or(0))
            .unwrap_or(0);
        let numerator = num_part.checked_mul(d).unwrap_or(0);

        let denom = ann
            .saturating_sub(1)
            .checked_mul(d)
            .unwrap_or(0)
            .checked_add(d_p.checked_mul(3).unwrap_or(0))
            .unwrap_or(0);

        if denom == 0 {
            return 0;
        }
        d = numerator / denom;

        // Convergence check
        let diff = if d > d_prev { d - d_prev } else { d_prev - d };
        if diff <= 1 {
            return d;
        }
    }
    d
}

/// Compute the new reserve of the output token after swapping `amount_in`.
///
/// Solves for y in the StableSwap invariant given x_new = reserve_in + amount_in.
/// Returns 0 on failure.
fn compute_y(new_reserve_in: u128, d: u128, amp: u128) -> u128 {
    // ann = A * n^n = A * 4
    let ann = amp.checked_mul(4).unwrap_or(0);
    if ann == 0 || d == 0 {
        return 0;
    }

    // c = D^3 / (4 * x_new * ann)  — but computed carefully
    // For n=2 with single known reserve x_new:
    //   c = D^(n+1) / (n^n * prod(known_x) * ann)
    //   c = D^3 / (4 * x_new * ann)
    // But we also need: b = x_new + D/ann
    // Newton: y_{n+1} = (y^2 + c) / (2*y + b - D)

    // c = D * D / (2 * x_new) * D / (2 * ann)  — iterative to avoid overflow
    let mut c = d;
    c = c
        .checked_mul(d)
        .unwrap_or(u128::MAX)
        .checked_div(new_reserve_in.checked_mul(2).unwrap_or(1))
        .unwrap_or(u128::MAX);
    c = c
        .checked_mul(d)
        .unwrap_or(u128::MAX)
        .checked_div(ann.checked_mul(2).unwrap_or(1))
        .unwrap_or(u128::MAX);

    let b = new_reserve_in
        .checked_add(d.checked_div(ann).unwrap_or(0))
        .unwrap_or(u128::MAX);

    let mut y = d;
    for _ in 0..32 {
        let y_prev = y;
        // y = (y^2 + c) / (2*y + b - D)
        let y_sq = y.checked_mul(y).unwrap_or(u128::MAX);
        let numerator = y_sq.checked_add(c).unwrap_or(u128::MAX);
        let denom = y
            .checked_mul(2)
            .unwrap_or(0)
            .checked_add(b)
            .unwrap_or(0)
            .checked_sub(d)
            .unwrap_or(0);
        if denom == 0 {
            return 0;
        }
        y = numerator / denom;

        let diff = if y > y_prev { y - y_prev } else { y_prev - y };
        if diff <= 1 {
            return y;
        }
    }
    y
}

/// Quote a Saber StableSwap trade.
///
/// `reserve_a` and `reserve_b` are the current vault balances.
/// `amount_in` is the input amount (before fee).
/// `amp` is the amplification factor.
/// `a_to_b`: true = swap A→B, false = swap B→A.
///
/// Returns the output amount after fee deduction, or 0 on error.
pub fn quote_saber_stable(
    reserve_a: u64,
    reserve_b: u64,
    amount_in: u64,
    amp: u64,
    fee_numerator: u64,
    fee_denominator: u64,
    a_to_b: bool,
) -> u64 {
    if reserve_a == 0 || reserve_b == 0 || amount_in == 0 || amp == 0 {
        return 0;
    }
    if fee_denominator == 0 {
        return 0;
    }

    let ra = reserve_a as u128;
    let rb = reserve_b as u128;
    let amp128 = amp as u128;

    let d = compute_d(ra, rb, amp128);
    if d == 0 {
        return 0;
    }

    let (reserve_in, reserve_out) = if a_to_b { (ra, rb) } else { (rb, ra) };

    let new_reserve_in = reserve_in.checked_add(amount_in as u128).unwrap_or(0);
    if new_reserve_in == 0 {
        return 0;
    }

    let new_reserve_out = compute_y(new_reserve_in, d, amp128);

    let gross_out = reserve_out.saturating_sub(new_reserve_out);
    if gross_out == 0 {
        return 0;
    }

    // Deduct trade fee
    let fee = gross_out
        .checked_mul(fee_numerator as u128)
        .unwrap_or(0)
        .checked_div(fee_denominator as u128)
        .unwrap_or(0);

    let net_out = gross_out.saturating_sub(fee);

    // Clamp to u64
    if net_out > u64::MAX as u128 {
        return 0;
    }
    net_out as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_d_balanced() {
        // Equal reserves, amp=100 → D should be close to 2M
        let d = compute_d(1_000_000, 1_000_000, 100);
        assert!(d >= 1_999_000 && d <= 2_001_000, "D = {d}");
    }

    #[test]
    fn test_stable_swap_equal_reserves() {
        // Swap 1000 from a pool with 1M/1M, amp=100, 4bps fee
        // Should get close to 1000 out (stable pair).
        let out = quote_saber_stable(1_000_000, 1_000_000, 1_000, 100, 4, 10_000, true);
        assert!(out >= 995 && out <= 1_000, "out = {out}");
    }

    #[test]
    fn test_stable_swap_imbalanced() {
        // Imbalanced reserves should give less favorable rate.
        let out = quote_saber_stable(2_000_000, 500_000, 1_000, 100, 4, 10_000, true);
        assert!(out > 0 && out < 1_000, "out = {out}");
    }

    #[test]
    fn test_stable_swap_zero() {
        assert_eq!(quote_saber_stable(0, 1_000_000, 1_000, 100, 4, 10_000, true), 0);
        assert_eq!(quote_saber_stable(1_000_000, 1_000_000, 0, 100, 4, 10_000, true), 0);
        assert_eq!(quote_saber_stable(1_000_000, 1_000_000, 1_000, 0, 4, 10_000, true), 0);
    }

    #[test]
    fn test_stable_swap_symmetry() {
        // For balanced reserves, a→b and b→a should give same output.
        let out_ab = quote_saber_stable(1_000_000, 1_000_000, 1_000, 100, 4, 10_000, true);
        let out_ba = quote_saber_stable(1_000_000, 1_000_000, 1_000, 100, 4, 10_000, false);
        assert_eq!(out_ab, out_ba);
    }

    #[test]
    fn test_decode_too_short() {
        let data = vec![0u8; 100];
        assert!(decode_saber_pool(&data).is_none());
    }
}
