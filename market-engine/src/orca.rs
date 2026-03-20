use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

pub const ORCA_TICK_ARRAY_SIZE: i32 = 88;
const FIXED_TICK_ARRAY_LEN: usize = 9_988;
const DYNAMIC_TICK_ARRAY_MIN_LEN: usize = 148;
const DYNAMIC_TICK_ARRAY_MAX_LEN: usize = 10_004;
const FIXED_TICK_SIZE: usize = 113;
const DYNAMIC_TICK_DATA_SIZE: usize = 96;

#[derive(Debug, Clone, Copy, Default)]
pub struct OrcaTick {
    pub initialized: bool,
    pub liquidity_net: i128,
}

#[derive(Debug, Clone)]
pub struct OrcaTickArray {
    pub start_tick_index: i32,
    pub ticks: [OrcaTick; ORCA_TICK_ARRAY_SIZE as usize],
}

#[derive(Debug, Clone)]
pub struct OrcaSwapQuote {
    pub amount_out: u64,
    pub traversed_arrays: Vec<i32>,
}

#[derive(Debug, Clone)]
enum TraversalEvent {
    InitializedTick(i32, i128),
    ArrayBoundary(i32),
}

pub fn decode_tick_array(data: &[u8], whirlpool: &Pubkey) -> Option<OrcaTickArray> {
    if data.len() == FIXED_TICK_ARRAY_LEN {
        decode_fixed_tick_array(data, whirlpool)
    } else if (DYNAMIC_TICK_ARRAY_MIN_LEN..=DYNAMIC_TICK_ARRAY_MAX_LEN).contains(&data.len()) {
        decode_dynamic_tick_array(data, whirlpool)
    } else {
        None
    }
}

pub fn tick_array_start_indexes(
    tick_current_index: i32,
    tick_spacing: u16,
    radius: i32,
) -> Vec<i32> {
    let current = tick_array_start_index(tick_current_index, tick_spacing);
    let offset = tick_array_offset(tick_spacing);
    (-radius..=radius)
        .map(|step| current + (step * offset))
        .collect()
}

pub fn tick_array_start_index(tick_index: i32, tick_spacing: u16) -> i32 {
    if tick_spacing == 0 {
        return 0; // Invalid pool — return 0 to avoid division by zero
    }
    let tick_spacing = tick_spacing as i32;
    tick_index
        .div_euclid(tick_spacing)
        .div_euclid(ORCA_TICK_ARRAY_SIZE)
        * tick_spacing
        * ORCA_TICK_ARRAY_SIZE
}

