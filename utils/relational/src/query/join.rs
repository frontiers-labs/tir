//! The executor of a [`Step::Join`]: a worst-case optimal join over tries,
//! which binds the run's variables one at a time by intersecting the cells
//! every atom reading the variable allows, and then hands the run's matches
//! on in the order the loop nest would have found them.

use smallvec::SmallVec;

use super::eval::{Allow, Eval, keyed, reads};
use super::ghd::Bag;
use super::ir::Resolution;
use super::plan::{Attr, JoinAtom, JoinPlan, Level, Plan, Step};
use super::set::Set;
use super::trie::{Node, Trie};
use crate::db::{Core, Database, Groups};
use crate::store::Group;

/// A join runs as the loop nest unless its first atom reaches at least this
/// many rows, and its tries are built only where the tables that atom reads
/// hold at least this many rows per cell: below that, probing each row costs
/// less than building and intersecting sets.
pub(crate) const JOIN_MIN_ROWS: usize = 32;

/// An atom read through several nodes intersects by its widest alone, and
/// admits the others' cells after, when those are at most this fraction of
/// it; else it intersects by their union.
const PATCH_DIVISOR: usize = 8;

/// A join's working arrays, kept so a search allocates nothing.
#[derive(Default)]
pub(super) struct JoinScratch {
    /// The cursors of every frame, stacked by depth, each frame's atom by
    /// atom.
    cursors: Vec<Cursor>,
    /// Per frame, where each atom's cursors start and where the frame ends.
    spans: Vec<u32>,
    /// Per depth, the cells every atom allows there.
    candidates: Vec<Vec<u32>>,
    /// Lists the cells of an atom's several tries are merged into.
    unions: Vec<Vec<u32>>,
    words: Vec<u64>,
    spare: Vec<u32>,
    /// Per atom, the ranks of the rows its leaves hold.
    leaves: Vec<Vec<u64>>,
    /// The ranks of a run's matches and the order they go in, one pair per
    /// join being bound.
    tuples: Vec<(Vec<u64>, Vec<u32>)>,
    /// Per depth, the cell bound there.
    path: Vec<u32>,
    /// Every atom's cursors and where each atom's start, kept while the bags
    /// of a decomposed run take turns.
    starts: Vec<Cursor>,
    start_spans: Vec<u32>,
    /// Per bag, its tuples and their order by the separator with its parent.
    bags: Vec<(Vec<u64>, Vec<u32>)>,
    /// A parent's distinct separator cells, sorted, a row per tuple.
    keys: Vec<u32>,
}

// Where an atom's relation is read: under one selected cell of one table,
// as written or swapped, or under a client table's bound columns.
#[derive(Clone, Copy)]
struct Cursor {
    /// Index into the join's sources.
    source: u32,
    // The selected cell; zero in a client table.
    cell: u32,
    node: Node,
}

/// One table an atom reads, in one reading.
struct Source<'a> {
    trie: &'a Trie,
    /// The rank of the table's row zero; a row's rank is this with the row
    /// shifted left by `shift`.
    base: u64,
    shift: u32,
    // The table's stamps, where the atom holds to rows the previous epoch
    // did not write.
    old: Option<&'a [u32]>,
}

/// A row's rank in the loop nest's order of an atom's rows: the table's
/// position in the selection first, then the row and the reading, or for a
/// sideways step, which looks each reading up in its own column, the reading
/// and the row.
fn rank(table: usize, reading: usize, sideways: bool) -> (u64, u32) {
    let table = (table as u64) << 33;
    match sideways {
        true => (table | (reading as u64) << 32, 0),
        false => (table | reading as u64, 1),
    }
}

/// The table position, row and reading of a rank.
fn unrank(rank: u64, sideways: bool) -> (usize, u32, bool) {
    let table = (rank >> 33) as usize;
    match sideways {
        true => (table, rank as u32, rank >> 32 & 1 == 1),
        false => (table, (rank >> 1) as u32, rank & 1 == 1),
    }
}

impl Plan {
    /// What `atom`, the first of a join, reaches through its step: `count`
    /// summed over the groupings it looks rows up in, both readings included,
    /// or every row of a client table.
    pub(super) fn reach<C, const S: usize>(
        &self,
        db: &Core<C, S>,
        groups: &Groups,
        resolutions: &[Resolution],
        atom: &JoinAtom,
        count: impl Fn(&Group) -> usize,
    ) -> usize {
        if let Step::Table { atom, .. } = atom.step {
            let tables = &resolutions[self.tuples(atom).table as usize].tables;
            return tables.iter().map(|(slot, _)| db.words(*slot).len()).sum();
        }
        let rows = self.rows(atom.atom());
        let tables = &resolutions[rows.select.param as usize].tables;
        tables
            .iter()
            .map(|(slot, _)| {
                let Some(table) = db.table(*slot) else {
                    return 0;
                };
                let Step::Parents { slot: arg, .. } = atom.step else {
                    return count(groups.get(*slot, table.arity()));
                };
                let arg = rows.args[arg as usize];
                let swapped = match Self::when(rows, resolutions, *slot).is_empty() {
                    true => 0,
                    false => count(groups.get(*slot, arg.swapped as usize)),
                };
                count(groups.get(*slot, arg.column as usize)) + swapped
            })
            .sum()
    }

