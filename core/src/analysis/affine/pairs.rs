//! What each pair of accesses on one chain says about the nest's order.

use super::build::Builder;
use super::*;

impl Builder<'_> {
    /// Every pair of accesses of one memory with at least one write among them.
    /// Accesses on different chains are of different memories, so they are not a
    /// pair at all.
    pub(super) fn pairs(&self) -> Vec<Pair> {
        let mut pairs = Vec::new();
        for left in 0..self.accesses.len() {
            for right in left + 1..self.accesses.len() {
                let (a, b) = (&self.accesses[left], &self.accesses[right]);
                if a.chain != b.chain || !(a.write || b.write) {
                    continue;
                }
                pairs.push(Pair {
                    left,
                    right,
                    dependence: self.dependence(a, b),
                });
            }
        }
        pairs
    }

    fn dependence(&self, a: &Access, b: &Access) -> Dependence {
        let (Offset::Affine(left), Offset::Affine(right)) = (&a.offset, &b.offset) else {
            return Dependence::Unknown;
        };
        if self.opaque || a.wrapping || b.wrapping {
            return Dependence::Unknown;
        }
        if a.base != b.base {
            return match (self.extremes(a, left), self.extremes(b, right)) {
                (Some(left), Some(right)) => Dependence::Conditional(Box::new((left, right))),
                _ => Dependence::Unknown,
            };
        }
        if !left.same_slopes(right) {
            return Dependence::Unknown;
        }
        let depth = self.loops.len();
        let coefficients: Vec<i128> = (0..depth).map(|d| left.counter_coefficient(d)).collect();
        let gap = left.constant_term() - right.constant_term();
        let (left_extent, right_extent) = (i128::from(a.extent), i128::from(b.extent));
        let targets = [
            (gap - right_extent + 1, gap + left_extent - 1),
            (-gap - left_extent + 1, -gap + right_extent - 1),
        ];
        let extents: Vec<Option<i128>> = self
            .loops
            .iter()
            .map(|l| l.trip.map(|trip| trip - 1))
            .collect();
        match dependence::distances(&coefficients, &extents, &targets) {
            Some(components) => Dependence::Distances(components),
            None => Dependence::Independent,
        }
    }

    /// The bytes an access can touch, as forms over the nest's symbols: every
    /// iteration index is replaced by the end of its range that the coefficient's
    /// sign asks for — the first iteration is zero, the last a form of its own.
    fn extremes(&self, access: &Access, offset: &AffineForm) -> Option<Extremes> {
        let (mut low, mut high) = (offset.clone(), offset.clone());
        for depth in 0..self.loops.len() {
            let last = self.last_iteration(depth)?;
            for (form, take_low) in [(&mut low, true), (&mut high, false)] {
                let coefficient = form.counter_coefficient(depth);
                form.set_counter_coefficient(depth, 0);
                if coefficient == 0 || (coefficient > 0) == take_low {
                    continue;
                }
                *form = form.add(&last.scale(coefficient));
            }
        }
        (low.is_uniform() && high.is_uniform()).then(|| Extremes {
            base: access.base,
            low,
            high: high.add(&AffineForm::constant(i128::from(access.extent) - 1)),
        })
    }
}
