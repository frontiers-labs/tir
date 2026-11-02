// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/sort/bigint.rs and src/sort/bigrat.rs. See libs/egglog/LICENSE-MIT.

use std::cmp::Ordering;
use std::fmt;
use std::ops::Add;

use tir_adt::APInt;

use super::prim;
use crate::ast::Literal;
use crate::prelude::BaseSort;
use crate::{BaseValue, BaseValues, Database, EGraph, TermDag, TermId, Value};

/// An integer of any size: a signed [`APInt`] of the fewest bits that hold
/// it, so equal integers are equal values.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BigInt(APInt);

impl BaseValue for BigInt {}

impl BigInt {
    pub fn new(value: i64) -> Self {
        Self::minimal(APInt::from_i64(value))
    }

    fn minimal(x: APInt) -> Self {
        let x = x.with_signed(true);
        let magnitude = if x.is_negative() { x.not() } else { x.clone() };
        let width = (x.width() - magnitude.count_leading_zeros() + 1).min(x.width());
        Self(x.truncate(width).with_signed(true))
    }

    fn at(&self, width: u64) -> APInt {
        self.0.sign_extend(width).with_signed(true)
    }

    // `f` on both operands sign-extended to `extra` bits past the wider one.
    fn apply(&self, other: &Self, extra: u64, f: impl Fn(&APInt, &APInt) -> APInt) -> Self {
        let width = self.0.width().max(other.0.width()) + extra;
        Self::minimal(f(&self.at(width), &other.at(width)))
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn is_negative(&self) -> bool {
        self.0.is_negative()
    }

    fn mul(&self, other: &Self) -> Self {
        self.apply(other, self.0.width().min(other.0.width()), APInt::mul)
    }

    fn sub(&self, other: &Self) -> Self {
        self.apply(other, 1, APInt::sub)
    }

    fn div(&self, other: &Self) -> Option<Self> {
        (!other.is_zero()).then(|| self.apply(other, 1, APInt::sdiv))
    }

    fn rem(&self, other: &Self) -> Option<Self> {
        (!other.is_zero()).then(|| self.apply(other, 1, APInt::srem))
    }

    fn neg(&self) -> Self {
        Self::minimal(self.at(self.0.width() + 1).neg())
    }

    fn abs(&self) -> Self {
        if self.is_negative() {
            self.neg()
        } else {
            self.clone()
        }
    }

    fn shift(&self, by: i64, left: bool) -> Option<Self> {
        let by = u64::try_from(by).ok()?;
        let width = self.0.width() + if left { by } else { 0 };
        let x = self.at(width);
        Some(Self::minimal(if left {
            x.shl(by)
        } else {
            x.ashr(by.min(width - 1))
        }))
    }

    fn gcd(&self, other: &Self) -> Self {
        let (mut a, mut b) = (self.abs(), other.abs());
        while let Some(r) = a.rem(&b) {
            (a, b) = (b, r);
        }
        a
    }

    fn sqrt(&self) -> Option<Self> {
        if self.is_negative() {
            return None;
        }
        let two = Self::new(2);
        let mut x = Self::new(1).shift(self.0.width().div_ceil(2) as i64, true)?;
        loop {
            let y = x.add(&self.div(&x)?).div(&two)?;
            if y >= x {
                return (x.mul(&x) == *self).then_some(x);
            }
            x = y;
        }
    }

    fn to_i64(&self) -> Option<i64> {
        (self.0.width() <= 64).then(|| self.0.to_i64())
    }

    fn to_f64(&self) -> f64 {
        self.to_string().parse().expect("a decimal integer")
    }
}

impl Add for &BigInt {
    type Output = BigInt;

    fn add(self, other: &BigInt) -> BigInt {
        self.apply(other, 1, APInt::add)
    }
}

impl Ord for BigInt {
    fn cmp(&self, other: &Self) -> Ordering {
        let width = self.0.width().max(other.0.width());
        self.at(width).scmp(&other.at(width))
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for BigInt {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (negative, digits) = match text.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("`{text}` is no decimal integer"));
        }
        let magnitude: APInt = digits.parse()?;
        let x = Self::minimal(magnitude.zero_extend(magnitude.width() + 1));
        Ok(if negative { x.neg() } else { x })
    }
}

/// A rational number: a numerator and a positive denominator with no common
/// factor.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BigRat {
    num: BigInt,
    den: BigInt,
}

impl BaseValue for BigRat {}

impl BigRat {
    /// `num / den` in lowest terms; `None` for a zero denominator.
    pub fn new(num: BigInt, den: BigInt) -> Option<Self> {
        if den.is_zero() {
            return None;
        }
        let g = num.gcd(&den);
        let (num, den) = (num.div(&g)?, den.div(&g)?);
        Some(if den.is_negative() {
            Self {
                num: num.neg(),
                den: den.neg(),
            }
        } else {
            Self { num, den }
        })
    }

    pub fn numer(&self) -> &BigInt {
        &self.num
    }

    pub fn denom(&self) -> &BigInt {
        &self.den
    }

