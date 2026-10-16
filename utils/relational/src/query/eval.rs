//! The evaluator: a plan's steps run as a loop nest over a database, with the
//! host consulted for what its parameters resolve to and for its functions.

use smallvec::SmallVec;

use super::ir::{
    Atom, Calls, Cmp, Expr, Guard, Host, Lattice, Out, Place, Read, Resolution, Scalar, Tuples, Var,
};
use super::join::{JOIN_MIN_ROWS, JoinScratch};
use super::plan::{Arg, Below, FactAnchor, JoinPlan, Plan, RowShape, Shape, Step};
use super::trie::Trie;
use crate::db::{Core, Database, Groups, holder};
use crate::store::Table;
use crate::{Ref, Words};

/// What a client keeps between searches for one plan: what its parameters
/// resolved to, which only changes when the host's version moves.
pub struct PlanCache {
    version: Option<u64>,
    resolutions: Vec<Resolution>,
    holders: Vec<u64>,
    reduced: Vec<SmallVec<[(u32, usize); 4]>>,
    /// Whether some row atom of the plan reads no table, so the plan has no
    /// match until a resolution grows.
    dead: bool,
    /// [`JOIN_MIN_ROWS`] unless a test forces a join or the loop nest.
    pub(crate) join_min_rows: usize,
    pub(crate) reduce_min_rows: usize,
    /// The evaluator's working arrays, kept so a search allocates nothing.
    scratch: Scratch,
    /// A join's working arrays, made by the first join a search runs.
    join: Option<Box<JoinScratch>>,
}

impl PlanCache {
    /// Hand back the matches of a search once they are spent, so the next
    /// search fills their arrays rather than growing new ones.
    pub fn recycle(&mut self, matches: Matches) {
        self.scratch.out = matches;
    }
}

impl Default for PlanCache {
    fn default() -> Self {
        Self {
            version: None,
            resolutions: Vec::new(),
            holders: Vec::new(),
            reduced: Vec::new(),
            dead: false,
            join_min_rows: JOIN_MIN_ROWS,
            reduce_min_rows: REDUCE_MIN_ROWS,
            scratch: Scratch::default(),
            join: None,
        }
    }
}

#[derive(Default)]
struct Scratch {
    bound: Vec<Ref>,
    scalars: Vec<u64>,
    trail: Vec<Var>,
    pool: Vec<Vec<u32>>,
    sets: Vec<Vec<u64>>,
    out: Matches,
}

/// What a variable no atom has reached holds.
pub const UNBOUND: Ref = Ref {
    id: u32::MAX,
    offset: 0,
};

/// `binding` unless it is [`UNBOUND`].
#[inline]
pub fn bound(binding: Ref) -> Option<Ref> {
    (binding.id != UNBOUND.id).then_some(binding)
}

/// Marks a trail entry that put the loose variable back.
const LOOSE: Var = 1 << 31;

/// What prunes a binding: `allowed(var, id)` rejects binding `var` to `id`.
pub trait Allow: Fn(Var, u32) -> bool {}

impl<F: Fn(Var, u32) -> bool + ?Sized> Allow for F {}

/// The searches no caller prunes.
pub(crate) type Unchecked = dyn Fn(Var, u32) -> bool;

/// A search expecting at most this many starting points looks rows up by
/// scanning the column instead of through a grouping: the scans cost less than
/// building one.
const SCAN_LIMIT: usize = 16;

// A search from fewer rows than this tries each of them: reducing them by the
// atoms below costs a pass over those atoms' rows.
const REDUCE_MIN_ROWS: usize = 64;

// An atom with more rows than this many times the rows it would reduce is not
// worth the pass.
const REDUCE_MAX_RATIO: usize = 16;

struct Reduced {
    column: u8,
    swapped: u8,
    ids: Vec<u64>,
}

// Not under a layer, since a grouping holds the base's rows alone.
fn reduces<C, const S: usize>(
    core: &Core<C, S>,
    below: &[Below],
    starts: usize,
    min: usize,
) -> bool {
    !below.is_empty() && starts >= min && !core.layered()
}

/// What a search works out before it reads a row.
pub struct Prepared {
    from: SmallVec<[(usize, usize, u64); 1]>,
    new: bool,
    scan: bool,
}

impl Prepared {
    /// How many rows the search starts from.
    pub fn starts(&self) -> usize {
        self.from.iter().map(|&(_, starts, _)| starts).sum()
    }
}

#[inline(always)]
fn reduced(below: &[Reduced], table: &Table, row: u32, swaps: bool) -> bool {
    let holds = |swapped: bool| {
        below.iter().all(|under| {
            let column = if swapped { under.swapped } else { under.column };
            let id = table.column(column as usize)[row as usize] as usize;
            under
                .ids
                .get(id / 64)
                .is_some_and(|word| word >> (id % 64) & 1 == 1)
        })
    };
    holds(false) || swaps && holds(true)
}

/// Run `$body` for each row of `$table` holding `$cell` in `$column`: read off
/// the grouping, or found by a scan when the search is too small to pay for
/// one.
macro_rules! lookup {
    ($eval:ident, $groups:ident, $slot:expr, $column:expr, $table:ident, $cell:expr, |$row:ident| $body:block) => {
        if $eval.scan {
            let rows = $eval.scan_rows($table.column($column), $cell);
            for &$row in &rows $body
            $eval.pool.push(rows);
        } else {
            for &$row in $groups.get($slot, $column).rows($cell) $body
        }
    };
}

/// The matches of one search, stored flat: a search that finds thousands builds
/// no vector per match. A match is the id the root bound, the reference every
/// variable bound ([`UNBOUND`] for one no atom reached) and the scalars.
#[derive(Default)]
pub struct Matches {
    roots: Vec<u32>,
    /// `roots.len()` rows of bindings back to back, and the same of scalars.
    bindings: Vec<Ref>,
    scalars: Vec<u64>,
    /// The length of a row of each.
    widths: (usize, usize),
}

impl Matches {
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Append a match. Every match of one search binds as many variables
    /// and scalars as the others.
    pub fn push(&mut self, root: u32, bindings: &[Ref], scalars: &[u64]) {
        self.widths = (bindings.len(), scalars.len());
        self.roots.push(root);
        self.bindings.extend_from_slice(bindings);
        self.scalars.extend_from_slice(scalars);
    }

    /// The root, bindings and scalars of match `index`.
    #[inline]
    pub fn get(&self, index: usize) -> (u32, &[Ref], &[u64]) {
        let (vars, scalars) = self.widths;
        (
            self.roots[index],
            &self.bindings[index * vars..(index + 1) * vars],
            &self.scalars[index * scalars..(index + 1) * scalars],
        )
    }

    fn clear(&mut self) {
        self.roots.clear();
        self.bindings.clear();
        self.scalars.clear();
    }

    /// Append the matches of another search of the same plan.
    pub fn extend(&mut self, other: Matches) {
        if !other.is_empty() {
            self.widths = other.widths;
        }
        self.roots.extend(other.roots);
        self.bindings.extend(other.bindings);
        self.scalars.extend(other.scalars);
    }
}

impl PlanCache {
    /// What parameter `param` resolved to at the last refresh.
    pub(crate) fn resolution(&self, param: u32) -> &Resolution {
        &self.resolutions[param as usize]
    }
}

