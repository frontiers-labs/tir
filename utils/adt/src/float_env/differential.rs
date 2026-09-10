use super::*;

fn random_bits(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

macro_rules! arithmetic_matches_native {
    ($name:ident, $float:ty, $integer:ty, $width:expr) => {
        #[test]
        fn $name() {
            let mut state = 0x1234_5678_9abc_def0;
            for _ in 0..50_000 {
                let operands = std::array::from_fn(|_| random_bits(&mut state) as $integer as u64);
                let [a, b, c] = operands.map(|bits| <$float>::from_bits(bits as $integer));
                if !a.is_finite() || !b.is_finite() || !c.is_finite() {
                    continue;
                }
                for (op, expected) in [
                    (FloatOp::Add, a + b),
                    (FloatOp::Sub, a - b),
                    (FloatOp::Mul, a * b),
                    (FloatOp::Div, a / b),
                    (FloatOp::Fma, a.mul_add(b, c)),
                    (FloatOp::Sqrt, a.sqrt()),
                ] {
                    if expected.is_nan() {
                        continue;
                    }
                    let result = eval_float(op, $width, $width, operands, RoundingMode::TiesToEven);
                    assert_eq!(
                        result.bits,
                        expected.to_bits() as u64,
                        "{op:?} {operands:x?}"
                    );
                }
            }
        }
    };
}

arithmetic_matches_native!(
    binary32_arithmetic_matches_native,
    f32,
    u32,
    FloatWidth::W32
);
arithmetic_matches_native!(
    binary64_arithmetic_matches_native,
    f64,
    u64,
    FloatWidth::W64
);

#[test]
fn conversions_match_native() {
    let mut state = 0xabc0_1234_5678_9def;
    for _ in 0..50_000 {
        let bits = random_bits(&mut state);
        for (op, destination, expected) in [
            (
                FloatOp::SignedToFloat,
                FloatWidth::W32,
                (bits as i64 as f32).to_bits() as u64,
            ),
            (
                FloatOp::SignedToFloat,
                FloatWidth::W64,
                (bits as i64 as f64).to_bits(),
            ),
            (
                FloatOp::UnsignedToFloat,
                FloatWidth::W32,
                (bits as f32).to_bits() as u64,
            ),
            (
                FloatOp::UnsignedToFloat,
                FloatWidth::W64,
                (bits as f64).to_bits(),
            ),
        ] {
            let result = eval_float(
                op,
                FloatWidth::W64,
                destination,
                [bits, 0, 0],
                RoundingMode::TiesToEven,
            );
            assert_eq!(result.bits, expected, "{op:?} {destination:?} {bits:x}");
        }
        let value = f64::from_bits(bits);
        if value.is_finite() {
            let result = eval_float(
                FloatOp::Convert,
                FloatWidth::W64,
                FloatWidth::W32,
                [bits, 0, 0],
                RoundingMode::TiesToEven,
            );
            assert_eq!(result.bits, (value as f32).to_bits() as u64, "{bits:x}");
        }
    }
}
