use std::cmp::Ordering;
use std::fmt;

/// Arbitrary-precision float over any exponent/mantissa widths: IEEE 754, BF16, FP8,
/// x86 extended, and custom formats.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct APFloat {
    exp_width: u32,
    /// Excludes the implicit leading bit unless `explicit_leading_bit`.
    mant_width: u32,
    explicit_leading_bit: bool,
    sign: bool,
    exponent: u32,
    mantissa_high: u64,
    mantissa_low: u64,
}

impl APFloat {
    /// Create a new APFloat with custom exponent and mantissa widths
    pub fn new(exp_width: u32, mant_width: u32, explicit_leading_bit: bool) -> Self {
        assert!(
            exp_width > 0 && exp_width <= 32,
            "Exponent width must be 1-32 bits"
        );
        assert!(
            mant_width > 0 && mant_width <= 128,
            "Mantissa width must be 1-128 bits"
        );

        APFloat {
            exp_width,
            mant_width,
            explicit_leading_bit,
            sign: false,
            exponent: 0,
            mantissa_high: 0,
            mantissa_low: 0,
        }
    }

    /// Create from raw bit representation
    pub fn from_bits(
        exp_width: u32,
        mant_width: u32,
        explicit_leading_bit: bool,
        bits: u128,
    ) -> Self {
        assert!(
            exp_width > 0 && exp_width <= 32,
            "Exponent width must be 1-32 bits"
        );
        assert!(
            mant_width > 0 && mant_width <= 128,
            "Mantissa width must be 1-128 bits"
        );

        let total_width = 1 + exp_width + mant_width;
        assert!(total_width <= 128, "Total width exceeds 128 bits");

        let sign_bit = total_width - 1;
        let sign = (bits >> sign_bit) & 1 == 1;

        let exp_mask = (1u32 << exp_width) - 1;
        let exponent = ((bits >> mant_width) as u32) & exp_mask;

        let mantissa_low = bits as u64;
        let mantissa_high = if mant_width > 64 {
            (bits >> 64) as u64
        } else {
            0
        };

        // Mask to only the mantissa bits
        let low_mask = if mant_width >= 64 {
            u64::MAX
        } else {
            (1u64 << mant_width) - 1
        };

        let high_mask = if mant_width > 64 {
            (1u64 << (mant_width - 64)) - 1
        } else {
            0
        };

        APFloat {
            exp_width,
            mant_width,
            explicit_leading_bit,
            sign,
            exponent,
            mantissa_high: mantissa_high & high_mask,
            mantissa_low: mantissa_low & low_mask,
        }
    }

    /// IEEE 754 binary16 (half precision): 1 sign, 5 exp, 10 mantissa
    pub fn half() -> Self {
        Self::new(5, 10, false)
    }

    /// BFloat16 (Brain Float): 1 sign, 8 exp, 7 mantissa
    pub fn bfloat16() -> Self {
        Self::new(8, 7, false)
    }

    /// IEEE 754 binary32 (single precision): 1 sign, 8 exp, 23 mantissa
    pub fn single() -> Self {
        Self::new(8, 23, false)
    }

    /// IEEE 754 binary64 (double precision): 1 sign, 11 exp, 52 mantissa
    pub fn double() -> Self {
        Self::new(11, 52, false)
    }

    /// IEEE 754 binary128 (quad precision): 1 sign, 15 exp, 112 mantissa
    pub fn quad() -> Self {
        Self::new(15, 112, false)
    }

    /// Get the exponent width
    pub fn exp_width(&self) -> u32 {
        self.exp_width
    }

    /// Get the mantissa width
    pub fn mant_width(&self) -> u32 {
        self.mant_width
    }

    /// Get the total bit width
    pub fn bit_width(&self) -> u32 {
        1 + self.exp_width + self.mant_width
    }

    /// Get the exponent bias (standard: 2^(exp_width-1) - 1)
    pub fn exponent_bias(&self) -> i32 {
        (1i32 << (self.exp_width - 1)) - 1
    }

