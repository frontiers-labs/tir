use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, BitAnd, BitOr, BitXor, Mul, Neg, Not, Sub};
use std::str::FromStr;

/// Arbitrary-precision integer (à la LLVM's APInt), signed or unsigned, width 1..=64.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct APInt {
    width: u32,
    signed: bool,
    value: u64,
}

impl APInt {
    /// Create a new APInt with the given width and value (unsigned)
    pub fn new(width: u32, value: u64) -> Self {
        assert!(width > 0 && width <= 64, "Width must be between 1 and 64");
        let mask = Self::mask_for_width(width);
        APInt {
            width,
            signed: false,
            value: value & mask,
        }
    }

    /// Create a new signed APInt with the given width and value
    pub fn new_signed(width: u32, value: i64) -> Self {
        assert!(width > 0 && width <= 64, "Width must be between 1 and 64");
        let mask = Self::mask_for_width(width);
        APInt {
            width,
            signed: true,
            value: (value as u64) & mask,
        }
    }

    /// Create an APInt from an unsigned value with automatic width
    pub fn from_u64(value: u64) -> Self {
        Self::new(64, value)
    }

    /// Create a signed APInt from a signed value with automatic width
    pub fn from_i64(value: i64) -> Self {
        Self::new_signed(64, value)
    }

    /// Create a zero-valued APInt of the given width
    pub fn zero(width: u32) -> Self {
        Self::new(width, 0)
    }

    /// Create a one-valued APInt of the given width
    pub fn one(width: u32) -> Self {
        Self::new(width, 1)
    }

    /// Create the maximum value for the given width
    pub fn max_value(width: u32, signed: bool) -> Self {
        if signed {
            let mask = Self::mask_for_width(width);
            let sign_bit = 1u64 << (width - 1);
            APInt {
                width,
                signed: true,
                value: mask & !sign_bit,
            }
        } else {
            APInt {
                width,
                signed: false,
                value: Self::mask_for_width(width),
            }
        }
    }

    /// Create the minimum value for the given width
    pub fn min_value(width: u32, signed: bool) -> Self {
        if signed {
            let sign_bit = 1u64 << (width - 1);
            APInt {
                width,
                signed: true,
                value: sign_bit,
            }
        } else {
            Self::zero(width)
        }
    }

    /// Get the bit width
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Check if this APInt is signed
    pub fn is_signed(&self) -> bool {
        self.signed
    }

    /// Get the raw value as u64
    pub fn raw_value(&self) -> u64 {
        self.value
    }

    /// Convert to u64 (truncating if necessary)
    pub fn to_u64(&self) -> u64 {
        self.value
    }

    /// Convert to i64, interpreting as signed
    pub fn to_i64(&self) -> i64 {
        if self.signed && self.is_negative() {
            // width >= 64: bit pattern is already the i64 (and `u64::MAX << 64` would panic).
            let extension = if self.width >= 64 {
                0
            } else {
                u64::MAX << self.width
            };
            (self.value | extension) as i64
        } else {
            self.value as i64
        }
    }

    /// Interpret the bit pattern as two's-complement at this width, regardless
    /// of the `signed` flag. Signed division/remainder are defined on the bits,
    /// so a value produced without the flag set (e.g. an `extract`) must still
    /// divide with its high bit as the sign.
    fn signed_at_width(&self) -> i64 {
        if self.width >= 64 {
            self.value as i64
        } else {
            let shift = 64 - self.width;
            ((self.value << shift) as i64) >> shift
        }
    }

    /// Check if the value is zero
    pub fn is_zero(&self) -> bool {
        self.value == 0
    }

    /// Check if the value is one
    pub fn is_one(&self) -> bool {
        self.value == 1
    }

    /// Check if negative (only meaningful for signed integers)
    pub fn is_negative(&self) -> bool {
        if !self.signed {
            return false;
        }
        let sign_bit = 1u64 << (self.width - 1);
        (self.value & sign_bit) != 0
    }

    /// Check if positive (only meaningful for signed integers)
    pub fn is_positive(&self) -> bool {
        !self.is_zero() && !self.is_negative()
    }

    /// Set the signedness
    pub fn set_signed(&mut self, signed: bool) {
        self.signed = signed;
    }

    /// Get a copy with different signedness
    pub fn with_signed(&self, signed: bool) -> Self {
        APInt {
            width: self.width,
            signed,
            value: self.value,
        }
    }

    /// Zero-extend to a larger width
    pub fn zero_extend(&self, new_width: u32) -> Self {
        assert!(new_width >= self.width, "Cannot extend to smaller width");
        APInt {
            width: new_width,
            signed: false,
            value: self.value,
        }
    }

