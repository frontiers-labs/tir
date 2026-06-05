use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, BitAnd, BitOr, BitXor, Mul, Neg, Not, Sub};
use std::str::FromStr;

/// Arbitrary Precision Integer similar to LLVM's APInt.
/// Supports integers of arbitrary bit width, both signed and unsigned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct APInt {
    /// The bit width of this integer
    width: u32,
    /// Whether this integer is signed
    signed: bool,
    /// The value, stored as a u64 for widths <= 64 bits
    /// For widths > 64, we'd extend this to use a `Vec<u64>`.
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
            // For signed: 0111...1 (max positive value)
            let mask = Self::mask_for_width(width);
            let sign_bit = 1u64 << (width - 1);
            APInt {
                width,
                signed: true,
                value: mask & !sign_bit,
            }
        } else {
            // For unsigned: 1111...1
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
            // For signed: 1000...0 (most negative value)
            let sign_bit = 1u64 << (width - 1);
            APInt {
                width,
                signed: true,
                value: sign_bit,
            }
        } else {
            // For unsigned: 0
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
            // Sign extend. At width >= 64 the bit pattern is already the i64, so
            // there are no upper bits to fill (and `u64::MAX << 64` would panic).
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
            // Need to extend the sign bit
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
            // Fill with sign bit
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
            // Arithmetic shift: extend sign bit
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

    /// Multiplication (low N bits of N*N -> 2N multiplication)
    /// Returns only the lower N bits of the result
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

    /// Multiplication high (upper N bits of N*N -> 2N multiplication)
    /// Returns the upper N bits of the unsigned result
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

    /// Signed multiplication high (upper N bits of signed N*N -> 2N multiplication)
    /// Returns the upper N bits of the signed result
    pub fn mulh(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");

        // Sign extend to 128 bits for proper signed multiplication
        let a_signed = if self.is_negative() {
            let extension = u128::MAX << self.width;
            (self.value as u128) | extension
        } else {
            self.value as u128
        };

        let b_signed = if other.is_negative() {
            let extension = u128::MAX << other.width;
            (other.value as u128) | extension
        } else {
            other.value as u128
        };

        let full_result = (a_signed as i128).wrapping_mul(b_signed as i128);
        let high_bits = ((full_result as u128) >> self.width) as u64;
        let mask = Self::mask_for_width(self.width);

        APInt {
            width: self.width,
            signed: true,
            value: high_bits & mask,
        }
    }

    /// Signed-unsigned multiplication high
    /// Returns the upper N bits of signed * unsigned multiplication
    pub fn mulhsu(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");

        // Sign extend self if negative
        let a_signed = if self.is_negative() {
            let extension = u128::MAX << self.width;
            (self.value as u128) | extension
        } else {
            self.value as u128
        };

        let b_unsigned = other.value as u128;
        let full_result = (a_signed as i128).wrapping_mul(b_unsigned as i128);
        let high_bits = ((full_result as u128) >> self.width) as u64;
        let mask = Self::mask_for_width(self.width);

        APInt {
            width: self.width,
            signed: false,
            value: high_bits & mask,
        }
    }

    /// Full multiplication returning both low and high parts
    /// Returns (low, high) where low contains lower N bits and high contains upper N bits
    pub fn mul_full(&self, other: &APInt) -> (Self, Self) {
        (self.mul(other), self.mulhu(other))
    }

    /// Full signed multiplication returning both low and high parts
    /// Returns (low, high) where low contains lower N bits and high contains upper N bits
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
        let result = self.to_i64().wrapping_div(other.to_i64());
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
        let result = self.to_i64().wrapping_rem(other.to_i64());
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
        // Adjust for the actual width
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

    /// Helper function to get the mask for a given width
    fn mask_for_width(width: u32) -> u64 {
        if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        }
    }
}

// Implement standard traits

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

    /// Parse an integer literal in the style of Rust/C integer literals.
    /// Supports:
    ///   - decimal:     `42`, `1_000`
    ///   - hexadecimal: `0x1F`, `0X1F`
    ///   - octal:       `0o77`, `0O77`
    ///   - binary:      `0b1010`, `0B1010`
    ///
    /// Underscores are allowed as digit separators and are ignored.
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

// Operator overloading

impl Add for APInt {
    type Output = APInt;
    fn add(self, other: APInt) -> APInt {
        APInt::add(&self, &other)
    }
}

impl Add for &APInt {
    type Output = APInt;
    fn add(self, other: &APInt) -> APInt {
        APInt::add(self, other)
    }
}

impl Sub for APInt {
    type Output = APInt;
    fn sub(self, other: APInt) -> APInt {
        APInt::sub(&self, &other)
    }
}

impl Sub for &APInt {
    type Output = APInt;
    fn sub(self, other: &APInt) -> APInt {
        APInt::sub(self, other)
    }
}