pub fn quote_exact_in(
    amount_in: u64,
    a_to_b: bool,
    sqrt_price_x64: u128,
    liquidity: u128,
    tick_current_index: i32,
    tick_spacing: u16,
    fee_bps: u64,
    tick_arrays: &HashMap<i32, OrcaTickArray>,
) -> OrcaSwapQuote {
    if amount_in == 0 || liquidity == 0 || sqrt_price_x64 == 0 || tick_spacing == 0 {
        return OrcaSwapQuote {
            amount_out: 0,
            traversed_arrays: vec![tick_array_start_index(tick_current_index, tick_spacing)],
        };
    }

    let fee_factor = (10_000.0 - fee_bps as f64).max(0.0) / 10_000.0;
    let offset = tick_array_offset(tick_spacing);
    let mut remaining_user_in = amount_in as f64;
    let mut amount_out = 0.0;
    let mut sqrt_price = sqrt_price_x64_to_f64(sqrt_price_x64);
    let mut liquidity = liquidity as f64;
    let mut tick_current = tick_current_index;
    let mut traversed_arrays = vec![tick_array_start_index(tick_current, tick_spacing)];

    for _ in 0..512 {
        if remaining_user_in <= f64::EPSILON || liquidity <= 0.0 {
            break;
        }

        let current_array = tick_array_start_index(tick_current, tick_spacing);
        if traversed_arrays.last().copied() != Some(current_array) {
            traversed_arrays.push(current_array);
        }

        let lower_boundary = current_array;
        let upper_boundary = current_array + offset;

        let event = next_event(
            tick_current,
            lower_boundary,
            upper_boundary,
            tick_spacing,
            a_to_b,
            tick_arrays,
        );

        let Some(event) = event else {
            let effective_in = remaining_user_in * fee_factor;
            if effective_in <= f64::EPSILON {
                break;
            }
            amount_out += partial_amount_out(a_to_b, sqrt_price, liquidity, effective_in);
            break;
        };

        let (target_tick, liquidity_net, crossing_boundary) = match event {
            TraversalEvent::InitializedTick(tick, liquidity_net) => (tick, liquidity_net, false),
            TraversalEvent::ArrayBoundary(tick) => (tick, 0, true),
        };

        let target_sqrt = tick_index_to_sqrt_price(target_tick);
        let required_effective_in =
            effective_input_to_reach_target(a_to_b, sqrt_price, target_sqrt, liquidity);

        if required_effective_in.is_finite()
            && required_effective_in > f64::EPSILON
            && remaining_user_in * fee_factor + 1e-12 >= required_effective_in
        {
            let required_user_in = required_effective_in / fee_factor.max(f64::EPSILON);
            remaining_user_in = (remaining_user_in - required_user_in).max(0.0);
            amount_out += amount_out_for_step(a_to_b, sqrt_price, target_sqrt, liquidity);
            sqrt_price = target_sqrt;

            if crossing_boundary {
                tick_current = if a_to_b {
                    target_tick - tick_spacing as i32
                } else {
                    target_tick
                };
                let next_array = tick_array_start_index(tick_current, tick_spacing);
                if traversed_arrays.last().copied() != Some(next_array) {
                    traversed_arrays.push(next_array);
                }
                continue;
            }

            let updated_liquidity = if a_to_b {
                liquidity as i128 - liquidity_net
            } else {
                liquidity as i128 + liquidity_net
            };
            liquidity = updated_liquidity.max(0) as f64;
            tick_current = if a_to_b {
                target_tick - tick_spacing as i32
            } else {
                target_tick
            };
        } else {
            let effective_in = remaining_user_in * fee_factor;
            amount_out += partial_amount_out(a_to_b, sqrt_price, liquidity, effective_in);
            break;
        }
    }

    OrcaSwapQuote {
        amount_out: amount_out.max(0.0) as u64,
        traversed_arrays,
    }
}

fn decode_fixed_tick_array(data: &[u8], whirlpool: &Pubkey) -> Option<OrcaTickArray> {
    if data.len() != FIXED_TICK_ARRAY_LEN {
        return None;
    }

    let start_tick_index = i32::from_le_bytes(data.get(8..12)?.try_into().ok()?);
    let tick_data = data.get(12..(12 + FIXED_TICK_SIZE * ORCA_TICK_ARRAY_SIZE as usize))?;
    let decoded_whirlpool = pubkey_from_bytes(data.get((data.len() - 32)..data.len())?)?;
    if &decoded_whirlpool != whirlpool {
        return None;
    }

    let mut ticks = [OrcaTick::default(); ORCA_TICK_ARRAY_SIZE as usize];
    for (idx, tick) in ticks.iter_mut().enumerate() {
        let offset = idx * FIXED_TICK_SIZE;
        let bytes = tick_data.get(offset..offset + FIXED_TICK_SIZE)?;
        tick.initialized = *bytes.first()? != 0;
        tick.liquidity_net = i128::from_le_bytes(bytes.get(1..17)?.try_into().ok()?);
    }

    Some(OrcaTickArray {
        start_tick_index,
        ticks,
    })
}