impl Plan {
    /// Bring `cache` up to the host's version: what each parameter resolves
    /// to, from scratch the first time and from what appeared since after
    /// that, a row atom's cells less the ones no row of a client table of
    /// `db` that the atom is held to has (see `Reduction`). Reports
    /// whether the plan can match at all.
    pub fn refresh<C, const S: usize, H: Host + ?Sized>(
        &self,
        db: &Core<C, S>,
        host: &H,
        cache: &mut PlanCache,
    ) -> bool {
        let version = host.version();
        if cache.version == Some(version) {
            return !cache.dead;
        }
        let since = cache.version;
        let params = self.query.params as usize;
        cache.resolutions.resize_with(params, Resolution::default);
        for (param, out) in cache.resolutions.iter_mut().enumerate() {
            host.resolve(param as u32, since, out);
        }
        // What a reduced selection held at the last refresh is reduced
        // already.
        let reductions = self.reductions();
        cache.reduced.resize_with(reductions.len(), SmallVec::new);
        for (by, held) in reductions.iter().zip(&cache.reduced) {
            let selected = &cache.resolutions[by.rows as usize].tables;
            let known =
                |(table, cells): &(u32, SmallVec<[u32; 4]>)| held.contains(&(*table, cells.len()));
            if selected.iter().all(known) {
                continue;
            }
            let tuples = self.tuples(by.atom);
            let [selected, read] = cache
                .resolutions
                .get_disjoint_mut([by.rows as usize, tuples.table as usize])
                .expect("a selection and a client table");
            self.reduce_by(db, tuples, by.column, read, held, selected);
        }
        for (by, held) in reductions.iter().zip(&mut cache.reduced) {
            let selected = cache.resolutions[by.rows as usize].tables.iter();
            held.clear();
            held.extend(selected.map(|(table, cells)| (*table, cells.len())));
        }
        cache.version = Some(version);
        let resolutions = &cache.resolutions;
        cache.holders.clear();
        cache.holders.extend(resolutions.iter().map(|resolved| {
            let tables = resolved.tables.iter();
            tables.fold(0, |held, (table, _)| held | holder(*table))
        }));
        cache.dead = self.query.atoms.iter().any(|atom| {
            let param = match atom {
                Atom::Rows(rows) => rows.select.param,
                Atom::Words(tuples) => tuples.table,
                Atom::Lattice(_) | Atom::Place(_) => return false,
            };
            resolutions[param as usize].tables.is_empty()
        });
        !cache.dead
    }

    // Drop from `selected`, past the cells `held` counts per table, each
    // cell no row of the client tables `read` names holds in `column`
    // beside the literals of `tuples`; and the tables left with none.
    fn reduce_by<C, const S: usize>(
        &self,
        db: &Core<C, S>,
        tuples: &Tuples,
        column: u8,
        read: &Resolution,
        held: &[(u32, usize)],
        selected: &mut Resolution,
    ) {
        let literal = |(at, term): (usize, &Expr)| match term {
            Expr::Lit(value) => Some((at as u8, u32::try_from(*value).ok())),
            _ => None,
        };
        let literals: SmallVec<[(u8, Option<u32>); 4]> = tuples
            .terms
            .iter()
            .enumerate()
            .filter_map(literal)
            .collect();
        let columns: SmallVec<[u8; 8]> = literals.iter().map(|literal| literal.0).collect();
        let columns: SmallVec<[u8; 8]> = columns.into_iter().chain([column]).collect();
        let tries: SmallVec<[&Trie; 1]> = read
            .tables
            .iter()
            .map(|(table, _)| db.words(*table).trie(&columns))
            .collect();
        let allows = |cell: u32| {
            tries.iter().any(|trie| {
                let node = literals
                    .iter()
                    .try_fold(trie.root(), |node, literal| trie.child(node, literal.1?));
                node.is_some_and(|node| trie.set(node).contains(cell))
            })
        };
        for at in (0..selected.tables.len()).rev() {
            let (table, cells) = &mut selected.tables[at];
            let known = held.iter().find(|(known, _)| known == table);
            let old = known.map_or(0, |known| known.1).min(cells.len());
            let grown = cells.len();
            let mut kept = old;
            for new in old..grown {
                if allows(cells[new]) {
                    cells[kept] = cells[new];
                    kept += 1;
                }
            }
            cells.truncate(kept);
            if kept == grown {
                continue;
            }
            if cells.is_empty() {
                selected.tables.remove(at);
                if at < selected.scans.len() {
                    selected.scans.remove(at);
                }
            } else if let Some(scan) = selected.scans.get_mut(at) {
                let cells = &selected.tables[at].1;
                scan.retain(|cell| cells.binary_search(cell).is_ok());
            }
        }
    }