    /// Create a zero value
    pub fn zero(
        exp_width: u32,
        mant_width: u32,
        explicit_leading_bit: bool,
        negative: bool,
    ) -> Self {
        APFloat {
            exp_width,
            mant_width,
            explicit_leading_bit,
            sign: negative,
            exponent: 0,
            mantissa_high: 0,
            mantissa_low: 0,
        }
    }

    /// Create positive or negative infinity
    pub fn infinity(
        exp_width: u32,
        mant_width: u32,
        explicit_leading_bit: bool,
        negative: bool,
    ) -> Self {
        let exp_max = (1u32 << exp_width) - 1;
        APFloat {
            exp_width,
            mant_width,
            explicit_leading_bit,
            sign: negative,
            exponent: exp_max,
            mantissa_high: 0,
            mantissa_low: 0,
        }
    }

    /// Create NaN (quiet NaN with highest mantissa bit set)
    pub fn nan(exp_width: u32, mant_width: u32, explicit_leading_bit: bool) -> Self {
        let exp_max = (1u32 << exp_width) - 1;
        let (mant_high, mant_low) = if mant_width > 64 {
            (1u64 << (mant_width - 64 - 1), 0)
        } else {
            (0, 1u64 << (mant_width - 1))
        };

        APFloat {
            exp_width,
            mant_width,
            explicit_leading_bit,
            sign: false,
            exponent: exp_max,
            mantissa_high: mant_high,
            mantissa_low: mant_low,
        }
    }

    /// Convert to raw bit representation
    pub fn to_bits(&self) -> u128 {
        let sign_bit = if self.sign { 1u128 } else { 0u128 };
        let sign_shifted = sign_bit << (self.bit_width() - 1);

        let exp_shifted = (self.exponent as u128) << self.mant_width;

        let mantissa = if self.mant_width > 64 {
            ((self.mantissa_high as u128) << 64) | (self.mantissa_low as u128)
        } else {
            self.mantissa_low as u128
        };

        sign_shifted | exp_shifted | mantissa
    }

    /// Create from f64 (creates a double precision APFloat)
    pub fn from_f64(value: f64) -> Self {
        Self::from_bits(11, 52, false, value.to_bits() as u128)
    }

    /// Convert to f32 (may lose precision or be inaccurate for non-standard formats)
    pub fn to_f32(&self) -> f32 {
        if self.is_binary32() {
            return f32::from_bits(self.to_bits() as u32);
        }
        self.to_f64() as f32
    }

