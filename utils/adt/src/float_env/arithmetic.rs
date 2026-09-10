use super::format::{Decoded, Finite, Format, shift_right_jam};
use super::{FloatOp, FloatResult, RoundingMode};

pub(super) fn eval(
    op: FloatOp,
    format: Format,
    bits: [u64; 3],
    rounding: RoundingMode,
) -> FloatResult {
    let values = bits.map(|bits| format.decode(bits));
    let count = match op {
        FloatOp::Sqrt => 1,
        FloatOp::Fma => 3,
        _ => 2,
    };
    let invalid_product = matches!(op, FloatOp::Mul | FloatOp::Fma)
        && (matches!(values[0], Decoded::Infinity(_)) && is_zero(values[1])
            || matches!(values[1], Decoded::Infinity(_)) && is_zero(values[0]));
    if let Some(result) = format.propagate_nan(&values[..count], invalid_product) {
        return result;
    }
    if invalid_product {
        return invalid(format);
    }
    match op {
        FloatOp::Add | FloatOp::Sub => {
            let right = if op == FloatOp::Sub {
                negate(values[1])
            } else {
                values[1]
            };
            add(format, values[0], right, rounding)
        }
        FloatOp::Mul | FloatOp::Fma => {
            let product = multiply(values[0], values[1]);
            if op == FloatOp::Fma {
                add(format, product, values[2], rounding)
            } else {
                finish(format, product, rounding)
            }
        }
        FloatOp::Div => divide(format, values[0], values[1], rounding),
        FloatOp::Sqrt => sqrt(format, values[0], rounding),
        FloatOp::Convert
        | FloatOp::SignedToFloat
        | FloatOp::UnsignedToFloat
        | FloatOp::FloatToSigned
        | FloatOp::FloatToUnsigned => unreachable!(),
    }
}

fn invalid(format: Format) -> FloatResult {
    FloatResult {
        bits: format.quiet_nan(),
        flags: 16,
    }
}

fn is_zero(value: Decoded) -> bool {
    matches!(value, Decoded::Finite(Finite { significand: 0, .. }))
}

fn negative(value: Decoded) -> bool {
    match value {
        Decoded::Finite(value) => value.negative,
        Decoded::Infinity(negative) => negative,
        Decoded::Nan { .. } => unreachable!(),
    }
}

fn negate(value: Decoded) -> Decoded {
    match value {
        Decoded::Finite(value) => Decoded::Finite(Finite {
            negative: !value.negative,
            ..value
        }),
        Decoded::Infinity(negative) => Decoded::Infinity(!negative),
        Decoded::Nan { .. } => unreachable!(),
    }
}

fn finish(format: Format, value: Decoded, rounding: RoundingMode) -> FloatResult {
    match value {
        Decoded::Finite(value) => format.pack(value, rounding),
        Decoded::Infinity(negative) => FloatResult {
            bits: format.infinity(negative),
            flags: 0,
        },
        Decoded::Nan { .. } => unreachable!(),
    }
}

fn add(format: Format, left: Decoded, right: Decoded, rounding: RoundingMode) -> FloatResult {
    match (left, right) {
        (Decoded::Infinity(a), Decoded::Infinity(b)) if a != b => invalid(format),
        (Decoded::Infinity(sign), _) | (_, Decoded::Infinity(sign)) => {
            finish(format, Decoded::Infinity(sign), rounding)
        }
        (Decoded::Finite(left), Decoded::Finite(right)) => {
            format.pack(add_finite(left, right, rounding), rounding)
        }
        _ => unreachable!(),
    }
}

fn add_finite(left: Finite, right: Finite, rounding: RoundingMode) -> Finite {
    if left.significand == 0 && right.significand == 0 {
        return Finite {
            negative: if left.negative == right.negative {
                left.negative
            } else {
                rounding == RoundingMode::TowardNegative
            },
            significand: 0,
            exponent: 0,
        };
    }
    if left.significand == 0 {
        return right;
    }
    if right.significand == 0 {
        return left;
    }
    let top = |value: Finite| value.exponent + 127 - value.significand.leading_zeros() as i32;
    let exponent = top(left).max(top(right)) - 120;
    let align = |value: Finite| {
        let shift = value.exponent - exponent;
        if shift >= 0 {
            value.significand << shift
        } else {
            shift_right_jam(value.significand, (-shift) as u32)
        }
    };
    let a = align(left);
    let b = align(right);
    let (negative, significand) = if left.negative == right.negative {
        (left.negative, a + b)
    } else if a == b {
        (rounding == RoundingMode::TowardNegative, 0)
    } else if a > b {
        (left.negative, a - b)
    } else {
        (right.negative, b - a)
    };
    Finite {
        negative,
        significand,
        exponent,
    }
}

fn multiply(left: Decoded, right: Decoded) -> Decoded {
    let negative = negative(left) ^ negative(right);
    match (left, right) {
        (Decoded::Finite(a), Decoded::Finite(b)) => Decoded::Finite(Finite {
            negative,
            significand: a.significand * b.significand,
            exponent: a.exponent + b.exponent,
        }),
        _ => Decoded::Infinity(negative),
    }
}

fn divide(format: Format, left: Decoded, right: Decoded, rounding: RoundingMode) -> FloatResult {
    let negative = negative(left) ^ negative(right);
    match (left, right) {
        (Decoded::Infinity(_), Decoded::Infinity(_)) => invalid(format),
        (Decoded::Infinity(_), _) => FloatResult {
            bits: format.infinity(negative),
            flags: 0,
        },
        (_, Decoded::Infinity(_)) => FloatResult {
            bits: format.sign(negative),
            flags: 0,
        },
        (Decoded::Finite(a), Decoded::Finite(b)) => {
            if b.significand == 0 {
                return if a.significand == 0 {
                    invalid(format)
                } else {
                    FloatResult {
                        bits: format.infinity(negative),
                        flags: 8,
                    }
                };
            }
            if a.significand == 0 {
                return FloatResult {
                    bits: format.sign(negative),
                    flags: 0,
                };
            }
            let a_top = 127 - a.significand.leading_zeros();
            let b_top = 127 - b.significand.leading_zeros();
            let numerator = a.significand << (126 - a_top);
            let denominator = b.significand << (63 - b_top);
            let significand = (numerator / denominator) | u128::from(numerator % denominator != 0);
            format.pack(
                Finite {
                    negative,
                    significand,
                    exponent: a.exponent - b.exponent + a_top as i32 - b_top as i32 - 63,
                },
                rounding,
            )
        }
        _ => unreachable!(),
    }
}

fn sqrt(format: Format, value: Decoded, rounding: RoundingMode) -> FloatResult {
    match value {
        Decoded::Infinity(false) => finish(format, value, rounding),
        Decoded::Infinity(true) => invalid(format),
        Decoded::Finite(mut value) => {
            if value.significand == 0 {
                return format.pack(value, rounding);
            }
            if value.negative {
                return invalid(format);
            }
            if value.exponent & 1 != 0 {
                value.significand <<= 1;
                value.exponent -= 1;
            }
            let top = 127 - value.significand.leading_zeros();
            let shift = (126 - top) & !1;
            let radicand = value.significand << shift;
            let root = radicand.isqrt();
            format.pack(
                Finite {
                    negative: false,
                    significand: root | u128::from(root * root != radicand),
                    exponent: value.exponent / 2 - shift as i32 / 2,
                },
                rounding,
            )
        }
        Decoded::Nan { .. } => unreachable!(),
    }
}