    /// Build what a search of this plan builds on first use and no row of
    /// `db` decides: the orders it is searched in, what its parameters
    /// resolve to, and the tries of the client tables every one of those
    /// orders looks rows up in or joins. A client calls it where it can
    /// best wait; `cache` and the tables keep what is built.
    pub fn prepare<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        cache: &mut PlanCache,
    ) {
        if !self.refresh(db, host, cache) {
            return;
        }
        let rooted = std::iter::once((&self.run, &self.joins));
        let anchored = self.anchors().iter();
        let anchored = anchored.map(|anchor| (&anchor.steps, &anchor.joins));
        let risen = self.fact_anchors().iter();
        let risen = risen.map(|anchor| (&anchor.steps, &anchor.joins));
        let trie = |atom: usize, columns: &[u8]| {
            let tables = &cache.resolutions[self.tuples(atom).table as usize].tables;
            for (table, _) in tables.iter().filter(|_| !columns.is_empty()) {
                db.words(*table).trie(columns);
            }
        };
        for (steps, joins) in rooted.chain(anchored).chain(risen) {
            for step in steps {
                if let Step::Table { atom, keys } = *step {
                    trie(atom, &key_columns(keys));
                }
            }
            for joined in joins.iter().flat_map(|join| &join.atoms) {
                if let Step::Table { atom, .. } = joined.step {
                    trie(atom, &joined.columns[0]);
                }
            }
        }
    }

    /// Every match at `roots`, each root canonicalized and visited once in the
    /// order first given, with every row atom below `fresh.1` held to rows the
    /// previous epoch did not write.
    ///
    /// `allowed(var, id)` prunes a binding the caller rejects. When `fresh.0`
    /// is set, a match nothing of whose rows or values the previous epoch
    /// touched is dropped: it existed an epoch earlier at a root that was
    /// searched then.
    pub fn search_roots<C, const S: usize, A: Allow + ?Sized>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        roots: impl IntoIterator<Item = u32>,
        allowed: Option<&A>,
        fresh: (bool, usize),
        cache: &mut PlanCache,
    ) -> Matches {
        if !self.refresh(db, host, cache) {
            return Matches::default();
        }
        let space = self.query.space;
        let mut roots: SmallVec<[u32; 16]> =
            roots.into_iter().map(|root| db.root(space, root)).collect();
        let mut marks = db.marks(space);
        roots.retain(|root| marks.insert(*root as usize));
        drop(marks);
        // A row atom at the root matches a row naming it, which a table
        // that never named the root has none of.
        let rooted = self.shapes.iter().find_map(|shape| match shape {
            Shape::Rows(rows) if rows.key == self.query.root => Some(rows.select.param),
            _ => None,
        });
        if let Some(param) = rooted {
            let held = cache.holders[param as usize];
            roots.retain(|root| db.naming(space, *root) & held != 0);
        }
        if roots.is_empty() {
            return Matches::default();
        }
        let scan = roots.len() <= SCAN_LIMIT;
        let mut ready = 0;
        if !scan {
            ready = self.group(db, cache, &self.run, &self.joins);
            for not in &self.nots {
                not.group(db, cache, &not.run, &[]);
            }
        }
        let groups = db.groups();
        let mut eval = self.eval(db, host, &groups, cache, allowed);
        eval.ready = ready;
        eval.scan = scan;
        eval.old_below = fresh.1;
        eval.only_new = fresh.0
            && self
                .query
                .atoms
                .iter()
                .any(|atom| matches!(atom, Atom::Rows(_)));
        for root in roots {
            if allowed.is_some_and(|allowed| !allowed(self.query.root, root)) {
                continue;
            }
            eval.loosen(self.query.root, root);
            self.step(&mut eval, root, 0);
            eval.bound[self.query.root as usize] = UNBOUND;
            eval.loose = None;
        }
        let (out, scratch) = eval.finish();
        cache.scratch = scratch;
        out
    }

    /// Every match, found by scanning the root atom's tables rather than by
    /// visiting ids. For an [`Self::anchorable`] plan.
    pub fn search_all<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        cache: &mut PlanCache,
    ) -> Matches {
        match self.prepare_all(db, host, cache) {
            Some(prepared) => self.run(db, &db.groups(), host, &prepared, cache),
            None => Matches::default(),
        }
    }

    /// What [`Self::search_all`] works out before it reads a row, with every
    /// grouping it reads built: [`Self::run`] then finds its matches from the
    /// database's core alone. `None` when the plan has no match.
    pub fn prepare_all<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        cache: &mut PlanCache,
    ) -> Option<Prepared> {
        if !self.refresh(db, host, cache) {
            return None;
        }
        let root = self
            .anchors()
            .iter()
            .position(|anchor| self.query.atoms[anchor.atom].key() == Some(self.query.root))
            .expect("an anchorable plan has a row at its root");
        let anchor = &self.anchors()[root];
        let rows = self.rows(anchor.atom);
        // The scan reads the root's rows by selected cell; the rest are
        // looked up.
        let tables = &cache.resolutions[rows.select.param as usize].tables;
        let column = rows.select.column as usize;
        for (table, _) in tables {
            db.prepare_group(*table, column);
        }
        let ready = self.group(db, cache, &anchor.steps[1..], &anchor.joins);
        let starts = {
            let groups = db.groups();
            let rows = tables.iter();
            rows.map(|(table, cells)| groups.count(*table, column, cells))
                .sum()
        };
        if reduces(db, &anchor.below, starts, cache.reduce_min_rows) {
            self.group_below(db, &cache.resolutions, &anchor.below);
        }
        Some(Prepared {
            from: SmallVec::from_elem((root, starts, ready), 1),
            new: false,
            scan: false,
        })
    }

    /// Every match with a row the previous epoch wrote, each once, found from
    /// those rows. For an [`Self::anchorable`] plan.
    ///
    /// A match with several new rows is found from the first of them in atom
    /// order: the search from a later one holds the earlier atoms to old rows.
    /// A match made new by a lattice value alone is not found here; see
    /// [`Self::search_facts`].
    pub fn search_new<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        cache: &mut PlanCache,
    ) -> Matches {
        match self.prepare_new(db, host, cache) {
            Some(prepared) => self.run(db, &db.groups(), host, &prepared, cache),
            None => Matches::default(),
        }
    }

    /// [`Self::prepare_all`] for [`Self::search_new`].
    pub fn prepare_new<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        cache: &mut PlanCache,
    ) -> Option<Prepared> {
        if !self.refresh(db, host, cache) {
            return None;
        }
        // Most epochs give most plans nothing to start from, so that is
        // settled before any grouping is built.
        for anchor in self.anchors() {
            let rows = self.rows(anchor.atom);
            let tables = cache.resolutions[rows.select.param as usize].tables.iter();
            for (table, _) in tables.filter(|(table, _)| db.wrote(*table)) {
                db.prepare_new_rows(*table, rows.select.column as usize);
            }
        }
        let mut starts = 0;
        let mut from: SmallVec<[(usize, usize, u64); 1]> = {
            let groups = db.groups();
            (0..self.anchors().len())
                .filter_map(|anchor| {
                    let rows = self.rows(self.anchors()[anchor].atom);
                    let column = rows.select.column as usize;
                    let found: usize = cache.resolutions[rows.select.param as usize]
                        .tables
                        .iter()
                        .filter(|(table, _)| db.wrote(*table))
                        .flat_map(|(table, cells)| {
                            cells
                                .iter()
                                .map(|&cell| groups.new_rows_with(*table, column, cell).len())
                                .chain(std::iter::once(layered_new(db, *table, column, cells)))
                        })
                        .sum();
                    starts += found;
                    (found != 0).then_some((anchor, found, 0))
                })
                .collect()
        };
        if from.is_empty() {
            return None;
        }
        let scan = starts <= SCAN_LIMIT;
        for (anchor, found, ready) in &mut from {
            let anchor = &self.anchors()[*anchor];
            if !scan {
                *ready = self.group(db, cache, &anchor.steps[1..], &anchor.joins);
            }
            if reduces(db, &anchor.below, *found, cache.reduce_min_rows) {
                self.group_below(db, &cache.resolutions, &anchor.below);
            }
        }
        Some(Prepared {
            from,
            new: true,
            scan,
        })
    }

    /// The matches of the search `prepared` was made for by this plan, over
    /// the database `core` is of and its `groups`. It reads nothing of the
    /// database but those two, so the searches of several plans may run at
    /// once, each with its own cache.
    pub fn run<C, const S: usize>(
        &self,
        core: &Core<C, S>,
        groups: &Groups,
        host: &dyn Calls,
        prepared: &Prepared,
        cache: &mut PlanCache,
    ) -> Matches {
        let min = cache.reduce_min_rows;
        let (resolutions, sets) = (&cache.resolutions, &mut cache.scratch.sets);
        let mut below: SmallVec<[SmallVec<[Reduced; 2]>; 1]> = prepared
            .from
            .iter()
            .map(|&(anchor, starts, _)| {
                let below = &self.anchors()[anchor].below;
                match reduces(core, below, starts, min) {
                    true => self.ids_below(core, groups, resolutions, below, starts, sets),
                    false => SmallVec::new(),
                }
            })
            .collect();
        let mut eval = self.eval::<C, S, Unchecked>(core, host, groups, cache, None);
        eval.scan = prepared.scan;
        for (&(anchor, _, ready), below) in prepared.from.iter().zip(below.drain(..)) {
            eval.ready = ready;
            eval.below = below;
            self.scan(&mut eval, anchor, prepared.new);
        }
        let (out, scratch) = eval.finish();
        cache.scratch = scratch;
        out
    }

    /// Every match at `roots` whose rows are all older than the previous epoch
    /// and one of whose lattice values is not: what [`Self::search_new`]
    /// leaves out.
    pub fn search_facts<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        roots: impl IntoIterator<Item = u32>,
        cache: &mut PlanCache,
    ) -> Matches {
        self.search_roots(
            db,
            host,
            roots,
            None::<&Unchecked>,
            (true, usize::MAX),
            cache,
        )
    }

    /// Every match one of whose lattice values the previous epoch raised and
    /// whose rows are all older than that, each once, found from those values:
    /// what [`Self::search_new`] leaves out. `minted` adds the values ids were
    /// minted with, which only a plan that reads no row needs. For a
    /// [`Self::fact_anchorable`] plan.
    pub fn search_risen<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        host: &dyn Host,
        minted: bool,
        cache: &mut PlanCache,
    ) -> Matches {
        if !self.refresh(db, host, cache) {
            return Matches::default();
        }
        let lists = |first_on_key: bool| match minted && first_on_key {
            true => &[false, true][..],
            false => &[false][..],
        };
        let starts = |anchor: &FactAnchor| {
            lists(anchor.first_on_key)
                .iter()
                .map(|&minted| db.risen(anchor.column, minted).len())
                .sum::<usize>()
        };
        let total: usize = self.fact_anchors().iter().map(starts).sum();
        if total == 0 {
            return Matches::default();
        }
        let scan = total <= SCAN_LIMIT;
        let ready: SmallVec<[u64; 4]> = self
            .fact_anchors()
            .iter()
            .map(|anchor| match scan {
                true => 0,
                false => self.group(db, cache, &anchor.steps, &anchor.joins),
            })
            .collect();
        // A row atom naming an anchor's key matches a row naming the id,
        // which a table that never named the id has none of.
        let names = |shape: &Shape, key: Var| match shape {
            Shape::Rows(rows) if rows.key == key || rows.args.iter().any(|arg| arg.var == key) => {
                Some(cache.holders[rows.select.param as usize])
            }
            _ => None,
        };
        let held: SmallVec<[u64; 4]> = self
            .fact_anchors()
            .iter()
            .map(|anchor| {
                self.shapes
                    .iter()
                    .find_map(|shape| names(shape, anchor.key))
            })
            .map(|held| held.unwrap_or(u64::MAX))
            .collect();
        let space = self.query.space;
        let groups = db.groups();
        let mut eval = self.eval::<C, S, Unchecked>(db, host, &groups, cache, None);
        eval.scan = scan;
        eval.only_new = true;
        eval.old_below = usize::MAX;
        for (index, anchor) in self.fact_anchors().iter().enumerate() {
            eval.fact_anchor = Some(index);
            eval.ready = ready[index];
            eval.facts_old = anchor.earlier;
            for &minted in lists(anchor.first_on_key) {
                for &key in db.risen(anchor.column, minted) {
                    if db.naming(space, key) & held[index] == 0 {
                        continue;
                    }
                    eval.loosen(anchor.key, key);
                    self.step(&mut eval, key, 0);
                    eval.bound[anchor.key as usize] = UNBOUND;
                    eval.loose = None;
                }
            }
        }
        let (out, scratch) = eval.finish();
        cache.scratch = scratch;
        out
    }

    fn eval<'a, C, const S: usize, A: Allow + ?Sized>(
        &self,
        db: &'a Core<C, S>,
        host: &'a dyn Calls,
        groups: &'a Groups,
        cache: &'a mut PlanCache,
        allowed: Option<&'a A>,
    ) -> Eval<'a, C, S, A> {
        let mut scratch = std::mem::take(&mut cache.scratch);
        // A lookup finds a representative in the cells, which name the
        // absorbed id until a canonicalization.
        debug_assert!(
            db.is_canonical(self.query.space),
            "search needs a canonical database"
        );
        scratch.bound.clear();
        scratch.bound.resize(self.query.vars as usize, UNBOUND);
        scratch.scalars.clear();
        scratch.scalars.resize(self.query.scalars as usize, 0);
        scratch.trail.clear();
        let mut out = std::mem::take(&mut scratch.out);
        out.clear();
        Eval {
            db,
            host,
            groups,
            resolutions: &cache.resolutions,
            holders: &cache.holders,
            nested: false,
            anchor: None,
            fact_anchor: None,
            facts_old: 0,
            old_below: 0,
            allowed,
            only_new: false,
            counting: false,
            hits: 0,
            bound: scratch.bound,
            scalars: scratch.scalars,
            trail: scratch.trail,
            scan: false,
            pool: scratch.pool,
            fresh: 0,
            join_min_rows: cache.join_min_rows,
            ready: 0,
            join: &mut cache.join,
            loose: None,
            below: SmallVec::new(),
            sets: scratch.sets,
            probes: Vec::new(),
            out,
        }
    }

    fn group_below<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        resolutions: &[Resolution],
        below: &[Below],
    ) {
        for under in below {
            let rows = self.rows(under.atom);
            for (table, _) in &resolutions[rows.select.param as usize].tables {
                db.prepare_group(*table, rows.select.column as usize);
            }
            self.group_below(db, resolutions, &under.below);
        }
    }

    fn ids_below<C, const S: usize>(
        &self,
        db: &Core<C, S>,
        groups: &Groups,
        resolutions: &[Resolution],
        below: &[Below],
        above: usize,
        sets: &mut Vec<Vec<u64>>,
    ) -> SmallVec<[Reduced; 2]> {
        let mut reduced_by = SmallVec::new();
        for under in below {
            let rows = self.rows(under.atom);
            let column = rows.select.column as usize;
            let tables = &resolutions[rows.select.param as usize].tables;
            let count = tables.iter();
            let count: usize = count
                .map(|(table, cells)| groups.count(*table, column, cells))
                .sum();
            if count > above.saturating_mul(REDUCE_MAX_RATIO) {
                continue;
            }
            let inner = self.ids_below(db, groups, resolutions, &under.below, count, sets);
            let mut ids = sets.pop().unwrap_or_default();
            ids.clear();
            ids.resize(db.ids(self.query.space).div_ceil(64), 0);
            for (slot, cells) in tables {
                let Some(table) = db.table(*slot) else {
                    continue;
                };
                let when = Self::when(rows, resolutions, *slot);
                let values = table.values();
                for &cell in cells {
                    let swaps = !when.is_empty() && reads(when, cell);
                    for &row in groups.get(*slot, column).rows(cell) {
                        if reduced(&inner, table, row, swaps) {
                            let id = values[row as usize] as usize;
                            ids[id / 64] |= 1 << (id % 64);
                        }
                    }
                }
            }
            sets.extend(inner.into_iter().map(|inner| inner.ids));
            reduced_by.push(Reduced {
                column: under.column,
                swapped: under.swapped,
                ids,
            });
        }
        reduced_by
    }

    /// The row atom at `atom`.
    pub(super) fn rows(&self, atom: usize) -> &RowShape {
        match &self.shapes[atom] {
            Shape::Rows(rows) => rows,
            _ => unreachable!("an anchor is a row atom"),
        }
    }

    pub(super) fn tuples(&self, atom: usize) -> &Tuples {
        match &self.shapes[atom] {
            Shape::Words(tuples) => tuples,
            _ => unreachable!("a table step is a client table atom"),
        }
    }

    /// The cells of `table` whose rows also match swapped.
    pub(super) fn when<'r>(
        rows: &RowShape,
        resolutions: &'r [Resolution],
        table: u32,
    ) -> &'r [u32] {
        match rows.when {
            Some(when) => resolutions[when as usize].cells_of(table),
            None => &[],
        }
    }

    /// Search from the rows of one anchor's atom: the ones the previous epoch
    /// wrote, or all of them.
    fn scan<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        anchor: usize,
        new: bool,
    ) {
        let atom = self.anchors()[anchor].atom;
        let rows = self.rows(atom);
        let (db, groups, resolutions) = (eval.db, eval.groups, eval.resolutions);
        eval.anchor = Some(anchor);
        eval.old_below = if new { atom } else { 0 };
        let column = rows.select.column as usize;
        let bind = (rows.tag, rows.key);
        let below = std::mem::take(&mut eval.below);
        let selected = &resolutions[rows.select.param as usize];
        for (position, (slot, cells)) in selected.tables.iter().enumerate() {
            // The rows listed as new for a table no epoch wrote since are
            // an earlier epoch's.
            if new && !db.wrote(*slot) {
                continue;
            }
            let scan = selected.scan(position);
            let when = Self::when(rows, resolutions, *slot);
            for (side, table) in db.sides(*slot) {
                let at = (*slot, side);
                if side != 0 {
                    // A layer's own rows are few: one pass over them, then the
                    // order a grouping would give, by selected cell and row.
                    if new && !db.side_new(side) {
                        continue;
                    }
                    let epoch = db.epoch() - 1;
                    let stamps = table.stamps();
                    let mut found: SmallVec<[(usize, u32); 16]> = table
                        .column(column)
                        .iter()
                        .enumerate()
                        .filter(|&(row, _)| !new || stamps[row] == epoch)
                        .filter(|&(_, &c)| reads(cells, c))
                        .filter_map(|(row, &c)| {
                            Some((scan.iter().position(|&cell| cell == c)?, row as u32))
                        })
                        .collect();
                    found.sort_unstable();
                    for (_, found) in found {
                        self.row(eval, 0, 0, atom, rows, at, table, found, bind, None, when);
                    }
                    continue;
                }
                for &cell in scan {
                    let swaps = !when.is_empty() && reads(when, cell);
                    if new {
                        for &(_, found) in groups.new_rows_with(*slot, column, cell) {
                            if reduced(&below, table, found, swaps) {
                                self.row(
                                    eval, 0, 0, atom, rows, at, table, found, bind, None, when,
                                );
                            }
                        }
                        continue;
                    }
                    for &found in groups.get(*slot, column).rows(cell) {
                        if reduced(&below, table, found, swaps) {
                            self.row(eval, 0, 0, atom, rows, at, table, found, bind, None, when);
                        }
                    }
                }
            }
        }
        eval.sets.extend(below.into_iter().map(|under| under.ids));
        eval.anchor = None;
    }

    /// Build the grouping each of `steps` looks rows up in: by value for an
    /// atom reached through it, by argument for one reached sideways; then
    /// the tries the joins among them read. Reports the joins whose tries it
    /// built, as a mask over their indices.
    fn group<C, const S: usize>(
        &self,
        db: &Database<C, S>,
        cache: &PlanCache,
        steps: &[Step],
        joins: &[JoinPlan],
    ) -> u64 {
        let resolutions = &cache.resolutions;
        for step in steps {
            let (atom, slot) = match *step {
                Step::Atom(atom) => (atom, None),
                Step::Parents { atom, slot } => (atom, Some(slot)),
                Step::Table { atom, keys } => {
                    let columns = key_columns(keys);
                    for (table, _) in &resolutions[self.tuples(atom).table as usize].tables {
                        if !columns.is_empty() {
                            db.words(*table).trie(&columns);
                        }
                    }
                    continue;
                }
                _ => continue,
            };
            let Shape::Rows(rows) = &self.shapes[atom] else {
                continue;
            };
            for (table_slot, _) in &resolutions[rows.select.param as usize].tables {
                let Some(table) = db.table(*table_slot) else {
                    continue;
                };
                match slot {
                    None if rows.within.is_some() => {
                        db.prepare_tag_order(*table_slot, table.arity())
                    }
                    None => db.prepare_group(*table_slot, table.arity()),
                    Some(slot) => {
                        let arg = rows.args[slot as usize];
                        db.prepare_group(*table_slot, arg.column as usize);
                        if rows.when.is_some() {
                            db.prepare_group(*table_slot, arg.swapped as usize);
                        }
                    }
                }
            }
        }
        let mut ready = 0;
        for step in steps {
            // A trie holds the base's rows alone, so a layer runs the loop nest.
            if let Step::Join(join) = *step
                && !db.layered()
                && join < 64
                && self.prepare_join(db, resolutions, &joins[join], cache.join_min_rows)
            {
                ready |= 1 << join;
            }
        }
        ready
    }

    pub(super) fn step<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
    ) {
        let steps = match (eval.anchor, eval.fact_anchor) {
            (Some(anchor), _) => &self.anchors()[anchor].steps,
            (None, Some(anchor)) => &self.fact_anchors()[anchor].steps,
            (None, None) => &self.run,
        };
        // A match of old rows and values alone is dropped at the end, and
        // nothing read from here on is new.
        let rooted = eval.anchor.is_none() && eval.fact_anchor.is_none();
        if index >= self.settled && eval.only_new && eval.fresh == 0 && rooted {
            return;
        }
        let mut index = index;
        let step = loop {
            let Some(&step) = steps.get(index) else {
                // A negated conjunction asks only whether a solution exists.
                if eval.counting {
                    eval.hits += 1;
                    return;
                }
                if !eval.only_new || eval.fresh > 0 {
                    let root =
                        bound(eval.bound[self.query.root as usize]).map_or(root, |root| root.id);
                    eval.out.push(root, &eval.bound, &eval.scalars);
                }
                return;
            };
            let Step::Join(join) = step else {
                break step;
            };
            // A join that would not pay is the run of steps that follows.
            if eval.ready >> join.min(63) & 1 == 1 {
                let joins = match (eval.anchor, eval.fact_anchor) {
                    (Some(anchor), _) => &self.anchors()[anchor].joins,
                    (None, Some(anchor)) => &self.fact_anchors()[anchor].joins,
                    (None, None) => &self.joins,
                };
                if self.pays(eval, &joins[join]) {
                    return self.join(eval, root, index, &joins[join]);
                }
            }
            index += 1;
        };
        if eval.counting && eval.hits > 0 {
            return;
        }
        let index_of_atom = match step {
            Step::Parents { atom, slot } => {
                let arg = self.rows(atom).args[slot as usize];
                let child = eval.bound[arg.var as usize];
                debug_assert_ne!(child, UNBOUND, "bound argument");
                self.parents(eval, root, index, atom, arg, child.id);
                return;
            }
            Step::Not(not) => {
                let outer = std::mem::replace(&mut eval.counting, true);
                let hits = std::mem::replace(&mut eval.hits, 0);
                let nested = std::mem::replace(&mut eval.nested, true);
                let mark = eval.trail.len();
                self.nots[not].step(eval, root, 0);
                eval.unbind(mark);
                let blocked = eval.hits > 0;
                eval.counting = outer;
                eval.hits = hits;
                eval.nested = nested;
                if !blocked {
                    self.step(eval, root, index + 1);
                }
                return;
            }
            Step::Guard(guard) => {
                self.guard(eval, root, index, &self.query.guards[guard]);
                return;
            }
            Step::Join(_) => unreachable!("a join is run or skipped above"),
            Step::Table { atom, keys } => return self.table(eval, root, index, atom, keys),
            Step::Atom(atom) => atom,
        };
        let key = match &self.shapes[index_of_atom] {
            Shape::Words(_) => unreachable!("a client table atom has a step of its own"),
            Shape::Rows(rows) => rows.key,
            Shape::Lattice(lattice) => lattice.key,
            Shape::Place(place) => place.key,
        };
        let reference = eval.bound[key as usize];
        debug_assert_ne!(reference, UNBOUND, "key bound by an earlier step");
        match &self.shapes[index_of_atom] {
            Shape::Rows(rows) => self.value(eval, root, index, index_of_atom, rows, reference.id),
            Shape::Lattice(lattice) => {
                self.lattice(eval, root, index, index_of_atom, lattice, reference)
            }
            Shape::Place(place) => self.place(eval, root, index, place, reference),
            Shape::Words(_) => unreachable!("a client table atom has a step of its own"),
        }
    }

    // Out of line, so the loop nest's step stays as small as it was.
    #[inline(never)]
    fn table<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        atom: usize,
        keys: u32,
    ) {
        let tuples = self.tuples(atom);
        let (db, resolutions) = (eval.db, eval.resolutions);
        if eval.probes.len() <= atom {
            eval.probes.resize_with(atom + 1, Probe::default);
        }
        // The orders one search steps may reach the atom under different
        // columns.
        if eval.probes[atom].tables.is_empty() || eval.probes[atom].keys != keys {
            let columns = key_columns(keys);
            let tables = resolutions[tuples.table as usize].tables.iter();
            let tables = tables.map(|(table, _)| {
                let words = db.words(*table);
                (words, (!columns.is_empty()).then(|| words.trie(&columns)))
            });
            eval.probes[atom] = Probe {
                keys,
                tables: tables.collect(),
                columns,
            };
        }
        for table in 0..eval.probes[atom].tables.len() {
            let probe = &eval.probes[atom];
            let (words, trie) = probe.tables[table];
            let rows = match trie {
                None => words.rows(),
                Some(trie) => {
                    let scalars = &eval.scalars;
                    let leaf = probe.columns.iter().try_fold(trie.root(), |node, &column| {
                        let cell = match &tuples.terms[column as usize] {
                            Expr::Scalar(slot) => scalars[*slot as usize] as i64,
                            term => term.eval(scalars)?,
                        };
                        trie.child(node, u32::try_from(cell as u64).ok()?)
                    });
                    leaf.map_or(&[][..], |leaf| trie.rows(leaf))
                }
            };
            for &row in rows {
                if eval.bind_words(tuples, keys, words, row) {
                    self.step(eval, root, index + 1);
                }
            }
        }
    }

    /// Run `guard`, and the steps after it if it holds.
    ///
    /// A rule may write a guard's result into the slot an earlier scalar lives
    /// in. From the root that is harmless: every match recomputes the earlier
    /// one first. An order that starts elsewhere can put a loop over rows
    /// between the two, so what the guard overwrote is put back for the next
    /// row.
    fn guard<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        guard: &Guard,
    ) {
        let written = guard.writes();
        // Most guards write one scalar or none, which needs no list.
        let before: SmallVec<[u64; 2]> = match written {
            [] | [_] => SmallVec::new(),
            _ => written
                .iter()
                .map(|&slot| eval.scalars[slot as usize])
                .collect(),
        };
        let one = written.first().map(|&slot| eval.scalars[slot as usize]);
        if eval.holds(guard) {
            self.step(eval, root, index + 1);
        }
        match (written, one) {
            ([slot], Some(value)) => eval.scalars[*slot as usize] = value,
            _ => {
                for (&slot, value) in written.iter().zip(before) {
                    eval.scalars[slot as usize] = value;
                }
            }
        }
    }

    /// A place atom reads what the id space knows of the reference.
    fn place<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        place: &Place,
        reference: Ref,
    ) {
        let (db, space) = (eval.db, self.query.space);
        match place.read {
            Read::Offset { base, offset } => {
                // The offset is read, so it is fixed from here on.
                let mark = eval.trail.len();
                eval.tighten(place.key);
                eval.scalars[offset as usize] = reference.offset;
                if eval.bind_one(base, reference.id.into()) {
                    self.step(eval, root, index + 1);
                }
                eval.unbind(mark);
            }
            Read::Pinned { offset } => {
                let found = db.find(space, reference);
                if db.is_zero(space, found.id) {
                    let mark = eval.trail.len();
                    eval.tighten(place.key);
                    eval.scalars[offset as usize] = found.offset;
                    self.step(eval, root, index + 1);
                    eval.unbind(mark);
                }
            }
            // An id's carrier never changes, so there is no freshness to
            // track.
            Read::Width { width } => {
                let mask = db.mask(space, reference.id);
                if mask != 0 {
                    eval.scalars[width as usize] = u64::from(mask.count_ones());
                    self.step(eval, root, index + 1);
                }
            }
            // An id stops being placed once and for all, so there is no
            // freshness to track either.
            Read::Conflicted => {
                if db.is_conflicted(space, reference.id) {
                    self.step(eval, root, index + 1);
                }
            }
        }
    }

    /// Every row the atom reads whose value is `value`.
    fn value<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        atom: usize,
        rows: &RowShape,
        value: u32,
    ) {
        let (db, groups, resolutions) = (eval.db, eval.groups, eval.resolutions);
        let select = rows.select.column as usize;
        // No row of a table that never named the id has it for its value.
        let naming = db.naming(self.query.space, value);
        if naming & eval.holders[rows.select.param as usize] == 0 {
            return;
        }
        for (slot, cells) in &resolutions[rows.select.param as usize].tables {
            if naming & holder(*slot) == 0 {
                continue;
            }
            let when = Self::when(rows, resolutions, *slot);
            for (side, table) in db.sides(*slot) {
                let at = (*slot, side);
                let bind = (rows.tag, rows.key);
                if side != 0 {
                    let rows_found = eval.scan_rows(table.values(), value);
                    for &row in &rows_found {
                        if reads(cells, table.column(select)[row as usize]) {
                            self.row(
                                eval, root, index, atom, rows, at, table, row, bind, None, when,
                            );
                        }
                    }
                    eval.pool.push(rows_found);
                    continue;
                }
                if let (Some((lo, hi)), false) = (&rows.within, eval.scan) {
                    // The plan placed the atom once both bounds were bound.
                    let bound = |expr: &Expr| {
                        expr.eval(&eval.scalars)
                            .map(|at| at.clamp(0, 1 << 32) as u64)
                    };
                    let (Some(lo), Some(hi)) = (bound(lo), bound(hi)) else {
                        return;
                    };
                    for &row in groups.tag_order(*slot, table.arity()).within(value, lo, hi) {
                        if reads(cells, table.column(select)[row as usize]) {
                            self.row(
                                eval, root, index, atom, rows, at, table, row, bind, None, when,
                            );
                        }
                    }
                    continue;
                }
                lookup!(eval, groups, *slot, table.arity(), table, value, |row| {
                    if reads(cells, table.column(select)[row as usize]) {
                        self.row(
                            eval, root, index, atom, rows, at, table, row, bind, None, when,
                        );
                    }
                });
            }
        }
    }

    /// Try one row of an atom's table, the one of side `side` at `slot`.
    /// `bind` names the scalar that takes the row's tag and the variable that
    /// takes the row's value, or is checked against it when bound. `swapped`
    /// picks the reading to try: as
    /// written or swapped alone, or with `None` both where `when` holds the
    /// row's selected cell.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn row<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        atom: usize,
        rows: &RowShape,
        (slot, side): (u32, usize),
        table: &Table,
        row: u32,
        bind: (Option<Scalar>, Var),
        swapped: Option<bool>,
        when: &[u32],
    ) {
        // A row a layer above its side re-keyed is read as its copy instead.
        if eval.db.stale(slot, side, row) {
            return;
        }
        let fresh = usize::from(table.stamps()[row as usize] + 1 == eval.db.epoch());
        if fresh == 1 && !eval.nested && atom < eval.old_below {
            return;
        }
        if let Some(slot) = bind.0 {
            eval.scalars[slot as usize] = table.tags()[row as usize] as u64;
        }
        if let Some(slot) = rows.cell {
            eval.scalars[slot as usize] =
                table.column(rows.select.column as usize)[row as usize] as u64;
        }
        let readings = match swapped {
            None if !when.is_empty()
                && reads(
                    when,
                    table.column(rows.select.column as usize)[row as usize],
                ) =>
            {
                0..2
            }
            None => 0..1,
            Some(swapped) => usize::from(swapped)..usize::from(swapped) + 1,
        };
        eval.fresh += fresh;
        for reading in readings {
            let mark = eval.trail.len();
            if eval.bind_value(bind.1, table, row)
                && eval.bind_row(&rows.args, table, row, reading == 1)
            {
                self.step(eval, root, index + 1);
            }
            eval.unbind(mark);
        }
        eval.fresh -= fresh;
    }

    /// Reach a row through a bound argument: the rows the atom reads that hold
    /// `child` in `arg`'s column. Everything already bound is checked against
    /// the row; the rest, including the row's value, is bound from it.
    fn parents<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        atom: usize,
        arg: Arg,
        child: u32,
    ) {
        let (db, groups, resolutions) = (eval.db, eval.groups, eval.resolutions);
        let rows = self.rows(atom);
        let select = rows.select.column as usize;
        let bind = (rows.tag, rows.key);
        // A variable's column holds ids, which a table that never named
        // the child has no row holding it in.
        let naming = db.naming(self.query.space, child);
        if naming & eval.holders[rows.select.param as usize] == 0 {
            return;
        }
        for (slot, cells) in &resolutions[rows.select.param as usize].tables {
            if naming & holder(*slot) == 0 {
                continue;
            }
            let when = Self::when(rows, resolutions, *slot);
            // Read swapped, the bound argument sits in the other column. No
            // row of a table with no cell in `when` reads swapped.
            let readings = if when.is_empty() { 1 } else { 2 };
            for (side, table) in db.sides(*slot) {
                debug_assert!(arg.column as usize >= table.plain_columns());
                let where_ = (*slot, side);
                for reading in 0..readings {
                    let at = if reading == 1 {
                        arg.swapped
                    } else {
                        arg.column
                    } as usize;
                    if side != 0 {
                        let rows_found = eval.scan_rows(table.column(at), child);
                        for &row in &rows_found {
                            let cell = table.column(select)[row as usize];
                            if reads(cells, cell) && (reading == 0 || reads(when, cell)) {
                                let swapped = Some(reading == 1);
                                self.row(
                                    eval, root, index, atom, rows, where_, table, row, bind,
                                    swapped, when,
                                );
                            }
                        }
                        eval.pool.push(rows_found);
                        continue;
                    }
                    lookup!(eval, groups, *slot, at, table, child, |row| {
                        let cell = table.column(select)[row as usize];
                        if reads(cells, cell) && (reading == 0 || reads(when, cell)) {
                            let swapped = Some(reading == 1);
                            self.row(
                                eval, root, index, atom, rows, where_, table, row, bind, swapped,
                                when,
                            );
                        }
                    });
                }
            }
        }
    }

    /// A lattice atom reads the value of `key`'s id: the column's stamp says
    /// which epoch proved it.
    fn lattice<C, const S: usize, A: Allow + ?Sized>(
        &self,
        eval: &mut Eval<'_, C, S, A>,
        root: u32,
        index: usize,
        atom: usize,
        lattice: &Lattice,
        key: Ref,
    ) {
        let db = eval.db;
        if eval.facts_old >> atom.min(63) & 1 == 1 && db.is_new(lattice.column, key.id) {
            return;
        }
        let mark = eval.trail.len();
        let held = match lattice.out {
            // The host read the offset, so it is fixed from here on.
            Out::Host(call) => {
                let held = eval.host.accepts(call, key);
                if held {
                    eval.tighten(lattice.key);
                }
                held
            }
            Out::Word(slot) => db
                .read(lattice.column, key.id)
                .map(|word| eval.scalars[slot as usize] = word)
                .is_some(),
            Out::In(param) => db.read(lattice.column, key.id).is_some_and(|word| {
                eval.resolutions[param as usize]
                    .cells
                    .contains(&(word as u32))
            }),
        };
        if !held {
            return;
        }
        let fresh = usize::from(db.is_new(lattice.column, key.id));
        eval.fresh += fresh;
        self.step(eval, root, index + 1);
        eval.fresh -= fresh;
        eval.unbind(mark);
    }
}

