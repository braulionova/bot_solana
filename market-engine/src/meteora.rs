use solana_sdk::pubkey::Pubkey;
use std::{collections::HashMap, sync::Arc};

pub const METEORA_DLMM_PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const LB_PAIR_DISCRIMINATOR: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
const BIN_ARRAY_DISCRIMINATOR: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
const MAX_BIN_ARRAY_SIZE: i64 = 70;
const BIN_ARRAY_BITMAP_SIZE: i64 = 512;
const SCALE_OFFSET: u32 = 64;
const FEE_PRECISION: u128 = 1_000_000_000;
const MAX_FEE_RATE: u128 = 100_000_000;
const MAX_CACHED_BIN_ARRAYS_PER_SIDE: usize = 6;

#[derive(Debug, Clone, Copy, Default)]
pub struct MeteoraStaticParameters {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub protocol_share: u16,
    pub base_fee_power_factor: u8,
}

#[derive(Debug, Clone, Default)]
pub struct MeteoraVariableParameters {
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
}

#[derive(Debug, Clone)]
pub struct MeteoraBin {
    pub amount_x: u64,
    pub amount_y: u64,
    pub price: u128,
}

#[derive(Debug, Clone)]
pub struct MeteoraBinArray {
    pub index: i64,
    pub bins: Vec<MeteoraBin>,
}

#[derive(Debug, Clone)]
pub struct MeteoraDecodedLbPair {
    pub active_id: i32,
    pub bin_step: u16,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub parameters: MeteoraStaticParameters,
    pub v_parameters: MeteoraVariableParameters,
    pub bin_array_bitmap: [u64; 16],
}

#[derive(Debug, Clone, Copy)]
enum Rounding {
    Up,
    Down,
}

pub fn decode_lb_pair(data: &[u8]) -> Option<MeteoraDecodedLbPair> {
    if data.len() < 712 || data[..8] != LB_PAIR_DISCRIMINATOR {
        return None;
    }

    Some(MeteoraDecodedLbPair {
        parameters: MeteoraStaticParameters {
            base_factor: read_u16(data, 8)?,
            filter_period: read_u16(data, 10)?,
            decay_period: read_u16(data, 12)?,
            reduction_factor: read_u16(data, 14)?,
            variable_fee_control: read_u32(data, 16)?,
            max_volatility_accumulator: read_u32(data, 20)?,
            min_bin_id: read_i32(data, 24)?,
            max_bin_id: read_i32(data, 28)?,
            protocol_share: read_u16(data, 32)?,
            base_fee_power_factor: *data.get(34)?,
        },
        v_parameters: MeteoraVariableParameters {
            volatility_accumulator: read_u32(data, 40)?,
            volatility_reference: read_u32(data, 44)?,
            index_reference: read_i32(data, 48)?,
            last_update_timestamp: read_i64(data, 56)?,
        },
        active_id: read_i32(data, 76)?,
        bin_step: read_u16(data, 80)?,
        token_x_mint: read_pubkey(data, 88)?,
        token_y_mint: read_pubkey(data, 120)?,
        reserve_x: read_pubkey(data, 152)?,
        reserve_y: read_pubkey(data, 184)?,
        bin_array_bitmap: read_u64_array::<16>(data, 584)?,
    })
}

pub fn decode_bin_array(data: &[u8], expected_lb_pair: &Pubkey) -> Option<MeteoraBinArray> {
    if data.len() < 10_144 || data[..8] != BIN_ARRAY_DISCRIMINATOR {
        return None;
    }

    let lb_pair = read_pubkey(data, 24)?;
    if &lb_pair != expected_lb_pair {
        return None;
    }

    let mut bins = Vec::with_capacity(MAX_BIN_ARRAY_SIZE as usize);
    let mut offset = 56usize;
    for _ in 0..MAX_BIN_ARRAY_SIZE {
        bins.push(MeteoraBin {
            amount_x: read_u64(data, offset)?,
            amount_y: read_u64(data, offset + 8)?,
            price: read_u128(data, offset + 16)?,
        });
        offset += 144;
    }

    Some(MeteoraBinArray {
        index: read_i64(data, 8)?,
        bins,
    })
}

pub fn derive_bin_array_pda(lb_pair: Pubkey, bin_array_index: i64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"bin_array",
            lb_pair.as_ref(),
            &bin_array_index.to_le_bytes(),
        ],
        &METEORA_DLMM_PROGRAM_ID
            .parse()
            .expect("valid Meteora DLMM program"),
    )
    .0
}

