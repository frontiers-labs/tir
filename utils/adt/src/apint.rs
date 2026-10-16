use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, BitAnd, BitOr, BitXor, Mul, Neg, Not, Sub};
use std::str::FromStr;

/// Arbitrary-precision integer (à la LLVM's APInt), signed or unsigned, of any
/// width from one bit. A value of at most 64 bits is one word; a wider one
/// keeps the limbs above its low word, little-endian, beside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct APInt {
    width: u64,
    signed: bool,
    value: u64,
    // Present exactly when `width > 64`, with the bits past `width` clear.
    high: Option<Box<[u64]>>,
}

impl APInt {
    /// Create a new APInt with the given width and value (unsigned)
    pub fn new(width: u64, value: u64) -> Self {
        assert!(width > 0, "Width must be at least 1");
        if width > 64 {
            return Self::from_words(width, false, vec![value]);
        }
        let mask = Self::mask_for_width(width);
        APInt {
            width,
            signed: false,
            value: value & mask,
            high: None,
        }
    }

    /// Create a new signed APInt with the given width and value
    pub fn new_signed(width: u64, value: i64) -> Self {
        assert!(width > 0, "Width must be at least 1");
        if width > 64 {
            let fill = if value < 0 { u64::MAX } else { 0 };
            let mut words = vec![fill; limb_count(width)];
            words[0] = value as u64;
            return Self::from_words(width, true, words);
        }
        let mask = Self::mask_for_width(width);
        APInt {
            width,
            signed: true,
            value: (value as u64) & mask,
            high: None,
        }
    }

    /// An unsigned value of `width` bits from its 64-bit limbs, least
    /// significant first. Missing limbs are zero; bits past `width` are dropped.
    pub fn from_limbs(width: u64, limbs: &[u64]) -> Self {
        assert!(width > 0, "Width must be at least 1");
        Self::from_words(width, false, limbs.to_vec())
    }

    /// The value's 64-bit limbs, least significant first: one per started
    /// 64 bits of the width.
    pub fn limbs(&self) -> Vec<u64> {
        let mut limbs = Vec::with_capacity(limb_count(self.width));
        limbs.push(self.value);
        limbs.extend(self.high.iter().flat_map(|high| high.iter().copied()));
        limbs
    }