    fn integer(value: BigInt) -> Self {
        Self {
            num: value,
            den: BigInt::new(1),
        }
    }

    fn mul(&self, other: &Self) -> Option<Self> {
        Self::new(self.num.mul(&other.num), self.den.mul(&other.den))
    }

    fn recip(&self) -> Option<Self> {
        Self::new(self.den.clone(), self.num.clone())
    }

    fn floor(&self) -> Self {
        let q = self.num.div(&self.den).expect("a positive denominator");
        let inexact = !self
            .num
            .rem(&self.den)
            .expect("a positive denominator")
            .is_zero();
        Self::integer(if inexact && self.num.is_negative() {
            q.sub(&BigInt::new(1))
        } else {
            q
        })
    }

    fn ceil(&self) -> Self {
        self.neg().floor().neg()
    }

    fn neg(&self) -> Self {
        Self {
            num: self.num.neg(),
            den: self.den.clone(),
        }
    }

    // Halves round away from zero.
    fn round(&self) -> Option<Self> {
        if self.num.is_negative() {
            return Some(self.neg().round()?.neg());
        }
        let half = Self::new(BigInt::new(1), BigInt::new(2))?;
        Some((self + &half).floor())
    }

    fn pow(&self, exponent: &Self) -> Option<Self> {
        if exponent.den != BigInt::new(1) {
            return None;
        }
        if exponent.num.is_negative() {
            return self.recip()?.pow(&exponent.neg());
        }
        let (mut base, mut n, mut out) = (
            self.clone(),
            exponent.num.to_i64()?,
            Self::integer(BigInt::new(1)),
        );
        while n > 0 {
            if n & 1 == 1 {
                out = out.mul(&base)?;
            }
            base = base.mul(&base)?;
            n >>= 1;
        }
        Some(out)
    }
}

impl Add for &BigRat {
    type Output = BigRat;

    fn add(self, other: &BigRat) -> BigRat {
        let num = &self.num.mul(&other.den) + &other.num.mul(&self.den);
        BigRat::new(num, self.den.mul(&other.den)).expect("a positive denominator")
    }
}

impl Ord for BigRat {
    fn cmp(&self, other: &Self) -> Ordering {
        self.num.mul(&other.den).cmp(&other.num.mul(&self.den))
    }
}

impl PartialOrd for BigRat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
pub struct BigIntSort;

impl BaseSort for BigIntSort {
    type Base = BigInt;

    fn name(&self) -> &str {
        "BigInt"
    }

    fn register_primitives(&self, eg: &mut EGraph) {
        register_bigint(eg.database_mut());
    }

    fn reconstruct_termdag(
        &self,
        base: &BaseValues,
        value: Value,
        termdag: &mut TermDag,
    ) -> TermId {
        let digits = termdag.lit(Literal::String(base.unwrap::<BigInt>(value).to_string()));
        termdag.app("from-string".to_owned(), vec![digits])
    }
}

#[derive(Debug)]
pub struct BigRatSort;

impl BaseSort for BigRatSort {
    type Base = BigRat;

    fn name(&self) -> &str {
        "BigRat"
    }

    fn register_primitives(&self, eg: &mut EGraph) {
        register_bigrat(eg.database_mut());
    }

    fn reconstruct_termdag(
        &self,
        base: &BaseValues,
        value: Value,
        termdag: &mut TermDag,
    ) -> TermId {
        let rat = base.unwrap::<BigRat>(value);
        let parts = [rat.num, rat.den].map(|part| {
            let digits = termdag.lit(Literal::String(part.to_string()));
            termdag.app("from-string".to_owned(), vec![digits])
        });
        termdag.app("bigrat".to_owned(), parts.to_vec())
    }
}

fn sorts<const N: usize>(db: &Database, names: [&str; N]) -> [crate::SortId; N] {
    names.map(|name| db.sort(name).expect("a declared sort"))
}