    /// Convert to f64 (may lose precision for quad/extended formats)
    pub fn to_f64(&self) -> f64 {
        if self.exp_width == 11 && self.mant_width == 52 && !self.explicit_leading_bit {
            return f64::from_bits(self.to_bits() as u64);
        }

        if self.is_nan() {
            return f64::NAN;
        }
        if self.is_infinity() {
            return if self.sign {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
        }
        if self.is_zero() {
            return if self.sign { -0.0 } else { 0.0 };
        }

        let converted = self.convert(11, 52, false);
        f64::from_bits(converted.to_bits() as u64)
    }

    /// Convert to a different floating-point format
    pub fn convert(&self, new_exp_width: u32, new_mant_width: u32, new_explicit: bool) -> Self {
        if self.exp_width == new_exp_width
            && self.mant_width == new_mant_width
            && self.explicit_leading_bit == new_explicit
        {
            return self.clone();
        }

        if self.is_nan() {
            return Self::nan(new_exp_width, new_mant_width, new_explicit);
        }
        if self.is_infinity() {
            return Self::infinity(new_exp_width, new_mant_width, new_explicit, self.sign);
        }

        let (significand, exp2) = self.decompose();
        if significand == 0 {
            return Self::zero(new_exp_width, new_mant_width, new_explicit, self.sign);
        }

        Self::compose(
            new_exp_width,
            new_mant_width,
            new_explicit,
            self.sign,
            significand,
            exp2,
        )
    }

    /// The magnitude as an integer significand and a power of two, so that the
    /// value is `significand * 2^exp2`. A subnormal's exponent field reads as
    /// zero but its true exponent is the format's minimum, and it carries no
    /// implicit leading one -- conflating the two is what makes a subnormal
    /// come out scaled by the whole exponent range.
    fn decompose(&self) -> (u128, i32) {
        let mantissa = self.mantissa();
        let fraction_bits = self.fraction_bits() as i32;
        let bias = self.exponent_bias();

        if self.exponent == 0 {
            (mantissa, 1 - bias - fraction_bits)
        } else if self.explicit_leading_bit {
            (mantissa, self.exponent as i32 - bias - fraction_bits)
        } else {
            (
                mantissa | (1u128 << self.mant_width),
                self.exponent as i32 - bias - fraction_bits,
            )
        }
    }

    /// Encode `significand * 2^exp2` into the given format, rounding to nearest
    /// with ties to even. A value below the format's smallest normal keeps that
    /// exponent and denormalizes the significand instead, which is what makes
    /// the subnormal range representable rather than a hole at zero.
    fn compose(
        exp_width: u32,
        mant_width: u32,
        explicit: bool,
        sign: bool,
        significand: u128,
        exp2: i32,
    ) -> Self {
        let bias = (1i32 << (exp_width - 1)) - 1;
        let fraction_bits = (if explicit { mant_width - 1 } else { mant_width }) as i32;
        let msb = (127 - significand.leading_zeros()) as i32;

        let min_normal = 1 - bias;
        let mut exponent = (exp2 + msb).max(min_normal);
        let shift = exp2 - exponent + fraction_bits;

        let mut mantissa = if shift >= 0 {
            significand << shift
        } else {
            shift_right_round(significand, (-shift) as u32)
        };

        // Rounding can carry the significand into the next binade.
        if mantissa >> (fraction_bits + 1) != 0 {
            mantissa >>= 1;
            exponent += 1;
        }

        let exp_max = (1u32 << exp_width) - 1;
        if mantissa == 0 {
            return Self::zero(exp_width, mant_width, explicit, sign);
        }
        if exponent + bias >= exp_max as i32 {
            return Self::infinity(exp_width, mant_width, explicit, sign);
        }

        let (biased, stored) = if mantissa >> fraction_bits == 0 {
            (0, mantissa)
        } else if explicit {
            ((exponent + bias) as u32, mantissa)
        } else {
            (
                (exponent + bias) as u32,
                mantissa & ((1u128 << mant_width) - 1),
            )
        };

        APFloat {
            exp_width,
            mant_width,
            explicit_leading_bit: explicit,
            sign,
            exponent: biased,
            mantissa_high: (stored >> 64) as u64,
            mantissa_low: stored as u64,
        }
    }

    /// The mantissa field as a single integer.
    fn mantissa(&self) -> u128 {
        ((self.mantissa_high as u128) << 64) | (self.mantissa_low as u128)
    }

    /// Bits of the mantissa field that sit below the leading one, which the
    /// explicit-leading-bit formats store rather than imply.
    fn fraction_bits(&self) -> u32 {
        if self.explicit_leading_bit {
            self.mant_width - 1
        } else {
            self.mant_width
        }
    }

    /// Check if this is zero
    pub fn is_zero(&self) -> bool {
        self.exponent == 0 && self.mantissa_high == 0 && self.mantissa_low == 0
    }

    /// Check if this is infinity
    pub fn is_infinity(&self) -> bool {
        let exp_max = (1u32 << self.exp_width) - 1;
        self.exponent == exp_max && self.mantissa_high == 0 && self.mantissa_low == 0
    }

    /// Check if this is NaN
    pub fn is_nan(&self) -> bool {
        let exp_max = (1u32 << self.exp_width) - 1;
        self.exponent == exp_max && (self.mantissa_high != 0 || self.mantissa_low != 0)
    }

    /// Check if this is negative
    pub fn is_negative(&self) -> bool {
        self.sign
    }

    /// Negate the value
    pub fn neg(&self) -> Self {
        APFloat {
            exp_width: self.exp_width,
            mant_width: self.mant_width,
            explicit_leading_bit: self.explicit_leading_bit,
            sign: !self.sign,
            exponent: self.exponent,
            mantissa_high: self.mantissa_high,
            mantissa_low: self.mantissa_low,
        }
    }

    /// Absolute value
    pub fn abs(&self) -> Self {
        APFloat {
            exp_width: self.exp_width,
            mant_width: self.mant_width,
            explicit_leading_bit: self.explicit_leading_bit,
            sign: false,
            exponent: self.exponent,
            mantissa_high: self.mantissa_high,
            mantissa_low: self.mantissa_low,
        }
    }

    /// Add via native f64 arithmetic; may lose precision for non-f64 formats.
    pub fn add(&self, other: &APFloat) -> Self {
        self.assert_same_format(other);
        if self.is_binary32() {
            return self.with_native((self.to_f32() + other.to_f32()) as f64);
        }
        self.with_native(self.to_f64() + other.to_f64())
    }

    /// Subtract two floating-point numbers
    pub fn sub(&self, other: &APFloat) -> Self {
        self.assert_same_format(other);
        if self.is_binary32() {
            return self.with_native((self.to_f32() - other.to_f32()) as f64);
        }
        self.with_native(self.to_f64() - other.to_f64())
    }

    /// Multiply two floating-point numbers
    pub fn mul(&self, other: &APFloat) -> Self {
        self.assert_same_format(other);
        if self.is_binary32() {
            return self.with_native((self.to_f32() * other.to_f32()) as f64);
        }
        self.with_native(self.to_f64() * other.to_f64())
    }

    /// Divide two floating-point numbers
    pub fn div(&self, other: &APFloat) -> Self {
        self.assert_same_format(other);
        if self.is_binary32() {
            return self.with_native((self.to_f32() / other.to_f32()) as f64);
        }
        self.with_native(self.to_f64() / other.to_f64())
    }

    /// Square root
    pub fn sqrt(&self) -> Self {
        if self.is_binary32() {
            return self.with_native(self.to_f32().sqrt() as f64);
        }
        self.with_native(self.to_f64().sqrt())
    }

    /// Fused multiply-add: (self * b) + c
    pub fn fma(&self, b: &APFloat, c: &APFloat) -> Self {
        self.assert_same_format(b);
        self.assert_same_format(c);
        if self.is_binary32() {
            return self.with_native(self.to_f32().mul_add(b.to_f32(), c.to_f32()) as f64);
        }
        self.with_native(self.to_f64().mul_add(b.to_f64(), c.to_f64()))
    }

    /// IEEE compare: `None` when either is NaN. May lose precision for non-f64 formats.
    pub fn compare(&self, other: &APFloat) -> Option<Ordering> {
        if self.is_nan() || other.is_nan() {
            return None;
        }

        if self.is_zero() && other.is_zero() {
            return Some(Ordering::Equal);
        }

        if self.is_infinity() && other.is_infinity() {
            if self.sign == other.sign {
                return Some(Ordering::Equal);
            } else {
                return Some(if self.sign {
                    Ordering::Less
                } else {
                    Ordering::Greater
                });
            }
        }

        self.to_f64().partial_cmp(&other.to_f64())
    }

    /// Less than
    pub fn lt(&self, other: &APFloat) -> bool {
        matches!(self.compare(other), Some(Ordering::Less))
    }

    /// Less than or equal
    pub fn le(&self, other: &APFloat) -> bool {
        matches!(self.compare(other), Some(Ordering::Less | Ordering::Equal))
    }

    /// Greater than
    pub fn gt(&self, other: &APFloat) -> bool {
        matches!(self.compare(other), Some(Ordering::Greater))
    }

    /// Greater than or equal
    pub fn ge(&self, other: &APFloat) -> bool {
        matches!(
            self.compare(other),
            Some(Ordering::Greater | Ordering::Equal)
        )
    }

    /// IEEE 754-2019 minimumNumber: the non-NaN operand when exactly one is
    /// NaN, the canonical NaN when both are; -0.0 is smaller than +0.0.
    /// (Named `minnum`, LLVM-style: `Ord::min` would shadow an inherent `min`.)
    pub fn minnum(&self, other: &APFloat) -> Self {
        self.extreme(other, true)
    }

    /// IEEE 754-2019 maximumNumber: the non-NaN operand when exactly one is
    /// NaN, the canonical NaN when both are; +0.0 is larger than -0.0.
    pub fn maxnum(&self, other: &APFloat) -> Self {
        self.extreme(other, false)
    }

    fn extreme(&self, other: &APFloat, pick_less: bool) -> Self {
        self.assert_same_format(other);
        match (self.is_nan(), other.is_nan()) {
            (true, true) => {
                return APFloat::nan(self.exp_width, self.mant_width, self.explicit_leading_bit);
            }
            (true, false) => return other.clone(),
            (false, true) => return self.clone(),
            (false, false) => {}
        }
        if self.is_zero() && other.is_zero() && self.sign != other.sign {
            return if pick_less == self.sign {
                self.clone()
            } else {
                other.clone()
            };
        }
        if self.lt(other) == pick_less {
            self.clone()
        } else {
            other.clone()
        }
    }
}

/// Shift right by `shift`, rounding to nearest with ties to even.
fn shift_right_round(value: u128, shift: u32) -> u128 {
    if shift == 0 {
        return value;
    }
    if shift >= 128 {
        // Everything shifts out, so the result can only be the tie-break: a
        // discarded part above half of the last place, which needs the shift
        // to be exactly the width.
        return u128::from(shift == 128 && value > (1u128 << 127));
    }

    let kept = value >> shift;
    let discarded = value & ((1u128 << shift) - 1);
    let half = 1u128 << (shift - 1);
    if discarded > half || (discarded == half && kept & 1 == 1) {
        kept + 1
    } else {
        kept
    }
}

impl APFloat {
    /// Assert two values share exponent and mantissa widths.
    fn assert_same_format(&self, other: &APFloat) {
        assert_eq!(
            self.exp_width, other.exp_width,
            "Exponent widths must match"
        );
        assert_eq!(
            self.mant_width, other.mant_width,
            "Mantissa widths must match"
        );
    }