pub fn candidate_bin_array_indices(active_id: i32, bitmap: &[u64; 16]) -> Vec<i64> {
    let active_index = bin_id_to_bin_array_index(active_id as i64);
    let mut out = vec![active_index];
    let mut negative_count = 0usize;
    let mut positive_count = 0usize;
    let mut distance = 1i64;

    while negative_count < MAX_CACHED_BIN_ARRAYS_PER_SIDE
        || positive_count < MAX_CACHED_BIN_ARRAYS_PER_SIDE
    {
        let negative_idx = active_index - distance;
        let positive_idx = active_index + distance;
        let mut progressed = false;

        if negative_count < MAX_CACHED_BIN_ARRAYS_PER_SIDE && bitmap_has_index(bitmap, negative_idx)
        {
            out.push(negative_idx);
            negative_count += 1;
            progressed = true;
        }
        if positive_count < MAX_CACHED_BIN_ARRAYS_PER_SIDE && bitmap_has_index(bitmap, positive_idx)
        {
            out.push(positive_idx);
            positive_count += 1;
            progressed = true;
        }

        if negative_idx < -BIN_ARRAY_BITMAP_SIZE && positive_idx > BIN_ARRAY_BITMAP_SIZE - 1 {
            break;
        }

        if !progressed
            && negative_idx < -BIN_ARRAY_BITMAP_SIZE
            && positive_idx > BIN_ARRAY_BITMAP_SIZE - 1
        {
            break;
        }

        distance += 1;
    }

    out.sort_unstable();
    out.dedup();
    out
}

pub fn quote_exact_in(
    amount_in: u64,
    swap_for_y: bool,
    active_id: i32,
    bin_step: u16,
    parameters: &MeteoraStaticParameters,
    v_parameters: &MeteoraVariableParameters,
    bin_arrays: &Arc<HashMap<i64, MeteoraBinArray>>,
) -> u64 {
    if amount_in == 0 || bin_arrays.is_empty() {
        return 0;
    }

    let direction = if swap_for_y { -1 } else { 1 };
    let mut remaining_in = amount_in as u128;
    let mut total_out = 0u128;
    let mut current_bin_id = active_id as i64;
    let mut guard = 0usize;

    while remaining_in > 0 && guard < 4_096 {
        let Some(bin) = get_bin(bin_arrays, current_bin_id) else {
            let current_array_index = bin_id_to_bin_array_index(current_bin_id);
            let Some(next_array_index) =
                next_cached_bin_array_index(bin_arrays, current_array_index, direction)
            else {
                break;
            };
            current_bin_id = if direction < 0 {
                bin_array_upper_bin_id(next_array_index)
            } else {
                bin_array_lower_bin_id(next_array_index)
            };
            guard += 1;
            continue;
        };

        let quote = swap_exact_in_quote_at_bin(
            bin,
            bin_step,
            parameters,
            v_parameters,
            remaining_in,
            swap_for_y,
        );
        if quote.amount_in_consumed == 0 || quote.amount_out == 0 {
            current_bin_id += direction;
            guard += 1;
            continue;
        }

        remaining_in = remaining_in.saturating_sub(quote.amount_in_consumed);
        total_out = total_out.saturating_add(quote.amount_out);

        if remaining_in == 0 {
            break;
        }

        current_bin_id += direction;
        guard += 1;
    }

    total_out.min(u64::MAX as u128) as u64
}

fn get_bin(bin_arrays: &Arc<HashMap<i64, MeteoraBinArray>>, bin_id: i64) -> Option<&MeteoraBin> {
    let array_index = bin_id_to_bin_array_index(bin_id);
    let bin_array = bin_arrays.get(&array_index)?;
    let lower = bin_array_lower_bin_id(array_index);
    let upper = bin_array_upper_bin_id(array_index);
    if bin_id < lower || bin_id > upper {
        return None;
    }

    let index = if bin_id >= 0 {
        (bin_id - lower) as usize
    } else {
        (MAX_BIN_ARRAY_SIZE - (upper - bin_id) - 1) as usize
    };
    bin_array.bins.get(index)
}

fn next_cached_bin_array_index(
    bin_arrays: &Arc<HashMap<i64, MeteoraBinArray>>,
    current_index: i64,
    direction: i64,
) -> Option<i64> {
    let mut indices: Vec<i64> = bin_arrays.keys().copied().collect();
    indices.sort_unstable();

    if direction < 0 {
        indices.into_iter().rev().find(|idx| *idx < current_index)
    } else {
        indices.into_iter().find(|idx| *idx > current_index)
    }
}

