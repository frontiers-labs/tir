use std::cmp::Ordering;

use super::{FloatResult, FloatWidth, RoundingMode};

#[derive(Clone, Copy)]
pub(super) struct Format {
    pub fraction: u32,
    pub exponent: u32,
}

#[derive(Clone, Copy)]
pub(super) struct Finite {
    pub negative: bool,
    pub significand: u128,
    pub exponent: i32,
}

#[derive(Clone, Copy)]
pub(super) enum Decoded {
    Finite(Finite),
    Infinity(bool),
    Nan { bits: u64, signaling: bool },
}

impl Format {
    pub fn new(width: FloatWidth) -> Self {
        match width {
            FloatWidth::W32 => Self {
                fraction: 23,
                exponent: 8,
            },
            FloatWidth::W64 => Self {
                fraction: 52,
                exponent: 11,
            },
        }
    }

    pub fn sign(self, negative: bool) -> u64 {
        u64::from(negative) << (self.fraction + self.exponent)
    }

    pub fn infinity(self, negative: bool) -> u64 {
        self.sign(negative) | (((1 << self.exponent) - 1) << self.fraction)
    }

    pub fn quiet_nan(self) -> u64 {
        self.infinity(false) | (1 << (self.fraction - 1))
    }

    pub fn decode(self, bits: u64) -> Decoded {
        let negative = bits & self.sign(true) != 0;
        let fraction = bits & ((1 << self.fraction) - 1);
        let exponent = ((bits >> self.fraction) & ((1 << self.exponent) - 1)) as i32;
        if exponent == (1 << self.exponent) - 1 {
            return if fraction == 0 {
                Decoded::Infinity(negative)
            } else {
                Decoded::Nan {
                    bits: self.sign(negative) | self.infinity(false) | fraction,
                    signaling: fraction & (1 << (self.fraction - 1)) == 0,
                }
            };
        }
        let significand = fraction | (u64::from(exponent != 0) << self.fraction);
        Decoded::Finite(Finite {
            negative,
            significand: significand.into(),
            exponent: exponent.max(1) - self.bias() - self.fraction as i32,
        })
    }

    fn bias(self) -> i32 {
        (1 << (self.exponent - 1)) - 1
    }

    pub fn pack(self, value: Finite, rounding: RoundingMode) -> FloatResult {
        if value.significand == 0 {
            return FloatResult {
                bits: self.sign(value.negative),
                flags: 0,
            };
        }
        let top = 127 - value.significand.leading_zeros() as i32;
        let minimum = 1 - self.bias();
        let unbounded_exponent = value.exponent + top;
        let tiny = if unbounded_exponent == minimum - 1 {
            let shift = top - self.fraction as i32;
            let rounded = if shift > 0 {
                round_shift(value.significand, shift as u32, value.negative, rounding).0
            } else {
                value.significand << -shift
            };
            rounded < 1 << (self.fraction + 1)
        } else {
            unbounded_exponent < minimum
        };
        let mut quantum = (value.exponent + top).max(minimum) - self.fraction as i32;
        let shift = quantum - value.exponent;
        let (mut significand, inexact) = if shift > 0 {
            round_shift(value.significand, shift as u32, value.negative, rounding)
        } else {
            (value.significand << -shift, false)
        };
        if significand >= 1 << (self.fraction + 1) {
            significand >>= 1;
            quantum += 1;
        }
        let exponent = quantum + self.fraction as i32;
        if exponent > self.bias() {
            let infinite = match rounding {
                RoundingMode::TiesToEven | RoundingMode::TiesToAway => true,
                RoundingMode::TowardZero => false,
                RoundingMode::TowardNegative => value.negative,
                RoundingMode::TowardPositive => !value.negative,
            };
            return FloatResult {
                bits: self.infinity(value.negative) - u64::from(!infinite),
                flags: 5,
            };
        }
        let subnormal = significand < 1 << self.fraction;
        let biased = if subnormal {
            0
        } else {
            (exponent + self.bias()) as u64
        };
        FloatResult {
            bits: self.sign(value.negative)
                | (biased << self.fraction)
                | (significand as u64 & ((1 << self.fraction) - 1)),
            flags: u8::from(inexact) | (u8::from(inexact && tiny) << 1),
        }
    }

    pub fn propagate_nan(self, values: &[Decoded], invalid: bool) -> Option<FloatResult> {
        let mut result = None;
        let mut flags = u8::from(invalid) << 4;
        for value in values {
            if let Decoded::Nan { bits, signaling } = *value {
                result.get_or_insert(bits | (1 << (self.fraction - 1)));
                flags |= u8::from(signaling) << 4;
            }
        }
        result.map(|bits| FloatResult { bits, flags })
    }
}

pub(super) fn round_shift(
    value: u128,
    shift: u32,
    negative: bool,
    rounding: RoundingMode,
) -> (u128, bool) {
    if shift == 0 {
        return (value, false);
    }
    let (truncated, remainder, half) = if shift < 128 {
        let remainder = value & ((1u128 << shift) - 1);
        (
            value >> shift,
            remainder,
            remainder.cmp(&(1u128 << (shift - 1))),
        )
    } else {
        let half = if shift == 128 {
            value.cmp(&(1u128 << 127))
        } else {
            Ordering::Less
        };
        (0, value, half)
    };
    let inexact = remainder != 0;
    let increment = inexact
        && match rounding {
            RoundingMode::TiesToEven => {
                half == Ordering::Greater || (half == Ordering::Equal && truncated & 1 != 0)
            }
            RoundingMode::TiesToAway => half != Ordering::Less,
            RoundingMode::TowardZero => false,
            RoundingMode::TowardNegative => negative,
            RoundingMode::TowardPositive => !negative,
        };
    (truncated + u128::from(increment), inexact)
}

pub(super) fn shift_right_jam(value: u128, shift: u32) -> u128 {
    match shift {
        0 => value,
        1..=127 => (value >> shift) | u128::from(value << (128 - shift) != 0),
        _ => u128::from(value != 0),
    }
}
