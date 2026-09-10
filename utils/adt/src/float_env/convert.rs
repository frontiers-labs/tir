use super::format::{Decoded, Finite, Format, round_shift};
use super::{FloatResult, FloatWidth, RoundingMode};

pub(super) fn from_integer(
    bits: u64,
    source: FloatWidth,
    destination: Format,
    signed: bool,
    rounding: RoundingMode,
) -> FloatResult {
    let (negative, significand) = match (source, signed) {
        (FloatWidth::W32, true) => ((bits as i32) < 0, (bits as i32).unsigned_abs() as u128),
        (FloatWidth::W64, true) => ((bits as i64) < 0, (bits as i64).unsigned_abs() as u128),
        (FloatWidth::W32, false) => (false, bits as u32 as u128),
        (FloatWidth::W64, false) => (false, bits as u128),
    };
    destination.pack(
        Finite {
            negative,
            significand,
            exponent: 0,
        },
        rounding,
    )
}

pub(super) fn float(
    bits: u64,
    source: Format,
    destination: Format,
    rounding: RoundingMode,
) -> FloatResult {
    match source.decode(bits) {
        Decoded::Finite(value) => destination.pack(value, rounding),
        Decoded::Infinity(negative) => FloatResult {
            bits: destination.infinity(negative),
            flags: 0,
        },
        Decoded::Nan { bits, signaling } => {
            let payload = bits & ((1 << source.fraction) - 1);
            let payload = if destination.fraction >= source.fraction {
                payload << (destination.fraction - source.fraction)
            } else {
                payload >> (source.fraction - destination.fraction)
            };
            FloatResult {
                bits: destination.sign(bits & source.sign(true) != 0)
                    | destination.quiet_nan()
                    | payload,
                flags: u8::from(signaling) << 4,
            }
        }
    }
}

pub(super) fn to_integer(
    bits: u64,
    source: Format,
    destination: FloatWidth,
    signed: bool,
    rounding: RoundingMode,
) -> FloatResult {
    let invalid = FloatResult { bits: 0, flags: 16 };
    let Decoded::Finite(value) = source.decode(bits) else {
        return invalid;
    };
    let width = match destination {
        FloatWidth::W32 => 32,
        FloatWidth::W64 => 64,
    };
    let (magnitude, inexact) = if value.exponent < 0 {
        round_shift(
            value.significand,
            (-value.exponent) as u32,
            value.negative,
            rounding,
        )
    } else {
        if value.exponent >= width {
            return invalid;
        }
        (value.significand << value.exponent, false)
    };
    let limit = if signed {
        (1u128 << (width - 1)) - u128::from(!value.negative)
    } else {
        (1u128 << width) - 1
    };
    if magnitude > limit || (!signed && value.negative && magnitude != 0) {
        return invalid;
    }
    let bits = if value.negative {
        0u64.wrapping_sub(magnitude as u64)
    } else {
        magnitude as u64
    };
    FloatResult {
        bits: if width == 32 {
            bits as u32 as u64
        } else {
            bits
        },
        flags: u8::from(inexact),
    }
}