fn decode_dynamic_tick_array(data: &[u8], whirlpool: &Pubkey) -> Option<OrcaTickArray> {
    let start_tick_index = i32::from_le_bytes(data.get(8..12)?.try_into().ok()?);
    let decoded_whirlpool = pubkey_from_bytes(data.get(12..44)?)?;
    if &decoded_whirlpool != whirlpool {
        return None;
    }

    let mut cursor = 60;
    let mut ticks = [OrcaTick::default(); ORCA_TICK_ARRAY_SIZE as usize];
    for tick in &mut ticks {
        let tag = *data.get(cursor)?;
        cursor += 1;
        match tag {
            0 => {}
            1 => {
                let bytes = data.get(cursor..cursor + DYNAMIC_TICK_DATA_SIZE)?;
                cursor += DYNAMIC_TICK_DATA_SIZE;
                tick.initialized = true;
                tick.liquidity_net = i128::from_le_bytes(bytes.get(0..16)?.try_into().ok()?);
            }
            _ => return None,
        }
    }

    Some(OrcaTickArray {
        start_tick_index,
        ticks,
    })
}

fn next_event(
    tick_current: i32,
    lower_boundary: i32,
    upper_boundary: i32,
    tick_spacing: u16,
    a_to_b: bool,
    tick_arrays: &HashMap<i32, OrcaTickArray>,
) -> Option<TraversalEvent> {
    let next_initialized = next_initialized_tick(tick_current, tick_spacing, a_to_b, tick_arrays)
        .map(|(tick, liq)| TraversalEvent::InitializedTick(tick, liq));

    let boundary_tick = if a_to_b {
        lower_boundary
    } else {
        upper_boundary
    };

    match next_initialized {
        Some(TraversalEvent::InitializedTick(init_tick, liq_net)) => {
            if a_to_b {
                if init_tick >= boundary_tick {
                    Some(TraversalEvent::InitializedTick(init_tick, liq_net))
                } else {
                    Some(TraversalEvent::ArrayBoundary(boundary_tick))
                }
            } else if init_tick <= boundary_tick {
                Some(TraversalEvent::InitializedTick(init_tick, liq_net))
            } else {
                Some(TraversalEvent::ArrayBoundary(boundary_tick))
            }
        }
        Some(other) => Some(other),
        None => Some(TraversalEvent::ArrayBoundary(boundary_tick)),
    }
}

fn next_initialized_tick(
    tick_current: i32,
    tick_spacing: u16,
    a_to_b: bool,
    tick_arrays: &HashMap<i32, OrcaTickArray>,
) -> Option<(i32, i128)> {
    let mut best: Option<(i32, i128)> = None;

    for tick_array in tick_arrays.values() {
        for (idx, tick) in tick_array.ticks.iter().enumerate() {
            if !tick.initialized {
                continue;
            }
            let tick_index = tick_array.start_tick_index + idx as i32 * tick_spacing as i32;
            let is_better = if a_to_b {
                tick_index < tick_current
                    && best
                        .map(|(best_tick, _)| tick_index > best_tick)
                        .unwrap_or(true)
            } else {
                tick_index > tick_current
                    && best
                        .map(|(best_tick, _)| tick_index < best_tick)
                        .unwrap_or(true)
            };
            if is_better {
                best = Some((tick_index, tick.liquidity_net));
            }
        }
    }

    best
}

fn effective_input_to_reach_target(
    a_to_b: bool,
    sqrt_price_current: f64,
    sqrt_price_target: f64,
    liquidity: f64,
) -> f64 {
    if a_to_b {
        if sqrt_price_target >= sqrt_price_current {
            return 0.0;
        }
        liquidity * (sqrt_price_current - sqrt_price_target)
            / (sqrt_price_current * sqrt_price_target)
    } else {
        if sqrt_price_target <= sqrt_price_current {
            return 0.0;
        }
        liquidity * (sqrt_price_target - sqrt_price_current)
    }
}

fn amount_out_for_step(
    a_to_b: bool,
    sqrt_price_current: f64,
    sqrt_price_next: f64,
    liquidity: f64,
) -> f64 {
    if a_to_b {
        liquidity * (sqrt_price_current - sqrt_price_next)
    } else {
        liquidity * ((1.0 / sqrt_price_current) - (1.0 / sqrt_price_next))
    }
}