    /// Sign-extend to a larger width
    pub fn sign_extend(&self, new_width: u32) -> Self {
        assert!(new_width >= self.width, "Cannot extend to smaller width");
        if self.is_negative() {
            let mask = Self::mask_for_width(self.width);
            let extension =
                (Self::mask_for_width(new_width) ^ mask) & Self::mask_for_width(new_width);
            APInt {
                width: new_width,
                signed: true,
                value: self.value | extension,
            }
        } else {
            APInt {
                width: new_width,
                signed: self.signed,
                value: self.value,
            }
        }
    }

    /// Truncate to a smaller width
    pub fn truncate(&self, new_width: u32) -> Self {
        assert!(new_width <= self.width, "Cannot truncate to larger width");
        let mask = Self::mask_for_width(new_width);
        APInt {
            width: new_width,
            signed: self.signed,
            value: self.value & mask,
        }
    }

    /// Extract bits from high to low (inclusive)
    pub fn extract_bits(&self, high: u32, low: u32) -> Self {
        assert!(high >= low, "High bit must be >= low bit");
        assert!(high < self.width, "High bit out of range");

        let new_width = high - low + 1;
        let mask = Self::mask_for_width(new_width);
        let value = (self.value >> low) & mask;

        APInt {
            width: new_width,
            signed: false,
            value,
        }
    }

