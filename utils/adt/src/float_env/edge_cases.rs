use super::*;

#[test]
fn cancellation_preserves_fused_precision_and_avoids_intermediate_overflow() {
    for (width, operands, bits) in [
        (
            FloatWidth::W32,
            [0x7f7f_ffff, 0x4000_0000, 0xff7f_ffff],
            0x7f7f_ffff,
        ),
        (
            FloatWidth::W64,
            [
                0x7fef_ffff_ffff_ffff,
                0x4000_0000_0000_0000,
                0xffef_ffff_ffff_ffff,
            ],
            0x7fef_ffff_ffff_ffff,
        ),
        (
            FloatWidth::W64,
            [
                0x3ff0_0000_0000_0001,
                0x3fef_ffff_ffff_fffe,
                0xbff0_0000_0000_0000,
            ],
            0xb970_0000_0000_0000,
        ),
    ] {
        assert_eq!(
            eval_float(
                FloatOp::Fma,
                width,
                width,
                operands,
                RoundingMode::TiesToEven
            ),
            FloatResult { bits, flags: 0 }
        );
    }
}

#[test]
fn exact_zero_sign_follows_rounding_and_operand_signs() {
    for rounding in [
        RoundingMode::TiesToEven,
        RoundingMode::TiesToAway,
        RoundingMode::TowardZero,
        RoundingMode::TowardNegative,
        RoundingMode::TowardPositive,
    ] {
        let negative_zero = u64::from(rounding == RoundingMode::TowardNegative) << 31;
        assert_eq!(
            eval_float(
                FloatOp::Sub,
                FloatWidth::W32,
                FloatWidth::W32,
                [0x3f80_0000, 0x3f80_0000, 0],
                rounding
            ),
            FloatResult {
                bits: negative_zero,
                flags: 0
            }
        );
        assert_eq!(
            eval_float(
                FloatOp::Add,
                FloatWidth::W32,
                FloatWidth::W32,
                [0x8000_0000, 0x8000_0000, 0],
                rounding
            ),
            FloatResult {
                bits: 0x8000_0000,
                flags: 0
            }
        );
    }
}

#[test]
fn overflow_result_observes_sign_and_rounding() {
    for rounding in [
        RoundingMode::TiesToEven,
        RoundingMode::TiesToAway,
        RoundingMode::TowardZero,
        RoundingMode::TowardNegative,
        RoundingMode::TowardPositive,
    ] {
        for negative in [false, true] {
            let sign = u64::from(negative) << 31;
            let infinite = match rounding {
                RoundingMode::TiesToEven | RoundingMode::TiesToAway => true,
                RoundingMode::TowardZero => false,
                RoundingMode::TowardNegative => negative,
                RoundingMode::TowardPositive => !negative,
            };
            let bits = sign | if infinite { 0x7f80_0000 } else { 0x7f7f_ffff };
            assert_eq!(
                eval_float(
                    FloatOp::Mul,
                    FloatWidth::W32,
                    FloatWidth::W32,
                    [sign | 0x7f7f_ffff, 0x4000_0000, 0],
                    rounding
                ),
                FloatResult { bits, flags: 5 }
            );
        }
    }
}

#[test]
fn tininess_is_detected_after_rounding() {
    for (rounding, bits, flags) in [
        (RoundingMode::TiesToEven, 0x0080_0000, 1),
        (RoundingMode::TiesToAway, 0x0080_0000, 1),
        (RoundingMode::TowardPositive, 0x0080_0000, 1),
        (RoundingMode::TowardZero, 0x007f_ffff, 3),
        (RoundingMode::TowardNegative, 0x007f_ffff, 3),
    ] {
        assert_eq!(
            eval_float(
                FloatOp::Convert,
                FloatWidth::W64,
                FloatWidth::W32,
                [0x380f_ffff_f000_0000, 0, 0],
                rounding
            ),
            FloatResult { bits, flags }
        );
    }
}

#[test]
fn tiny_unbounded_result_raises_underflow_when_encoded_as_normal() {
    assert_eq!(
        eval_float(
            FloatOp::Mul,
            FloatWidth::W32,
            FloatWidth::W32,
            [0x0080_0000, 0x3f7f_ffff, 0],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: 0x0080_0000,
            flags: 3
        }
    );
}

#[test]
fn nan_payloads_and_sign_survive_quieting_and_conversion() {
    assert_eq!(
        eval_float(
            FloatOp::Sqrt,
            FloatWidth::W32,
            FloatWidth::W32,
            [0xff81_2345, 0, 0],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: 0xffc1_2345,
            flags: 16
        }
    );
    assert_eq!(
        eval_float(
            FloatOp::Add,
            FloatWidth::W32,
            FloatWidth::W32,
            [0x7fc1_1111, 0xff81_2345, 0],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: 0x7fc1_1111,
            flags: 16
        }
    );
    let widened = 0xfff0_0000_0000_0000 | (0x0041_2345u64 << 29);
    assert_eq!(
        eval_float(
            FloatOp::Convert,
            FloatWidth::W32,
            FloatWidth::W64,
            [0xff81_2345, 0, 0],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: widened,
            flags: 16
        }
    );
    assert_eq!(
        eval_float(
            FloatOp::Convert,
            FloatWidth::W64,
            FloatWidth::W32,
            [widened | 123, 0, 0],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: 0xffc1_2345,
            flags: 0
        }
    );
}

#[test]
fn integer_rounding_precedes_range_validation() {
    for (rounding, bits, flags) in [
        (RoundingMode::TiesToEven, 0, 1),
        (RoundingMode::TiesToAway, 0, 1),
        (RoundingMode::TowardZero, 0, 1),
        (RoundingMode::TowardPositive, 0, 1),
        (RoundingMode::TowardNegative, 0, 16),
    ] {
        assert_eq!(
            eval_float(
                FloatOp::FloatToUnsigned,
                FloatWidth::W32,
                FloatWidth::W32,
                [0xbe80_0000, 0, 0],
                rounding
            ),
            FloatResult { bits, flags }
        );
    }
    assert_eq!(
        eval_float(
            FloatOp::FloatToSigned,
            FloatWidth::W64,
            FloatWidth::W32,
            [(-2_147_483_648.5f64).to_bits(), 0, 0],
            RoundingMode::TiesToEven
        ),
        FloatResult {
            bits: 0x8000_0000,
            flags: 1
        }
    );
}