    fn from_words(width: u64, signed: bool, mut words: Vec<u64>) -> Self {
        words.resize(limb_count(width), 0);
        if let Some(top) = words.last_mut() {
            *top &= Self::mask_for_width(width - 64 * (limb_count(width) as u64 - 1));
        }
        let high = (width > 64).then(|| words[1..].into());
        APInt {
            width,
            signed,
            value: words[0],
            high,
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
    pub fn zero(width: u64) -> Self {
        Self::new(width, 0)
    }

    /// Create a one-valued APInt of the given width
    pub fn one(width: u64) -> Self {
        Self::new(width, 1)
    }

    /// Create the maximum value for the given width
    pub fn max_value(width: u64, signed: bool) -> Self {
        if width > 64 {
            let ones = Self::from_words(width, signed, vec![u64::MAX; limb_count(width)]);
            return if signed {
                ones.lshr(1).with_signed(true)
            } else {
                ones
            };
        }
        if signed {
            let mask = Self::mask_for_width(width);
            let sign_bit = 1u64 << (width - 1);
            APInt {
                width,
                signed: true,
                value: mask & !sign_bit,
                high: None,
            }
        } else {
            APInt {
                width,
                signed: false,
                value: Self::mask_for_width(width),
                high: None,
            }
        }
    }

    /// Get the bit width
    pub fn width(&self) -> u64 {
        self.width
    }

    /// Check if this APInt is signed
    pub fn is_signed(&self) -> bool {
        self.signed
    }

    /// Convert to u64 (truncating if necessary)
    pub fn to_u64(&self) -> u64 {
        self.value
    }

    /// The limbs above the lowest; none up to 64 bits.
    pub fn high_limbs(&self) -> &[u64] {
        self.high.as_deref().unwrap_or(&[])
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
            && self
                .high
                .as_ref()
                .is_none_or(|high| high.iter().all(|&limb| limb == 0))
    }

    /// Check if negative (only meaningful for signed integers)
    pub fn is_negative(&self) -> bool {
        self.signed && self.top_bit()
    }

    fn top_bit(&self) -> bool {
        self.bit(self.width - 1)
    }

    fn bit(&self, index: u64) -> bool {
        let limb = match index / 64 {
            0 => self.value,
            n => self.high.as_ref().map_or(0, |high| high[n as usize - 1]),
        };
        limb >> (index % 64) & 1 != 0
    }

    /// Get a copy with different signedness
    pub fn with_signed(&self, signed: bool) -> Self {
        APInt {
            signed,
            ..self.clone()
        }
    }

    /// Zero-extend to a larger width
    pub fn zero_extend(&self, new_width: u64) -> Self {
        assert!(new_width >= self.width, "Cannot extend to smaller width");
        Self::from_words(new_width, false, self.limbs())
    }

    /// Sign-extend to a larger width
    pub fn sign_extend(&self, new_width: u64) -> Self {
        assert!(new_width >= self.width, "Cannot extend to smaller width");
        if !self.is_negative() {
            return Self::from_words(new_width, self.signed, self.limbs());
        }
        let mut words = self.limbs();
        let top = words.len() - 1;
        words[top] |= !Self::mask_for_width(self.width - 64 * top as u64);
        words.resize(limb_count(new_width), u64::MAX);
        Self::from_words(new_width, true, words)
    }

    /// Truncate to a smaller width
    pub fn truncate(&self, new_width: u64) -> Self {
        assert!(new_width <= self.width, "Cannot truncate to larger width");
        Self::from_words(new_width, self.signed, self.limbs())
    }

    /// Extract bits from high to low (inclusive)
    pub fn extract_bits(&self, high: u64, low: u64) -> Self {
        assert!(high >= low, "High bit must be >= low bit");
        assert!(high < self.width, "High bit out of range");

        let new_width = high - low + 1;
        if self.width > 64 {
            return Self::from_words(new_width, false, lshr_words(&self.limbs(), low));
        }
        let mask = Self::mask_for_width(new_width);
        let value = (self.value >> low) & mask;

        APInt {
            width: new_width,
            signed: false,
            value,
            high: None,
        }
    }

    /// Logical shift left
    pub fn shl(&self, shift: u64) -> Self {
        if shift >= self.width {
            return Self::zero(self.width);
        }
        if self.width > 64 {
            return Self::from_words(self.width, self.signed, shl_words(&self.limbs(), shift));
        }
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed,
            value: (self.value << shift) & mask,
            high: None,
        }
    }

    /// Logical shift right
    pub fn lshr(&self, shift: u64) -> Self {
        if shift >= self.width {
            return Self::zero(self.width);
        }
        if self.width > 64 {
            return Self::from_words(self.width, false, lshr_words(&self.limbs(), shift));
        }
        APInt {
            width: self.width,
            signed: false,
            value: self.value >> shift,
            high: None,
        }
    }

    /// Arithmetic shift right (preserves sign for signed integers)
    pub fn ashr(&self, shift: u64) -> Self {
        if shift == 0 {
            return self.clone();
        }
        if self.width > 64 {
            if !self.is_negative() {
                return self.lshr(shift);
            }
            let filled = self.not().lshr(shift).not();
            return filled.with_signed(self.signed);
        }
        if shift >= self.width {
            if self.is_negative() {
                return APInt {
                    width: self.width,
                    signed: self.signed,
                    value: Self::mask_for_width(self.width),
                    high: None,
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
                high: None,
            }
        } else {
            self.lshr(shift)
        }
    }

    fn limbwise(&self, other: &APInt, f: impl Fn(u64, u64) -> u64) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        let words = self
            .limbs()
            .into_iter()
            .zip(other.limbs())
            .map(|(a, b)| f(a, b))
            .collect();
        Self::from_words(self.width, self.signed && other.signed, words)
    }

    /// Bitwise AND
    pub fn and(&self, other: &APInt) -> Self {
        if self.width > 64 {
            return self.limbwise(other, |a, b| a & b);
        }
        assert_eq!(self.width, other.width, "Widths must match");
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value & other.value,
            high: None,
        }
    }