    /// Logical shift left
    pub fn shl(&self, shift: u32) -> Self {
        if shift >= self.width {
            return Self::zero(self.width);
        }
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed,
            value: (self.value << shift) & mask,
        }
    }

    /// Logical shift right
    pub fn lshr(&self, shift: u32) -> Self {
        if shift >= self.width {
            return Self::zero(self.width);
        }
        APInt {
            width: self.width,
            signed: false,
            value: self.value >> shift,
        }
    }

    /// Arithmetic shift right (preserves sign for signed integers)
    pub fn ashr(&self, shift: u32) -> Self {
        if shift == 0 {
            return self.clone();
        }
        if shift >= self.width {
            if self.is_negative() {
                return APInt {
                    width: self.width,
                    signed: self.signed,
                    value: Self::mask_for_width(self.width),
                };
            } else {
                return Self::zero(self.width);
            }
        }

        if self.signed && self.is_negative() {
            let mask = Self::mask_for_width(self.width);
            let sign_extension = (mask << (self.width - shift)) & mask;
            APInt {
                width: self.width,
                signed: self.signed,
                value: (self.value >> shift) | sign_extension,
            }
        } else {
            self.lshr(shift)
        }
    }

    /// Bitwise AND
    pub fn and(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value & other.value,
        }
    }

    /// Bitwise OR
    pub fn or(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value | other.value,
        }
    }

    /// Bitwise XOR
    pub fn xor(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value ^ other.value,
        }
    }

    /// Bitwise NOT
    pub fn not(&self) -> Self {
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed,
            value: (!self.value) & mask,
        }
    }

    /// Addition
    pub fn add(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value.wrapping_add(other.value) & mask,
        }
    }

    /// Subtraction
    pub fn sub(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value.wrapping_sub(other.value) & mask,
        }
    }

    /// Multiplication, low N bits of the N*N -> 2N product.
    pub fn mul(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        let full_result = (self.value as u128) * (other.value as u128);
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: (full_result as u64) & mask,
        }
    }

    /// Unsigned multiplication high, upper N bits of the N*N -> 2N product.
    pub fn mulhu(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        let full_result = (self.value as u128) * (other.value as u128);
        let high_bits = (full_result >> self.width) as u64;
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: false,
            value: high_bits & mask,
        }
    }

    /// Signed multiplication high, upper N bits of the signed N*N -> 2N product.
    pub fn mulh(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        let full_result = (self.sext_u128() as i128).wrapping_mul(other.sext_u128() as i128);
        let high_bits = ((full_result as u128) >> self.width) as u64;
        let mask = Self::mask_for_width(self.width);

        APInt {
            width: self.width,
            signed: true,
            value: high_bits & mask,
        }
    }

    /// Signed-unsigned multiplication high, upper N bits of signed * unsigned.
    pub fn mulhsu(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        let b_unsigned = other.value as u128;
        let full_result = (self.sext_u128() as i128).wrapping_mul(b_unsigned as i128);
        let high_bits = ((full_result as u128) >> self.width) as u64;
        let mask = Self::mask_for_width(self.width);

        APInt {
            width: self.width,
            signed: false,
            value: high_bits & mask,
        }
    }

    /// Full unsigned multiplication as `(low N bits, high N bits)`.
    pub fn mul_full(&self, other: &APInt) -> (Self, Self) {
        (self.mul(other), self.mulhu(other))
    }

    /// Full signed multiplication as `(low N bits, high N bits)`.
    pub fn mul_full_signed(&self, other: &APInt) -> (Self, Self) {
        (self.mul(other), self.mulh(other))
    }

    /// Unsigned division
    pub fn udiv(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        APInt {
            width: self.width,
            signed: false,
            value: self.value / other.value,
        }
    }

    /// Signed division
    pub fn sdiv(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        let mask = Self::mask_for_width(self.width);
        let result = self.signed_at_width().wrapping_div(other.signed_at_width());
        APInt {
            width: self.width,
            signed: true,
            value: (result as u64) & mask,
        }
    }

    /// Unsigned remainder
    pub fn urem(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        APInt {
            width: self.width,
            signed: false,
            value: self.value % other.value,
        }
    }

    /// Signed remainder
    pub fn srem(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        let mask = Self::mask_for_width(self.width);
        let result = self.signed_at_width().wrapping_rem(other.signed_at_width());
        APInt {
            width: self.width,
            signed: true,
            value: (result as u64) & mask,
        }
    }

    /// Negate (two's complement)
    pub fn neg(&self) -> Self {
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed,
            value: (!self.value).wrapping_add(1) & mask,
        }
    }

    /// Absolute value
    pub fn abs(&self) -> Self {
        if self.is_negative() {
            self.neg()
        } else {
            self.clone()
        }
    }

    /// Unsigned comparison
    pub fn ucmp(&self, other: &APInt) -> Ordering {
        assert_eq!(self.width, other.width, "Widths must match");
        self.value.cmp(&other.value)
    }

    /// Signed comparison
    pub fn scmp(&self, other: &APInt) -> Ordering {
        assert_eq!(self.width, other.width, "Widths must match");
        self.to_i64().cmp(&other.to_i64())
    }

    /// Primary ordering key: magnitude as i128 (signed sign-extended, unsigned zero-extended).
    fn numeric_key(&self) -> i128 {
        if self.signed {
            self.to_i64() as i128
        } else {
            self.value as i128
        }
    }

    /// Unsigned less than
    pub fn ult(&self, other: &APInt) -> bool {
        self.ucmp(other) == Ordering::Less
    }

    /// Unsigned less than or equal
    pub fn ule(&self, other: &APInt) -> bool {
        matches!(self.ucmp(other), Ordering::Less | Ordering::Equal)
    }

    /// Unsigned greater than
    pub fn ugt(&self, other: &APInt) -> bool {
        self.ucmp(other) == Ordering::Greater
    }

    /// Unsigned greater than or equal
    pub fn uge(&self, other: &APInt) -> bool {
        matches!(self.ucmp(other), Ordering::Greater | Ordering::Equal)
    }

    /// Signed less than
    pub fn slt(&self, other: &APInt) -> bool {
        self.scmp(other) == Ordering::Less
    }

    /// Signed less than or equal
    pub fn sle(&self, other: &APInt) -> bool {
        matches!(self.scmp(other), Ordering::Less | Ordering::Equal)
    }

    /// Signed greater than
    pub fn sgt(&self, other: &APInt) -> bool {
        self.scmp(other) == Ordering::Greater
    }

    /// Signed greater than or equal
    pub fn sge(&self, other: &APInt) -> bool {
        matches!(self.scmp(other), Ordering::Greater | Ordering::Equal)
    }

    /// Count leading zeros
    pub fn count_leading_zeros(&self) -> u32 {
        if self.is_zero() {
            return self.width;
        }
        let mask = Self::mask_for_width(self.width);
        let effective_value = self.value & mask;
        let leading_zeros = effective_value.leading_zeros();
        leading_zeros - (64 - self.width)
    }

    /// Count trailing zeros
    pub fn count_trailing_zeros(&self) -> u32 {
        if self.is_zero() {
            return self.width;
        }
        let trailing_zeros = self.value.trailing_zeros();
        std::cmp::min(trailing_zeros, self.width)
    }

    /// Count the number of set bits (population count)
    pub fn count_ones(&self) -> u32 {
        let mask = Self::mask_for_width(self.width);
        (self.value & mask).count_ones()
    }

    /// Value sign-extended to a full 128-bit pattern (negatives fill the high bits).
    fn sext_u128(&self) -> u128 {
        if self.is_negative() {
            (self.value as u128) | (u128::MAX << self.width)
        } else {
            self.value as u128
        }
    }

    /// Low-`width`-bits mask.
    fn mask_for_width(width: u32) -> u64 {
        if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        }
    }
}

