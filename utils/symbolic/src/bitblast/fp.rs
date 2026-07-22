//! Integer-to-float conversion circuits (`SIToFP`/`UIToFP`) as combinational
//! bit-vector logic, so the QF_BV oracle can prove float-conversion identities.
//! All vectors little-endian (bit 0 = LSB). Round-to-nearest, ties-to-even; the
//! input range (≤64 bits) never overflows the exponent, so no infinity/NaN path.

use tir_graph::{Dag, NodeId};

use super::{BitblastError, Blaster};
use crate::sat::Lit;

impl<V> Blaster<'_, V> {
    /// `SIToFP`/`UIToFP`: children are `[value, exponent_width, mantissa_width]`.
    pub(super) fn encode_int_to_float(
        &mut self,
        id: NodeId,
        signed: bool,
    ) -> Result<Vec<Lit>, BitblastError> {
        let value = self.child_bits(id, 0);
        let children: Vec<NodeId> = self.graph.children(id).collect();
        let exp = self.const_u64(children[1])? as usize;
        let mant = self.const_u64(children[2])? as usize;
        Ok(self.int_to_float(&value, signed, exp, mant))
    }

    /// Convert the integer `value` to the IEEE binary format with `e` exponent
    /// and `m` mantissa bits. The leading one is barrel-normalized to the top of
    /// a power-of-two working register; the mantissa is the `m` bits below it,
    /// rounded with the remaining guard and sticky bits.
    fn int_to_float(&mut self, value: &[Lit], signed: bool, e: usize, m: usize) -> Vec<Lit> {
        let zero = self.zero();
        let (sign, magnitude) = if signed {
            let sign = *value.last().expect("non-empty operand");
            let neg = self.negate(value);
            (sign, self.mux_bits(sign, &neg, value))
        } else {
            (zero, value.to_vec())
        };

        // A power-of-two register wide enough to hold the value plus a guard bit
        // below the mantissa, so every dropped bit contributes to the sticky.
        let mut l = 1usize;
        while l < value.len().max(m + 2) {
            l <<= 1;
        }
        let mut cur = magnitude;
        cur.resize(l, zero);
        let is_zero = self.nor(&cur);

        // Barrel-normalize: shift the leading one up to bit `l - 1`, recording the
        // shift count (the count of leading zeros) as the exponent adjustment.
        let mut stages = 0usize;
        while (1usize << stages) < l {
            stages += 1;
        }
        let mut shift = vec![zero; stages];
        for k in (0..stages).rev() {
            let by = 1usize << k;
            let high_zero = self.nor(&cur[l - by..l]);
            let shifted = shift_left(&cur, by, zero);
            cur = self.mux_bits(high_zero, &shifted, &cur);
            shift[k] = high_zero;
        }

        // Split the normalized register at the mantissa boundary: leading one at
        // `l - 1`, `m` fraction bits below it, then a guard bit and the sticky OR.
        let frac_lo = l - 1 - m;
        let guard = cur[frac_lo - 1];
        let sticky = self.or_reduce(&cur[..frac_lo - 1]);
        let frac_lsb = cur[frac_lo];
        let sticky_or_lsb = self.gate_or(sticky, frac_lsb);
        let round_up = self.gate_and(guard, sticky_or_lsb);

        // Round the significand (leading one + fraction). A carry past its top bit
        // means it rolled to 2.0: the fraction is zero and the exponent gains one.
        let mut significand = cur[frac_lo..l].to_vec();
        significand.push(zero);
        let addend = vec![zero; significand.len()];
        let (rounded, _) = self.adder(&significand, &addend, round_up);
        let overflow = rounded[m + 1];
        let fraction = rounded[..m].to_vec();

        // biased exponent = (l - 1 + bias) - shift + overflow, at `e` bits.
        let bias = (1u64 << (e - 1)) - 1;
        let base = self.const_bits((l as u64 - 1) + bias, e);
        let mut shift_ext = shift;
        shift_ext.resize(e, zero);
        let unrounded = self.subtract(&base, &shift_ext);
        let padding = vec![zero; e];
        let (biased, _) = self.adder(&unrounded, &padding, overflow);

        let mut out = fraction;
        out.extend_from_slice(&biased);
        out.push(sign);
        let zeros = vec![zero; out.len()];
        self.mux_bits(is_zero, &zeros, &out)
    }

    /// One bit: whether every bit of `bits` is zero.
    fn nor(&mut self, bits: &[Lit]) -> Lit {
        self.or_reduce(bits).negate()
    }

    /// One bit: the OR of every bit of `bits` (false for an empty slice).
    fn or_reduce(&mut self, bits: &[Lit]) -> Lit {
        let mut acc = self.zero();
        for &b in bits {
            acc = self.gate_or(acc, b);
        }
        acc
    }

    /// The width-`w` constant `value` as fixed literals.
    fn const_bits(&self, value: u64, w: usize) -> Vec<Lit> {
        let (one, zero) = (self.one, self.zero());
        (0..w)
            .map(|i| if (value >> i) & 1 == 1 { one } else { zero })
            .collect()
    }
}

/// Shift `bits` left by the constant `by`, filling vacated low positions with `fill`.
fn shift_left(bits: &[Lit], by: usize, fill: Lit) -> Vec<Lit> {
    (0..bits.len())
        .map(|p| if p >= by { bits[p - by] } else { fill })
        .collect()
}