fn swap_exact_in_quote_at_bin(
    bin: &MeteoraBin,
    bin_step: u16,
    parameters: &MeteoraStaticParameters,
    v_parameters: &MeteoraVariableParameters,
    in_amount: u128,
    swap_for_y: bool,
) -> BinSwapQuote {
    if (swap_for_y && bin.amount_y == 0) || (!swap_for_y && bin.amount_x == 0) {
        return BinSwapQuote::default();
    }

    let max_amount_out = if swap_for_y {
        bin.amount_y
    } else {
        bin.amount_x
    } as u128;
    let Some(mut max_amount_in_without_fee) = (if swap_for_y {
        shl_div_u64(bin.amount_y, bin.price, Rounding::Up)
    } else {
        mul_shr_u64(bin.amount_x, bin.price, Rounding::Up)
    }) else {
        return BinSwapQuote::default();
    };

    let max_fee = compute_fee(
        bin_step,
        parameters,
        v_parameters,
        max_amount_in_without_fee,
    );
    max_amount_in_without_fee = max_amount_in_without_fee.saturating_add(max_fee);

    if in_amount > max_amount_in_without_fee {
        return BinSwapQuote {
            amount_in_consumed: max_amount_in_without_fee,
            amount_out: max_amount_out,
        };
    }

    let fee = compute_fee_from_amount(bin_step, parameters, v_parameters, in_amount);
    let amount_in_after_fee = in_amount.saturating_sub(fee);
    let Some(amount_in_after_fee_u64) = u64::try_from(amount_in_after_fee).ok() else {
        return BinSwapQuote::default();
    };
    let Some(computed_out) = get_out_amount(bin, amount_in_after_fee_u64, swap_for_y) else {
        return BinSwapQuote::default();
    };

    BinSwapQuote {
        amount_in_consumed: in_amount,
        amount_out: (computed_out as u128).min(max_amount_out),
    }
}

fn get_out_amount(bin: &MeteoraBin, in_amount: u64, swap_for_y: bool) -> Option<u64> {
    let out = if swap_for_y {
        mul_shr_u64(in_amount, bin.price, Rounding::Down)?
    } else {
        shl_div_u64(in_amount, bin.price, Rounding::Down)?
    };
    u64::try_from(out).ok()
}

fn get_base_fee(bin_step: u16, parameters: &MeteoraStaticParameters) -> u128 {
    let power = 10u128.saturating_pow(parameters.base_fee_power_factor as u32);
    parameters.base_factor as u128 * bin_step as u128 * 10 * power
}

fn get_variable_fee(
    bin_step: u16,
    parameters: &MeteoraStaticParameters,
    v_parameters: &MeteoraVariableParameters,
) -> u128 {
    if parameters.variable_fee_control == 0 {
        return 0;
    }

    let square = (v_parameters.volatility_accumulator as u128 * bin_step as u128).pow(2);
    let numerator = parameters.variable_fee_control as u128 * square;
    numerator.div_ceil(100_000_000_000)
}

fn total_fee(
    bin_step: u16,
    parameters: &MeteoraStaticParameters,
    v_parameters: &MeteoraVariableParameters,
) -> u128 {
    let fee = get_base_fee(bin_step, parameters).saturating_add(get_variable_fee(
        bin_step,
        parameters,
        v_parameters,
    ));
    fee.min(MAX_FEE_RATE)
}

fn compute_fee(
    bin_step: u16,
    parameters: &MeteoraStaticParameters,
    v_parameters: &MeteoraVariableParameters,
    in_amount: u128,
) -> u128 {
    let total_fee = total_fee(bin_step, parameters, v_parameters);
    let denominator = FEE_PRECISION.saturating_sub(total_fee).max(1);
    (in_amount.saturating_mul(total_fee)).div_ceil(denominator)
}

fn compute_fee_from_amount(
    bin_step: u16,
    parameters: &MeteoraStaticParameters,
    v_parameters: &MeteoraVariableParameters,
    in_amount_with_fees: u128,
) -> u128 {
    let total_fee = total_fee(bin_step, parameters, v_parameters);
    (in_amount_with_fees.saturating_mul(total_fee)).div_ceil(FEE_PRECISION)
}

fn mul_shr_u64(x: u64, y: u128, rounding: Rounding) -> Option<u128> {
    let hi = (y >> SCALE_OFFSET) as u64;
    let lo = y as u64;
    let high_part = x as u128 * hi as u128;
    let low_product = x as u128 * lo as u128;
    let quotient = high_part.checked_add(low_product >> SCALE_OFFSET)?;
    let remainder = low_product & ((1u128 << SCALE_OFFSET) - 1);
    Some(if matches!(rounding, Rounding::Up) && remainder > 0 {
        quotient.saturating_add(1)
    } else {
        quotient
    })
}

fn shl_div_u64(x: u64, y: u128, rounding: Rounding) -> Option<u128> {
    if y == 0 {
        return None;
    }
    let numerator = (x as u128) << SCALE_OFFSET;
    let quotient = numerator / y;
    let remainder = numerator % y;
    Some(if matches!(rounding, Rounding::Up) && remainder > 0 {
        quotient.saturating_add(1)
    } else {
        quotient
    })
}