    /// Bitwise OR
    pub fn or(&self, other: &APInt) -> Self {
        if self.width > 64 {
            return self.limbwise(other, |a, b| a | b);
        }
        assert_eq!(self.width, other.width, "Widths must match");
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value | other.value,
            high: None,
        }
    }

    /// Bitwise XOR
    pub fn xor(&self, other: &APInt) -> Self {
        if self.width > 64 {
            return self.limbwise(other, |a, b| a ^ b);
        }
        assert_eq!(self.width, other.width, "Widths must match");
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value ^ other.value,
            high: None,
        }
    }

    /// Bitwise NOT
    pub fn not(&self) -> Self {
        if self.width > 64 {
            let words = self.limbs().into_iter().map(|limb| !limb).collect();
            return Self::from_words(self.width, self.signed, words);
        }
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed,
            value: (!self.value) & mask,
            high: None,
        }
    }

    /// Addition
    pub fn add(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        if self.width > 64 {
            let words = add_words(&self.limbs(), &other.limbs());
            return Self::from_words(self.width, self.signed && other.signed, words);
        }
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value.wrapping_add(other.value) & mask,
            high: None,
        }
    }

    /// Subtraction
    pub fn sub(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        if self.width > 64 {
            let words = sub_words(&self.limbs(), &other.limbs());
            return Self::from_words(self.width, self.signed && other.signed, words);
        }
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: self.value.wrapping_sub(other.value) & mask,
            high: None,
        }
    }

    /// Multiplication, low N bits of the N*N -> 2N product.
    pub fn mul(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        if self.width > 64 {
            let words = mul_words(&self.limbs(), &other.limbs());
            return Self::from_words(self.width, self.signed && other.signed, words);
        }
        let full_result = (self.value as u128) * (other.value as u128);
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed && other.signed,
            value: (full_result as u64) & mask,
            high: None,
        }
    }

    /// Unsigned multiplication high, upper N bits of the N*N -> 2N product.
    pub fn mulhu(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        if self.width > 64 {
            let double = 2 * self.width;
            let product = APInt::mul(&self.zero_extend(double), &other.zero_extend(double));
            return product.extract_bits(double - 1, self.width);
        }
        let full_result = (self.value as u128) * (other.value as u128);
        let high_bits = (full_result >> self.width) as u64;
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: false,
            value: high_bits & mask,
            high: None,
        }
    }

    /// Signed multiplication high, upper N bits of the signed N*N -> 2N product.
    pub fn mulh(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        if self.width > 64 {
            let double = 2 * self.width;
            let product = APInt::mul(&self.sign_extend(double), &other.sign_extend(double));
            return product
                .extract_bits(double - 1, self.width)
                .with_signed(true);
        }
        let full_result = (self.sext_u128() as i128).wrapping_mul(other.sext_u128() as i128);
        let high_bits = ((full_result as u128) >> self.width) as u64;
        let mask = Self::mask_for_width(self.width);

        APInt {
            width: self.width,
            signed: true,
            value: high_bits & mask,
            high: None,
        }
    }

    /// Unsigned division
    pub fn udiv(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        if self.width > 64 {
            let (quotient, _) = divrem_words(&self.limbs(), &other.limbs(), self.width);
            return Self::from_words(self.width, false, quotient);
        }
        APInt {
            width: self.width,
            signed: false,
            value: self.value / other.value,
            high: None,
        }
    }

    /// Signed division
    pub fn sdiv(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        if self.width > 64 {
            let (quotient, _) = self.signed_divrem(other);
            return quotient;
        }
        let mask = Self::mask_for_width(self.width);
        let result = self.signed_at_width().wrapping_div(other.signed_at_width());
        APInt {
            width: self.width,
            signed: true,
            value: (result as u64) & mask,
            high: None,
        }
    }

    /// Unsigned remainder
    pub fn urem(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        if self.width > 64 {
            let (_, remainder) = divrem_words(&self.limbs(), &other.limbs(), self.width);
            return Self::from_words(self.width, false, remainder);
        }
        APInt {
            width: self.width,
            signed: false,
            value: self.value % other.value,
            high: None,
        }
    }

    /// Signed remainder
    pub fn srem(&self, other: &APInt) -> Self {
        assert_eq!(self.width, other.width, "Widths must match");
        assert!(!other.is_zero(), "Division by zero");
        if self.width > 64 {
            let (_, remainder) = self.signed_divrem(other);
            return remainder;
        }
        let mask = Self::mask_for_width(self.width);
        let result = self.signed_at_width().wrapping_rem(other.signed_at_width());
        APInt {
            width: self.width,
            signed: true,
            value: (result as u64) & mask,
            high: None,
        }
    }

    // Truncating division on the bits, the high bit the sign whatever the
    // flag says, as `signed_at_width` reads a word.
    fn signed_divrem(&self, other: &APInt) -> (Self, Self) {
        let magnitude = |value: &APInt| {
            if value.top_bit() {
                value.neg().limbs()
            } else {
                value.limbs()
            }
        };
        let (quotient, remainder) = divrem_words(&magnitude(self), &magnitude(other), self.width);
        let quotient = Self::from_words(self.width, true, quotient);
        let remainder = Self::from_words(self.width, true, remainder);
        (
            if self.top_bit() != other.top_bit() {
                quotient.neg()
            } else {
                quotient
            },
            if self.top_bit() {
                remainder.neg()
            } else {
                remainder
            },
        )
    }

    /// Negate (two's complement)
    pub fn neg(&self) -> Self {
        if self.width > 64 {
            return APInt::sub(&Self::zero(self.width), self).with_signed(self.signed);
        }
        let mask = Self::mask_for_width(self.width);
        APInt {
            width: self.width,
            signed: self.signed,
            value: (!self.value).wrapping_add(1) & mask,
            high: None,
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
        if self.width > 64 {
            return cmp_words(&self.limbs(), &other.limbs());
        }
        self.value.cmp(&other.value)
    }

    /// Signed comparison. Like signed division, this is defined on the bit
    /// pattern rather than on the `signed` flag: a value produced without the
    /// flag set (e.g. an `extract`, or a vector lane out of a `split`) must
    /// still compare with its high bit as the sign.
    pub fn scmp(&self, other: &APInt) -> Ordering {
        assert_eq!(self.width, other.width, "Widths must match");
        if self.width > 64 {
            return other
                .top_bit()
                .cmp(&self.top_bit())
                .then_with(|| cmp_words(&self.limbs(), &other.limbs()));
        }
        self.signed_at_width().cmp(&other.signed_at_width())
    }

    // The numeric order of two values of any widths: a signed value's high bit
    // is its sign.
    fn numeric_cmp(&self, other: &APInt) -> Ordering {
        if self.width <= 64 && other.width <= 64 {
            return self.numeric_key().cmp(&other.numeric_key());
        }
        let width = self.width.max(other.width) + 1;
        let widen = |value: &APInt| {
            if value.is_negative() {
                value.sign_extend(width)
            } else {
                value.zero_extend(width)
            }
        };
        widen(self).scmp(&widen(other))
    }

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
    pub fn count_leading_zeros(&self) -> u64 {
        if self.is_zero() {
            return self.width;
        }
        if self.width > 64 {
            let limbs = self.limbs();
            let top = limbs.iter().rposition(|&limb| limb != 0).unwrap_or(0);
            let set = 64 * top as u64 + 64 - u64::from(limbs[top].leading_zeros());
            return self.width - set;
        }
        let mask = Self::mask_for_width(self.width);
        let effective_value = self.value & mask;
        let leading_zeros = u64::from(effective_value.leading_zeros());
        leading_zeros - (64 - self.width)
    }

    /// Count trailing zeros
    pub fn count_trailing_zeros(&self) -> u64 {
        if self.is_zero() {
            return self.width;
        }
        if self.width > 64 {
            let limbs = self.limbs();
            let low = limbs.iter().position(|&limb| limb != 0).unwrap_or(0);
            return 64 * low as u64 + u64::from(limbs[low].trailing_zeros());
        }
        let trailing_zeros = u64::from(self.value.trailing_zeros());
        std::cmp::min(trailing_zeros, self.width)
    }

    /// Count the number of set bits (population count)
    pub fn count_ones(&self) -> u64 {
        if self.width > 64 {
            return self
                .limbs()
                .iter()
                .map(|limb| u64::from(limb.count_ones()))
                .sum();
        }
        let mask = Self::mask_for_width(self.width);
        u64::from((self.value & mask).count_ones())
    }

    fn sext_u128(&self) -> u128 {
        if self.is_negative() {
            (self.value as u128) | (u128::MAX << self.width)
        } else {
            self.value as u128
        }
    }

    fn mask_for_width(width: u64) -> u64 {
        if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        }
    }

    fn decimal(&self) -> String {
        const CHUNK: u64 = 10_000_000_000_000_000_000;
        if self.width <= 64 {
            return self.value.to_string();
        }
        let mut words = self.limbs();
        let mut chunks = Vec::new();
        while words.iter().any(|&word| word != 0) {
            let mut remainder = 0u128;
            for word in words.iter_mut().rev() {
                let current = (remainder << 64) | u128::from(*word);
                *word = (current / u128::from(CHUNK)) as u64;
                remainder = current % u128::from(CHUNK);
            }
            chunks.push(remainder as u64);
        }
        let mut text = chunks.pop().unwrap_or(0).to_string();
        for chunk in chunks.iter().rev() {
            text.push_str(&format!("{chunk:019}"));
        }
        text
    }
}