impl Ord for APInt {
    /// Numeric value, then width/signedness, so the order is total and consistent with `Eq`/`Hash`.
    fn cmp(&self, other: &Self) -> Ordering {
        self.numeric_key()
            .cmp(&other.numeric_key())
            .then_with(|| self.width.cmp(&other.width))
            .then_with(|| self.signed.cmp(&other.signed))
    }
}

impl PartialOrd for APInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for APInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.signed {
            write!(f, "{}", self.to_i64())
        } else {
            write!(f, "{}", self.value)
        }
    }
}

impl fmt::Binary for APInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mask = Self::mask_for_width(self.width);
        let value = self.value & mask;
        write!(f, "{:0width$b}", value, width = self.width as usize)
    }
}

impl fmt::LowerHex for APInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.value)
    }
}

impl fmt::UpperHex for APInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}", self.value)
    }
}

impl FromStr for APInt {
    type Err = String;

    /// Parse a Rust/C-style integer literal: decimal or `0x`/`0o`/`0b` prefixed, `_` separators ignored.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty string".to_string());
        }

        let (radix, digits) =
            if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                (16u64, rest)
            } else if let Some(rest) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
                (8u64, rest)
            } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
                (2u64, rest)
            } else {
                (10u64, s)
            };

        let clean: String = digits.chars().filter(|&c| c != '_').collect();
        if clean.is_empty() {
            return Err(format!("no digits in '{s}'"));
        }

        let mut value: u64 = 0;
        for ch in clean.chars() {
            let digit = ch
                .to_digit(radix as u32)
                .ok_or_else(|| format!("invalid digit '{ch}' for radix {radix}"))?
                as u64;
            value = value
                .checked_mul(radix)
                .and_then(|v| v.checked_add(digit))
                .ok_or_else(|| format!("value overflows u64: '{s}'"))?;
        }

        let width = if value == 0 {
            1
        } else {
            64 - value.leading_zeros()
        }
        .max(1);
        Ok(APInt::new(width, value))
    }
}

macro_rules! impl_binop {
    ($trait:ident, $method:ident, $imp:ident) => {
        impl $trait for APInt {
            type Output = APInt;
            fn $method(self, other: APInt) -> APInt {
                APInt::$imp(&self, &other)
            }
        }

        impl $trait for &APInt {
            type Output = APInt;
            fn $method(self, other: &APInt) -> APInt {
                APInt::$imp(self, other)
            }
        }
    };
}

macro_rules! impl_unop {
    ($trait:ident, $method:ident, $imp:ident) => {
        impl $trait for APInt {
            type Output = APInt;
            fn $method(self) -> APInt {
                APInt::$imp(&self)
            }
        }

        impl $trait for &APInt {
            type Output = APInt;
            fn $method(self) -> APInt {
                APInt::$imp(self)
            }
        }
    };
}