    /// The IEEE binary32 interchange format. Arithmetic on it has to be done in
    /// `f32`: computing in `f64` and narrowing the result afterwards rounds
    /// twice, which lands one ulp off for about one product in two thousand.
    fn is_binary32(&self) -> bool {
        self.exp_width == 8 && self.mant_width == 23 && !self.explicit_leading_bit
    }

    /// Wrap a native-f64 result back into this value's format.
    fn with_native(&self, value: f64) -> Self {
        Self::from_f64(value).convert(self.exp_width, self.mant_width, self.explicit_leading_bit)
    }

    /// IEEE total-order key (à la `f64::total_cmp`); only comparable within one format.
    fn total_key(&self) -> i128 {
        let magnitude = (self.to_bits() & ((1u128 << (self.bit_width() - 1)) - 1)) as i128;
        if self.sign { -magnitude - 1 } else { magnitude }
    }
}

impl Ord for APFloat {
    /// Structural total order (not IEEE: `NaN == NaN`, `-0 < +0`); use [`APFloat::compare`] for IEEE.
    fn cmp(&self, other: &Self) -> Ordering {
        self.total_key()
            .cmp(&other.total_key())
            .then_with(|| self.exp_width.cmp(&other.exp_width))
            .then_with(|| self.mant_width.cmp(&other.mant_width))
            .then_with(|| self.explicit_leading_bit.cmp(&other.explicit_leading_bit))
            .then_with(|| self.exponent.cmp(&other.exponent))
            .then_with(|| self.mantissa_high.cmp(&other.mantissa_high))
            .then_with(|| self.mantissa_low.cmp(&other.mantissa_low))
            .then_with(|| self.sign.cmp(&other.sign))
    }
}

impl PartialOrd for APFloat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for APFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_nan() {
            write!(f, "NaN")
        } else if self.is_infinity() {
            write!(f, "{}inf", if self.sign { "-" } else { "" })
        } else {
            write!(f, "{}", self.to_f64())
        }
    }
}