#[rustfmt::skip]
fn register_bigint(db: &mut Database) {
    let [z, i, b, s, u] = sorts(db, ["BigInt", "i64", "bool", "String", "Unit"]);
    prim!(db, "bigint", [i] -> z, |a: i64| Some(BigInt::new(a)));
    prim!(db, "+", [z, z] -> z, |a: BigInt, c: BigInt| Some(&a + &c));
    prim!(db, "-", [z, z] -> z, |a: BigInt, c: BigInt| Some(a.sub(&c)));
    prim!(db, "*", [z, z] -> z, |a: BigInt, c: BigInt| Some(a.mul(&c)));
    prim!(db, "/", [z, z] -> z, |a: BigInt, c: BigInt| a.div(&c));
    prim!(db, "%", [z, z] -> z, |a: BigInt, c: BigInt| a.rem(&c));
    prim!(db, "&", [z, z] -> z, |a: BigInt, c: BigInt| Some(a.apply(&c, 0, APInt::and)));
    prim!(db, "|", [z, z] -> z, |a: BigInt, c: BigInt| Some(a.apply(&c, 0, APInt::or)));
    prim!(db, "^", [z, z] -> z, |a: BigInt, c: BigInt| Some(a.apply(&c, 0, APInt::xor)));
    prim!(db, "<<", [z, i] -> z, |a: BigInt, n: i64| a.shift(n, true));
    prim!(db, ">>", [z, i] -> z, |a: BigInt, n: i64| a.shift(n, false));
    prim!(db, "not-Z", [z] -> z, |a: BigInt| Some(BigInt::minimal(a.0.not())));
    prim!(db, "bits", [z] -> z, |a: BigInt| {
        let magnitude = a.abs().0;
        i64::try_from(magnitude.width() - magnitude.count_leading_zeros()).ok().map(BigInt::new)
    });
    prim!(db, "<", [z, z] -> u, |a: BigInt, c: BigInt| (a < c).then_some(()));
    prim!(db, ">", [z, z] -> u, |a: BigInt, c: BigInt| (a > c).then_some(()));
    prim!(db, "<=", [z, z] -> u, |a: BigInt, c: BigInt| (a <= c).then_some(()));
    prim!(db, ">=", [z, z] -> u, |a: BigInt, c: BigInt| (a >= c).then_some(()));
    prim!(db, "bool-=", [z, z] -> b, |a: BigInt, c: BigInt| Some(a == c));
    prim!(db, "bool-<", [z, z] -> b, |a: BigInt, c: BigInt| Some(a < c));
    prim!(db, "bool->", [z, z] -> b, |a: BigInt, c: BigInt| Some(a > c));
    prim!(db, "bool-<=", [z, z] -> b, |a: BigInt, c: BigInt| Some(a <= c));
    prim!(db, "bool->=", [z, z] -> b, |a: BigInt, c: BigInt| Some(a >= c));
    prim!(db, "min", [z, z] -> z, |a: BigInt, c: BigInt| Some(a.min(c)));
    prim!(db, "max", [z, z] -> z, |a: BigInt, c: BigInt| Some(a.max(c)));
    prim!(db, "to-string", [z] -> s, |a: BigInt| Some(a.to_string()));
    prim!(db, "from-string", [s] -> z, |a: String| a.parse::<BigInt>().ok());
}

#[rustfmt::skip]
fn register_bigrat(db: &mut Database) {
    let [q, z, i, f, u] = sorts(db, ["BigRat", "BigInt", "i64", "f64", "Unit"]);
    prim!(db, "+", [q, q] -> q, |a: BigRat, c: BigRat| Some(&a + &c));
    prim!(db, "-", [q, q] -> q, |a: BigRat, c: BigRat| Some(&a + &c.neg()));
    prim!(db, "*", [q, q] -> q, |a: BigRat, c: BigRat| a.mul(&c));
    prim!(db, "/", [q, q] -> q, |a: BigRat, c: BigRat| a.mul(&c.recip()?));
    prim!(db, "min", [q, q] -> q, |a: BigRat, c: BigRat| Some(a.min(c)));
    prim!(db, "max", [q, q] -> q, |a: BigRat, c: BigRat| Some(a.max(c)));
    prim!(db, "neg", [q] -> q, |a: BigRat| Some(a.neg()));
    prim!(db, "abs", [q] -> q, |a: BigRat| Some(if a.num.is_negative() { a.neg() } else { a }));
    prim!(db, "floor", [q] -> q, |a: BigRat| Some(a.floor()));
    prim!(db, "ceil", [q] -> q, |a: BigRat| Some(a.ceil()));
    prim!(db, "round", [q] -> q, |a: BigRat| a.round());
    prim!(db, "bigrat", [z, z] -> q, |n: BigInt, d: BigInt| BigRat::new(n, d));
    prim!(db, "numer", [q] -> z, |a: BigRat| Some(a.num));
    prim!(db, "denom", [q] -> z, |a: BigRat| Some(a.den));
    prim!(db, "to-f64", [q] -> f, |a: BigRat| Some(a.num.to_f64() / a.den.to_f64()));
    prim!(db, "to-i64", [q] -> i, |a: BigRat| (a.den == BigInt::new(1)).then(|| a.num.to_i64()).flatten());
    prim!(db, "pow", [q, q] -> q, |a: BigRat, c: BigRat| a.pow(&c));
    prim!(db, "log", [q] -> q, |a: BigRat| (a == BigRat::integer(BigInt::new(1))).then(|| BigRat::integer(BigInt::new(0))));
    prim!(db, "sqrt", [q] -> q, |a: BigRat| BigRat::new(a.num.sqrt()?, a.den.sqrt()?));
    prim!(db, "cbrt", [q] -> q, |a: BigRat| (a == BigRat::integer(BigInt::new(1))).then_some(a));
    prim!(db, "<", [q, q] -> u, |a: BigRat, c: BigRat| (a < c).then_some(()));
    prim!(db, ">", [q, q] -> u, |a: BigRat, c: BigRat| (a > c).then_some(()));
    prim!(db, "<=", [q, q] -> u, |a: BigRat, c: BigRat| (a <= c).then_some(()));
    prim!(db, ">=", [q, q] -> u, |a: BigRat, c: BigRat| (a >= c).then_some(()));
}
