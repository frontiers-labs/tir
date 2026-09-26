//! Arithmetic, shift and comparison circuits; all vectors little-endian (bit 0 = LSB).

use tir_adt::NodeId;

use super::{BitblastError, Blaster, Shift};
use crate::lang::SymKind;
use crate::sat::Lit;

impl<V> Blaster<'_, V> {
    /// Ripple-carry adder: returns the width-`n` sum and the carry-out.
    pub(super) fn adder(&mut self, a: &[Lit], b: &[Lit], carry_in: Lit) -> (Vec<Lit>, Lit) {
        let mut carry = carry_in;
        let mut sum = Vec::with_capacity(a.len());
        for (&ai, &bi) in a.iter().zip(b) {
            let axb = self.gate_xor(ai, bi);
            let s = self.gate_xor(axb, carry);
            let ab = self.gate_and(ai, bi);
            let cx = self.gate_and(carry, axb);
            carry = self.gate_or(ab, cx);
            sum.push(s);
        }
        (sum, carry)
    }

    fn invert(bits: &[Lit]) -> Vec<Lit> {
        bits.iter().map(|l| l.negate()).collect()
    }

    /// `a - b` two's complement: returns the difference and the carry-out, where
    /// the carry-out is the unsigned `a >= b` predicate.
    fn sub_carry(&mut self, a: &[Lit], b: &[Lit]) -> (Vec<Lit>, Lit) {
        let nb = Self::invert(b);
        let one = self.one;
        self.adder(a, &nb, one)
    }

    /// `a - b` (two's complement, width preserved).
    pub(super) fn subtract(&mut self, a: &[Lit], b: &[Lit]) -> Vec<Lit> {
        self.sub_carry(a, b).0
    }

    /// `-a` (two's complement negation).
    pub(super) fn negate(&mut self, a: &[Lit]) -> Vec<Lit> {
        let na = Self::invert(a);
        let zeros = vec![self.zero(); a.len()];
        let one = self.one;
        self.adder(&na, &zeros, one).0
    }

    /// Shift-and-add multiplier, result truncated to the operand width.
    pub(super) fn multiply(&mut self, a: &[Lit], b: &[Lit]) -> Vec<Lit> {
        let w = a.len();
        let mut acc = vec![self.zero(); w];
        for (i, &bi) in b.iter().enumerate() {
            // partial = (a & bi) << i, truncated to w bits.
            let mut partial = vec![self.zero(); w];
            for j in 0..(w - i) {
                partial[j + i] = self.gate_and(a[j], bi);
            }
            let z = self.zero();
            acc = self.adder(&acc, &partial, z).0;
        }
        acc
    }

    /// `a >= b` for equal-width unsigned vectors: the carry-out of `a - b`.
    pub(super) fn uge(&mut self, a: &[Lit], b: &[Lit]) -> Lit {
        self.sub_carry(a, b).1
    }

    /// Encode a comparison node into its one-bit result.
    pub(super) fn compare(&mut self, id: NodeId, kind: SymKind) -> Result<Vec<Lit>, BitblastError> {
        let (a, b) = self.binop_bits(id);
        // Signed comparisons reduce to unsigned ones with the sign bit flipped.
        let flip = |v: &[Lit]| {
            let mut v = v.to_vec();
            let top = v.len() - 1;
            v[top] = v[top].negate();
            v
        };
        let bit = match kind {
            SymKind::ULt => self.uge(&a, &b).negate(),
            SymKind::UGe => self.uge(&a, &b),
            SymKind::UGt => self.uge(&b, &a).negate(),
            SymKind::ULe => self.uge(&b, &a),
            SymKind::Lt => self.uge(&flip(&a), &flip(&b)).negate(),
            SymKind::Ge => self.uge(&flip(&a), &flip(&b)),
            SymKind::Gt => self.uge(&flip(&b), &flip(&a)).negate(),
            SymKind::Le => self.uge(&flip(&b), &flip(&a)),
            _ => unreachable!("compare called with non-comparison kind"),
        };
        Ok(vec![bit])
    }

    /// Encode bvudiv/bvurem/bvsdiv/bvsrem; `want_quotient` picks the quotient,
    /// `signed` applies the two's-complement sign rules. SMT-LIB div-by-zero
    /// (`bvudiv x 0 = ~0`, `bvurem x 0 = x`) falls out of the unsigned core.
    pub(super) fn divrem(
        &mut self,
        id: NodeId,
        signed: bool,
        want_quotient: bool,
    ) -> Result<Vec<Lit>, BitblastError> {
        let (a, b) = self.binop_bits(id);
        if !signed {
            let (q, r) = self.udivrem(&a, &b);
            return Ok(if want_quotient { q } else { r });
        }

        let sign_a = *a.last().expect("non-empty operand");
        let sign_b = *b.last().expect("non-empty operand");
        let neg_a = self.negate(&a);
        let neg_b = self.negate(&b);
        let mag_a = self.mux_bits(sign_a, &neg_a, &a);
        let mag_b = self.mux_bits(sign_b, &neg_b, &b);
        let (q, r) = self.udivrem(&mag_a, &mag_b);

        if want_quotient {
            // Quotient sign is the xor of operand signs.
            let result_sign = self.gate_xor(sign_a, sign_b);
            let neg_q = self.negate(&q);
            Ok(self.mux_bits(result_sign, &neg_q, &q))
        } else {
            // Remainder follows the dividend's sign.
            let neg_r = self.negate(&r);
            Ok(self.mux_bits(sign_a, &neg_r, &r))
        }
    }

    /// Restoring division on unsigned width-`w` vectors, returning
    /// `(quotient, remainder)` each `w` bits.
    fn udivrem(&mut self, a: &[Lit], b: &[Lit]) -> (Vec<Lit>, Vec<Lit>) {
        let w = a.len();
        let zero = self.zero();
        // Remainder carries one guard bit so `rem << 1` cannot overflow.
        let mut rem = vec![zero; w + 1];
        let mut b_ext = b.to_vec();
        b_ext.push(zero);
        let mut quot = vec![zero; w];

        for i in (0..w).rev() {
            // rem = (rem << 1) | a_i
            for k in (1..=w).rev() {
                rem[k] = rem[k - 1];
            }
            rem[0] = a[i];

            let (diff, ge) = self.sub_carry(&rem, &b_ext);
            rem = self.mux_bits(ge, &diff, &rem);
            quot[i] = ge;
        }
        rem.truncate(w);
        (quot, rem)
    }

    /// Barrel shifter for `bvshl`/`bvlshr`/`bvashr`.
    pub(super) fn shift(&mut self, id: NodeId, kind: Shift) -> Result<Vec<Lit>, BitblastError> {
        let (a, amount) = self.binop_bits(id);
        let w = a.len();
        let fill = match kind {
            Shift::Arithmetic => *a.last().expect("non-empty operand"),
            _ => self.zero(),
        };

        // Stages 2^0..up to the width cover every in-range shift; higher amount bits overflow.
        let mut stages = 0usize;
        while (1usize << stages) < w {
            stages += 1;
        }

        let mut cur = a;
        for (k, &amount_bit) in amount.iter().enumerate().take(stages) {
            let by = 1usize << k;
            let shifted = self.shift_const(&cur, by, kind, fill);
            cur = self.mux_bits(amount_bit, &shifted, &cur);
        }

        // Any higher amount bit set means shift >= w: produce the fill pattern.
        let mut overflow = self.zero();
        for &bit in &amount[stages..] {
            overflow = self.gate_or(overflow, bit);
        }
        let saturated = vec![fill; w];
        Ok(self.mux_bits(overflow, &saturated, &cur))
    }

    /// Shift `bits` by constant `by`, filling vacated positions with `fill`.
    fn shift_const(&self, bits: &[Lit], by: usize, kind: Shift, fill: Lit) -> Vec<Lit> {
        let w = bits.len();
        (0..w)
            .map(|p| match kind {
                Shift::Left => {
                    if p >= by {
                        bits[p - by]
                    } else {
                        fill
                    }
                }
                Shift::Logical | Shift::Arithmetic => {
                    if p + by < w {
                        bits[p + by]
                    } else {
                        fill
                    }
                }
            })
            .collect()
    }
}