fn limb_count(width: u64) -> usize {
    width.div_ceil(64) as usize
}

fn add_words(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut carry = false;
    a.iter()
        .zip(b)
        .map(|(&a, &b)| {
            let (sum, first) = a.overflowing_add(b);
            let (sum, second) = sum.overflowing_add(u64::from(carry));
            carry = first || second;
            sum
        })
        .collect()
}

fn sub_words(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut borrow = false;
    a.iter()
        .zip(b)
        .map(|(&a, &b)| {
            let (difference, first) = a.overflowing_sub(b);
            let (difference, second) = difference.overflowing_sub(u64::from(borrow));
            borrow = first || second;
            difference
        })
        .collect()
}

// The low `a.len()` limbs of the product.
fn mul_words(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut product = vec![0u64; a.len()];
    for (i, &a) in a.iter().enumerate() {
        let mut carry = 0u128;
        for (j, &b) in b.iter().enumerate().take(product.len() - i) {
            let current = u128::from(a) * u128::from(b) + u128::from(product[i + j]) + carry;
            product[i + j] = current as u64;
            carry = current >> 64;
        }
    }
    product
}

fn shl_words(words: &[u64], shift: u64) -> Vec<u64> {
    let (limbs, bits) = ((shift / 64) as usize, shift % 64);
    (0..words.len())
        .map(|i| {
            let at = |k: usize| i.checked_sub(k).map_or(0, |index| words[index]);
            match bits {
                0 => at(limbs),
                _ => at(limbs) << bits | at(limbs + 1) >> (64 - bits),
            }
        })
        .collect()
}

