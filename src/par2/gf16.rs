//! Original PAR2 field arithmetic, derived from the PAR2 2.0 format equations.
//! Values are little-endian 16-bit polynomial words, not ordinary integers.
//! This module never reads files or certifies parity, repair, or admission.

use crate::Result;
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

pub const FIELD_PERIOD: u32 = 65_535;
pub const MAX_INPUT_SLICES: u32 = 32_768;
pub const MAX_BUFFER_BYTES: usize = 1 << 20;
const POLYNOMIAL: u32 = 0x1_100b;
const CHUNK_BYTES: usize = 64 << 10;

struct Tables {
    logarithms: Box<[u16]>,
    powers: Box<[u16]>,
    input_logarithms: Box<[u16]>,
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        // Generate our own bounded tables by reducing x^16 modulo the format
        // polynomial x^16 + x^12 + x^3 + x + 1. No downloaded lookup data.
        let mut logarithms = vec![0; FIELD_PERIOD as usize + 1];
        let mut powers = Vec::with_capacity(FIELD_PERIOD as usize);
        let mut input_logarithms = Vec::with_capacity(MAX_INPUT_SLICES as usize);
        let mut value = 1u32;
        for exponent in 0..FIELD_PERIOD {
            logarithms[value as usize] = exponent as u16;
            powers.push(value as u16);
            if exponent != 0 && gcd(exponent, FIELD_PERIOD) == 1 {
                input_logarithms.push(exponent as u16);
            }
            value <<= 1;
            if value & (1 << 16) != 0 {
                value ^= POLYNOMIAL;
            }
        }
        debug_assert_eq!(value, 1);
        debug_assert_eq!(input_logarithms.len(), MAX_INPUT_SLICES as usize);
        Tables {
            logarithms: logarithms.into_boxed_slice(),
            powers: powers.into_boxed_slice(),
            input_logarithms: input_logarithms.into_boxed_slice(),
        }
    })
}

fn scaled(table: &Tables, value: u16, factor_logarithm: u16) -> u16 {
    if value == 0 {
        return 0;
    }
    let mut exponent =
        u32::from(table.logarithms[usize::from(value)]) + u32::from(factor_logarithm);
    if exponent >= FIELD_PERIOD {
        exponent -= FIELD_PERIOD;
    }
    table.powers[exponent as usize]
}

/// Field multiplication modulo 0x1100b; addition/subtraction is bitwise XOR.
pub fn multiply(left: u16, right: u16) -> u16 {
    if left == 0 || right == 0 {
        return 0;
    }
    let table = tables();
    scaled(table, left, table.logarithms[usize::from(right)])
}

/// Zero has no multiplicative inverse and is never silently treated as one.
pub fn inverse(value: u16) -> Option<u16> {
    if value == 0 {
        return None;
    }
    let table = tables();
    let exponent =
        (FIELD_PERIOD - u32::from(table.logarithms[usize::from(value)])) % FIELD_PERIOD;
    Some(table.powers[exponent as usize])
}

/// An empty product is one, including 0^0. Any positive power of zero is zero.
pub fn power(value: u16, exponent: u32) -> u16 {
    if exponent == 0 {
        return 1;
    }
    if value == 0 {
        return 0;
    }
    let table = tables();
    let exponent = (u64::from(table.logarithms[usize::from(value)]) * u64::from(exponent))
        % u64::from(FIELD_PERIOD);
    table.powers[exponent as usize]
}

/// Zero-based global input-slice position, in Main/file/slice order. PAR2 uses
/// successive primitive powers of two, not consecutive integer coefficients.
/// Recovery exponents follow the reader's supported unique period 0..65535.
pub fn coefficient(input_slice: u32, recovery_exponent: u32) -> Result<u16> {
    if input_slice >= MAX_INPUT_SLICES || recovery_exponent >= FIELD_PERIOD {
        return Err("PAR2 coefficient exceeds the input-slice or recovery-exponent bounds".into());
    }
    let table = tables();
    let exponent = (u64::from(table.input_logarithms[input_slice as usize])
        * u64::from(recovery_exponent))
        % u64::from(FIELD_PERIOD);
    Ok(table.powers[exponent as usize])
}

fn active(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("PAR2 field operation was cancelled".into())
    }
}

/// XOR scaled input words into caller-owned output, bounded to 1 MiB. This is
/// arithmetic only: the caller must separately verify identities and checksums.
pub fn add_scaled(destination: &mut [u8], input: &[u8], factor: u16) -> Result<()> {
    add_scaled_cancellable(destination, input, factor, &AtomicBool::new(true))
}

/// Checks cancellation before and after each bounded chunk. Invalid inputs and
/// initial cancellation leave output untouched. Mid-operation cancellation can
/// leave a partial sum; callers must discard it, never adopt it as repaired data.
pub fn add_scaled_cancellable(
    destination: &mut [u8],
    input: &[u8],
    factor: u16,
    flag: &AtomicBool,
) -> Result<()> {
    if destination.len() != input.len()
        || input.len() > MAX_BUFFER_BYTES
        || !input.len().is_multiple_of(2)
    {
        return Err("PAR2 field buffers require equal bounded whole-word lengths".into());
    }
    active(flag)?;
    if factor == 0 || input.is_empty() {
        return Ok(());
    }
    if factor == 1 {
        for (output, data) in destination
            .chunks_mut(CHUNK_BYTES)
            .zip(input.chunks(CHUNK_BYTES))
        {
            active(flag)?;
            for (target, source) in output.iter_mut().zip(data) {
                *target ^= *source;
            }
            active(flag)?;
        }
    } else {
        let table = tables();
        let logarithm = table.logarithms[usize::from(factor)];
        for (output, data) in destination
            .chunks_mut(CHUNK_BYTES)
            .zip(input.chunks(CHUNK_BYTES))
        {
            active(flag)?;
            for (target, source) in output
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(data.as_chunks::<2>().0)
            {
                let value = scaled(table, u16::from_le_bytes(*source), logarithm);
                *target = (u16::from_le_bytes(*target) ^ value).to_le_bytes();
            }
            active(flag)?;
        }
    }
    active(flag)
}
