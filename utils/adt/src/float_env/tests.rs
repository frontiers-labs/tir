use super::*;

#[test]
fn unsigned_conversion_avoids_double_rounding() {
    assert_eq!(
        eval_float(
            FloatOp::UnsignedToFloat,
            FloatWidth::W64,
            FloatWidth::W32,
            [0x8000_0080_0000_0001, 0, 0],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: 0x5f00_0001,
            flags: 1
        }
    );
}

#[test]
fn fused_invalid_product_raises_invalid_with_nan_addend() {
    assert_eq!(
        eval_float(
            FloatOp::Fma,
            FloatWidth::W64,
            FloatWidth::W64,
            [0x7ff0_0000_0000_0000, 0, 0x7ff8_1234_5678_9abc],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: 0x7ff8_1234_5678_9abc,
            flags: 16
        }
    );
}

#[test]
fn narrowing_float_conversion_observes_all_rounding_modes() {
    for (rounding, bits) in [
        (RoundingMode::TiesToEven, 0x3f80_0000),
        (RoundingMode::TowardZero, 0x3f80_0000),
        (RoundingMode::TowardNegative, 0x3f80_0000),
        (RoundingMode::TowardPositive, 0x3f80_0001),
        (RoundingMode::TiesToAway, 0x3f80_0001),
    ] {
        assert_eq!(
            eval_float(
                FloatOp::Convert,
                FloatWidth::W64,
                FloatWidth::W32,
                [0x3ff0_0000_1000_0000, 0, 0],
                rounding
            ),
            FloatResult { bits, flags: 1 }
        );
    }
}

#[test]
fn arithmetic_reports_ieee_results_and_flags() {
    for (op, operands, bits, flags) in [
        (FloatOp::Add, [0x3f80_0000, 0x3380_0000, 0], 0x3f80_0000, 1),
        (FloatOp::Sub, [0x3f80_0000, 0x3f80_0000, 0], 0, 0),
        (FloatOp::Mul, [0x7f7f_ffff, 0x4000_0000, 0], 0x7f80_0000, 5),
        (FloatOp::Mul, [1, 0x3f00_0000, 0], 0, 3),
        (FloatOp::Div, [0x3f80_0000, 0, 0], 0x7f80_0000, 8),
        (FloatOp::Sqrt, [0xbf80_0000, 0, 0], 0x7fc0_0000, 16),
    ] {
        assert_eq!(
            eval_float(
                op,
                FloatWidth::W32,
                FloatWidth::W32,
                operands,
                RoundingMode::TiesToEven
            ),
            FloatResult { bits, flags },
            "{op:?}"
        );
    }
}

#[test]
fn signed_integer_conversion_uses_source_sign_and_rounding() {
    assert_eq!(
        eval_float(
            FloatOp::SignedToFloat,
            FloatWidth::W32,
            FloatWidth::W32,
            [(-16_777_217i32) as u32 as u64, 0, 0],
            RoundingMode::TowardNegative
        ),
        FloatResult {
            bits: 0xcb80_0001,
            flags: 1
        }
    );
    assert_eq!(
        eval_float(
            FloatOp::SignedToFloat,
            FloatWidth::W64,
            FloatWidth::W64,
            [(-9_007_199_254_740_993i64) as u64, 0, 0],
            RoundingMode::TowardZero
        ),
        FloatResult {
            bits: 0xc340_0000_0000_0000,
            flags: 1
        }
    );
}

#[test]
fn invalid_integer_conversions_report_invalid() {
    for (op, source, destination, input) in [
        (
            FloatOp::FloatToSigned,
            FloatWidth::W32,
            FloatWidth::W32,
            0x7fc0_0000,
        ),
        (
            FloatOp::FloatToSigned,
            FloatWidth::W64,
            FloatWidth::W32,
            0x41e0_0000_0000_0000,
        ),
        (
            FloatOp::FloatToSigned,
            FloatWidth::W32,
            FloatWidth::W64,
            0xff80_0000,
        ),
        (
            FloatOp::FloatToUnsigned,
            FloatWidth::W64,
            FloatWidth::W32,
            0x7ff8_0000_0000_0000,
        ),
        (
            FloatOp::FloatToUnsigned,
            FloatWidth::W32,
            FloatWidth::W64,
            0x7f80_0000,
        ),
        (
            FloatOp::FloatToUnsigned,
            FloatWidth::W32,
            FloatWidth::W32,
            0xbf80_0000,
        ),
    ] {
        assert_eq!(
            eval_float(
                op,
                source,
                destination,
                [input, 0, 0],
                RoundingMode::TowardZero
            ),
            FloatResult { bits: 0, flags: 16 }
        );
    }
}

#[test]
fn fused_multiply_add_rounds_once() {
    assert_eq!(
        eval_float(
            FloatOp::Fma,
            FloatWidth::W32,
            FloatWidth::W32,
            [0x3f80_0001, 0x3f7f_fffe, 0xbf80_0000],
            RoundingMode::TiesToEven,
        ),
        FloatResult {
            bits: 0xa880_0000,
            flags: 0
        }
    );
}

#[test]
fn integer_conversion_observes_ties_rounding() {
    let convert = |rounding| {
        eval_float(
            FloatOp::UnsignedToFloat,
            FloatWidth::W32,
            FloatWidth::W32,
            [16_777_217, 0, 0],
            rounding,
        )
    };
    assert_eq!(
        convert(RoundingMode::TiesToEven),
        FloatResult {
            bits: 0x4b80_0000,
            flags: 1
        }
    );
    assert_eq!(
        convert(RoundingMode::TiesToAway),
        FloatResult {
            bits: 0x4b80_0001,
            flags: 1
        }
    );
}