fn lshr_words(words: &[u64], shift: u64) -> Vec<u64> {
    let (limbs, bits) = ((shift / 64) as usize, shift % 64);
    (0..words.len())
        .map(|i| {
            let at = |k: usize| words.get(i + k).copied().unwrap_or(0);
            match bits {
                0 => at(limbs),
                _ => at(limbs) >> bits | at(limbs + 1) << (64 - bits),
            }
        })
        .collect()
}

fn cmp_words(a: &[u64], b: &[u64]) -> Ordering {
    a.iter().rev().cmp(b.iter().rev())
}

// Restoring division, a bit at a time.
fn divrem_words(a: &[u64], b: &[u64], width: u64) -> (Vec<u64>, Vec<u64>) {
    let mut quotient = vec![0u64; a.len()];
    // One limb more than the operands: the remainder shifted left may pass
    // the width before the divisor is taken off it.
    let mut remainder = vec![0u64; a.len() + 1];
    let mut divisor = b.to_vec();
    divisor.push(0);
    for bit in (0..width).rev() {
        remainder = shl_words(&remainder, 1);
        remainder[0] |= a[(bit / 64) as usize] >> (bit % 64) & 1;
        if cmp_words(&remainder, &divisor) != Ordering::Less {
            remainder = sub_words(&remainder, &divisor);
            quotient[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }
    remainder.pop();
    (quotient, remainder)
}

impl Ord for APInt {
    /// Numeric value, then width/signedness, so the order is total and consistent with `Eq`/`Hash`.
    fn cmp(&self, other: &Self) -> Ordering {
        self.numeric_cmp(other)
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
        if self.signed && self.width <= 64 {
            write!(f, "{}", self.to_i64())
        } else if self.is_negative() {
            write!(f, "-{}", self.neg().decimal())
        } else {
            write!(f, "{}", self.decimal())
        }
    }
}

impl fmt::Binary for APInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits: String = (0..self.width)
            .rev()
            .map(|bit| if self.bit(bit) { '1' } else { '0' })
            .collect();
        f.write_str(&digits)
    }
}