fn bitmap_has_index(bitmap: &[u64; 16], bin_array_index: i64) -> bool {
    if !(-BIN_ARRAY_BITMAP_SIZE..BIN_ARRAY_BITMAP_SIZE).contains(&bin_array_index) {
        return false;
    }
    let offset = (bin_array_index + BIN_ARRAY_BITMAP_SIZE) as usize;
    let word = offset / 64;
    let bit = offset % 64;
    bitmap
        .get(word)
        .map(|chunk| ((chunk >> bit) & 1) == 1)
        .unwrap_or(false)
}

fn bin_array_lower_bin_id(bin_array_index: i64) -> i64 {
    bin_array_index * MAX_BIN_ARRAY_SIZE
}

fn bin_array_upper_bin_id(bin_array_index: i64) -> i64 {
    bin_array_lower_bin_id(bin_array_index) + MAX_BIN_ARRAY_SIZE - 1
}

fn bin_id_to_bin_array_index(bin_id: i64) -> i64 {
    let idx = bin_id / MAX_BIN_ARRAY_SIZE;
    let rem = bin_id % MAX_BIN_ARRAY_SIZE;
    if bin_id < 0 && rem != 0 {
        idx - 1
    } else {
        idx
    }
}

fn read_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    let bytes = data.get(offset..offset + 32)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Some(Pubkey::new_from_array(out))
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32(data: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i64(data: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn read_u128(data: &[u8], offset: usize) -> Option<u128> {
    Some(u128::from_le_bytes(
        data.get(offset..offset + 16)?.try_into().ok()?,
    ))
}

fn read_u64_array<const N: usize>(data: &[u8], offset: usize) -> Option<[u64; N]> {
    let mut out = [0u64; N];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = read_u64(data, offset + idx * 8)?;
    }
    Some(out)
}

#[derive(Default)]
struct BinSwapQuote {
    amount_in_consumed: u128,
    amount_out: u128,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> MeteoraStaticParameters {
        MeteoraStaticParameters {
            base_factor: 0,
            filter_period: 0,
            decay_period: 0,
            reduction_factor: 0,
            variable_fee_control: 0,
            max_volatility_accumulator: 0,
            min_bin_id: -1000,
            max_bin_id: 1000,
            protocol_share: 0,
            base_fee_power_factor: 0,
        }
    }

    fn v_params() -> MeteoraVariableParameters {
        MeteoraVariableParameters {
            volatility_accumulator: 0,
            volatility_reference: 0,
            index_reference: 0,
            last_update_timestamp: 0,
        }
    }

    #[test]
    fn bin_id_to_bin_array_index_matches_sdk_for_negative_bins() {
        assert_eq!(bin_id_to_bin_array_index(-1), -1);
        assert_eq!(bin_id_to_bin_array_index(-70), -1);
        assert_eq!(bin_id_to_bin_array_index(-71), -2);
        assert_eq!(bin_id_to_bin_array_index(0), 0);
        assert_eq!(bin_id_to_bin_array_index(69), 0);
        assert_eq!(bin_id_to_bin_array_index(70), 1);
    }

    #[test]
    fn quote_exact_in_crosses_multiple_bins_for_swap_for_y() {
        let mut bins = vec![
            MeteoraBin {
                amount_x: 0,
                amount_y: 0,
                price: 1u128 << SCALE_OFFSET,
            };
            70
        ];
        bins[35] = MeteoraBin {
            amount_x: 10,
            amount_y: 10,
            price: 1u128 << SCALE_OFFSET,
        };
        bins[34] = MeteoraBin {
            amount_x: 10,
            amount_y: 10,
            price: 1u128 << SCALE_OFFSET,
        };

        let mut map = HashMap::new();
        map.insert(0, MeteoraBinArray { index: 0, bins });
        let out = quote_exact_in(15, true, 35, 1, &params(), &v_params(), &Arc::new(map));
        assert_eq!(out, 15);
    }

    #[test]
    fn quote_exact_in_crosses_into_next_cached_array() {
        let mut first_bins = vec![
            MeteoraBin {
                amount_x: 0,
                amount_y: 0,
                price: 1u128 << SCALE_OFFSET,
            };
            70
        ];
        let mut second_bins = first_bins.clone();
        first_bins[69] = MeteoraBin {
            amount_x: 5,
            amount_y: 0,
            price: 1u128 << SCALE_OFFSET,
        };
        second_bins[0] = MeteoraBin {
            amount_x: 7,
            amount_y: 0,
            price: 1u128 << SCALE_OFFSET,
        };

        let mut map = HashMap::new();
        map.insert(
            0,
            MeteoraBinArray {
                index: 0,
                bins: first_bins,
            },
        );
        map.insert(
            1,
            MeteoraBinArray {
                index: 1,
                bins: second_bins,
            },
        );
        let out = quote_exact_in(10, false, 69, 1, &params(), &v_params(), &Arc::new(map));
        assert_eq!(out, 10);
    }
}