fn layered_new<C, const S: usize>(
    db: &Database<C, S>,
    slot: u32,
    column: usize,
    cells: &[u32],
) -> usize {
    if !db.layered() {
        return 0;
    }
    let epoch = db.epoch() - 1;
    db.sides(slot)
        .filter(|&(side, _)| side != 0 && db.side_new(side))
        .map(|(_, table)| {
            let column = table.column(column);
            let stamps = table.stamps();
            (0..table.len())
                .filter(|&row| stamps[row] == epoch && reads(cells, column[row]))
                .count()
        })
        .sum()
}

pub(super) fn key_columns(keys: u32) -> SmallVec<[u8; 8]> {
    (0..32).filter(|column| keys >> column & 1 == 1).collect()
}

pub(super) fn keyed<'w>(
    words: &'w Words,
    tuples: &Tuples,
    keys: u32,
    scalars: &[u64],
) -> &'w [u32] {
    let columns = key_columns(keys);
    if columns.is_empty() {
        return words.rows();
    }
    let trie = words.trie(&columns);
    let leaf = columns.iter().try_fold(trie.root(), |node, &column| {
        let cell = tuples.terms[column as usize].eval(scalars)?;
        trie.child(node, u32::try_from(cell as u64).ok()?)
    });
    leaf.map_or(&[], |leaf| trie.rows(leaf))
}