impl Mul for APInt {
    type Output = APInt;
    fn mul(self, other: APInt) -> APInt {
        APInt::mul(&self, &other)
    }
}

impl Mul for &APInt {
    type Output = APInt;
    fn mul(self, other: &APInt) -> APInt {
        APInt::mul(self, other)
    }
}

impl BitAnd for APInt {
    type Output = APInt;
    fn bitand(self, other: APInt) -> APInt {
        self.and(&other)
    }
}

impl BitAnd for &APInt {
    type Output = APInt;
    fn bitand(self, other: &APInt) -> APInt {
        self.and(other)
    }
}

impl BitOr for APInt {
    type Output = APInt;
    fn bitor(self, other: APInt) -> APInt {
        self.or(&other)
    }
}

impl BitOr for &APInt {
    type Output = APInt;
    fn bitor(self, other: &APInt) -> APInt {
        self.or(other)
    }
}

impl BitXor for APInt {
    type Output = APInt;
    fn bitxor(self, other: APInt) -> APInt {
        self.xor(&other)
    }
}

impl BitXor for &APInt {
    type Output = APInt;
    fn bitxor(self, other: &APInt) -> APInt {
        self.xor(other)
    }
}

impl Not for APInt {
    type Output = APInt;
    fn not(self) -> APInt {
        APInt::not(&self)
    }
}

impl Not for &APInt {
    type Output = APInt;
    fn not(self) -> APInt {
        APInt::not(self)
    }
}

impl Neg for APInt {
    type Output = APInt;
    fn neg(self) -> APInt {
        APInt::neg(&self)
    }
}