#[cfg(test)]
mod tests {

    // Arithmetic done in `f64` and narrowed afterwards rounds twice, which shows
    // up as a one-ulp error. Each operation must round exactly once, into the
    // format the operands are in.
    #[test]
    fn binary32_arithmetic_rounds_once() {
        let cases = [
            (0x7f7f7f7fu32, 0x81818181u32),
            (0x3fc00000, 0x40490fdb),
            (0x7f7fffff, 0x33800000),
            (0x00800001, 0x3f800001),
        ];
        for (a_bits, b_bits) in cases {
            let a = APFloat::from_bits(8, 23, false, a_bits as u128);
            let b = APFloat::from_bits(8, 23, false, b_bits as u128);
            let (x, y) = (f32::from_bits(a_bits), f32::from_bits(b_bits));
            assert_eq!(
                a.mul(&b).to_bits() as u32,
                (x * y).to_bits(),
                "mul {a_bits:#x}"
            );
            assert_eq!(
                a.add(&b).to_bits() as u32,
                (x + y).to_bits(),
                "add {a_bits:#x}"
            );
            assert_eq!(
                a.sub(&b).to_bits() as u32,
                (x - y).to_bits(),
                "sub {a_bits:#x}"
            );
            assert_eq!(
                a.div(&b).to_bits() as u32,
                (x / y).to_bits(),
                "div {a_bits:#x}"
            );
            assert_eq!(
                a.fma(&b, &b).to_bits() as u32,
                x.mul_add(y, y).to_bits(),
                "fma {a_bits:#x}"
            );
            assert_eq!(
                a.sqrt().to_bits() as u32,
                x.sqrt().to_bits(),
                "sqrt {a_bits:#x}"
            );
        }
    }
    use super::*;
    use proptest::prelude::*;

