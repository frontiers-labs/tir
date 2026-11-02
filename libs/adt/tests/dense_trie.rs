use std::ops::Range;

use proptest::prelude::*;
use tir_adt::DenseTrie;

fn range(trie: &DenseTrie, first: u32) -> Range<u32> {
    let mut out = vec![Range::default()];
    trie.ranges(&[std::slice::from_ref(&first)], &mut out);
    out.swap_remove(0)
}

fn rows_in(trie: &DenseTrie, width: usize, range: Range<u32>) -> Vec<Vec<u32>> {
    let cells = |i: usize| &trie.column(i)[range.start as usize..range.end as usize];
    (0..range.len())
        .map(|at| (0..width).map(|i| cells(i)[at]).collect())
        .collect()
}

#[test]
fn largest_key_is_preserved() {
    let trie = DenseTrie::build(&[&[u32::MAX]], |_| true);
    assert_eq!(trie.column(0), &[u32::MAX]);
    assert_eq!(range(&trie, u32::MAX), 0..1);
}

proptest! {
    /// A batched lookup gives the live rows with each prefix in sorted
    /// order, the same as filtering and sorting the columns by hand, also for
    /// a trie rebuilt from an earlier table. Few first values make buckets longer than the block
    /// paths; prefixes reach past the largest value. A stride spreads the
    /// first values too thin to count, and the middle values with them.
    #[test]
    fn ranges_match_a_filter_over_the_columns(
        width in 1usize..=4,
        span in 1u32..40,
        rows in prop::collection::vec((any::<[u32; 4]>(), any::<bool>()), 0..400),
        prefixes in prop::collection::vec(any::<[u32; 4]>(), 1..40),
        length in 0usize..=4,
        earlier in prop::collection::vec(any::<[u32; 4]>(), 0..400),
        earlier_width in 1usize..=4,
        ids in any::<bool>(),
        stride in prop_oneof![Just(1u32), Just(1000)],
        offset in 0u32..3,
    ) {
        // With `ids`, the last column holds row ids, in order as an engine's do.
        let cell = |row: usize, c: usize, value: u32| match c {
            0 => value % span * stride + offset,
            _ if ids && c == width - 1 => row as u32,
            _ => value % 6 * stride,
        };
        let rows: Vec<(Vec<u32>, bool)> = rows.iter().enumerate()
            .map(|(at, (row, live))| ((0..width).map(|c| cell(at, c, row[c])).collect(), *live))
            .collect();
        let columns: Vec<Vec<u32>> = (0..width).map(|c| rows.iter().map(|(row, _)| row[c]).collect()).collect();
        let columns: Vec<&[u32]> = columns.iter().map(Vec::as_slice).collect();
        let earlier: Vec<Vec<u32>> = (0..earlier_width).map(|c| earlier.iter().map(|row| row[c] % 64).collect()).collect();
        let earlier: Vec<&[u32]> = earlier.iter().map(Vec::as_slice).collect();
        let mut trie = DenseTrie::build(&earlier, |row| row % 3 != 0);
        trie.rebuild(&columns, |row| rows[row].1);

        let prefixes: Vec<Vec<u32>> = prefixes.iter()
            .map(|prefix| (0..length).map(|c| match c {
                0 => prefix[c] % (span + 2) * stride + prefix[c] / 7 % 4,
                _ => prefix[c] % 7 * stride,
            }).collect())
            .collect();
        let batch: Vec<Vec<u32>> = (0..length).map(|c| prefixes.iter().map(|prefix| prefix[c]).collect()).collect();
        let batch: Vec<&[u32]> = batch.iter().map(Vec::as_slice).collect();
        let mut ranges = vec![0..0; prefixes.len()];
        trie.ranges(&batch, &mut ranges);

        for (prefix, batched) in prefixes.iter().zip(ranges) {
            let mut want: Vec<Vec<u32>> = rows.iter()
                .filter(|(row, live)| *live && row.starts_with(prefix))
                .map(|(row, _)| row.clone())
                .collect();
            want.sort_unstable();
            prop_assert_eq!(rows_in(&trie, width, batched), want, "batched prefix {:?}", prefix);
        }
    }

    /// A trie built from the first rows of a table and then merged with the
    /// rest, after some rows died, holds what a trie built from the whole
    /// table holds.
    #[test]
    fn merging_appended_rows_matches_a_build(
        width in 2usize..=4,
        stride in prop_oneof![Just(1u32), Just(1000)],
        rows in prop::collection::vec((any::<[u32; 3]>(), any::<bool>(), any::<bool>()), 0..300),
        appended in 0usize..300,
    ) {
        let appended = appended.min(rows.len());
        // Row ids last, as merging needs; `rows[i].1` is whether row `i`
        // lived before the merge, and `rows[i].2` whether it lives after.
        let columns: Vec<Vec<u32>> = (0..width)
            .map(|c| match c {
                _ if c == width - 1 => (0..rows.len() as u32).collect(),
                0 => rows.iter().map(|(row, ..)| row[0] % 20 * stride).collect(),
                _ => rows.iter().map(|(row, ..)| row[c] % 5 * stride).collect(),
            })
            .collect();
        let columns: Vec<&[u32]> = columns.iter().map(Vec::as_slice).collect();
        let earlier: Vec<&[u32]> = columns.iter().map(|column| &column[..appended]).collect();
        let mut trie = DenseTrie::build(&earlier, |row| rows[row].1);
        let live = |row: usize| rows[row].2 && (row >= appended || rows[row].1);
        trie.merge(&columns, appended, live);
        let want = DenseTrie::build(&columns, live);
        let len = want.column(0).len() as u32;
        prop_assert_eq!(rows_in(&trie, width, 0..trie.column(0).len() as u32), rows_in(&want, width, 0..len));
        for value in 0..20 * stride + 2 {
            prop_assert_eq!(rows_in(&trie, width, range(&trie, value)), rows_in(&want, width, range(&want, value)));
        }
    }
}
