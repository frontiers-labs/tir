//! A table sorted by a column order and read as a trie: one level per column,
//! each node the distinct cells under the prefix above it, the rows at the
//! leaves. What a join intersects and descends through.

use smallvec::SmallVec;

use super::set::Set;
use crate::store::{ABSENT, Delta, Refresh, TAIL, Table, drop_row};

/// The sorted projection of a table onto a column order.
///
/// The base is built over the rows the table had when it was last rebuilt.
/// The rows placed since are a delta built the same way, small and rebuilt on
/// every refresh, read as a trie of its own; a base leaf drops the rows the
/// delta took over.
#[derive(Debug, Default)]
pub(crate) struct Trie {
    /// The column read at each level.
    columns: SmallVec<[u8; 6]>,
    base: Part,
    delta: Part,
    /// Where each row sits in the base's rows, [`TAIL`] in the delta, or
    /// [`ABSENT`].
    pos: Vec<u32>,
    changes: Delta,
}

/// A trie over some rows: its levels, the rows sorted by the columns and then
/// ascending, leaf `n`'s rows at `rows[from..to]` for `leaves[n] == (from,
/// to)`, and the words of every node laid out as a bitset.
#[derive(Debug, Default)]
struct Part {
    levels: Vec<Level>,
    rows: Vec<u32>,
    leaves: Vec<(u32, u32)>,
    bits: Vec<u64>,
}

/// One level: node `n` holds `cells[starts[n]..starts[n + 1]]`, and the node
/// below its `i`-th cell is node `starts[n] + i` of the next level (or that
/// leaf, under the last level).
#[derive(Debug, Default)]
struct Level {
    starts: Vec<u32>,
    cells: Vec<u32>,
    /// Per node, its range of the bitset words; empty for a node kept only as
    /// a list.
    words: Vec<(u32, u32)>,
}

/// A node of a trie: which part it is of, its level and its index there.
/// Level `columns.len()` is the leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Node {
    delta: bool,
    level: u32,
    index: u32,
}

impl Node {
    pub(crate) fn depth(self) -> usize {
        self.level as usize
    }
}

impl Trie {
    pub(crate) fn new(columns: &[u8]) -> Self {
        Self {
            columns: SmallVec::from_slice(columns),
            ..Self::default()
        }
    }

    pub(crate) fn fixed(cells: &[&[u32]], rows: usize) -> Self {
        let mut trie = Self::default();
        trie.base.rows.extend(0..rows as u32);
        trie.base.rows.sort_unstable_by(|&a, &b| {
            let key = |row: u32| cells.iter().map(move |column| column[row as usize]);
            key(a).cmp(key(b)).then(a.cmp(&b))
        });
        trie.base.build(cells);
        trie
    }

    pub(crate) fn refresh(&mut self, table: &Table) {
        let columns: SmallVec<[&[u32]; 6]> = self
            .columns
            .iter()
            .map(|&column| table.column(column as usize))
            .collect();
        let order = |&a: &u32, &b: &u32| {
            columns
                .iter()
                .map(|column| column[a as usize].cmp(&column[b as usize]))
                .find(|order| order.is_ne())
                .unwrap_or(a.cmp(&b))
        };
        match self.changes.refresh(table) {
            Refresh::Current => return,
            Refresh::Merged => {
                if self.pos.len() < table.len() {
                    self.pos.resize(table.len(), ABSENT);
                }
                let Part { rows, leaves, .. } = &mut self.base;
                for row in self.changes.leaving() {
                    match self.pos[row as usize] {
                        ABSENT => {}
                        TAIL => self.pos[row as usize] = ABSENT,
                        _ => drop_row(rows, &mut self.pos, leaves, row),
                    }
                }
                let pos = &mut self.pos;
                self.delta.rows.retain(|&row| pos[row as usize] == TAIL);
                for &row in &self.changes.changed {
                    pos[row as usize] = TAIL;
                }
                self.delta.rows.extend_from_slice(&self.changes.changed);
                self.delta.rows.sort_unstable_by(order);
                self.delta.build(&columns);
                return;
            }
            Refresh::Rebuild => {}
        }
        self.base.rows.clear();
        self.base.rows.extend(0..table.len() as u32);
        self.base.rows.sort_unstable_by(order);
        self.pos.clear();
        self.pos.resize(table.len(), ABSENT);
        for (at, &row) in self.base.rows.iter().enumerate() {
            self.pos[row as usize] = at as u32;
        }
        self.base.build(&columns);
        self.delta.rows.clear();
        self.delta.build(&columns);
        self.changes.rebuilt(table.len());
    }