    // A result below the target format's smallest normal is a subnormal, not
    // zero: narrowing has to denormalize the significand instead of dropping
    // the exponent on the floor. Truncating leaves the mantissa reading as if
    // it were still normalized, which is wrong by many orders of magnitude
    // rather than by one ulp.
    #[test]
    fn narrowing_into_the_subnormal_range_keeps_the_value() {
        for bits in [
            0x0000_0001u32,
            0x0000_0003,
            0x0000_00ff,
            0x007f_ffff,
            0x0040_0000,
        ] {
            let wide = APFloat::from_f64(f32::from_bits(bits) as f64);
            assert_eq!(
                wide.convert(8, 23, false).to_bits() as u32,
                bits,
                "narrowing {bits:#x}"
            );
        }
    }

    // The same value seen through the wider format: a subnormal carries no
    // implicit leading one and its exponent is the format's minimum, not the
    // bias-relative reading of a zero exponent field.
    #[test]
    fn widening_a_subnormal_keeps_the_value() {
        for bits in [0x0000_0001u32, 0x0000_0003, 0x007f_ffff] {
            let narrow = APFloat::from_bits(8, 23, false, bits as u128);
            assert_eq!(
                narrow.to_f64(),
                f32::from_bits(bits) as f64,
                "widening {bits:#x}"
            );
        }
    }

