//! Original CI-only arithmetic fixtures. The fixed constants come from the
//! authors' PAR2 2.0 Recovery Slice specification; the oracle is authored here.
use mynou::par2::gf16;
use std::sync::atomic::AtomicBool;

// A polynomial product followed by long division, independent of the runtime
// logarithm tables and their generator-step construction. Never a runtime helper.
fn reference_multiply(left: u16, right: u16) -> u16 {
    let mut polynomial = 0u32;
    for bit in 0..16 {
        if right & (1 << bit) != 0 {
            polynomial ^= u32::from(left) << bit;
        }
    }
    for bit in (16..=30).rev() {
        if polynomial & (1 << bit) != 0 {
            polynomial ^= 0x1_100bu32 << (bit - 16);
        }
    }
    polynomial as u16
}

fn reference_power(mut value: u16, mut exponent: u32) -> u16 {
    let mut product = 1;
    while exponent != 0 {
        if exponent & 1 != 0 {
            product = reference_multiply(product, value);
        }
        value = reference_multiply(value, value);
        exponent >>= 1;
    }
    product
}

#[test]
fn published_input_constants_match_main_order_and_recovery_exponents() {
    let constants = [2, 4, 16, 128, 256, 2048, 8192, 16384, 4107, 32856, 17132];
    for (index, value) in constants.into_iter().enumerate() {
        assert_eq!(gf16::coefficient(index as u32, 0).unwrap(), 1);
        assert_eq!(gf16::coefficient(index as u32, 1).unwrap(), value);
        for exponent in [2, 3, 17, 257, 65534] {
            assert_eq!(
                gf16::coefficient(index as u32, exponent).unwrap(),
                reference_power(value, exponent)
            );
        }
    }
}

#[test]
fn polynomial_basis_products_and_all_field_values_match_independent_long_division() {
    for left in 0..16 {
        for right in 0..16 {
            assert_eq!(
                gf16::multiply(1 << left, 1 << right),
                reference_multiply(1 << left, 1 << right)
            );
        }
    }
    for value in 0..=u16::MAX {
        for factor in [2, 0x5a5a] {
            assert_eq!(
                gf16::multiply(value, factor),
                reference_multiply(value, factor)
            );
        }
    }
    assert_eq!(gf16::multiply(0x8000, 2), 0x100b);
    assert_eq!(gf16::multiply(0xffff, 2), 0xeff5);
    assert_eq!(gf16::inverse(2), Some(0x8805));
}

#[test]
fn every_nonzero_field_value_has_a_verified_inverse_and_zero_has_none() {
    assert_eq!(gf16::inverse(0), None);
    assert_eq!(gf16::inverse(1), Some(1));
    for value in 1..=u16::MAX {
        let inverse = gf16::inverse(value).unwrap();
        assert_eq!(reference_multiply(value, inverse), 1);
        assert_eq!(gf16::multiply(value, inverse), 1);
    }
}

#[test]
fn every_input_constant_is_distinct_and_has_the_full_field_period() {
    let mut seen = vec![false; 65536];
    for index in 0..gf16::MAX_INPUT_SLICES {
        let value = gf16::coefficient(index, 1).unwrap();
        assert_ne!(value, 0);
        assert!(!seen[usize::from(value)], "repeated constant at {index}");
        seen[usize::from(value)] = true;
        assert_eq!(gf16::power(value, gf16::FIELD_PERIOD), 1);
        for divisor in [3, 5, 17, 257] {
            assert_ne!(gf16::power(value, gf16::FIELD_PERIOD / divisor), 1);
        }
    }
    assert!(gf16::coefficient(gf16::MAX_INPUT_SLICES, 0).is_err());
    assert!(gf16::coefficient(u32::MAX, 1).is_err());
    assert!(gf16::coefficient(0, gf16::FIELD_PERIOD).is_err());
    assert!(gf16::coefficient(0, u32::MAX).is_err());
}

#[test]
fn powers_include_zero_empty_products_and_full_width_exponents_without_overflow() {
    for value in [0, 1, 2, 0x8000, 0x1234, 0xffff] {
        for exponent in [0, 1, 2, 65534, 65535, 65536, u32::MAX] {
            assert_eq!(
                gf16::power(value, exponent),
                reference_power(value, exponent)
            );
        }
        assert_eq!(gf16::multiply(value, 0), 0);
        assert_eq!(gf16::multiply(value, 1), value);
    }
}

#[test]
fn scaled_sums_use_little_endian_words_and_preserve_the_read_only_input() {
    let input = [0x00, 0x80, 0x01, 0x00, 0xff, 0xff, 0x34, 0x12];
    let mut output = [0; 8];
    gf16::add_scaled(&mut output, &input, 2).unwrap();
    assert_eq!(output, [0x0b, 0x10, 0x02, 0x00, 0xf5, 0xef, 0x68, 0x24]);
    let doubled = output;
    gf16::add_scaled(&mut output, &input, 0).unwrap();
    assert_eq!(output, doubled);
    gf16::add_scaled(&mut output, &input, 2).unwrap();
    assert_eq!(output, [0; 8]);
    gf16::add_scaled(&mut output, &input, 1).unwrap();
    assert_eq!(output, input);
    assert_eq!(input, [0x00, 0x80, 0x01, 0x00, 0xff, 0xff, 0x34, 0x12]);
}

#[test]
fn coefficient_buffers_cross_chunks_with_correct_original_polynomial_results() {
    let input: Vec<u8> = (0..gf16::MAX_BUFFER_BYTES)
        .map(|index| (index.wrapping_mul(37) >> 3) as u8)
        .collect();
    let mut output = vec![0x5a; input.len()];
    let factor = gf16::coefficient(19, 7).unwrap();
    gf16::add_scaled(&mut output, &input, factor).unwrap();
    for (actual, source) in output
        .as_chunks::<2>()
        .0
        .iter()
        .zip(input.as_chunks::<2>().0)
    {
        assert_eq!(
            u16::from_le_bytes(*actual),
            0x5a5a ^ reference_multiply(u16::from_le_bytes(*source), factor)
        );
    }
    gf16::add_scaled(&mut output, &input, factor).unwrap();
    assert!(output.iter().all(|byte| *byte == 0x5a));
}

#[test]
fn invalid_buffers_and_initial_cancellation_never_modify_caller_output() {
    let mut output = [0x5a; 4];
    assert!(gf16::add_scaled(&mut output, &[1, 2], 1).is_err());
    assert_eq!(output, [0x5a; 4]);
    let mut odd = [0x5a; 3];
    assert!(gf16::add_scaled(&mut odd, &[1, 2, 3], 2).is_err());
    assert_eq!(odd, [0x5a; 3]);
    let cancelled = AtomicBool::new(false);
    for factor in [0, 1, 2] {
        assert!(
            gf16::add_scaled_cancellable(&mut output, &[1, 2, 3, 4], factor, &cancelled).is_err()
        );
        assert_eq!(output, [0x5a; 4]);
    }
    let input = vec![1; gf16::MAX_BUFFER_BYTES + 2];
    let mut excessive = vec![0x5a; input.len()];
    assert!(gf16::add_scaled(&mut excessive, &input, 0).is_err());
    assert!(excessive.iter().all(|byte| *byte == 0x5a));
    gf16::add_scaled(&mut [], &[], 2).unwrap();
    assert!(gf16::add_scaled_cancellable(&mut [], &[], 2, &cancelled).is_err());
}