/// Whether `cell` is one of `cells`, which is ascending.
pub(super) fn reads(cells: &[u32], cell: u32) -> bool {
    match cells {
        [only] => *only == cell,
        _ => cells.binary_search(&cell).is_ok(),
    }
}

#[derive(Default)]
struct Probe<'a> {
    keys: u32,
    columns: SmallVec<[u8; 8]>,
    tables: SmallVec<[(&'a Words, Option<&'a Trie>); 1]>,
}

/// State one search threads through the loop nest.
pub(super) struct Eval<'a, C, const S: usize, A: ?Sized> {
    pub(super) db: &'a Core<C, S>,
    host: &'a dyn Calls,
    pub(super) groups: &'a Groups,
    /// Per parameter, what it resolved to; shared by the plan and its nots.
    pub(super) resolutions: &'a [Resolution],
    holders: &'a [u64],
    /// Whether a negated sub-conjunction is being stepped.
    pub(super) nested: bool,
    /// The anchor whose order is being stepped, when the search started at a
    /// row atom rather than at the root.
    anchor: Option<usize>,
    /// The fact anchor whose order is being stepped, when the search started
    /// at a risen value.
    fact_anchor: Option<usize>,
    /// The lattice atoms, as a mask over atom indices, that match only values
    /// older than the previous epoch.
    facts_old: u64,
    /// Row atoms below this index match only rows older than the previous
    /// epoch.
    pub(super) old_below: usize,
    /// `None` allows every binding.
    pub(super) allowed: Option<&'a A>,
    only_new: bool,
    /// Inside a negated conjunction: count solutions and stop at the first,
    /// rather than emit them.
    counting: bool,
    hits: usize,
    /// Per variable, its reference or [`UNBOUND`].
    pub(super) bound: Vec<Ref>,
    pub(super) scalars: Vec<u64>,
    /// Variables this branch bound, to undo on the way out. One with
    /// [`LOOSE`] set was the loose variable and had its offset fixed.
    pub(super) trail: Vec<Var>,
    /// The variable the search started at, bound to its id at offset zero
    /// until an atom fixes its offset: a row atom to the row's value, or one
    /// that reads the offset to the offset it read.
    loose: Option<Var>,
    /// Whether rows are looked up by scanning, with no grouping built.
    pub(super) scan: bool,
    /// Vectors a scan's hits go in, reused.
    pool: Vec<Vec<u32>>,
    /// How many rows and values of the partial match the previous epoch
    /// touched.
    pub(super) fresh: usize,
    pub(super) join_min_rows: usize,
    /// The joins of the order being stepped whose tries are built, as a mask
    /// over their indices: the rest run as the loop nest.
    ready: u64,
    pub(super) join: &'a mut Option<Box<JoinScratch>>,
    below: SmallVec<[Reduced; 2]>,
    sets: Vec<Vec<u64>>,
    probes: Vec<Probe<'a>>,
    out: Matches,
}