    /// Build the tries a join reads, unless the tables its first atom looks
    /// rows up in hold too few rows per cell for a join to pay. Reports
    /// whether it built them.
    pub(super) fn prepare_join<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        resolutions: &[Resolution],
        join: &JoinPlan,
        min_rows: usize,
    ) -> bool {
        let first = &join.atoms[0];
        let (rows, cells) = {
            let groups = db.groups();
            let rows = self.reach(db, &groups, resolutions, first, Group::len);
            let cells = match first.step {
                Step::Table { .. } => 1,
                _ => self.reach(db, &groups, resolutions, first, Group::cells),
            };
            (rows, cells)
        };
        if rows < min_rows.saturating_mul(cells.max(1)) {
            return false;
        }
        for atom in &join.atoms {
            if let Step::Table { atom: index, .. } = atom.step {
                for (slot, _) in &resolutions[self.tuples(index).table as usize].tables {
                    db.words(*slot).trie(&atom.columns[0]);
                }
                continue;
            }
            let rows = self.rows(atom.atom());
            for (slot, _) in &resolutions[rows.select.param as usize].tables {
                db.prepare_trie(*slot, &atom.columns[0]);
                if !atom.columns[1].is_empty() && !Self::when(rows, resolutions, *slot).is_empty() {
                    db.prepare_trie(*slot, &atom.columns[1]);
                }
            }
        }
        true
    }

    /// Whether the join, its tries built, is worth running rather than its
    /// run's steps: whether its first atom reaches enough rows.
    #[inline(never)]
    pub(super) fn pays<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &Eval<'_, C, S, A>,
        join: &JoinPlan,
    ) -> bool {
        let first = &join.atoms[0];
        let key = match first.step {
            Step::Table { atom, keys } => {
                let tuples = self.tuples(atom);
                let tables = &eval.resolutions[tuples.table as usize].tables;
                let rows = |(slot, _): &(u32, _)| {
                    keyed(eval.db.words(*slot), tuples, keys, &eval.scalars).len()
                };
                return tables.iter().map(rows).sum::<usize>() >= eval.join_min_rows;
            }
            Step::Parents { atom, slot } => self.rows(atom).args[slot as usize].var,
            _ => self.rows(first.atom()).key,
        };
        let key = eval.bound[key as usize].id;
        let count = |group: &Group| group.rows(key).len();
        self.reach(eval.db, eval.groups, eval.resolutions, first, count) >= eval.join_min_rows
    }

    /// Run the join at step `index`, then the steps after its run, once per
    /// match of the run, in the loop nest's order.
    // Out of line, so the loop nest's step stays as small as it was.
    #[inline(never)]
    pub(super) fn join<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        join: &JoinPlan,
    ) {
        let (mut tuples, mut order) = eval
            .join
            .get_or_insert_default()
            .tuples
            .pop()
            .unwrap_or_default();
        tuples.clear();
        self.enumerate(eval, join, &mut tuples);
        // The loop nest meets the run's matches ordered by each atom's row
        // rank in step order, which is the order of the rank tuples.
        let width = join.atoms.len();
        order.clear();
        order.extend(0..(tuples.len() / width) as u32);
        let rows = |at: u32| &tuples[at as usize * width..(at as usize + 1) * width];
        order.sort_unstable_by(|&a, &b| rows(a).cmp(rows(b)));
        let (db, resolutions) = (eval.db, eval.resolutions);
        for &at in &order {
            let mark = eval.trail.len();
            let mut fresh = 0;
            let mut held = true;
            for (atom, &rank) in join.atoms.iter().zip(rows(at)) {
                let sideways = matches!(atom.step, Step::Parents { .. });
                let (table, row, swapped) = unrank(rank, sideways);
                if let Step::Table { atom, .. } = atom.step {
                    let tuples = self.tuples(atom);
                    let slot = resolutions[tuples.table as usize].tables[table].0;
                    // The tries agree on every term but the scalars to bind.
                    held = held && eval.bind_words(tuples, 0, db.words(slot), row);
                    continue;
                }
                let shape = self.rows(atom.atom());
                let slot = resolutions[shape.select.param as usize].tables[table].0;
                let table = db.table(slot).expect("a joined table");
                if let Some(tag) = shape.tag {
                    eval.scalars[tag as usize] = table.tags()[row as usize] as u64;
                }
                if let Some(cell) = shape.cell {
                    eval.scalars[cell as usize] =
                        table.column(shape.select.column as usize)[row as usize] as u64;
                }
                fresh += usize::from(table.stamps()[row as usize] + 1 == db.epoch());
                held = held
                    && eval.bind_value(shape.key, table, row)
                    && eval.bind_row(&shape.args, table, row, swapped);
            }
            // The tries are over ids, so the rows of a tuple agree on the id
            // of every variable; binding them is what holds their weights to
            // one reference per variable, as it does in the loop nest.
            if held {
                eval.fresh += fresh;
                self.step(eval, root, index + 1 + join.len);
                eval.fresh -= fresh;
            }
            eval.unbind(mark);
        }
        eval.join
            .get_or_insert_default()
            .tuples
            .push((tuples, order));
    }

    /// Append the rank tuple of every match of the join's run to `out`, in no
    /// particular order.
    fn enumerate<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        join: &JoinPlan,
        out: &mut Vec<u64>,
    ) {
        let (db, groups, resolutions) = (eval.db, eval.groups, eval.resolutions);
        let scratch = &mut **eval.join.get_or_insert_default();
        scratch.cursors.clear();
        scratch.spans.clear();
        let mut sources: SmallVec<[Source<'_>; 16]> = SmallVec::new();
        for atom in &join.atoms {
            scratch.spans.push(scratch.cursors.len() as u32);
            if let Step::Table { atom: index, .. } = atom.step {
                let tuples = self.tuples(index);
                let tables = &resolutions[tuples.table as usize].tables;
                for (position, (slot, _)) in tables.iter().enumerate() {
                    let trie = db.words(*slot).trie(&atom.columns[0]);
                    let (base, shift) = rank(position, 0, false);
                    sources.push(Source {
                        trie,
                        base,
                        shift,
                        old: None,
                    });
                    let found = atom.levels.iter().try_fold(trie.root(), |node, level| {
                        let Level::Key(column) = level else {
                            return Err(Some(node));
                        };
                        let cell = tuples.terms[*column as usize].eval(&eval.scalars);
                        let cell = cell.and_then(|cell| u32::try_from(cell as u64).ok());
                        cell.and_then(|cell| trie.child(node, cell)).ok_or(None)
                    });
                    if let Ok(node) | Err(Some(node)) = found {
                        let source = sources.len() as u32 - 1;
                        scratch.cursors.push(Cursor {
                            source,
                            cell: 0,
                            node,
                        });
                    }
                }
                if scratch.cursors.len() as u32 == *scratch.spans.last().unwrap() {
                    return;
                }
                continue;
            }
            let rows = self.rows(atom.atom());
            let sideways = matches!(atom.step, Step::Parents { .. });
            let old = !eval.nested && atom.atom() < eval.old_below;
            let tables = &resolutions[rows.select.param as usize].tables;
            for (position, (slot, cells)) in tables.iter().enumerate() {
                let Some(table) = db.table(*slot) else {
                    continue;
                };
                let when = Self::when(rows, resolutions, *slot);
                let readings = match atom.columns[1].is_empty() || when.is_empty() {
                    true => 1,
                    false => 2,
                };
                for reading in 0..readings {
                    let trie = groups.trie(*slot, &atom.columns[reading]);
                    let (base, shift) = rank(position, reading, sideways);
                    sources.push(Source {
                        trie,
                        base,
                        shift,
                        old: old.then(|| table.stamps()),
                    });
                    let source = sources.len() as u32 - 1;
                    for &cell in cells {
                        if reading == 1 && !reads(when, cell) {
                            continue;
                        }
                        // The rows placed since the trie's base are read as
                        // a trie of their own.
                        for root in [Some(trie.root()), trie.delta_root()].into_iter().flatten() {
                            let found = atom.levels[1..].iter().try_fold(
                                trie.child(root, cell),
                                |node, level| match level {
                                    Level::Known(var) => {
                                        let cell = eval.bound[*var as usize].id;
                                        Ok(node.and_then(|node| trie.child(node, cell)))
                                    }
                                    _ => Err(node),
                                },
                            );
                            if let Some(node) = found.unwrap_or_else(|node| node) {
                                scratch.cursors.push(Cursor { source, cell, node });
                            }
                        }
                    }
                }
            }
            if scratch.cursors.len() as u32 == *scratch.spans.last().unwrap() {
                return;
            }
        }
        scratch.spans.push(scratch.cursors.len() as u32);
        scratch
            .candidates
            .resize_with(join.vars.len().max(scratch.candidates.len()), Vec::new);
        scratch
            .leaves
            .resize_with(join.atoms.len().max(scratch.leaves.len()), Vec::new);
        scratch
            .path
            .resize(join.vars.len().max(scratch.path.len()), 0);
        let mut run = Run {
            join,
            sources,
            epoch: db.epoch(),
            atoms: SmallVec::new(),
            vars: SmallVec::new(),
            held: SmallVec::new(),
            stride: 0,
            cells: false,
        };
        match &join.bags[..] {
            [bag] => {
                run.enter(bag, true);
                run.descend(scratch, eval.allowed, &[], 0, out);
            }
            bags => run.decomposed(scratch, eval.allowed, bags, out),
        }
    }
}