impl fmt::LowerHex for APInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let limbs = self.limbs();
        let top = limbs.iter().rposition(|&limb| limb != 0).unwrap_or(0);
        write!(f, "{:x}", limbs[top])?;
        limbs[..top]
            .iter()
            .rev()
            .try_for_each(|limb| write!(f, "{limb:016x}"))
    }
}

impl fmt::UpperHex for APInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format!("{self:x}").to_uppercase())
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

        let mut words = vec![0u64];
        for ch in clean.chars() {
            let digit = ch
                .to_digit(radix as u32)
                .ok_or_else(|| format!("invalid digit '{ch}' for radix {radix}"))?
                as u64;
            let mut carry = u128::from(digit);
            for word in &mut words {
                let current = u128::from(*word) * u128::from(radix) + carry;
                *word = current as u64;
                carry = current >> 64;
            }
            if carry != 0 {
                words.push(carry as u64);
            }
        }

        let top = words.len() - 1;
        let width = (64 * top as u64 + 64 - u64::from(words[top].leading_zeros())).max(1);
        Ok(APInt::from_words(width, false, words))
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

    // Signed comparison is defined on the bit pattern, like signed division:
    // a lane that came out of an `extract` or `split` carries no signedness
    // flag but still has its high bit as the sign.
    #[test]
    fn signed_compare_uses_the_bits_not_the_flag() {
        let minus_16 = APInt::new(8, 0xF0);
        let plus_48 = APInt::new(8, 0x30);
        assert!(minus_16.slt(&plus_48));
        assert!(!minus_16.sgt(&plus_48));
        assert!(plus_48.sgt(&minus_16));
        assert!(minus_16.ugt(&plus_48));
    }

    #[test]
    fn signed_compare_at_narrow_widths() {
        let min = APInt::new(9, 0x100);
        let max = APInt::new(9, 0x0FF);
        assert!(min.slt(&max));
        assert!(max.sgt(&min));
        assert!(min.ugt(&max));
    }

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
        fn test_shl(x in prop::num::u64::ANY, s in 0u64..64) {
            let res = APInt::from_u64(x).shl(s);
            prop_assert_eq!(res.to_u64(), x << s);
        }

        #[test]
        fn test_lshr(x in prop::num::u64::ANY, s in 0u64..64) {
            let res = APInt::from_u64(x).lshr(s);
            prop_assert_eq!(res.to_u64(), x >> s);
        }

        #[test]
        fn test_ashr(x in prop::num::i64::ANY, s in 0u64..64) {
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

    // Wide values against Rust's 128-bit arithmetic, at 128 bits and at a
    // width that ends inside a limb.
    fn wide(width: u64, value: u128) -> APInt {
        APInt::from_limbs(width, &[value as u64, (value >> 64) as u64])
    }

    fn wide_value(value: &APInt) -> u128 {
        let limbs = value.limbs();
        u128::from(limbs[0]) | u128::from(limbs[1]) << 64
    }

    fn masked(width: u64, value: u128) -> u128 {
        value & (u128::MAX >> (128 - width))
    }

    fn signed(width: u64, value: u128) -> i128 {
        ((value << (128 - width)) as i128) >> (128 - width)
    }

    #[test]
    fn wide_display_and_parse() {
        let value: APInt = "340282366920938463463374607431768211455".parse().unwrap();
        assert_eq!(value.width(), 128);
        assert_eq!(value.to_string(), u128::MAX.to_string());
        assert_eq!(format!("{value:x}"), format!("{:x}", u128::MAX));
        let minus_two = APInt::new_signed(200, -2);
        assert_eq!(minus_two.to_string(), "-2");
        assert_eq!(minus_two.count_ones(), 199);
    }

    proptest! {
        #[test]
        fn wide_arithmetic(x in any::<u128>(), y in any::<u128>(), width in prop::sample::select(vec![65u64, 100, 128])) {
            let (a, b) = (wide(width, x), wide(width, y));
            let (x, y) = (masked(width, x), masked(width, y));
            prop_assert_eq!(wide_value(&APInt::add(&a, &b)), masked(width, x.wrapping_add(y)));
            prop_assert_eq!(wide_value(&APInt::sub(&a, &b)), masked(width, x.wrapping_sub(y)));
            prop_assert_eq!(wide_value(&APInt::mul(&a, &b)), masked(width, x.wrapping_mul(y)));
            prop_assert_eq!(wide_value(&APInt::neg(&a)), masked(width, x.wrapping_neg()));
            prop_assert_eq!(wide_value(&APInt::not(&a)), masked(width, !x));
            prop_assert_eq!(wide_value(&a.and(&b)), x & y);
            prop_assert_eq!(a.ucmp(&b), x.cmp(&y));
            prop_assert_eq!(a.scmp(&b), signed(width, x).cmp(&signed(width, y)));
            if let (Some(quotient), Some(remainder)) = (x.checked_div(y), x.checked_rem(y)) {
                prop_assert_eq!(wide_value(&a.udiv(&b)), quotient);
                prop_assert_eq!(wide_value(&a.urem(&b)), remainder);
                let (sx, sy) = (signed(width, x), signed(width, y));
                prop_assert_eq!(wide_value(&a.sdiv(&b)), masked(width, sx.wrapping_div(sy) as u128));
                prop_assert_eq!(wide_value(&a.srem(&b)), masked(width, sx.wrapping_rem(sy) as u128));
            }
        }

        #[test]
        fn wide_shifts(x in any::<u128>(), s in 0u64..140, width in prop::sample::select(vec![65u64, 100, 128])) {
            let a = wide(width, x).with_signed(true);
            let x = masked(width, x);
            let expect = |v: u128| if s >= width { 0 } else { masked(width, v) };
            prop_assert_eq!(wide_value(&a.shl(s)), expect(x << s.min(127)));
            prop_assert_eq!(wide_value(&a.lshr(s)), expect(x >> s.min(127)));
            let ashr = (signed(width, x) >> s.min(width - 1)) as u128;
            prop_assert_eq!(wide_value(&a.ashr(s)), masked(width, ashr));
            prop_assert_eq!(a.count_leading_zeros(), if x == 0 { width } else { u64::from(x.leading_zeros()) - (128 - width) });
            prop_assert_eq!(a.count_trailing_zeros(), if x == 0 { width } else { u64::from(x.trailing_zeros()) });
        }

        #[test]
        fn wide_extensions(x in prop::num::i64::ANY) {
            let extended = APInt::new_signed(64, x).sign_extend(128);
            prop_assert_eq!(wide_value(&extended), x as i128 as u128);
            prop_assert_eq!(wide_value(&APInt::new_signed(128, x)), x as i128 as u128);
            prop_assert_eq!(extended.truncate(64).to_i64(), x);
            prop_assert_eq!(wide_value(&extended.extract_bits(127, 64).zero_extend(128)), (x as i128 >> 64) as u64 as u128);
            let product = APInt::from_i64(x).zero_extend(128).mulhu(&APInt::from_u64(3).zero_extend(128));
            prop_assert_eq!(wide_value(&product), 0);
        }
    }

    fn arb_apint() -> impl Strategy<Value = APInt> {
        (1u64..=64, any::<bool>(), any::<u64>()).prop_map(|(w, signed, v)| {
            if signed {
                APInt::new_signed(w, v as i64)
            } else {
                APInt::new(w, v)
            }
        })
    }
}