impl<C, const S: usize, A: Allow + ?Sized> Eval<'_, C, S, A> {
    /// The matches found, and the working arrays for the next search.
    fn finish(self) -> (Matches, Scratch) {
        let scratch = Scratch {
            bound: self.bound,
            scalars: self.scalars,
            trail: self.trail,
            pool: self.pool,
            sets: self.sets,
            out: Matches::default(),
        };
        (self.out, scratch)
    }

    /// The rows of `column` holding `cell`, in a vector from the pool.
    #[inline]
    fn scan_rows(&mut self, column: &[u32], cell: u32) -> Vec<u32> {
        let mut rows = self.pool.pop().unwrap_or_default();
        rows.clear();
        tir_adt::simd::select_eq(column, cell, &mut rows);
        rows
    }

    /// Start a search at `id`, with `var` bound to it and loose.
    fn loosen(&mut self, var: Var, id: u32) {
        self.bound[var as usize] = id.into();
        self.loose = Some(var);
    }

    /// Fix the offset of `var` where it is, if it is the loose variable.
    fn tighten(&mut self, var: Var) {
        if self.loose == Some(var) {
            self.loose = None;
            self.trail.push(var | LOOSE);
        }
    }

    /// Bind `var` to the value of a table row, or check it against the value
    /// when bound.
    #[inline(always)]
    pub(super) fn bind_value(&mut self, var: Var, table: &Table, row: u32) -> bool {
        let id = table.values()[row as usize];
        if !table.weighted() {
            // A row looked up by its value, in a table without weights, has
            // the value the variable already holds at offset zero.
            let prior = self.bound[var as usize];
            let settled = prior.id == id && prior.offset == 0 && self.loose != Some(var);
            return settled || self.bind_one(var, id.into());
        }
        self.bind_one(var, Ref::new(id, table.weight(table.arity(), row)))
    }

    /// Bind a row atom's arguments to the cells of a table row, read swapped
    /// or not, or report the row inconsistent with what is already bound.
    /// Whatever it bound before failing is on the trail.
    #[inline(always)]
    pub(super) fn bind_row(
        &mut self,
        args: &[Arg],
        table: &Table,
        row: u32,
        swapped: bool,
    ) -> bool {
        let weighted = table.weighted();
        for arg in args {
            let column = if swapped { arg.swapped } else { arg.column } as usize;
            // A canonical table names every id by its representative.
            let id = table.column(column)[row as usize];
            let cell = match weighted {
                true => Ref::new(id, table.weight(column, row)),
                false => id.into(),
            };
            if !self.bind_one(arg.var, cell) {
                return false;
            }
        }
        true
    }

    pub(super) fn bind_words(
        &mut self,
        tuples: &Tuples,
        keys: u32,
        words: &Words,
        row: u32,
    ) -> bool {
        let free = || {
            let terms = tuples.terms.iter().enumerate();
            terms
                .filter(|&(column, _)| keys >> column & 1 == 0)
                .filter_map(|(column, term)| {
                    Some((
                        term.lone()? as usize,
                        words.column(column)[row as usize] as u64,
                    ))
                })
        };
        for (slot, cell) in free() {
            self.scalars[slot] = cell;
        }
        free().all(|(slot, cell)| self.scalars[slot] == cell)
    }

    #[inline(always)]
    pub(super) fn bind_one(&mut self, var: Var, reference: Ref) -> bool {
        let prior = self.bound[var as usize];
        // A variable shared by two atoms must bind the same reference.
        if prior == reference {
            return true;
        }
        if prior.id != UNBOUND.id {
            if self.loose != Some(var) || prior.id != reference.id {
                return false;
            }
            self.bound[var as usize] = reference;
            self.loose = None;
            self.trail.push(var | LOOSE);
            return true;
        }
        if self
            .allowed
            .is_some_and(|allowed| !allowed(var, reference.id))
        {
            return false;
        }
        self.bound[var as usize] = reference;
        self.trail.push(var);
        true
    }

    /// Whether `guard` holds of the scalars bound so far, binding its own
    /// output if it does.
    fn holds(&mut self, guard: &Guard) -> bool {
        match guard {
            Guard::Cmp(cmp, a, b) => {
                let (Some(a), Some(b)) = (a.eval(&self.scalars), b.eval(&self.scalars)) else {
                    return false;
                };
                match cmp {
                    Cmp::Lt => a < b,
                    Cmp::Le => a <= b,
                    Cmp::Eq => a == b,
                    Cmp::Ne => a != b,
                }
            }
            Guard::Distinct(pairs) => !pairs
                .iter()
                .all(|&(a, b)| self.bound[a as usize] == self.bound[b as usize]),
            Guard::Let { out, value } => {
                let Some(value) = value.eval(&self.scalars) else {
                    return false;
                };
                self.scalars[*out as usize] = value as u64;
                true
            }
            Guard::Extern { call, args, out } => {
                let mut words: SmallVec<[u64; 4]> = SmallVec::new();
                for arg in args {
                    let Some(value) = arg.eval(&self.scalars) else {
                        return false;
                    };
                    words.push(value as u64);
                }
                let mut values: SmallVec<[u64; 2]> = SmallVec::from_elem(0, out.len());
                if !self.host.call(*call, &words, &mut values) {
                    return false;
                }
                for (&slot, value) in out.iter().zip(values) {
                    self.scalars[slot as usize] = value;
                }
                true
            }
        }
    }

    pub(super) fn unbind(&mut self, mark: usize) {
        while self.trail.len() > mark {
            let var = self.trail.pop().expect("above the mark");
            if var & LOOSE == 0 {
                self.bound[var as usize] = UNBOUND;
                continue;
            }
            let var = var & !LOOSE;
            self.bound[var as usize] = self.bound[var as usize].id.into();
            self.loose = Some(var);
        }
    }
}