fn partial_amount_out(
    a_to_b: bool,
    sqrt_price_current: f64,
    liquidity: f64,
    effective_input: f64,
) -> f64 {
    if effective_input <= f64::EPSILON {
        return 0.0;
    }

    if a_to_b {
        let sqrt_price_next =
            (sqrt_price_current * liquidity) / (liquidity + effective_input * sqrt_price_current);
        amount_out_for_step(true, sqrt_price_current, sqrt_price_next, liquidity)
    } else {
        let sqrt_price_next = sqrt_price_current + (effective_input / liquidity);
        amount_out_for_step(false, sqrt_price_current, sqrt_price_next, liquidity)
    }
}

fn sqrt_price_x64_to_f64(value: u128) -> f64 {
    value as f64 / (1u128 << 64) as f64
}

fn tick_index_to_sqrt_price(tick_index: i32) -> f64 {
    1.0001f64.powf(tick_index as f64 / 2.0)
}

fn tick_array_offset(tick_spacing: u16) -> i32 {
    tick_spacing as i32 * ORCA_TICK_ARRAY_SIZE
}

fn pubkey_from_bytes(bytes: &[u8]) -> Option<Pubkey> {
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Some(Pubkey::new_from_array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fixed_tick_array_layout() {
        let whirlpool = Pubkey::new_unique();
        let mut bytes = vec![0u8; FIXED_TICK_ARRAY_LEN];
        bytes[8..12].copy_from_slice(&(-11_264i32).to_le_bytes());
        bytes[12] = 1;
        bytes[13..29].copy_from_slice(&123i128.to_le_bytes());
        let whirlpool_offset = bytes.len() - 32;
        bytes[whirlpool_offset..].copy_from_slice(whirlpool.as_ref());

        let decoded = decode_tick_array(&bytes, &whirlpool).unwrap();
        assert_eq!(decoded.start_tick_index, -11_264);
        assert!(decoded.ticks[0].initialized);
        assert_eq!(decoded.ticks[0].liquidity_net, 123);
    }

    #[test]
    fn decodes_dynamic_tick_array_layout() {
        let whirlpool = Pubkey::new_unique();
        let mut bytes = Vec::with_capacity(DYNAMIC_TICK_ARRAY_MIN_LEN + DYNAMIC_TICK_DATA_SIZE);
        bytes.resize(60, 0);
        bytes[8..12].copy_from_slice(&(0i32).to_le_bytes());
        bytes[12..44].copy_from_slice(whirlpool.as_ref());
        bytes.push(1);
        bytes.extend_from_slice(&77i128.to_le_bytes());
        bytes.extend_from_slice(&[0u8; DYNAMIC_TICK_DATA_SIZE - 16]);
        for _ in 1..ORCA_TICK_ARRAY_SIZE {
            bytes.push(0);
        }

        let decoded = decode_tick_array(&bytes, &whirlpool).unwrap();
        assert_eq!(decoded.start_tick_index, 0);
        assert!(decoded.ticks[0].initialized);
        assert_eq!(decoded.ticks[0].liquidity_net, 77);
    }

    #[test]
    fn traversal_selects_arrays_in_swap_direction() {
        let tick_spacing = 128;
        let current_start = tick_array_start_index(1_000, tick_spacing);
        let previous_start = current_start - tick_array_offset(tick_spacing);
        let mut arrays = HashMap::new();
        arrays.insert(
            current_start,
            OrcaTickArray {
                start_tick_index: current_start,
                ticks: [OrcaTick::default(); ORCA_TICK_ARRAY_SIZE as usize],
            },
        );
        arrays.insert(
            previous_start,
            OrcaTickArray {
                start_tick_index: previous_start,
                ticks: [OrcaTick::default(); ORCA_TICK_ARRAY_SIZE as usize],
            },
        );

        let quote = quote_exact_in(
            1_000_000_000,
            true,
            ((1.0001f64.powf(1_000f64 / 2.0)) * (1u128 << 64) as f64) as u128,
            1_000_000_000,
            1_000,
            tick_spacing,
            30,
            &arrays,
        );

        assert_eq!(quote.traversed_arrays[0], current_start);
        assert!(quote.traversed_arrays.contains(&previous_start));
    }
}