    pub(crate) fn columns(&self) -> &[u8] {
        &self.columns
    }

    pub(crate) fn root(&self) -> Node {
        Node {
            delta: false,
            level: 0,
            index: 0,
        }
    }

    pub(crate) fn delta_root(&self) -> Option<Node> {
        (!self.delta.rows.is_empty()).then_some(Node {
            delta: true,
            level: 0,
            index: 0,
        })
    }

    fn part(&self, node: Node) -> &Part {
        match node.delta {
            true => &self.delta,
            false => &self.base,
        }
    }

    pub(crate) fn set(&self, node: Node) -> Set<'_> {
        let part = self.part(node);
        let level = &part.levels[node.level as usize];
        let at = node.index as usize;
        let cells = &level.cells[level.starts[at] as usize..level.starts[at + 1] as usize];
        match level.words[at] {
            (from, to) if from < to => Set::Bits {
                base: cells[0] / 64 * 64,
                words: &part.bits[from as usize..to as usize],
                len: cells.len() as u32,
            },
            _ => Set::Uint(cells),
        }
    }

    #[inline]
    pub(crate) fn child(&self, node: Node, cell: u32) -> Option<Node> {
        let level = &self.part(node).levels[node.level as usize];
        let at = node.index as usize;
        let from = level.starts[at] as usize;
        let cells = &level.cells[from..level.starts[at + 1] as usize];
        let found = cells.binary_search(&cell).ok()?;
        Some(Node {
            delta: node.delta,
            level: node.level + 1,
            index: (from + found) as u32,
        })
    }

    pub(crate) fn rows(&self, leaf: Node) -> &[u32] {
        let part = self.part(leaf);
        debug_assert_eq!(leaf.level as usize, part.levels.len(), "a leaf");
        let (from, to) = part.leaves[leaf.index as usize];
        &part.rows[from as usize..to as usize]
    }
}