#[cfg(test)]
mod tests {
    use smallvec::{SmallVec, smallvec};

    use super::super::ir::{
        Calls, Cmp, Expr, Guard, Host, Nested, Param, Query, Rows, Select, Term,
    };
    use super::*;
    use crate::db::SpaceId;

    struct Tables;

    impl Calls for Tables {
        fn call(&self, _: u32, _: &[u64], _: &mut [u64]) -> bool {
            false
        }
    }

    impl Host for Tables {
        fn version(&self) -> u64 {
            1
        }

        fn resolve(&self, param: Param, since: Option<u64>, out: &mut Resolution) {
            if since.is_none() {
                out.tables.push((param, SmallVec::from_slice(&[0])));
            }
        }
    }

    fn rows(param: Param, vars: &[Var], tag: u32, within: Option<(Expr, Expr)>) -> Atom {
        let mut terms: SmallVec<[Term; 6]> = smallvec![Term::Any];
        terms.extend(vars.iter().map(|&var| Term::Var(var)));
        Atom::Rows(Rows {
            select: Select { column: 0, param },
            terms,
            tag: Some(tag),
            cell: None,
            swap: None,
            tag_within: within,
        })
    }

    // `span(b) -> r` tag lo, `end(e) -> r` tag hi, `at(op) -> b` tag p with
    // lo <= p < hi + 1: as an interval of the last atom, or as two guards.
    fn query(interval: bool) -> Query {
        let (r, b, e, op) = (0, 1, 2, 3);
        let (lo, hi) = (
            Expr::Scalar(0),
            Expr::Add(Box::new(Expr::Scalar(1)), Box::new(Expr::Lit(1))),
        );
        let at = Expr::Scalar(2);
        let (within, guards) = match interval {
            true => (Some((lo, hi)), Vec::new()),
            false => (
                None,
                vec![
                    Guard::Cmp(Cmp::Le, lo, at.clone()),
                    Guard::Cmp(Cmp::Lt, at, hi),
                ],
            ),
        };
        Query {
            space: SpaceId::nth(0),
            vars: 4,
            scalars: 3,
            params: 3,
            root: r,
            atoms: vec![
                rows(2, &[op, b], 2, within),
                rows(0, &[b, r], 0, None),
                rows(1, &[e, r], 1, None),
            ],
            guards,
            nots: Vec::new(),
        }
    }

