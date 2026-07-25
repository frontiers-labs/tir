//! Float comparison and integer/float conversion circuits (`SIToFP`/`UIToFP`)
//! as combinational bit-vector logic, so the QF_BV oracle can prove
//! floating-point identities. All vectors little-endian (bit 0 = LSB).
//! Conversions round to nearest, ties-to-even; the input range (≤64 bits) never
//! overflows the exponent, so no infinity/NaN path.

use tir_graph::{Dag, NodeId};

use super::{BitblastError, Blaster};
use crate::lang::SymKind;
use crate::sat::Lit;

impl<V> Blaster<'_, V> {
    pub(super) fn encode_float_compare(
        &mut self,
        id: NodeId,
        kind: SymKind,
    ) -> Result<Vec<Lit>, BitblastError> {
        let lhs = self.child_bits(id, 0);
        let rhs = self.child_bits(id, 1);
        let (exp_width, mant_width) =
            float_format(lhs.len()).ok_or(BitblastError::UnknownWidth(id.index()))?;
        if rhs.len() != lhs.len() {
            return Err(BitblastError::UnknownWidth(id.index()));
        }

        let lhs_fraction = &lhs[..mant_width];
        let lhs_exponent = &lhs[mant_width..mant_width + exp_width];
        let lhs_sign = lhs[mant_width + exp_width];
        let rhs_fraction = &rhs[..mant_width];
        let rhs_exponent = &rhs[mant_width..mant_width + exp_width];
        let rhs_sign = rhs[mant_width + exp_width];

        let lhs_nan = self.float_is_nan(lhs_fraction, lhs_exponent);
        let rhs_nan = self.float_is_nan(rhs_fraction, rhs_exponent);
        let unordered = self.gate_or(lhs_nan, rhs_nan);
        let ordered = unordered.negate();

        let lhs_zero = self.float_is_zero(lhs_fraction, lhs_exponent);
        let rhs_zero = self.float_is_zero(rhs_fraction, rhs_exponent);
        let both_zero = self.gate_and(lhs_zero, rhs_zero);
        let bit_equal = self.eq_bits(&lhs, &rhs);
        let equal_value = self.gate_or(bit_equal, both_zero);
        let ordered_equal = self.gate_and(ordered, equal_value);

        let signs_differ = self.gate_xor(lhs_sign, rhs_sign);
        let negative_before_positive = self.gate_and(lhs_sign, signs_differ);
        let lhs_magnitude = &lhs[..lhs.len() - 1];
        let rhs_magnitude = &rhs[..rhs.len() - 1];
        let positive_less = self.uge(lhs_magnitude, rhs_magnitude).negate();
        let negative_less = self.uge(rhs_magnitude, lhs_magnitude).negate();
        let same_sign_less = self.gate_mux(lhs_sign, negative_less, positive_less);
        let numeric_less = self.gate_mux(signs_differ, negative_before_positive, same_sign_less);
        let nonzero_less = self.gate_and(both_zero.negate(), numeric_less);
        let ordered_less = self.gate_and(ordered, nonzero_less);

        let neither_less_nor_equal = self.gate_and(ordered_less.negate(), ordered_equal.negate());
        let ordered_greater = self.gate_and(ordered, neither_less_nor_equal);
        let result = match kind {
            SymKind::Eq => ordered_equal,
            SymKind::Ne => ordered_equal.negate(),
            SymKind::Lt => ordered_less,
            SymKind::Le => self.gate_and(ordered, ordered_greater.negate()),
            SymKind::Gt => ordered_greater,
            SymKind::Ge => self.gate_and(ordered, ordered_less.negate()),
            _ => unreachable!(),
        };
        Ok(vec![result])
    }

    fn float_is_nan(&mut self, fraction: &[Lit], exponent: &[Lit]) -> Lit {
        let exponent_all_ones = exponent
            .iter()
            .copied()
            .fold(self.one, |all, bit| self.gate_and(all, bit));
        let fraction_nonzero = self.or_reduce(fraction);
        self.gate_and(exponent_all_ones, fraction_nonzero)
    }

    fn float_is_zero(&mut self, fraction: &[Lit], exponent: &[Lit]) -> Lit {
        let fraction_zero = self.nor(fraction);
        let exponent_zero = self.nor(exponent);
        self.gate_and(fraction_zero, exponent_zero)
    }

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

    /// `FPToSI`/`FPToUI`: children are `[value, destination_width]`.
    fn float_to_int_operand(&mut self, id: NodeId) -> Result<FloatToInt, BitblastError> {
        let bits = self.child_bits(id, 0);
        let children: Vec<NodeId> = self.graph.children(id).collect();
        let width = self.const_u64(children[1])? as usize;
        let (exp_width, mant_width) =
            float_format(bits.len()).ok_or(BitblastError::UnknownWidth(children[0].index()))?;
        Ok(FloatToInt {
            bits,
            width,
            exp_width,
            mant_width,
        })
    }

    /// The conversion's value bits: the significand shifted by the unbiased
    /// exponent, negated for a signed destination when the sign bit is set.
    pub(super) fn encode_float_to_int(
        &mut self,
        id: NodeId,
        signed: bool,
    ) -> Result<Vec<Lit>, BitblastError> {
        let operand = self.float_to_int_operand(id)?;
        let exponent_is_zero = self.nor(operand.exponent());
        let implicit = exponent_is_zero.negate();

        let mut significand = operand.fraction().to_vec();
        significand.push(implicit);
        let work_width = operand.width.max(significand.len());
        significand.resize(work_width, self.zero());

        let one = self.const_bits(1, operand.exp_width);
        let mut effective_exponent = self.mux_bits(exponent_is_zero, &one, operand.exponent());
        effective_exponent.push(self.zero());

        let base = self.const_bits(
            (operand.bias() + operand.mant_width) as u64,
            operand.exp_width + 1,
        );
        let shift_left = self.uge(&effective_exponent, &base);
        let left_amount = self.subtract(&effective_exponent, &base);
        let right_amount = self.subtract(&base, &effective_exponent);
        let left = self.shift_by_bits(&significand, &left_amount, true);
        let right = self.shift_by_bits(&significand, &right_amount, false);
        let mut magnitude = self.mux_bits(shift_left, &left, &right);
        magnitude.truncate(operand.width);

        if !signed {
            return Ok(magnitude);
        }
        let negative = self.negate(&magnitude);
        Ok(self.mux_bits(operand.sign(), &negative, &magnitude))
    }

    /// The conversion is defined on finite values whose truncation fits the
    /// destination: the exponent stays below the destination's magnitude limit,
    /// with the signed minimum admitted exactly at the boundary exponent.
    pub(super) fn float_to_int_defined(
        &mut self,
        id: NodeId,
        signed: bool,
    ) -> Result<Lit, BitblastError> {
        let operand = self.float_to_int_operand(id)?;
        let all_ones = self.const_bits((1u64 << operand.exp_width) - 1, operand.exp_width);
        let finite = self.eq_bits(operand.exponent(), &all_ones).negate();

        let less_than_exponent = |this: &mut Self, limit: usize| {
            let bound = this.const_bits(limit as u64, operand.exp_width);
            this.uge(operand.exponent(), &bound).negate()
        };

        let (negative_ok, positive_ok) = if signed {
            let threshold = operand.bias() + operand.width - 1;
            let below = less_than_exponent(self, threshold);
            let at_threshold = {
                let threshold = self.const_bits(threshold as u64, operand.exp_width);
                self.eq_bits(operand.exponent(), &threshold)
            };
            let boundary_fraction_ok = if operand.mant_width >= operand.width - 1 {
                let limit = 1u64 << (operand.mant_width - (operand.width - 1));
                let limit = self.const_bits(limit, operand.mant_width);
                self.uge(operand.fraction(), &limit).negate()
            } else {
                self.nor(operand.fraction())
            };
            let boundary_ok = self.gate_and(at_threshold, boundary_fraction_ok);
            (self.gate_or(below, boundary_ok), below)
        } else {
            (
                less_than_exponent(self, operand.bias()),
                less_than_exponent(self, operand.bias() + operand.width),
            )
        };

        let negative = self.gate_and(operand.sign(), negative_ok);
        let positive = self.gate_and(operand.sign().negate(), positive_ok);
        let in_range = self.gate_or(negative, positive);
        Ok(self.gate_and(finite, in_range))
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

    fn shift_by_bits(&mut self, bits: &[Lit], amount: &[Lit], left: bool) -> Vec<Lit> {
        let width = bits.len();
        let mut stages = 0usize;
        while (1usize << stages) < width {
            stages += 1;
        }

        let mut current = bits.to_vec();
        for (stage, &enabled) in amount.iter().enumerate().take(stages) {
            let by = 1usize << stage;
            let shifted = (0..width)
                .map(|index| {
                    let source = if left {
                        index.checked_sub(by)
                    } else {
                        index.checked_add(by).filter(|&source| source < width)
                    };
                    source.map_or_else(|| self.zero(), |source| current[source])
                })
                .collect::<Vec<_>>();
            current = self.mux_bits(enabled, &shifted, &current);
        }

        let overflow = self.or_reduce(&amount[stages.min(amount.len())..]);
        let zeros = vec![self.zero(); width];
        self.mux_bits(overflow, &zeros, &current)
    }
}

/// A float-to-int conversion's operand: the value's IEEE fields and the
/// destination integer width.
struct FloatToInt {
    bits: Vec<Lit>,
    width: usize,
    exp_width: usize,
    mant_width: usize,
}

impl FloatToInt {
    fn fraction(&self) -> &[Lit] {
        &self.bits[..self.mant_width]
    }

    fn exponent(&self) -> &[Lit] {
        &self.bits[self.mant_width..self.mant_width + self.exp_width]
    }

    fn sign(&self) -> Lit {
        self.bits[self.mant_width + self.exp_width]
    }

    fn bias(&self) -> usize {
        (1usize << (self.exp_width - 1)) - 1
    }
}

fn float_format(width: usize) -> Option<(usize, usize)> {
    match width {
        32 => Some((8, 23)),
        64 => Some((11, 52)),
        _ => None,
    }
}

/// Shift `bits` left by the constant `by`, filling vacated low positions with `fill`.
fn shift_left(bits: &[Lit], by: usize, fill: Lit) -> Vec<Lit> {
    (0..bits.len())
        .map(|p| if p >= by { bits[p - by] } else { fill })
        .collect()
}