impl Neg for &APInt {
    type Output = APInt;
    fn neg(self) -> APInt {
        APInt::neg(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_creation() {
        let a = APInt::new(8, 42);
        assert_eq!(a.width(), 8);
        assert_eq!(a.to_u64(), 42);
        assert!(!a.is_signed());
    }

    #[test]
    fn test_signed_creation() {
        let a = APInt::new_signed(8, -1);
        assert!(a.is_signed());
        assert!(a.is_negative());
        assert_eq!(a.to_i64(), -1);
    }

    #[test]
    fn test_arithmetic() {
        let a = APInt::new(8, 10);
        let b = APInt::new(8, 20);

        assert_eq!(APInt::add(&a, &b).to_u64(), 30);
        assert_eq!(APInt::sub(&b, &a).to_u64(), 10);
        assert_eq!(APInt::mul(&a, &b).to_u64(), 200);
    }

    #[test]
    fn test_mul_overflow() {
        // 200 * 200 = 40000 = 0x9C40
        // In 8 bits, this should give us low = 0x40, high = 0x9C
        let a = APInt::new(8, 200);
        let b = APInt::new(8, 200);

        let low = APInt::mul(&a, &b);
        let high = a.mulhu(&b);

        assert_eq!(low.to_u64(), 0x40);
        assert_eq!(high.to_u64(), 0x9C);

        // Verify with full multiplication
        let (low2, high2) = a.mul_full(&b);
        assert_eq!(low2.to_u64(), 0x40);
        assert_eq!(high2.to_u64(), 0x9C);
    }

    #[test]
    fn test_mulh_signed() {
        // Test signed multiplication high
        // -1 * -1 = 1 (in 8-bit signed)
        let a = APInt::new_signed(8, -1);
        let b = APInt::new_signed(8, -1);

        let low = APInt::mul(&a, &b);
        let high = a.mulh(&b);

        // -1 * -1 = 1, so low = 1, high should be 0 (positive result)
        assert_eq!(low.to_u64(), 1);
        assert_eq!(high.to_i64(), 0);

        // Test -2 * 64 = -128 (fits in 8 bits)
        let c = APInt::new_signed(8, -2);
        let d = APInt::new_signed(8, 64);
        let low = APInt::mul(&c, &d);
        let high = c.mulh(&d);

        assert_eq!(low.to_i64(), -128);
        assert_eq!(high.to_i64(), -1); // Sign extension

        // Test larger multiplication: -100 * -100 = 10000
        // 10000 = 0x2710, in 8 bits: low = 0x10, high = 0x27
        let e = APInt::new_signed(8, -100);
        let f = APInt::new_signed(8, -100);
        let low = APInt::mul(&e, &f);
        let high = e.mulh(&f);

        assert_eq!(low.to_u64(), 0x10);
        assert_eq!(high.to_u64(), 0x27);
    }

    #[test]
    fn test_mulhsu() {
        // Test signed-unsigned multiplication high
        // -1 (0xFF) * 2 = -2 when signed * unsigned
        let a = APInt::new_signed(8, -1);
        let b = APInt::new(8, 2);

        let low = APInt::mul(&a, &b);
        let high = a.mulhsu(&b);

        // -1 * 2 = -2 = 0xFFFE in 16 bits (sign extended)
        // low = 0xFE, high = 0xFF
        assert_eq!(low.to_u64(), 0xFE);
        assert_eq!(high.to_u64(), 0xFF);
    }

    #[test]
    fn test_mul_32bit() {
        // Test with 32-bit values
        let a = APInt::new(32, 0xFFFFFFFF);
        let b = APInt::new(32, 0xFFFFFFFF);

        let low = APInt::mul(&a, &b);
        let high = a.mulhu(&b);

        // 0xFFFFFFFF * 0xFFFFFFFF = 0xFFFFFFFE00000001
        assert_eq!(low.to_u64(), 0x00000001);
        assert_eq!(high.to_u64(), 0xFFFFFFFE);
    }

    #[test]
    fn test_mul_64bit() {
        // Test with 64-bit values
        let a = APInt::new(64, 0x123456789ABCDEF0);
        let b = APInt::new(64, 2);

        let low = APInt::mul(&a, &b);
        let high = a.mulhu(&b);

        assert_eq!(low.to_u64(), 0x2468ACF13579BDE0);
        assert_eq!(high.to_u64(), 0); // No overflow for this multiplication
    }

    #[test]
    fn test_neg() {
        let one = APInt::new_signed(8, 1);
        assert_eq!(one.neg().to_i64(), -1);

        let minus_one = APInt::new_signed(8, -1);
        assert_eq!(minus_one.neg().to_i64(), 1);

        let zero = APInt::new_signed(8, 0);
        assert_eq!(zero.neg().to_i64(), 0);
    }

    #[test]
    fn test_abs() {
        assert_eq!(APInt::new_signed(8, -5).abs().to_i64(), 5);
        assert_eq!(APInt::new_signed(8, 5).abs().to_i64(), 5);
    }

    #[test]
    fn test_bitwise() {
        let a = APInt::new(8, 0b11110000);
        let b = APInt::new(8, 0b10101010);

        assert_eq!(a.and(&b).to_u64(), 0b10100000);
        assert_eq!(a.or(&b).to_u64(), 0b11111010);
        assert_eq!(a.xor(&b).to_u64(), 0b01011010);
        assert_eq!(a.not().to_u64(), 0b00001111);
    }

    #[test]
    fn test_shifts() {
        let a = APInt::new(8, 0b00001111);

        assert_eq!(a.shl(2).to_u64(), 0b00111100);
        assert_eq!(a.lshr(2).to_u64(), 0b00000011);
    }

    #[test]
    fn test_arithmetic_shift() {
        let a = APInt::new_signed(8, -16); // 0b11110000
        assert_eq!(a.ashr(2).to_u64(), 0b11111100); // Still negative
    }

    #[test]
    fn test_arithmetic_shift_64_bit_by_zero() {
        let a = APInt::new_signed(64, -16);
        let shifted = a.ashr(0);

        assert_eq!(shifted.width(), 64);
        assert_eq!(shifted.to_i64(), -16);
        assert!(shifted.is_signed());
    }

    #[test]
    fn test_signed_division_overflow_wraps() {
        let min = APInt::min_value(64, true);
        let minus_one = APInt::new_signed(64, -1);

        assert_eq!(min.sdiv(&minus_one).to_i64(), i64::MIN);
        assert_eq!(min.srem(&minus_one).to_i64(), 0);
    }

    #[test]
    fn test_comparisons() {
        let a = APInt::new(8, 10);
        let b = APInt::new(8, 20);

        assert!(a.ult(&b));
        assert!(b.ugt(&a));
        assert_eq!(a.ucmp(&b), Ordering::Less);
    }

    #[test]
    fn test_overflow() {
        let a = APInt::new(8, 255);
        let b = APInt::new(8, 1);

        // Should wrap around
        assert_eq!(APInt::add(&a, &b).to_u64(), 0);
    }

    #[test]
    fn test_sign_extend() {
        let a = APInt::new_signed(8, -1);
        let b = a.sign_extend(16);

        assert_eq!(b.width(), 16);
        assert_eq!(b.to_i64(), -1);
    }

    #[test]
    fn test_zero_extend() {
        let a = APInt::new(8, 255);
        let b = a.zero_extend(16);

        assert_eq!(b.width(), 16);
        assert_eq!(b.to_u64(), 255);
    }

    #[test]
    fn test_extract_bits() {
        let a = APInt::new(8, 0b11010110);
        let b = a.extract_bits(5, 2); // Extract bits 5-2: 0b1101

        assert_eq!(b.to_u64(), 0b0101);
        assert_eq!(b.width(), 4);
    }

    #[test]
    fn test_count_operations() {
        let a = APInt::new(8, 0b00110110);

        assert_eq!(a.count_ones(), 4);
        assert_eq!(a.count_leading_zeros(), 2);
        assert_eq!(a.count_trailing_zeros(), 1);
    }
}