    // Arithmetic whose result lands in the subnormal range, which is how the
    // narrowing bug reaches the simulator: every binary32 operation wraps its
    // result back into the format through a narrowing convert.
    #[test]
    fn binary32_arithmetic_reaches_subnormals() {
        let cases: [(u32, u32); 4] = [
            (0x0000_0001, 0x0000_0000),
            (0x0000_0003, 0x0000_0000),
            (0x0000_0001, 0x0000_0001),
            (0x0080_0000, 0x0080_0000),
        ];
        for (a_bits, b_bits) in cases {
            let a = APFloat::from_bits(8, 23, false, a_bits as u128);
            let b = APFloat::from_bits(8, 23, false, b_bits as u128);
            let (x, y) = (f32::from_bits(a_bits), f32::from_bits(b_bits));
            assert_eq!(
                a.add(&b).to_bits() as u32,
                (x + y).to_bits(),
                "add {a_bits:#x}"
            );
            assert_eq!(
                a.sub(&b).to_bits() as u32,
                (x - y).to_bits(),
                "sub {a_bits:#x}"
            );
        }
    }

    // Narrowing is round-to-nearest-even, including when the result is
    // subnormal and when rounding carries it up into the smallest normal.
    #[test]
    fn narrowing_rounds_to_nearest_even() {
        let ulp = f32::from_bits(1) as f64;
        let cases = [
            (ulp * 0.5, 0x0000_0000u32),
            (f64::from_bits((ulp * 0.5).to_bits() + 1), 0x0000_0001),
            (ulp * 1.5, 0x0000_0002),
            (ulp * 2.5, 0x0000_0002),
            (ulp * 3.5, 0x0000_0004),
            (f32::MIN_POSITIVE as f64 - ulp * 0.5, 0x0080_0000),
        ];
        for (value, want) in cases {
            assert_eq!(
                APFloat::from_f64(value).convert(8, 23, false).to_bits() as u32,
                want,
                "rounding {value:e}"
            );
        }
    }

    // Underflow past the smallest subnormal still gives a signed zero, and
    // overflow past the largest finite still gives a signed infinity.
    #[test]
    fn narrowing_saturates_at_both_ends() {
        let tiny = APFloat::from_f64(-f64::MIN_POSITIVE).convert(8, 23, false);
        assert_eq!(tiny.to_bits() as u32, 0x8000_0000);
        let huge = APFloat::from_f64(f64::MAX).convert(8, 23, false);
        assert_eq!(huge.to_bits() as u32, f32::INFINITY.to_bits());
    }

    /// Equality that treats any two NaNs as equal (f64 NaN != NaN otherwise).
    fn f64_eq(a: f64, b: f64) -> bool {
        a == b || (a.is_nan() && b.is_nan())
    }

    proptest! {
        // Narrowing has to agree with the hardware conversion over the whole
        // range, subnormals and rounding boundaries included.
        #[test]
        fn narrowing_to_binary32_matches_native(x in prop::num::f64::ANY) {
            let narrowed = APFloat::from_f64(x).convert(8, 23, false);
            let native = x as f32;
            if native.is_nan() {
                prop_assert!(narrowed.is_nan());
            } else {
                prop_assert_eq!(narrowed.to_bits() as u32, native.to_bits());
            }
        }

        // And widening has to be exact, since every binary32 value is a
        // binary64 value.
        #[test]
        fn widening_from_binary32_is_exact(bits in prop::num::u32::ANY) {
            let value = f32::from_bits(bits);
            prop_assume!(!value.is_nan());
            let widened = APFloat::from_bits(8, 23, false, bits as u128).to_f64();
            prop_assert_eq!(widened.to_bits(), (value as f64).to_bits());
        }

        #[test]
        fn test_add(x in prop::num::f64::ANY, y in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).add(&APFloat::from_f64(y));
            prop_assert!(f64_eq(res.to_f64(), x + y));
        }