impl Part {
    fn build(&mut self, columns: &[&[u32]]) {
        self.levels.resize_with(columns.len(), Level::default);
        for level in &mut self.levels {
            level.starts.clear();
            level.cells.clear();
            level.words.clear();
        }
        self.levels[0].starts.push(0);
        self.leaves.clear();
        for (at, &row) in self.rows.iter().enumerate() {
            // The first column this row differs from the one before in opens
            // a cell there and a node at every level below.
            let differs = match at {
                0 => 0,
                _ => {
                    let before = self.rows[at - 1] as usize;
                    let row = row as usize;
                    columns
                        .iter()
                        .position(|column| column[before] != column[row])
                        .unwrap_or(columns.len())
                }
            };
            for (depth, level) in self.levels.iter_mut().enumerate().skip(differs) {
                if depth > differs {
                    level.starts.push(level.cells.len() as u32);
                }
                level.cells.push(columns[depth][row as usize]);
            }
            if differs < columns.len() {
                if let Some(leaf) = self.leaves.last_mut() {
                    leaf.1 = at as u32;
                }
                self.leaves.push((at as u32, 0));
            }
        }
        for level in &mut self.levels {
            level.starts.push(level.cells.len() as u32);
        }
        if let Some(leaf) = self.leaves.last_mut() {
            leaf.1 = self.rows.len() as u32;
        }
        self.bits.clear();
        for level in &mut self.levels {
            for node in level.starts.windows(2) {
                let cells = &level.cells[node[0] as usize..node[1] as usize];
                let from = self.bits.len() as u32;
                if let (Some(&min), Some(&max)) = (cells.first(), cells.last()) {
                    let base = min / 64 * 64;
                    let words = (max - base) as usize / 64 + 1;
                    // Bits when the words take no more room than the list.
                    if words * 2 <= cells.len() {
                        self.bits.resize(self.bits.len() + words, 0);
                        let words = &mut self.bits[from as usize..];
                        for &cell in cells {
                            words[(cell - base) as usize / 64] |= 1 << ((cell - base) % 64);
                        }
                    }
                }
                level.words.push((from, self.bits.len() as u32));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn leaves(trie: &Trie, node: Node, path: &mut Vec<u32>, out: &mut Vec<(Vec<u32>, u32)>) {
        if node.depth() == trie.columns().len() {
            let rows = trie.rows(node);
            assert!(rows.windows(2).all(|pair| pair[0] < pair[1]), "ascending");
            out.extend(rows.iter().map(|&row| (path.clone(), row)));
            return;
        }
        let mut cells = Vec::new();
        trie.set(node).list(&mut cells);
        assert!(
            cells.windows(2).all(|pair| pair[0] < pair[1]),
            "ascending, distinct"
        );
        assert_eq!(trie.set(node).len(), cells.len());
        for cell in cells {
            path.push(cell);
            leaves(
                trie,
                trie.child(node, cell).expect("a listed cell has a child"),
                path,
                out,
            );
            path.pop();
        }
    }

    proptest! {
        /// However the table grew and was repaired between refreshes, the
        /// base and the delta together hold every row once under its
        /// projection, each node's cells are the distinct ones below it, and
        /// a cell no row has has no child.
        #[test]
        fn a_trie_is_the_sorted_projection(
            rows in prop::collection::vec((0u32..3, 0u32..200, 0u32..6, 0u32..40), 0..300),
            cuts in prop::collection::vec((0usize..300, prop::option::of(1u32..200)), 0..8),
            keyed in any::<bool>(),
            columns in prop::sample::select(vec![vec![0u8, 3, 1, 2], vec![0, 3, 2, 1], vec![2, 0], vec![1]]),
        ) {
            let mut table = if keyed { Table::new(3) } else { Table::bag(3) };
            let mut trie = Trie::new(&columns);
            let mut cuts: Vec<_> = cuts.into_iter().map(|(cut, repair)| (cut.min(rows.len()), repair)).collect();
            cuts.sort();
            cuts.push((rows.len(), None));
            let mut inserted = 0;
            for (cut, moved) in cuts {
                for &(a, b, c, value) in &rows[inserted..cut] {
                    if !keyed || table.get(&[a, b, c], &[]).is_none() {
                        table.insert(&[a, b, c], &[], (value, 0), 0, 1);
                    }
                }
                inserted = cut;
                // One cell comes to stand for the one below it: a low one
                // moves many rows, a high one few.
                if let Some(moved) = moved {
                    let map: Vec<u32> = (0..200).map(|cell| if cell == moved { moved - 1 } else { cell }).collect();
                    let rename = crate::store::Dense {
                        map: &map,
                        delta: None,
                        mask: &[],
                    };
                    table.repair(&rename, None, 2, false, &mut Default::default());
                }
                trie.refresh(&table);
                let mut want: Vec<(Vec<u32>, u32)> = (0..table.len() as u32)
                    .map(|row| (columns.iter().map(|&c| table.column(c as usize)[row as usize]).collect(), row))
                    .collect();
                want.sort();
                let mut got = Vec::new();
                for root in [Some(trie.root()), trie.delta_root()].into_iter().flatten() {
                    leaves(&trie, root, &mut Vec::new(), &mut got);
                    prop_assert_eq!(trie.child(root, 1000), None);
                }
                got.sort();
                prop_assert_eq!(got, want);
            }
        }
    }
}