/// What a join's descent reads and does not change.
struct Run<'r, 'a> {
    join: &'r JoinPlan,
    sources: SmallVec<[Source<'a>; 16]>,
    epoch: u32,
    /// The bag being joined: its atoms by position in the run, its variables
    /// by index into the attribute order.
    atoms: SmallVec<[usize; 8]>,
    vars: SmallVec<[u32; 8]>,
    /// Per variable of the bag, the column of its parent's separator cells it
    /// is held to, and how many columns those have.
    held: SmallVec<[Option<u8>; 8]>,
    stride: usize,
    /// Whether each tuple starts with the bag's cells, by which a decomposed
    /// run's bags are matched to each other.
    cells: bool,
}

/// Where the separator's cells sit in a tuple of a bag with variables `vars`.
fn positions(vars: u64, separator: u64) -> SmallVec<[usize; 8]> {
    bits(separator)
        .map(|var| (vars & ((1 << var) - 1)).count_ones() as usize)
        .collect()
}

fn bits(mask: u64) -> impl Iterator<Item = u32> {
    (0..64).filter(move |bit| mask >> bit & 1 == 1)
}

/// The cells of tuple `at` at `positions`: its separator key.
fn key<'t>(
    tuples: &'t [u64],
    width: usize,
    positions: &'t [usize],
    at: u32,
) -> impl Iterator<Item = u64> + 't {
    let tuple = &tuples[at as usize * width..];
    positions.iter().map(move |&position| tuple[position])
}

impl Run<'_, '_> {
    /// Join `bag` next, its separator held to its parent's cells unless it
    /// is the root.
    fn enter(&mut self, bag: &Bag, root: bool) {
        self.atoms = bits(bag.atoms).map(|atom| atom as usize).collect();
        self.vars = bits(bag.vars).collect();
        let separator = if root { 0 } else { bag.separator };
        self.held = SmallVec::from_elem(None, self.vars.len());
        for (column, at) in positions(bag.vars, separator).into_iter().enumerate() {
            self.held[at] = Some(column as u8);
        }
        self.stride = separator.count_ones() as usize;
        self.cells = self.join.bags.len() > 1;
    }

    /// Join a decomposed run's bags root first, each child holding its
    /// separator to the cells its parent's tuples have; drop, leaves first,
    /// each parent tuple no child tuple agrees with; then combine one tuple
    /// of every bag, each child's looked up by its parent's separator cells.
    fn decomposed<A: Allow + ?Sized>(
        &mut self,
        scratch: &mut JoinScratch,
        allowed: Option<&A>,
        bags: &[Bag],
        out: &mut Vec<u64>,
    ) {
        std::mem::swap(&mut scratch.cursors, &mut scratch.starts);
        std::mem::swap(&mut scratch.spans, &mut scratch.start_spans);
        scratch
            .bags
            .resize_with(bags.len().max(scratch.bags.len()), Default::default);
        let width = |bag: &Bag| (bag.vars.count_ones() + bag.atoms.count_ones()) as usize;
        let links: SmallVec<[Link; 4]> = bags
            .iter()
            .map(|bag| Link {
                span: width(bag),
                at: positions(bag.vars, bag.separator),
                up_span: width(&bags[bag.parent]),
                up_at: positions(bags[bag.parent].vars, bag.separator),
            })
            .collect();
        let mut keys = std::mem::take(&mut scratch.keys);
        let mut found = true;
        for (index, bag) in bags.iter().enumerate() {
            keys.clear();
            if index > 0 {
                // The parent's distinct separator cells, sorted.
                let Link { up_span, up_at, .. } = &links[index];
                let (tuples, order) = &mut scratch.bags[bag.parent];
                let key = |tuple| key(tuples, *up_span, up_at, tuple);
                order.clear();
                order.extend(0..(tuples.len() / up_span) as u32);
                order.sort_unstable_by(|&a, &b| key(a).cmp(key(b)));
                for &tuple in order.iter() {
                    let last = &keys[keys.len().saturating_sub(up_at.len())..];
                    if keys.is_empty() || !key(tuple).eq(last.iter().map(|&cell| cell as u64)) {
                        keys.extend(key(tuple).map(|cell| cell as u32));
                    }
                }
            }
            self.enter(bag, index == 0);
            scratch.cursors.clear();
            scratch.spans.clear();
            for &atom in &self.atoms {
                scratch.spans.push(scratch.cursors.len() as u32);
                let from = scratch.start_spans[atom] as usize;
                let to = scratch.start_spans[atom + 1] as usize;
                scratch.cursors.extend_from_slice(&scratch.starts[from..to]);
            }
            scratch.spans.push(scratch.cursors.len() as u32);
            let mut tuples = std::mem::take(&mut scratch.bags[index].0);
            tuples.clear();
            self.descend(scratch, allowed, &keys, 0, &mut tuples);
            found = !tuples.is_empty();
            scratch.bags[index].0 = tuples;
            if !found {
                break;
            }
        }
        scratch.keys = keys;
        // Leaves first: a child's tuples sorted by separator, and its parent
        // left with the tuples that agree with one of them.
        for (index, bag) in bags.iter().enumerate().skip(1).rev() {
            if !found {
                return;
            }
            let Link {
                span,
                at,
                up_span,
                up_at,
            } = &links[index];
            let (above, below) = scratch.bags.split_at_mut(index);
            let (tuples, order) = &mut below[0];
            let own = |tuple| key(tuples, *span, at, tuple);
            order.clear();
            order.extend(0..(tuples.len() / span) as u32);
            order.sort_unstable_by(|&a, &b| own(a).cmp(own(b)));
            let up = &mut above[bag.parent].0;
            let mut kept = 0;
            for tuple in 0..(up.len() / up_span) as u32 {
                let agrees = order
                    .binary_search_by(|&other| own(other).cmp(key(up, *up_span, up_at, tuple)))
                    .is_ok();
                if agrees {
                    let from = tuple as usize * up_span;
                    up.copy_within(from..from + up_span, kept * up_span);
                    kept += 1;
                }
            }
            up.truncate(kept * up_span);
            found = kept > 0;
        }
        if !found {
            return;
        }
        // Where each atom's rank sits: its bag, and its place in the tuple.
        let mut place: SmallVec<[(usize, usize); 8]> =
            SmallVec::from_elem((0, 0), self.join.atoms.len());
        for (index, bag) in bags.iter().enumerate() {
            for (slot, atom) in bits(bag.atoms).enumerate() {
                place[atom as usize] = (index, bag.vars.count_ones() as usize + slot);
            }
        }
        let mut chosen: SmallVec<[u32; 8]> = SmallVec::from_elem(0, bags.len());
        combine(&scratch.bags, bags, &links, &place, &mut chosen, 0, out);
    }