    fn listed(matches: &Matches) -> Vec<(u32, Vec<Ref>, Vec<u64>)> {
        let mut listed: Vec<_> = (0..matches.len())
            .map(|at| {
                let (root, bindings, scalars) = matches.get(at);
                (root, bindings.to_vec(), scalars.to_vec())
            })
            .collect();
        listed.sort();
        listed
    }

    // Three blocks of ops at scattered points, and ranges over them with
    // bounds before, inside and past the ops, empty ones among them.
    #[test]
    fn an_interval_reads_the_rows_its_guards_would_keep() {
        let space = SpaceId::nth(0);
        let mut db: Database<(), 1> = Database::new();
        for _ in 0..3 {
            db.add_table(Table::bag(2).plain(1), space);
        }
        let ids: Vec<u32> = (0..103).map(|_| db.mint(space, 0, false)).collect();
        let (blocks, ops, ranges) = (&ids[..3], &ids[3..63], &ids[63..]);
        let mut seed = 7u32;
        let mut next = |bound: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 8) % bound
        };
        for &op in ops {
            let block = blocks[next(3) as usize];
            db.insert(2, &[0, op], &[], (block, 0), next(50));
        }
        for &range in ranges {
            let (block, lo) = (blocks[next(3) as usize], next(60));
            db.insert(0, &[0, block], &[], (range, 0), lo);
            db.insert(1, &[0, range], &[], (range, 0), lo + next(20));
        }
        db.advance();
        let (interval, guarded) = (Plan::compile(query(true)), Plan::compile(query(false)));
        let (mut interval_cache, mut guarded_cache) = (PlanCache::default(), PlanCache::default());
        let within = listed(&interval.search_all(&db, &Tables, &mut interval_cache));
        let filtered = listed(&guarded.search_all(&db, &Tables, &mut guarded_cache));
        assert!(!within.is_empty());
        assert_eq!(within, filtered);
        let roots = 0..db.ids(space) as u32;
        let rooted = listed(&interval.search_roots(
            &db,
            &Tables,
            roots,
            None::<&Unchecked>,
            (false, 0),
            &mut interval_cache,
        ));
        assert_eq!(rooted, filtered);
    }

    // `x -> r`, `y -> x`, not `y -> x` in a third table: the negation names
    // `y`, which the query binds after `x`, so it asks about that `y` alone.
    #[test]
    fn a_negation_waits_for_every_variable_it_shares() {
        let space = SpaceId::nth(0);
        let mut db: Database<(), 1> = Database::new();
        for _ in 0..3 {
            db.add_table(Table::bag(2).plain(1), space);
        }
        let [r, x, y1, y2] = [(); 4].map(|_| db.mint(space, 0, false));
        db.insert(0, &[0, x], &[], (r, 0), 0);
        db.insert(1, &[0, y1], &[], (x, 0), 0);
        db.insert(1, &[0, y2], &[], (x, 0), 0);
        db.insert(2, &[0, y1], &[], (x, 0), 0);
        db.advance();
        let (vr, vx, vy) = (0, 1, 2);
        let plan = Plan::compile(Query {
            space,
            vars: 3,
            scalars: 1,
            params: 3,
            root: vr,
            atoms: vec![rows(0, &[vx, vr], 0, None), rows(1, &[vy, vx], 0, None)],
            guards: Vec::new(),
            nots: vec![Nested {
                atoms: vec![rows(2, &[vy, vx], 0, None)],
                guards: Vec::new(),
            }],
        });
        let matches = plan.search_roots(
            &db,
            &Tables,
            0..db.ids(space) as u32,
            None::<&Unchecked>,
            (false, 0),
            &mut PlanCache::default(),
        );
        let ys: Vec<u32> = listed(&matches)
            .iter()
            .map(|(_, bindings, _)| bindings[vy as usize].id)
            .collect();
        assert_eq!(ys, [y2]);
    }
}