        #[test]
        fn test_sub(x in prop::num::f64::ANY, y in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).sub(&APFloat::from_f64(y));
            prop_assert!(f64_eq(res.to_f64(), x - y));
        }

        #[test]
        fn test_mul(x in prop::num::f64::ANY, y in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).mul(&APFloat::from_f64(y));
            prop_assert!(f64_eq(res.to_f64(), x * y));
        }

        #[test]
        fn test_div(x in prop::num::f64::ANY, y in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).div(&APFloat::from_f64(y));
            prop_assert!(f64_eq(res.to_f64(), x / y));
        }

        #[test]
        fn test_sqrt(x in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).sqrt();
            prop_assert!(f64_eq(res.to_f64(), x.sqrt()));
        }

        #[test]
        fn test_fma(x in prop::num::f64::ANY, y in prop::num::f64::ANY, z in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).fma(&APFloat::from_f64(y), &APFloat::from_f64(z));
            prop_assert!(f64_eq(res.to_f64(), x.mul_add(y, z)));
        }

        #[test]
        fn test_neg(x in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).neg();
            prop_assert!(f64_eq(res.to_f64(), -x));
        }

        #[test]
        fn test_abs(x in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).abs();
            prop_assert!(f64_eq(res.to_f64(), x.abs()));
        }

        #[test]
        fn test_compare(x in prop::num::f64::ANY, y in prop::num::f64::ANY) {
            let res = APFloat::from_f64(x).compare(&APFloat::from_f64(y));
            prop_assert_eq!(res, x.partial_cmp(&y));
        }

        #[test]
        fn test_total_order_matches_f64(x in prop::num::f64::ANY, y in prop::num::f64::ANY) {
            prop_assert_eq!(APFloat::from_f64(x).cmp(&APFloat::from_f64(y)), x.total_cmp(&y));
        }

        #[test]
        fn test_ord_consistent_with_eq(x in prop::num::f64::ANY, y in prop::num::f64::ANY) {
            let a = APFloat::from_f64(x);
            let b = APFloat::from_f64(y);
            prop_assert_eq!(a == b, a.cmp(&b) == Ordering::Equal);
            prop_assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
        }
    }

    #[test]
    fn neg_zero_orders_below_pos_zero() {
        let pos = APFloat::from_f64(0.0);
        let neg = APFloat::from_f64(-0.0);
        assert_ne!(pos, neg);
        assert_eq!(neg.cmp(&pos), Ordering::Less);
    }

    #[test]
    fn nan_is_structurally_equal() {
        let a = APFloat::from_f64(f64::NAN);
        let b = APFloat::from_f64(f64::NAN);
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn min_max_pick_the_extreme_operand() {
        let a = APFloat::from_f64(1.5);
        let b = APFloat::from_f64(-2.5);
        assert_eq!(a.minnum(&b).to_f64(), -2.5);
        assert_eq!(a.maxnum(&b).to_f64(), 1.5);
        assert_eq!(b.minnum(&a).to_f64(), -2.5);
        assert_eq!(b.maxnum(&a).to_f64(), 1.5);
    }

    #[test]
    fn min_max_return_the_non_nan_operand() {
        let nan = APFloat::from_f64(f64::NAN);
        let one = APFloat::from_f64(1.0);
        assert_eq!(nan.minnum(&one).to_f64(), 1.0);
        assert_eq!(one.minnum(&nan).to_f64(), 1.0);
        assert_eq!(nan.maxnum(&one).to_f64(), 1.0);
        assert_eq!(one.maxnum(&nan).to_f64(), 1.0);
    }

    #[test]
    fn min_max_of_two_nans_is_nan() {
        let nan = APFloat::from_f64(f64::NAN);
        assert!(nan.minnum(&nan).is_nan());
        assert!(nan.maxnum(&nan).is_nan());
    }

    #[test]
    fn min_max_distinguish_signed_zero() {
        let pos = APFloat::from_f64(0.0);
        let neg = APFloat::from_f64(-0.0);
        assert!(pos.minnum(&neg).is_negative());
        assert!(!pos.maxnum(&neg).is_negative());
        assert!(neg.minnum(&pos).is_negative());
        assert!(!neg.maxnum(&pos).is_negative());
    }
}