    /// Bind the bag's variable at `depth` to each cell every atom reading it
    /// allows, descend the atoms' cursors under it, and go on to the next;
    /// past the last, collect the leaves. `held` is the rows of the parent's
    /// separator cells that agree with the cells bound so far.
    fn descend<A: Allow + ?Sized>(
        &self,
        scratch: &mut JoinScratch,
        allowed: Option<&A>,
        held: &[u32],
        depth: usize,
        out: &mut Vec<u64>,
    ) {
        let width = self.atoms.len();
        let frame = depth * (width + 1);
        let Some(&attr) = self.vars.get(depth) else {
            return self.leaves(scratch, frame, out);
        };
        let var = self.join.vars[attr as usize];
        let span = |spans: &[u32], atom: usize| {
            spans[frame + atom] as usize..spans[frame + atom + 1] as usize
        };
        let here = Level::Attr(attr);
        let reads_here = |scratch: &JoinScratch, atom: usize| {
            let first = scratch.cursors[span(&scratch.spans, atom).start];
            self.join.atoms[self.atoms[atom]]
                .levels
                .get(first.node.depth())
                == Some(&here)
        };
        let trie = |cursor: &Cursor| self.sources[cursor.source as usize].trie;
        // A separator variable allows only the cells the parent's tuples
        // have, which are sorted in this column under the cells bound so far.
        let column = self.held[depth].map(usize::from);
        let cells = column.map(|column| {
            let mut cells = scratch.unions.pop().unwrap_or_default();
            cells.clear();
            for row in held.chunks_exact(self.stride) {
                if cells.last() != Some(&row[column]) {
                    cells.push(row[column]);
                }
            }
            cells
        });
        // An atom whose selected cell the attribute is allows the cells its
        // cursors are under.
        let selects = |atom: usize| self.join.atoms[self.atoms[atom]].cell == Some(attr);
        let mut selected: SmallVec<[Vec<u32>; 2]> = SmallVec::new();
        for atom in (0..width).filter(|&atom| selects(atom)) {
            let mut under = scratch.unions.pop().unwrap_or_default();
            under.clear();
            let cursors = &scratch.cursors[span(&scratch.spans, atom)];
            under.extend(cursors.iter().map(|cursor| cursor.cell));
            under.sort_unstable();
            under.dedup();
            selected.push(under);
        }
        let lists: SmallVec<[&[u32]; 4]> =
            cells.iter().chain(&selected).map(Vec::as_slice).collect();
        // An atom read through one node allows its set; one read through
        // several allows the union of their cells, taken as the widest set
        // and the others' cells admitted after when those are few.
        let mut several: SmallVec<[usize; 8]> = SmallVec::new();
        let mut patched: SmallVec<[(usize, usize); 8]> = SmallVec::new();
        let mut candidates = std::mem::take(&mut scratch.candidates[depth]);
        {
            let mut sets: SmallVec<[Set<'_>; 8]> =
                lists.iter().map(|cells| Set::Uint(cells)).collect();
            for atom in 0..width {
                if !reads_here(scratch, atom) {
                    continue;
                }
                let cursors = &scratch.cursors[span(&scratch.spans, atom)];
                let width = |cursor: &Cursor| trie(cursor).set(cursor.node).len();
                let (widest, _) = cursors
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, cursor)| width(cursor))
                    .expect("an atom has a cursor");
                let rest: usize =
                    cursors.iter().map(width).sum::<usize>() - width(&cursors[widest]);
                if rest * PATCH_DIVISOR <= width(&cursors[widest]) {
                    sets.push(trie(&cursors[widest]).set(cursors[widest].node));
                    if cursors.len() > 1 {
                        patched.push((atom, widest));
                    }
                } else {
                    several.push(atom);
                }
            }
            if sets.is_empty() {
                // Only unions read the variable: the one over fewest cells
                // starts the candidates.
                let reach = |&atom: &usize| -> usize {
                    let cursors = &scratch.cursors[span(&scratch.spans, atom)];
                    cursors
                        .iter()
                        .map(|cursor| trie(cursor).set(cursor.node).len())
                        .sum()
                };
                let at = (0..several.len()).min_by_key(|&at| reach(&several[at]));
                let atom = several.remove(at.expect("an atom reads the variable"));
                let mut pool = || scratch.unions.pop().unwrap_or_default();
                let (mut listed, mut spare) = (pool(), pool());
                candidates.clear();
                for cursor in &scratch.cursors[span(&scratch.spans, atom)] {
                    listed.clear();
                    trie(cursor).set(cursor.node).list(&mut listed);
                    merge(&mut candidates, &listed, &mut spare);
                }
                scratch.unions.extend([listed, spare]);
            } else {
                Set::intersect(
                    &mut sets,
                    &mut candidates,
                    &mut scratch.words,
                    &mut scratch.spare,
                );
            }
        }
        // The unions are applied last, a node at a time.
        for atom in several {
            let mut pool = || scratch.unions.pop().unwrap_or_default();
            let (mut part, mut out, mut spare) = (pool(), pool(), pool());
            out.clear();
            for cursor in &scratch.cursors[span(&scratch.spans, atom)] {
                let (trie, node) = (trie(cursor), cursor.node);
                let set = trie.set(node);
                Set::intersect(
                    &mut [Set::Uint(&candidates), set],
                    &mut part,
                    &mut scratch.words,
                    &mut scratch.spare,
                );
                merge(&mut out, &part, &mut spare);
            }
            std::mem::swap(&mut candidates, &mut out);
            scratch.unions.extend([part, out, spare]);
        }
        if !patched.is_empty() {
            self.admit(scratch, frame, here, &lists, &patched, &mut candidates);
        }
        drop(lists);
        scratch.unions.extend(cells.into_iter().chain(selected));
        let next = frame + width + 1;
        let mut from = 0;
        'cells: for &cell in &candidates {
            let held = match column {
                Some(column) => {
                    let rows = held.len() / self.stride;
                    let at = |row: usize| held[row * self.stride + column];
                    while at(from) < cell {
                        from += 1;
                    }
                    let to = (from..rows).find(|&row| at(row) != cell).unwrap_or(rows);
                    let rows = &held[from * self.stride..to * self.stride];
                    from = to;
                    rows
                }
                None => held,
            };
            if let (Attr::Var(var), Some(allowed)) = (var, allowed)
                && !allowed(var, cell)
            {
                continue;
            }
            scratch.path[depth] = cell;
            let end = scratch.spans[frame + width] as usize;
            scratch.cursors.truncate(end);
            scratch.spans.truncate(next);
            for atom in 0..width {
                scratch.spans.push(scratch.cursors.len() as u32);
                let cursors = span(&scratch.spans, atom);
                if selects(atom) {
                    for at in cursors {
                        let cursor = scratch.cursors[at];
                        if cursor.cell == cell {
                            scratch.cursors.push(cursor);
                        }
                    }
                    continue;
                }
                if !reads_here(scratch, atom) {
                    scratch.cursors.extend_from_within(cursors);
                    continue;
                }
                let levels = &self.join.atoms[self.atoms[atom]].levels;
                for at in cursors {
                    let Cursor {
                        source,
                        cell: selected,
                        mut node,
                    } = scratch.cursors[at];
                    let trie = self.sources[source as usize].trie;
                    // A variable an atom reads twice is a level per column.
                    while levels.get(node.depth()) == Some(&here) {
                        match trie.child(node, cell) {
                            Some(child) => node = child,
                            None => break,
                        }
                    }
                    if levels.get(node.depth()) != Some(&here) {
                        scratch.cursors.push(Cursor {
                            source,
                            cell: selected,
                            node,
                        });
                    }
                }
                if scratch.cursors.len() as u32 == scratch.spans[next + atom] {
                    continue 'cells;
                }
            }
            scratch.spans.push(scratch.cursors.len() as u32);
            self.descend(scratch, allowed, held, depth + 1, out);
        }
        scratch.candidates[depth] = candidates;
    }

    // Admit to `candidates` the cells of the nodes beside the widest of each
    // atom in `patched` that every atom allows and each of `lists`, the
    // separator's cells and the selected cells, has.
    fn admit(
        &self,
        scratch: &mut JoinScratch,
        frame: usize,
        here: Level,
        lists: &[&[u32]],
        patched: &[(usize, usize)],
        candidates: &mut Vec<u32>,
    ) {
        let span = |atom: usize| {
            scratch.spans[frame + atom] as usize..scratch.spans[frame + atom + 1] as usize
        };
        let trie = |cursor: &Cursor| self.sources[cursor.source as usize].trie;
        let reads_here = |atom: usize| {
            let first = scratch.cursors[span(atom).start];
            self.join.atoms[self.atoms[atom]]
                .levels
                .get(first.node.depth())
                == Some(&here)
        };
        let mut pool = || scratch.unions.pop().unwrap_or_default();
        let (mut extras, mut listed, mut part, mut spare) = (pool(), pool(), pool(), pool());
        extras.clear();
        for &(atom, widest) in patched {
            for (at, cursor) in scratch.cursors[span(atom)].iter().enumerate() {
                if at != widest {
                    listed.clear();
                    trie(cursor).set(cursor.node).list(&mut listed);
                    merge(&mut extras, &listed, &mut spare);
                }
            }
        }
        part.clear();
        'extras: for &cell in &extras {
            if lists
                .iter()
                .any(|cells| cells.binary_search(&cell).is_err())
            {
                continue;
            }
            for atom in 0..self.atoms.len() {
                let allows = |cursor: &Cursor| trie(cursor).child(cursor.node, cell).is_some();
                if reads_here(atom) && !scratch.cursors[span(atom)].iter().any(allows) {
                    continue 'extras;
                }
            }
            part.push(cell);
        }
        if !part.is_empty() {
            merge(candidates, &part, &mut spare);
        }
        scratch.unions.extend([extras, listed, part, spare]);
    }

    /// Every atom is at its leaves: the product of the rows they hold, each
    /// as a tuple of ranks, after the bag's cells if it keeps them.
    fn leaves(&self, scratch: &mut JoinScratch, frame: usize, out: &mut Vec<u64>) {
        let width = self.atoms.len();
        for atom in 0..width {
            let list = &mut scratch.leaves[atom];
            list.clear();
            let cursors =
                scratch.spans[frame + atom] as usize..scratch.spans[frame + atom + 1] as usize;
            for cursor in &scratch.cursors[cursors] {
                let source = &self.sources[cursor.source as usize];
                for &row in source.trie.rows(cursor.node) {
                    if source
                        .old
                        .is_some_and(|stamps| stamps[row as usize] + 1 == self.epoch)
                    {
                        continue;
                    }
                    list.push(source.base | (row as u64) << source.shift);
                }
            }
            if list.is_empty() {
                return;
            }
        }
        let leaves = &scratch.leaves[..width];
        let cells = &scratch.path[..self.vars.len() * usize::from(self.cells)];
        let mut at: SmallVec<[usize; 8]> = SmallVec::from_elem(0, width);
        loop {
            out.extend(cells.iter().map(|&cell| cell as u64));
            out.extend(leaves.iter().zip(&at).map(|(list, &at)| list[at]));
            let mut atom = width;
            loop {
                if atom == 0 {
                    return;
                }
                atom -= 1;
                at[atom] += 1;
                if at[atom] < leaves[atom].len() {
                    break;
                }
                at[atom] = 0;
            }
        }
    }
}