impl_binop!(Add, add, add);
impl_binop!(Sub, sub, sub);
impl_binop!(Mul, mul, mul);
impl_binop!(BitAnd, bitand, and);
impl_binop!(BitOr, bitor, or);
impl_binop!(BitXor, bitxor, xor);
impl_unop!(Not, not, not);
impl_unop!(Neg, neg, neg);

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn test_add(x in prop::num::i64::ANY, y in prop::num::i64::ANY) {
            let res = APInt::from_i64(x) + APInt::from_i64(y);
            prop_assert_eq!(res.to_i64(), x.wrapping_add(y));
        }

        #[test]
        fn test_sub(x in prop::num::i64::ANY, y in prop::num::i64::ANY) {
            let res = APInt::from_i64(x) - APInt::from_i64(y);
            prop_assert_eq!(res.to_i64(), x.wrapping_sub(y));
        }

        #[test]
        fn test_mul(x in prop::num::i64::ANY, y in prop::num::i64::ANY) {
            let res = APInt::from_i64(x) * APInt::from_i64(y);
            prop_assert_eq!(res.to_i64(), x.wrapping_mul(y));
        }

        #[test]
        fn test_neg(x in prop::num::i64::ANY) {
            let res = -APInt::from_i64(x);
            prop_assert_eq!(res.to_i64(), x.wrapping_neg());
        }

        #[test]
        fn test_abs(x in prop::num::i64::ANY) {
            let res = APInt::from_i64(x).abs();
            prop_assert_eq!(res.to_i64(), x.wrapping_abs());
        }

        #[test]
        fn test_and(x in prop::num::u64::ANY, y in prop::num::u64::ANY) {
            let res = APInt::from_u64(x) & APInt::from_u64(y);
            prop_assert_eq!(res.to_u64(), x & y);
        }

        #[test]
        fn test_or(x in prop::num::u64::ANY, y in prop::num::u64::ANY) {
            let res = APInt::from_u64(x) | APInt::from_u64(y);
            prop_assert_eq!(res.to_u64(), x | y);
        }

        #[test]
        fn test_xor(x in prop::num::u64::ANY, y in prop::num::u64::ANY) {
            let res = APInt::from_u64(x) ^ APInt::from_u64(y);
            prop_assert_eq!(res.to_u64(), x ^ y);
        }

        #[test]
        fn test_not(x in prop::num::u64::ANY) {
            let res = !APInt::from_u64(x);
            prop_assert_eq!(res.to_u64(), !x);
        }

        #[test]
        fn test_shl(x in prop::num::u64::ANY, s in 0u32..64) {
            let res = APInt::from_u64(x).shl(s);
            prop_assert_eq!(res.to_u64(), x << s);
        }

        #[test]
        fn test_lshr(x in prop::num::u64::ANY, s in 0u32..64) {
            let res = APInt::from_u64(x).lshr(s);
            prop_assert_eq!(res.to_u64(), x >> s);
        }

        #[test]
        fn test_ashr(x in prop::num::i64::ANY, s in 0u32..64) {
            let res = APInt::from_i64(x).ashr(s);
            prop_assert_eq!(res.to_i64(), x >> s);
        }

        #[test]
        fn test_udiv(x in prop::num::u64::ANY, y in 1u64..=u64::MAX) {
            let res = APInt::from_u64(x).udiv(&APInt::from_u64(y));
            prop_assert_eq!(res.to_u64(), x / y);
        }

        #[test]
        fn test_urem(x in prop::num::u64::ANY, y in 1u64..=u64::MAX) {
            let res = APInt::from_u64(x).urem(&APInt::from_u64(y));
            prop_assert_eq!(res.to_u64(), x % y);
        }

        #[test]
        fn test_sdiv(x in prop::num::i64::ANY, y in prop::num::i64::ANY.prop_filter("nonzero", |y| *y != 0)) {
            let res = APInt::from_i64(x).sdiv(&APInt::from_i64(y));
            prop_assert_eq!(res.to_i64(), x.wrapping_div(y));
        }

        #[test]
        fn test_srem(x in prop::num::i64::ANY, y in prop::num::i64::ANY.prop_filter("nonzero", |y| *y != 0)) {
            let res = APInt::from_i64(x).srem(&APInt::from_i64(y));
            prop_assert_eq!(res.to_i64(), x.wrapping_rem(y));
        }

        #[test]
        fn test_mulhu(x in prop::num::u64::ANY, y in prop::num::u64::ANY) {
            let res = APInt::from_u64(x).mulhu(&APInt::from_u64(y));
            prop_assert_eq!(res.to_u64(), ((x as u128 * y as u128) >> 64) as u64);
        }

        #[test]
        fn test_mulh(x in prop::num::i64::ANY, y in prop::num::i64::ANY) {
            let res = APInt::from_i64(x).mulh(&APInt::from_i64(y));
            prop_assert_eq!(res.to_i64(), ((x as i128 * y as i128) >> 64) as i64);
        }

        #[test]
        fn test_mulhsu(x in prop::num::i64::ANY, y in prop::num::u64::ANY) {
            let res = APInt::from_i64(x).mulhsu(&APInt::from_u64(y));
            prop_assert_eq!(res.to_u64(), ((x as i128 * y as i128) >> 64) as u64);
        }

        #[test]
        fn test_ord_signed(x in prop::num::i64::ANY, y in prop::num::i64::ANY) {
            prop_assert_eq!(APInt::from_i64(x).cmp(&APInt::from_i64(y)), x.cmp(&y));
        }

        #[test]
        fn test_ord_unsigned(x in prop::num::u64::ANY, y in prop::num::u64::ANY) {
            prop_assert_eq!(APInt::from_u64(x).cmp(&APInt::from_u64(y)), x.cmp(&y));
        }

        #[test]
        fn test_ord_consistent_with_eq(a in arb_apint(), b in arb_apint()) {
            prop_assert_eq!(a == b, a.cmp(&b) == Ordering::Equal);
            prop_assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
        }
    }

    fn arb_apint() -> impl Strategy<Value = APInt> {
        (1u32..=64, any::<bool>(), any::<u64>()).prop_map(|(w, signed, v)| {
            if signed {
                APInt::new_signed(w, v as i64)
            } else {
                APInt::new(w, v)
            }
        })
    }
}
