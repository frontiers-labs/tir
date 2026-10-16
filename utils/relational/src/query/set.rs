//! Sets of cells as a join intersects them: an ascending list, or a bitset
//! when the cells are dense enough that the words take no more room than the
//! list would.

/// A set of distinct cells.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Set<'a> {
    /// Ascending, distinct.
    Uint(&'a [u32]),
    /// Bit `i` of the words is cell `base + i`, and `base` is a multiple of
    /// 64, so two bitsets line up word for word. `len` bits are set.
    Bits {
        base: u32,
        words: &'a [u64],
        len: u32,
    },
}

/// A list larger than the other by more than this factor is searched rather
/// than merged against.
const GALLOP: usize = 32;

impl Set<'_> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Set::Uint(cells) => cells.len(),
            Set::Bits { len, .. } => *len as usize,
        }
    }

    pub(crate) fn contains(&self, cell: u32) -> bool {
        match *self {
            Set::Uint(cells) => cells.binary_search(&cell).is_ok(),
            Set::Bits { base, words, .. } => cell.checked_sub(base).is_some_and(|bit| {
                words
                    .get(bit as usize / 64)
                    .is_some_and(|word| word >> (bit % 64) & 1 == 1)
            }),
        }
    }

    pub(crate) fn list(&self, out: &mut Vec<u32>) {
        match *self {
            Set::Uint(cells) => out.extend_from_slice(cells),
            Set::Bits { base, words, .. } => list_bits(base, words, out),
        }
    }

    pub(crate) fn intersect(
        sets: &mut [Set<'_>],
        out: &mut Vec<u32>,
        words: &mut Vec<u64>,
        spare: &mut Vec<u32>,
    ) {
        out.clear();
        sets.sort_unstable_by_key(Set::len);
        let [first, rest @ ..] = sets else {
            return;
        };
        let Some((second, rest)) = rest.split_first() else {
            first.list(out);
            return;
        };
        match (*first, *second) {
            (
                Set::Bits {
                    base, words: small, ..
                },
                Set::Bits {
                    base: other,
                    words: large,
                    ..
                },
            ) => {
                // Only the words both ranges cover can hold a common cell.
                let from = base.max(other);
                let to = (base as u64 + small.len() as u64 * 64)
                    .min(other as u64 + large.len() as u64 * 64);
                if (from as u64) < to {
                    let count = ((to - from as u64) / 64) as usize;
                    let at = |start: u32| ((from - start) / 64) as usize;
                    words.clear();
                    words.extend_from_slice(&small[at(base)..at(base) + count]);
                    tir_adt::simd::and_words(words, &large[at(other)..at(other) + count]);
                    list_bits(from, words, out);
                }
            }
            (a, b) => pair(a, b, out),
        }
        for set in rest {
            if out.is_empty() {
                return;
            }
            std::mem::swap(out, spare);
            out.clear();
            pair(Set::Uint(spare), *set, out);
        }
    }
}

fn pair(a: Set<'_>, b: Set<'_>, out: &mut Vec<u32>) {
    match (a, b) {
        (Set::Uint(a), Set::Uint(b)) if b.len() > a.len().saturating_mul(GALLOP) => {
            gallop(a, b, out)
        }
        (Set::Uint(a), Set::Uint(b)) => tir_adt::simd::intersect_sorted(a, b, out),
        (Set::Uint(cells), bits) | (bits, Set::Uint(cells)) => {
            out.extend(cells.iter().copied().filter(|&cell| bits.contains(cell)))
        }
        (Set::Bits { .. }, Set::Bits { .. }) => unreachable!("one side is a list"),
    }
}

fn gallop(small: &[u32], large: &[u32], out: &mut Vec<u32>) {
    let mut from = 0;
    for &cell in small {
        let mut step = 1;
        while from + step < large.len() && large[from + step] < cell {
            step *= 2;
        }
        let to = (from + step + 1).min(large.len());
        from += large[from..to].partition_point(|&other| other < cell);
        if from == large.len() {
            return;
        }
        if large[from] == cell {
            out.push(cell);
        }
    }
}

fn list_bits(base: u32, words: &[u64], out: &mut Vec<u32>) {
    for (at, &word) in words.iter().enumerate() {
        let mut word = word;
        while word != 0 {
            out.push(base + at as u32 * 64 + word.trailing_zeros());
            word &= word - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    fn bits(cells: &BTreeSet<u32>) -> (u32, Vec<u64>) {
        let base = cells.first().map_or(0, |&min| min / 64 * 64);
        let mut words = vec![
            0u64;
            cells
                .last()
                .map_or(0, |&max| (max - base) as usize / 64 + 1)
        ];
        for &cell in cells {
            words[(cell - base) as usize / 64] |= 1 << ((cell - base) % 64);
        }
        (base, words)
    }

    proptest! {
        /// Whatever the layouts and sizes, the intersection is the model's,
        /// ascending. A short list against a long one takes the gallop.
        #[test]
        fn intersect_agrees_with_the_model(
            inputs in prop::collection::vec(
                (prop::collection::btree_set(0u32..1024, 0..400), any::<bool>()),
                1..5,
            ),
            short in prop::option::of(prop::collection::btree_set(0u32..1024, 0..4)),
        ) {
            let mut inputs = inputs;
            // A list of up to three cells against one of hundreds gallops.
            inputs.extend(short.map(|short| (short, false)));
            let encoded: Vec<(u32, Vec<u64>)> = inputs.iter().map(|(cells, _)| bits(cells)).collect();
            let lists: Vec<Vec<u32>> = inputs.iter().map(|(cells, _)| cells.iter().copied().collect()).collect();
            let mut sets: Vec<Set<'_>> = inputs
                .iter()
                .zip(&encoded)
                .zip(&lists)
                .map(|(((cells, dense), (base, words)), list)| match dense {
                    true => Set::Bits { base: *base, words, len: cells.len() as u32 },
                    false => Set::Uint(list),
                })
                .collect();
            let mut want = inputs[0].0.clone();
            for (cells, _) in &inputs[1..] {
                want = want.intersection(cells).copied().collect();
            }
            let (mut out, mut words, mut spare) = (Vec::new(), Vec::new(), Vec::new());
            Set::intersect(&mut sets, &mut out, &mut words, &mut spare);
            prop_assert_eq!(out, want.into_iter().collect::<Vec<_>>());
        }

    }
}