// Both lists are ascending and distinct, and `union` stays so.
fn merge(union: &mut Vec<u32>, cells: &[u32], spare: &mut Vec<u32>) {
    spare.clear();
    let (mut a, mut b) = (0, 0);
    while a < union.len() && b < cells.len() {
        let (x, y) = (union[a], cells[b]);
        spare.push(x.min(y));
        a += usize::from(x <= y);
        b += usize::from(y <= x);
    }
    spare.extend_from_slice(&union[a..]);
    spare.extend_from_slice(&cells[b..]);
    std::mem::swap(union, spare);
}

/// How a bag's tuples are laid out: their width and where the separator's
/// cells sit in them, then the same of its parent's.
struct Link {
    span: usize,
    at: SmallVec<[usize; 8]>,
    up_span: usize,
    up_at: SmallVec<[usize; 8]>,
}

/// One tuple per bag from bag `index` on, each child's agreeing with its
/// parent's on their separator: the rank tuple of each such match of the
/// run.
fn combine(
    tuples: &[(Vec<u64>, Vec<u32>)],
    bags: &[Bag],
    links: &[Link],
    place: &[(usize, usize)],
    chosen: &mut [u32],
    index: usize,
    out: &mut Vec<u64>,
) {
    if index == bags.len() {
        out.extend(
            place
                .iter()
                .map(|&(bag, at)| tuples[bag].0[chosen[bag] as usize * links[bag].span + at]),
        );
        return;
    }
    let Link {
        span,
        at,
        up_span,
        up_at,
    } = &links[index];
    let (own, order) = &tuples[index];
    let range = match index {
        0 => 0..own.len() / span,
        _ => {
            let parent = bags[index].parent;
            let wanted = || key(&tuples[parent].0, *up_span, up_at, chosen[parent]);
            let from = order.partition_point(|&other| key(own, *span, at, other).lt(wanted()));
            let to = order.partition_point(|&other| key(own, *span, at, other).le(wanted()));
            from..to
        }
    };
    for position in range {
        chosen[index] = match index {
            0 => position as u32,
            _ => order[position],
        };
        combine(tuples, bags, links, place, chosen, index + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use smallvec::{SmallVec, smallvec};

    use std::sync::Arc;

    use super::super::ir::{
        Atom, Cmp, Expr, Guard, Host, Param, Query, Rows, Scalar, Select, Swap, Term, Tuples,
    };
    use super::super::{Matches, PlanCache, Unchecked};
    use super::*;
    use crate::Words;
    use crate::db::SpaceId;
    use crate::store::Table;

    const IDS: u32 = 24;
    /// Two selections over the binary tables, one over the unary table, and
    /// the cells of the binary tables whose rows also read swapped.
    const BINARY: Param = 0;
    const OTHER: Param = 1;
    const UNARY: Param = 2;
    const SWAP: Param = 3;
    // The client table.
    const STEP: Param = 4;

    struct Selections;

    impl Host for Selections {
        fn version(&self) -> u64 {
            1
        }

        fn resolve(&self, param: Param, since: Option<u64>, out: &mut Resolution) {
            if since.is_some() {
                return;
            }
            let tables: &[(u32, &[u32])] = match param {
                BINARY => &[(1, &[0, 2]), (0, &[0, 1])],
                OTHER => &[(0, &[1, 2]), (1, &[1])],
                UNARY => &[(2, &[0, 1])],
                STEP => &[(0, &[])],
                _ => &[(1, &[0, 1]), (0, &[1])],
            };
            out.tables = tables
                .iter()
                .map(|&(table, cells)| (table, SmallVec::from_slice(cells)))
                .collect();
        }
    }

    impl crate::query::Calls for Selections {
        fn call(&self, _: u32, _: &[u64], _: &mut [u64]) -> bool {
            false
        }
    }

    type Db = Database<(), 1>;

    /// A bag and a keyed table of binary rows and a bag of unary ones, each
    /// row `(kind, args..) -> value` with a few kinds and few argument cells,
    /// filled over two epochs, with `between` run on the first. Every cell
    /// that names an id carries one of `spread` weights, so with more than
    /// one two rows can name an id and disagree on the reference.
    fn database(
        rows: &[(u32, u32, u32, u32, u32)],
        spread: u64,
        split: usize,
        between: impl Fn(&Db),
    ) -> Db {
        let space = SpaceId::nth(0);
        let mut db = Db::new();
        for _ in 0..IDS {
            db.mint(space, 0xff, false);
        }
        let weight = |cell: u32, salt: usize| (u64::from(cell) + salt as u64 / 7) % spread;
        db.add_table(Table::bag(3).plain(1), space);
        db.add_table(Table::new(3).plain(1), space);
        db.add_table(Table::bag(2).plain(1), space);
        for (at, &(table, kind, x, y, value)) in rows.iter().enumerate() {
            if at == split {
                between(&db);
                db.advance();
            }
            let tag = at as u32;
            let weights = [0, weight(x, at), weight(y, at + 3)];
            let value = (value, weight(value, at + 5));
            match table % 3 {
                2 => db.insert(2, &[kind, x], &weights[..2], value, tag),
                1 if db.table(1).unwrap().get(&[kind, x, y], &weights).is_some() => continue,
                table => db.insert(table, &[kind, x, y], &weights, value, tag),
            };
        }
        db.advance();
        db
    }

    fn atom(param: Param, args: &[u32], value: u32, tag: Option<u32>, swap: bool) -> Atom {
        let mut terms: SmallVec<[Term; 6]> = smallvec![Term::Any];
        terms.extend(args.iter().map(|&arg| Term::Var(arg)));
        terms.push(Term::Var(value));
        Atom::Rows(Rows {
            select: Select { column: 0, param },
            terms,
            tag,
            cell: None,
            swap: (swap && args.len() == 2).then_some(Swap {
                columns: [1, 2],
                when: SWAP,
            }),
            tag_within: None,
        })
    }

    /// Queries with a variable two atoms share and neither has bound when
    /// they are reached: a chain, a triangle, a triangle whose second atom
    /// reads one variable twice, a triangle reached sideways, and two
    /// triangles joined by a bridge reached sideways.
    fn query(shape: usize, swap: bool, guard: bool) -> Query {
        let (r, a, b, x, y, c, d, e) = (0, 1, 2, 3, 4, 5, 6, 7);
        let atoms = match shape {
            0 => vec![
                atom(BINARY, &[a, b], r, Some(0), swap),
                atom(UNARY, &[x], a, Some(1), swap),
                atom(UNARY, &[x], b, None, swap),
            ],
            1 => vec![
                atom(BINARY, &[a, b], r, Some(0), swap),
                atom(OTHER, &[x, y], a, Some(1), swap),
                atom(BINARY, &[x, y], b, None, swap),
            ],
            2 => vec![
                atom(BINARY, &[a, b], r, Some(0), swap),
                atom(OTHER, &[x, x], a, Some(1), swap),
                atom(BINARY, &[x, y], b, None, swap),
            ],
            3 => vec![
                atom(BINARY, &[a, b], r, Some(0), swap),
                atom(OTHER, &[a, x], c, Some(1), swap),
                atom(BINARY, &[b, x], d, None, swap),
            ],
            _ => vec![
                atom(BINARY, &[a, b], r, Some(0), swap),
                atom(UNARY, &[x], a, Some(1), swap),
                atom(UNARY, &[x], b, None, swap),
                atom(OTHER, &[x, c], y, None, swap),
                atom(UNARY, &[d], c, None, swap),
                atom(UNARY, &[e], d, None, swap),
                atom(UNARY, &[e], c, None, swap),
            ],
        };
        Query {
            space: SpaceId::nth(0),
            vars: 8,
            scalars: 2,
            params: 4,
            root: r,
            atoms,
            // Placed as soon as the root's atom binds both, which splits the
            // run there.
            guards: match guard {
                true => vec![Guard::Distinct(smallvec![(a, b)])],
                false => Vec::new(),
            },
            nots: Vec::new(),
        }
    }

    fn listed(matches: &Matches) -> Vec<(u32, Vec<crate::Ref>, Vec<u64>)> {
        (0..matches.len())
            .map(|at| {
                let (root, bindings, scalars) = matches.get(at);
                (root, bindings.to_vec(), scalars.to_vec())
            })
            .collect()
    }

    /// A chain whose every variable is looked up by the atom after the one
    /// that binds it has nothing to intersect, so it is not joined.
    #[test]
    fn a_chain_of_lookups_is_not_joined() {
        let (r, a, x, y) = (0, 1, 2, 3);
        let plan = Plan::compile(Query {
            space: SpaceId::nth(0),
            vars: 4,
            scalars: 0,
            params: 4,
            root: r,
            atoms: vec![
                atom(UNARY, &[a], r, None, false),
                atom(UNARY, &[x], a, None, false),
                atom(UNARY, &[y], x, None, false),
            ],
            guards: Vec::new(),
            nots: Vec::new(),
        });
        assert!(plan.joins.is_empty(), "{:?}", plan.run);
    }

    // `atom`, also binding its selected cell to `cell`.
    fn selecting(atom: Atom, cell: Scalar) -> Atom {
        let Atom::Rows(rows) = atom else {
            unreachable!("a row atom")
        };
        Atom::Rows(Rows {
            cell: Some(cell),
            ..rows
        })
    }

    const R: u32 = 0;
    // The selected cells of the three row atoms, then the scalars only the
    // client table binds.
    const CELLS: [Scalar; 3] = [2, 3, 4];
    const FREE: Scalar = 5;

    // `BINARY(a, b) = r`, `UNARY(x) = a` and `UNARY(y) = b`, each binding its
    // selected cell, and after them the client table atoms of `tables`. A
    // `guard` reads the first cell, so it ends the run the first atom is in.
    fn stepped(tables: &[&[Expr]], swap: bool, guard: bool) -> Query {
        let (a, b, x, y) = (1, 2, 3, 4);
        let mut atoms = vec![
            selecting(atom(BINARY, &[a, b], R, Some(0), swap), CELLS[0]),
            selecting(atom(UNARY, &[x], a, Some(1), swap), CELLS[1]),
            selecting(atom(UNARY, &[y], b, None, swap), CELLS[2]),
        ];
        atoms.extend(tables.iter().map(|terms| {
            Atom::Words(Tuples {
                table: STEP,
                terms: terms.to_vec(),
            })
        }));
        let small = Guard::Cmp(Cmp::Le, Expr::Scalar(CELLS[0]), Expr::Lit(9));
        Query {
            space: SpaceId::nth(0),
            vars: 5,
            scalars: 8,
            params: 5,
            root: R,
            atoms,
            guards: Vec::from_iter(guard.then_some(small)),
            nots: Vec::new(),
        }
    }

    fn words(rows: &[[u32; 5]]) -> Arc<Words> {
        let mut words = Words::new(5);
        for row in rows {
            words.push(row);
        }
        Arc::new(words)
    }

    // The matches `base` and a row of `words` per atom of `tables` make,
    // base match by base match and row by row, the first atom's rows
    // outermost: a term that is a scalar from `FREE` on takes the first
    // cell it meets, and every other term must evaluate to its cell.
    fn extended(
        base: &[(u32, Vec<crate::Ref>, Vec<u64>)],
        tables: &[&[Expr]],
        words: &Words,
    ) -> Vec<(u32, Vec<crate::Ref>, Vec<u64>)> {
        let mut matches: Vec<_> = base
            .iter()
            .map(|found| (found.clone(), [false; 3]))
            .collect();
        for terms in tables {
            let mut out = Vec::new();
            for ((root, bindings, scalars), taken) in &matches {
                for row in 0..words.len() {
                    let (mut scalars, mut taken) = (scalars.clone(), *taken);
                    let holds = terms.iter().enumerate().all(|(column, term)| {
                        let cell = u64::from(words.column(column)[row]);
                        match term {
                            Expr::Scalar(slot)
                                if *slot >= FREE
                                    && !std::mem::replace(
                                        &mut taken[(slot - FREE) as usize],
                                        true,
                                    ) =>
                            {
                                scalars[*slot as usize] = cell;
                                true
                            }
                            term => term.eval(&scalars) == Some(cell as i64),
                        }
                    });
                    if holds {
                        out.push(((*root, bindings.clone(), scalars), taken));
                    }
                }
            }
            matches = out;
        }
        matches.into_iter().map(|(found, _)| found).collect()
    }

    // The terms a column of a client table atom is drawn from: for the
    // first three columns, mostly the selected cell of the row atom of that
    // index.
    fn term(column: usize, choice: usize) -> Expr {
        let scalar = |slot: Scalar| Expr::Scalar(slot);
        match (column, choice) {
            (0..3, 0..=4) => scalar(CELLS[column]),
            (0, 5) => Expr::And(Box::new(scalar(CELLS[0])), Box::new(Expr::Lit(1))),
            (_, 6) => Expr::Lit(1),
            (_, choice) => scalar(FREE + (column + choice) as Scalar % 3),
        }
    }

    // A client table atom whose scalars other atoms of the run bind is
    // joined with them, on the selected cells and on what a second client
    // table atom binds; one whose term computes on such a scalar is left
    // out of the run.
    #[test]
    fn a_client_table_is_planned_into_the_run() {
        let [l, t1, t2] = CELLS.map(Expr::Scalar);
        let [f, g, h] = [FREE, FREE + 1, FREE + 2].map(Expr::Scalar);
        let first = [l.clone(), t1.clone(), t2.clone(), f.clone(), g.clone()];
        let second = [g.clone(), h.clone(), h.clone(), f.clone(), Expr::Lit(1)];
        let plan = Plan::compile(stepped(&[&first, &second], false, false));
        let table = |atom, keys| Step::Table { atom, keys };
        let run = [
            Step::Join(0),
            Step::Atom(0),
            Step::Atom(1),
            Step::Atom(2),
            table(3, 0b00111),
            table(4, 0b11001),
        ];
        assert_eq!(plan.run, run);
        assert_eq!(plan.joins[0].atoms[0].cell, Some(0));
        assert_eq!(plan.joins[0].atoms[4].levels[0], Level::Key(4));
        // The first table holds the binary atom to its first column. The
        // unary atoms share one selection, which neither may reduce.
        let held = |plan: &Plan| -> Vec<(Param, usize, u8)> {
            let held = plan.reductions().iter();
            held.map(|by| (by.rows, by.atom, by.column)).collect()
        };
        assert_eq!(held(&plan), [(BINARY, 3, 0)]);

        let masked = Expr::And(Box::new(l), Box::new(Expr::Lit(1)));
        let plan = Plan::compile(stepped(&[&[masked, t1, t2, f, g]], false, false));
        assert_eq!(plan.run, run[1..5]);
        assert!(plan.joins.is_empty());
        assert!(held(&plan).is_empty());
    }

    // The rows a key names are met in the order the client added them, as a
    // probe and as a join.
    #[test]
    fn a_key_with_several_rows_matches_once_per_row_in_their_order() {
        let mut db = database(
            &[(0, 0, 1, 2, 0), (2, 1, 5, 0, 1), (2, 0, 6, 0, 2)],
            1,
            3,
            |_| {},
        );
        db.add_words(words(&[
            [0, 1, 0, 9, 0],
            [0, 0, 0, 3, 0],
            [0, 1, 0, 7, 0],
            [0, 1, 1, 8, 0],
            [0, 1, 0, 9, 0],
        ]));
        let [l, t1, t2] = CELLS.map(Expr::Scalar);
        let terms = [l, t1, t2, Expr::Scalar(FREE), Expr::Lit(0)];
        let plan = Plan::compile(stepped(&[&terms], false, false));
        for join_min_rows in [0, usize::MAX] {
            let mut cache = PlanCache::default();
            cache.join_min_rows = join_min_rows;
            let found = plan.search_all(&db, &Selections, &mut cache);
            let bound: Vec<u64> = listed(&found)
                .iter()
                .map(|(_, _, scalars)| scalars[FREE as usize])
                .collect();
            assert_eq!(bound, [9, 7, 9]);
        }
    }

    proptest! {
        // Client table atoms extend each match of the atoms before them by
        // each of their rows that agrees with the match, in row order: as
        // probes, as a join and at the sizes that choose between the two,
        // from the root, from every row and from the new rows.
        #[test]
        fn a_client_table_matches_the_nested_loop_in_order(
            rows in prop::collection::vec((0u32..3, 0u32..3, 0u32..8, 0u32..8, 0u32..IDS), 20..200),
            table in prop::collection::vec([0u32..3, 0u32..2, 0u32..2, 0u32..3, 0u32..3], 0..120),
            choices in prop::collection::vec(prop::array::uniform5(0usize..9), 1..3),
            split in 0usize..200,
            swap in any::<bool>(),
            guard in any::<bool>(),
            old_below in 0usize..4,
        ) {
            let tables: Vec<Vec<Expr>> = choices
                .iter()
                .map(|choices| (0..5).map(|column| term(column, choices[column])).collect())
                .collect();
            let tables: Vec<&[Expr]> = tables.iter().map(Vec::as_slice).collect();
            let plan = Plan::compile(stepped(&tables, swap, guard));
            let base = Plan::compile(stepped(&[], swap, guard));
            let table = words(&table);
            let mut db = database(&rows, 1, split.min(rows.len() - 10), |_| {});
            db.add_words(table.clone());
            let search = |plan: &Plan, join_min_rows: Option<usize>| {
                let mut cache = PlanCache::default();
                cache.join_min_rows = join_min_rows.unwrap_or(cache.join_min_rows);
                let roots = plan.search_roots(&db, &Selections, 0..IDS, None::<&Unchecked>, (false, 0), &mut cache);
                let fresh = plan.search_roots(&db, &Selections, 0..IDS, None::<&Unchecked>, (true, old_below), &mut cache);
                let all = plan.search_all(&db, &Selections, &mut cache);
                let new = plan.search_new(&db, &Selections, &mut cache);
                [roots, fresh, all, new].iter().map(listed).collect::<Vec<_>>()
            };
            let nest = search(&plan, Some(usize::MAX));
            prop_assert_eq!(&search(&plan, Some(0)), &nest);
            prop_assert_eq!(&search(&plan, None), &nest);
            // An atom reading every row atom's cell is placed after them all
            // from wherever a search starts, so the nest meets its rows as
            // the model does; one placed before a row atom is met earlier.
            let last: Vec<&[Expr]> = plan
                .steps
                .iter()
                .filter_map(|step| match *step {
                    Step::Table { atom, .. } => Some(tables[atom - 3]),
                    _ => None,
                })
                .collect();
            let reads_all = |terms: &&[Expr]| CELLS.iter().all(|&cell| terms.contains(&Expr::Scalar(cell)));
            let ordered = tables.iter().all(reads_all);
            for (found, base) in nest.into_iter().zip(search(&base, Some(usize::MAX))) {
                let mut model = extended(&base, if ordered { &last } else { &tables }, &table);
                let mut found = found;
                if !ordered {
                    model.sort();
                    found.sort();
                }
                prop_assert_eq!(found, model);
            }
        }

        /// The join emits exactly the loop nest's matches in the loop nest's
        /// order, from the root, from every row and from the new rows, under
        /// a pruning callback and with old rows required below an atom,
        /// whether or not the cells carry weights.
        #[test]
        fn a_join_matches_the_loop_nest_in_order(
            rows in prop::collection::vec((0u32..3, 0u32..3, 0u32..8, 0u32..8, 0u32..IDS), 100..400),
            spread in 1u64..3,
            split in 0usize..400,
            shape in 0usize..5,
            swap in any::<bool>(),
            guard in any::<bool>(),
            prune in 0u32..4,
            old_below in 0usize..4,
        ) {
            let plan = Plan::compile(query(shape, swap, guard));
            // Indexes built over the first epoch's rows merge the second's.
            let db = database(&rows, spread, split.min(rows.len() - 20), |db| {
                let mut cache = PlanCache::default();
                cache.join_min_rows = 0;
                plan.search_all(db, &Selections, &mut cache);
            });
            prop_assert!(plan.joins.len() == 1, "the query has a join: {:?}", plan.run);
            let allowed = |var: u32, id: u32| prune == 0 || !(var * 7 + id).is_multiple_of(prune + 3);
            let search = |join_min_rows, reduce_min_rows| {
                let mut cache = PlanCache::default();
                cache.join_min_rows = join_min_rows;
                cache.reduce_min_rows = reduce_min_rows;
                let roots = plan.search_roots(&db, &Selections, 0..IDS, Some(&allowed), (false, 0), &mut cache);
                let fresh = plan.search_roots(&db, &Selections, 0..IDS, Some(&allowed), (true, old_below), &mut cache);
                let all = plan.search_all(&db, &Selections, &mut cache);
                let new = plan.search_new(&db, &Selections, &mut cache);
                [roots, fresh, all, new].iter().map(listed).collect::<Vec<_>>()
            };
            let nest = search(usize::MAX, usize::MAX);
            prop_assert_eq!(&search(0, usize::MAX), &nest);
            // Nor does reducing the rows a search starts from change them.
            prop_assert_eq!(&search(usize::MAX, 0), &nest);
            prop_assert_eq!(&search(0, 0), &nest);
        }
    }
}
