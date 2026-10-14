//! The database: the tables a client defines, the id spaces their cells name,
//! and everything a fixpoint evaluation and an assumption layer need on top.
//!
//! An id space mints `u32` cells and keeps a union-find over them, which
//! places each id at a constant offset from its representative: a cell and its
//! weight are a [`Ref`]. Equating two references is immediate for
//! [`Database::find`]; the tables catch up when the client asks for
//! [`Database::canonicalize`], which rewrites every table of the space through
//! the union-find, carrying the offsets, and reports the rows that came to
//! share a key. What the client does with those is its own business.
//!
//! Every row written is stamped with the current epoch, and every id minted,
//! equated, re-keyed or raised in a lattice column is logged against its space,
//! so a semi-naive client asks only what the epoch before wrote.
//!
//! An assumption is a layer over the base: a partition over the base's
//! representatives holding only the ids the layer equated, and per table the
//! rows the layer wrote — minted under it, or copies of lower rows re-keyed by
//! its equalities. A lower row some cell of which the layer absorbed is stale
//! under it: its copy answers instead. Nothing of the base is copied or
//! rebuilt when a layer opens or closes, so the base's indexes serve every
//! layer. Layers nest as a stack.
//!
//! The indexes a search reads, rows grouped by a column and tries over a
//! column order, keep a base built over the rows a table had and place the
//! rows appended or moved since into it; the base is rebuilt only when those
//! outgrow a fraction of it.

use std::cell::{self, RefCell, RefMut};
use std::sync::Arc;

use smallvec::SmallVec;

use crate::column::{Column, Fact, Join};
use crate::query::trie::Trie;
use crate::store::{Collision, Dense, Group, Rename, Repair, RowView, Table, TagOrder, Values};
use crate::unionfind::{Merge, UnionFind};
use crate::{Csr, FxHashMap, Ref, Words};

/// An id space: the ids one union-find partitions. A database has a fixed
/// number of them, numbered from zero, so a client names them as constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpaceId(u32);

impl SpaceId {
    /// The `index`th space.
    pub const fn nth(index: u32) -> Self {
        Self(index)
    }
}

/// A lattice column over the ids of one space. Lattice columns are numbered
/// from zero in the order they are declared, so a client that declares its
/// schema up front can name them as constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LatticeId(u32);

impl LatticeId {
    /// The `index`th lattice column declared.
    pub const fn nth(index: u32) -> Self {
        Self(index)
    }

    /// The position the column was declared at; also its bit in a mask of
    /// columns.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Tables, id spaces and lattice columns, with the change log and the
/// assumption layers over all of them. `C` is the client's own state, kept
/// here beside the tables. The number of id spaces, `SPACES`, is part of the
/// type, so a constant [`SpaceId`] costs no more than a field.
pub struct Database<C, const SPACES: usize> {
    core: Core<C, SPACES>,
    groups: RefCell<Groups>,
    /// Per space, the reverse index over the base and the version it was
    /// built at.
    reverse: RefCell<Vec<(u64, Csr)>>,
    holders: RefCell<Vec<(u64, Csr)>>,
    /// Reusable per-id "already seen" marks for the read-side sweeps, so a
    /// sweep that visits a handful of ids does not first zero an array the size
    /// of the space.
    marks: RefCell<Marks>,
}

/// What a [`Database`] holds, without the indexes it derives on demand:
/// everything a search reads once those are built, and so what the searches
/// of one round share between threads.
pub struct Core<C, const SPACES: usize> {
    state: State<C, SPACES>,
    /// Lattice columns keep an undo log per layer.
    lattices: Vec<Lattice>,
    layers: Vec<Layer<SPACES>>,
    /// Bumped whenever a table or a partition changes, so a derived index
    /// knows it is stale.
    version: u64,
    // An index over the base alone is keyed by this: a layer's writes leave
    // it.
    based: u64,
    // No layer or rename touches these.
    words: Vec<Arc<Words>>,
    spare: Vec<Vec<Table>>,
    stats: Stats,
    probed: Vec<u32>,
}

impl<C, const SPACES: usize> std::ops::Deref for Database<C, SPACES> {
    type Target = Core<C, SPACES>;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl<C, const SPACES: usize> std::ops::DerefMut for Database<C, SPACES> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

/// Cumulative work, for a fixpoint test and the saturation counters. A layer
/// does not roll these back, since they count work, not state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Ids equated away.
    pub merges: usize,
    /// Ids minted.
    pub adds: usize,
    /// Rows a canonicalization re-keyed.
    pub repairs: usize,
    /// Lattice entries a raise moved. A round that only raised a fact changed
    /// no table and no partition, and is not a fixpoint.
    pub raises: usize,
    /// Canonicalizations that had an equate to push into the tables.
    pub rebuilds: usize,
    /// Batches of keys looked up together, and the keys in them.
    pub batches: usize,
    pub batched: usize,
}

/// A row [`Database::canonicalize`] re-keyed, offered to the client with the
/// space's placement and the client's own state: answering true takes the
/// row out of its table, the client having taken its equation elsewhere.
pub type Dissolve<'a, C> = &'a mut dyn FnMut(&dyn Placement, &C, RowView<'_>) -> bool;

/// What a partition knows of a representative, as a reader of the current
/// layer sees it.
pub trait Placement {
    /// Whether `id`'s representative is a carrier's zero.
    fn is_zero(&self, id: u32) -> bool;
    /// The mask offsets on `id`'s set are reduced by.
    fn mask(&self, id: u32) -> u64;
}

impl Placement for UnionFind {
    fn is_zero(&self, id: u32) -> bool {
        UnionFind::is_zero(self, id)
    }

    fn mask(&self, id: u32) -> u64 {
        UnionFind::mask(self, id)
    }
}

/// A reference as the trace spells it: the id alone at offset zero.
struct Shown(Ref);

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.offset {
            0 => write!(f, "{}", self.0.id),
            offset => write!(f, "{}+{}", self.0.id, offset),
        }
    }
}

/// A lattice column and the space its keys are ids of.
struct Lattice {
    column: Column<u64>,
    space: SpaceId,
}

struct State<C, const SPACES: usize> {
    tables: Vec<Table>,
    /// Per table, the space its non-plain cells name.
    table_space: Vec<SpaceId>,
    spaces: [Space; SPACES],
    client: C,
    /// Per lattice column, the keys whose value rose.
    rises: Vec<Rises>,
    /// The stamp a row written now gets. Bumped by [`Database::advance`], so
    /// `stamp == epoch - 1` reads as "written during the epoch that just
    /// ended".
    epoch: u32,
}

struct Space {
    uf: UnionFind,
    /// Ids minted and not equated away.
    live: usize,
    /// Whether an equate happened since the last canonicalization.
    dirty: bool,
    /// Whether an equate was a [`Merge::Contradiction`].
    contradicted: bool,
    // Per id, the base tables with a row naming it past the plain columns,
    // as [`holder`] bits. An id that absorbs another takes its bits.
    holding: Vec<u64>,
    // The tables with a row naming an id absorbed since the last
    // canonicalization of the base, and whether one of those ids is older
    // than the epoch: `fresh` is the first id the epoch minted. A row naming
    // an id of the epoch was written in it.
    unclean: u64,
    aged: bool,
    fresh: u32,
    /// Ids changed since the last [`Database::take_changed`], possibly
    /// non-canonical. An epoch touches the same id many times, so entries are
    /// deduplicated as they are logged.
    changed: Vec<u32>,
    /// Per id, the `changed_epoch` it was last logged in.
    changed_at: Vec<u32>,
    changed_epoch: u32,
    changed_all: bool,
}

// The bit of `Space::holding` for the table at `slot`. The slots past the
// word share its last bit.
#[inline]
pub(crate) fn holder(slot: u32) -> u64 {
    1 << slot.min(63)
}

impl Space {
    // What `Database::new` starts a space with.
    fn clear(&mut self) {
        self.uf.clear();
        self.live = 0;
        self.dirty = false;
        self.contradicted = false;
        self.holding.clear();
        self.unclean = 0;
        self.aged = false;
        self.fresh = 0;
        self.changed.clear();
        self.changed_at.clear();
        self.changed_epoch = 1;
        // Nothing has been searched yet, so everything is new.
        self.changed_all = true;
    }

    // Note that a row of the base table at `slot` names each of `ids`. One
    // absorbed already is renamed by the next canonicalization, whatever
    // epoch that runs in.
    fn hold(&mut self, slot: u32, ids: impl Iterator<Item = u32>) {
        let minted = self.uf.len();
        for id in ids {
            if self.holding.len() <= id as usize {
                self.holding.resize(minted.max(id as usize + 1), 0);
            }
            self.holding[id as usize] |= holder(slot);
            if (id as usize) < minted && self.uf.parents()[id as usize] != id {
                self.holding[self.uf.root(id) as usize] |= holder(slot);
                self.unclean |= holder(slot);
                self.aged = true;
            }
        }
    }

    // `UnionFind::union` in the base: the tables naming the absorbed id are
    // to be canonicalized, and name the survivor from then on.
    fn union(&mut self, a: Ref, b: Ref) -> Merge {
        let merge = self.uf.union(a, b);
        if let Merge::Merged { survivor, absorbed } = merge
            && let Some(&held) = self.holding.get(absorbed as usize)
            && held != 0
        {
            self.unclean |= held;
            self.aged |= absorbed < self.fresh;
            self.hold_all(survivor, held);
        }
        merge
    }

    fn hold_all(&mut self, id: u32, tables: u64) {
        if self.holding.len() <= id as usize {
            self.holding.resize(self.uf.len(), 0);
        }
        self.holding[id as usize] |= tables;
    }
}

/// The keys of one lattice column whose value rose: on an id that already
/// existed, and on an id minted with it. The two are kept apart because an id
/// minted with a value has only new rows around it, which a client searches
/// from anyway.
#[derive(Clone, Default)]
struct Rises {
    rising: [Vec<u32>; 2],
    /// The same for the epoch [`Database::advance`] closed, canonical and
    /// ascending.
    risen: [Vec<u32>; 2],
}

struct Layer<const SPACES: usize> {
    overlay: [Overlay; SPACES],
    own: Vec<Option<Table>>,
    saved: [Saved; SPACES],
    rises: Vec<Rises>,
    // Put back when the layer closes. A lower row stamped at it is new to the
    // layer's first round.
    epoch: u32,
    ids: [usize; SPACES],
    /// Per space, the ids minted, equated or re-keyed under the layer.
    touched: Vec<Vec<u32>>,
    stale: Vec<Vec<Vec<u64>>>,
}

struct Copied {
    slot: u32,
    key: SmallVec<[u32; 8]>,
    weights: SmallVec<[u64; 8]>,
    weighted: bool,
    value: (u32, u64),
    tag: u32,
    key_moved: bool,
    settled: bool,
}

struct Saved {
    live: usize,
    dirty: bool,
    contradicted: bool,
    changed: Vec<u32>,
    changed_all: bool,
}

// Only the ids the layer absorbed have entries.
#[derive(Default)]
struct Overlay {
    // Flattened at each canonicalization, so an entry then names a
    // representative of the layer.
    parent: FxHashMap<u32, (u32, u64)>,
    roots: FxHashMap<u32, (u64, bool)>,
    absorbed: Vec<u64>,
    pending: Vec<u32>,
    shifted: bool,
}

impl Overlay {
    #[inline]
    fn absorbed(&self, id: u32) -> bool {
        let word = self.absorbed.get(id as usize / 64);
        word.is_some_and(|word| word >> (id % 64) & 1 == 1)
    }

    fn absorb(&mut self, id: u32) {
        let at = id as usize / 64;
        if self.absorbed.len() <= at {
            self.absorbed.resize(at + 1, 0);
        }
        self.absorbed[at] |= 1 << (id % 64);
        self.pending.push(id);
    }

    /// `reference`, a representative of what lies below, as this layer places
    /// it. `mask` answers for the destination.
    #[inline]
    fn find(&self, reference: Ref, mask: impl Fn(u32) -> u64) -> Ref {
        if !self.absorbed(reference.id) {
            return reference;
        }
        let (mut id, mut offset) = (reference.id, reference.offset);
        while let Some(&(parent, delta)) = self.parent.get(&id) {
            offset = offset.wrapping_add(delta);
            id = parent;
        }
        Ref {
            id,
            offset: offset & mask(id),
        }
    }
}

impl<C: Default, const SPACES: usize> Default for Database<C, SPACES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Default, const SPACES: usize> Database<C, SPACES> {
    /// Remove every row, id and lattice value, keeping the schema (the
    /// tables, the client tables and the lattice columns) and what the tables
    /// allocated: the database reads as a new one given that schema. A client
    /// that builds many small databases of one schema skips the allocations
    /// of each.
    pub fn clear(&mut self) {
        assert!(self.layers.is_empty(), "cleared under a layer");
        let core = &mut self.core;
        let state = &mut core.state;
        for table in &mut state.tables {
            table.clear();
        }
        for space in &mut state.spaces {
            space.clear();
        }
        state.client = C::default();
        for rises in &mut state.rises {
            *rises = Rises::default();
        }
        state.epoch = 1;
        for lattice in &mut core.lattices {
            lattice.column.clear();
        }
        core.version = 1;
        core.based = 1;
        core.stats = Stats::default();
        core.probed.clear();
        *self.groups.get_mut() = Groups::default();
        self.reverse.get_mut().clear();
        self.holders.get_mut().clear();
        *self.marks.get_mut() = Marks::default();
    }

    pub fn new() -> Self {
        let core = Core {
            state: State {
                tables: Vec::new(),
                table_space: Vec::new(),
                spaces: std::array::from_fn(|_| Space {
                    uf: UnionFind::new(),
                    live: 0,
                    dirty: false,
                    contradicted: false,
                    holding: Vec::new(),
                    unclean: 0,
                    aged: false,
                    fresh: 0,
                    changed: Vec::new(),
                    changed_at: Vec::new(),
                    changed_epoch: 1,
                    // Nothing has been searched yet, so everything is new.
                    changed_all: true,
                }),
                client: C::default(),
                rises: Vec::new(),
                epoch: 1,
            },
            lattices: Vec::new(),
            layers: Vec::new(),
            version: 1,
            based: 1,
            words: Vec::new(),
            spare: Vec::new(),
            stats: Stats::default(),
            probed: Vec::new(),
        };
        Self {
            core,
            groups: RefCell::default(),
            reverse: RefCell::default(),
            holders: RefCell::default(),
            marks: RefCell::default(),
        }
    }
}

impl<C, const SPACES: usize> Core<C, SPACES> {
    // ---- schema -----------------------------------------------------------

    /// Add `table`, whose cells past its plain columns name ids of `space`,
    /// at the next slot.
    pub fn add_table(&mut self, table: Table, space: SpaceId) -> u32 {
        self.state.tables.push(table);
        self.state.table_space.push(space);
        self.state.tables.len() as u32 - 1
    }

    /// Add `table`, a client table the [`Tuples`](crate::query::Tuples)
    /// atoms range over, at the next slot of its own numbering. The table and
    /// the tries built over it are shared, not copied: a client builds it
    /// once for every database that reads it.
    pub fn add_words(&mut self, table: Arc<Words>) -> u32 {
        self.words.push(table);
        self.words.len() as u32 - 1
    }

    /// The client table at `slot`.
    #[inline]
    pub fn words(&self, slot: u32) -> &Words {
        &self.words[slot as usize]
    }

    /// A new lattice column keyed on the ids of `space`. Declared with no
    /// layer open: a layer has nothing to put back for it.
    pub fn add_lattice(&mut self, space: SpaceId, join: Join) -> LatticeId {
        assert!(self.layers.is_empty(), "declared under a layer");
        self.lattices.push(Lattice {
            column: Column::new(join),
            space,
        });
        self.state.rises.push(Rises::default());
        LatticeId(self.lattices.len() as u32 - 1)
    }

    #[inline]
    fn touch(&mut self) {
        self.version += 1;
        if self.layers.is_empty() {
            self.based = self.version;
        }
    }

    // ---- ids --------------------------------------------------------------

    /// Mint an id of `space` in a set of its own. References to it carry
    /// offsets reduced by `mask`, the [`crate::unionfind::mask`] of its
    /// carrier; a `pinned` id is its carrier's zero, which survives every
    /// equate, so an id equated with it at an offset reads as that constant.
    #[inline]
    pub fn mint(&mut self, space: SpaceId, mask: u64, pinned: bool) -> u32 {
        let state = &mut self.state.spaces[space.0 as usize];
        let id = state.uf.push(mask, pinned);
        state.live += 1;
        self.stats.adds += 1;
        if let Some(layer) = self.layers.last_mut() {
            layer.touched[space.0 as usize].push(id);
        }
        self.log_change(space, id);
        id
    }

    /// `reference` as its set's representative plus an offset.
    #[inline(always)]
    pub fn find(&self, space: SpaceId, reference: impl Into<Ref>) -> Ref {
        let at = space.0 as usize;
        let found = self.state.spaces[at].uf.find(reference.into());
        if self.layers.is_empty() {
            return found;
        }
        self.find_through(space, found, self.layers.len())
    }

    #[inline(always)]
    fn find_through(&self, space: SpaceId, found: Ref, depth: usize) -> Ref {
        let at = space.0 as usize;
        let mut found = found;
        for layer in 0..depth {
            let overlay = &self.layers[layer].overlay[at];
            if overlay.absorbed(found.id) {
                found = overlay.find(found, |id| self.mask_below(space, id, layer + 1));
            }
        }
        found
    }

    /// The representative of `id`'s set, whatever offset `id` sits at from
    /// it: the set's pinned id, or the smallest id in it.
    #[inline]
    pub fn root(&self, space: SpaceId, id: u32) -> u32 {
        if self.layers.is_empty() {
            return self.state.spaces[space.0 as usize].uf.root(id);
        }
        self.find(space, id).id
    }

    /// The union-find of the base: what every layer places further.
    #[inline]
    pub fn partition(&self, space: SpaceId) -> &UnionFind {
        &self.state.spaces[space.0 as usize].uf
    }

    /// The partition of `space` as the current layer reads it.
    pub fn placement(&self, space: SpaceId) -> Placed<'_, C, SPACES> {
        Placed { db: self, space }
    }

    /// How many ids `space` had minted when the outermost open layer opened.
    pub fn ids_before_layers(&self, space: SpaceId) -> Option<usize> {
        self.layers.first().map(|layer| layer.ids[space.0 as usize])
    }

    #[inline]
    fn mask_below(&self, space: SpaceId, id: u32, depth: usize) -> u64 {
        let at = space.0 as usize;
        for layer in self.layers[..depth].iter().rev() {
            if let Some(&(mask, _)) = layer.overlay[at].roots.get(&id) {
                return mask;
            }
        }
        self.state.spaces[at].uf.masks()[id as usize]
    }

    /// The mask offsets on `id`'s set are reduced by.
    #[inline]
    pub fn mask(&self, space: SpaceId, id: u32) -> u64 {
        if self.layers.is_empty() {
            return self.state.spaces[space.0 as usize].uf.mask(id);
        }
        let root = self.root(space, id);
        self.mask_below(space, root, self.layers.len())
    }

    /// Whether `id`'s representative is a carrier's zero.
    #[inline]
    pub fn is_zero(&self, space: SpaceId, id: u32) -> bool {
        // A zero survives every equate, so no layer pins an id.
        let uf = &self.state.spaces[space.0 as usize].uf;
        uf.pinned(self.root(space, id))
    }

    /// Whether an equate tried to put `id`'s set at a non-zero offset from
    /// itself.
    pub fn is_conflicted(&self, space: SpaceId, id: u32) -> bool {
        let at = space.0 as usize;
        let uf = &self.state.spaces[at].uf;
        if self.layers.is_empty() {
            return uf.is_conflicted(id);
        }
        let root = self.root(space, id);
        for layer in self.layers.iter().rev() {
            if let Some(&(_, conflict)) = layer.overlay[at].roots.get(&root) {
                return conflict;
            }
        }
        uf.is_conflicted(root)
    }

    /// Whether an equate of `space` since the outermost open layer (or ever,
    /// with none open) was a [`Merge::Contradiction`].
    #[inline]
    pub fn contradicted(&self, space: SpaceId) -> bool {
        self.state.spaces[space.0 as usize].contradicted
    }

    /// One past the highest id `space` ever minted, live or not: the size an
    /// array indexed by its ids needs.
    #[inline]
    pub fn ids(&self, space: SpaceId) -> usize {
        self.state.spaces[space.0 as usize].uf.len()
    }

    /// Ids minted and not equated away. An id minted under a closed layer
    /// stays allocated, its own representative, but no longer counts.
    #[inline]
    pub fn live(&self, space: SpaceId) -> usize {
        self.state.spaces[space.0 as usize].live
    }

    /// Record `a == b`. Unless they were equal already, the lattice columns
    /// over the space join the absorbed id's value onto the survivor's; the
    /// tables keep naming the absorbed id until [`Self::canonicalize`].
    ///
    /// Two references naming one id at different offsets, or two pinned ids,
    /// are a [`Merge::Contradiction`]: nothing merges, the sets are marked
    /// conflicted, and [`Self::contradicted`] reports it.
    #[inline]
    pub fn equate(&mut self, space: SpaceId, a: Ref, b: Ref) -> Merge {
        let at = space.0 as usize;
        let (a, b) = (self.find(space, a), self.find(space, b));
        if a == b {
            return Merge::Same;
        }
        let merge = match self.layers.is_empty() {
            true => self.state.spaces[at].union(a, b),
            false => self.union_in_layer(space, a, b),
        };
        let Merge::Merged { survivor, absorbed } = merge else {
            if crate::trace_enabled() {
                eprintln!("X {} {}", Shown(a), Shown(b));
            }
            self.touch();
            self.state.spaces[at].contradicted = true;
            self.log_change(space, a.id);
            self.log_change(space, b.id);
            return merge;
        };
        self.touch();
        if crate::trace_enabled() {
            eprintln!("U {} {} -> {survivor}", Shown(a), Shown(b));
        }
        let state = &mut self.state.spaces[at];
        state.live -= 1;
        state.dirty = true;
        if let Some(layer) = self.layers.last_mut() {
            layer.touched[at].push(survivor);
        }
        self.log_change(space, survivor);
        self.stats.merges += 1;
        for lattice in 0..self.lattices.len() {
            let entry = &self.lattices[lattice];
            if entry.space == space && !entry.column.is_empty() {
                self.merge(LatticeId(lattice as u32), absorbed, survivor);
            }
        }
        merge
    }

    fn union_in_layer(&mut self, space: SpaceId, a: Ref, b: Ref) -> Merge {
        let at = space.0 as usize;
        let depth = self.layers.len();
        if a.id == b.id {
            self.conflict(space, a.id);
            return Merge::Contradiction;
        }
        let uf = &self.state.spaces[at].uf;
        let (pa, pb) = (uf.pinned(a.id), uf.pinned(b.id));
        if pa && pb {
            self.conflict(space, a.id);
            self.conflict(space, b.id);
            return Merge::Contradiction;
        }
        let (survivor, absorbed) = match pa || (!pb && a.id < b.id) {
            true => (a, b),
            false => (b, a),
        };
        let mask =
            self.mask_below(space, survivor.id, depth) | self.mask_below(space, absorbed.id, depth);
        let conflict =
            self.is_conflicted(space, survivor.id) || self.is_conflicted(space, absorbed.id);
        // absorbed.id + absorbed.offset == survivor.id + survivor.offset
        let delta = survivor.offset.wrapping_sub(absorbed.offset) & mask;
        let overlay = &mut self.layers[depth - 1].overlay[at];
        overlay.parent.insert(absorbed.id, (survivor.id, delta));
        overlay.roots.insert(survivor.id, (mask, conflict));
        overlay.absorb(absorbed.id);
        overlay.shifted |= delta != 0;
        Merge::Merged {
            survivor: survivor.id,
            absorbed: absorbed.id,
        }
    }

    fn conflict(&mut self, space: SpaceId, id: u32) {
        let at = space.0 as usize;
        let mask = self.mask_below(space, id, self.layers.len());
        let overlay = &mut self.layers.last_mut().expect("an open layer").overlay[at];
        overlay.roots.insert(id, (mask, true));
    }

    /// Whether every table of `space` names every id by its representative.
    #[inline]
    pub fn is_canonical(&self, space: SpaceId) -> bool {
        !self.state.spaces[space.0 as usize].dirty
    }

    fn canonicalize_base(
        &mut self,
        space: SpaceId,
        mut dissolve: Option<Dissolve<'_, C>>,
        report: &mut Repair,
    ) -> bool {
        let State {
            tables,
            table_space,
            spaces,
            client,
            epoch,
            ..
        } = &mut self.state;
        let state = &mut spaces[space.0 as usize];
        if !std::mem::take(&mut state.dirty) {
            // Only an equate un-flattens the union-find, and it leaves the
            // space dirty, so a clean one is already flat.
            return false;
        }
        state.uf.flatten();
        let unclean = std::mem::take(&mut state.unclean);
        let recent = !std::mem::take(&mut state.aged);
        self.version += 1;
        self.based = self.version;
        self.stats.rebuilds += 1;
        report.rekeyed.clear();
        report.collisions.clear();
        let uf = &state.uf;
        let client = &*client;
        let rename = Dense {
            map: uf.parents(),
            delta: uf.deltas(),
            mask: uf.masks(),
        };
        let mut offer = dissolve
            .as_mut()
            .map(|dissolve| |row: RowView<'_>| dissolve(uf, client, row));
        for (slot, (table, &over)) in tables.iter_mut().zip(table_space.iter()).enumerate() {
            // A table none of whose rows names an absorbed id reads the same.
            if over == space && unclean & holder(slot as u32) != 0 {
                let offer = offer
                    .as_mut()
                    .map(|offer| offer as &mut dyn FnMut(RowView<'_>) -> bool);
                table.repair(&rename, offer, *epoch, recent, report);
            }
        }
        self.stats.repairs += report.rekeyed.len();
        for &id in &report.rekeyed {
            self.log_change(space, id);
        }
        true
    }

    fn canonicalize_layer(
        &mut self,
        space: SpaceId,
        mut dissolve: Option<Dissolve<'_, C>>,
        report: &mut Repair,
        holders: &Csr,
    ) -> bool {
        let at = space.0 as usize;
        if !std::mem::take(&mut self.state.spaces[at].dirty) {
            return false;
        }
        let depth = self.layers.len();
        // Point every entry of the overlay straight at its representative.
        let flat: SmallVec<[(u32, Ref); 8]> = self.layers[depth - 1].overlay[at]
            .parent
            .keys()
            .map(|&id| (id, self.find(space, id)))
            .collect();
        let overlay = &mut self.layers[depth - 1].overlay[at];
        for (id, found) in flat {
            overlay.parent.insert(id, (found.id, found.offset));
        }
        let pending = std::mem::take(&mut overlay.pending);
        self.version += 1;
        self.stats.rebuilds += 1;
        report.rekeyed.clear();
        report.collisions.clear();
        let epoch = self.state.epoch;
        // The own rows, through the overlay.
        let mut own = std::mem::take(&mut self.layers[depth - 1].own);
        let placed = self.placement(space);
        let client = &self.state.client;
        let rename = Composed {
            db: self,
            space,
            depth,
        };
        let mut offer = dissolve
            .as_mut()
            .map(|dissolve| |row: RowView<'_>| dissolve(&placed, client, row));
        let mut rekeyed: Vec<(u32, u64)> = Vec::new();
        let mut local = Repair::default();
        for (slot, table) in own.iter_mut().enumerate() {
            let Some(table) = table else {
                continue;
            };
            if self.state.table_space[slot] != space {
                continue;
            }
            local.rekeyed.clear();
            local.collisions.clear();
            let offer = offer
                .as_mut()
                .map(|offer| offer as &mut dyn FnMut(RowView<'_>) -> bool);
            let before = table.version();
            if table.repair(&rename, offer, epoch, false, &mut local) {
                rekeyed.push((slot as u32, before));
            }
            report.rekeyed.extend_from_slice(&local.rekeyed);
            report.collisions.extend_from_slice(&local.collisions);
        }
        self.layers[depth - 1].own = own;
        // An own row re-keyed now may read as a lower row that is not stale.
        for (slot, before) in rekeyed {
            self.collide_with_lower(space, slot, before, epoch, report);
        }
        // A row of the base or a lower layer that names an id the layer just
        // absorbed is stale from here on: its copy, re-keyed, joins the own
        // rows.
        let mut copies: Vec<(u32, u32, u32)> = Vec::new();
        self.copies(space, &pending, holders, &mut copies);
        for &(slot, side, row) in &copies {
            let copy = self.rekeyed(space, slot, side, row, &mut dissolve);
            self.copy_row(copy, epoch, report);
        }
        self.stats.repairs += report.rekeyed.len();
        let layer = self.layers.last_mut().expect("an open layer");
        layer.touched[at].extend_from_slice(&report.rekeyed);
        for &id in &report.rekeyed {
            self.log_change(space, id);
        }
        true
    }

    // Marks each row stale under the current layer.
    fn copies(
        &mut self,
        space: SpaceId,
        absorbed: &[u32],
        holders: &Csr,
        out: &mut Vec<(u32, u32, u32)>,
    ) {
        if absorbed.is_empty() {
            return;
        }
        let depth = self.layers.len();
        let mut found: Vec<(u32, u32, u32)> = Vec::new();
        {
            for &id in absorbed {
                found.extend(holders.get(id).iter().map(|&packed| {
                    let (slot, row) = unpack(packed);
                    (slot, 0, row)
                }));
            }
        }
        for side in 1..depth {
            for (slot, own) in self.layers[side - 1].own.iter().enumerate() {
                let Some(own) = own else {
                    continue;
                };
                if self.state.table_space[slot] != space {
                    continue;
                }
                for column in own.plain_columns()..=own.arity() {
                    let cells = own.column(column);
                    found.extend(
                        (0..own.len() as u32)
                            .filter(|&row| absorbed.contains(&cells[row as usize]))
                            .map(|row| (slot as u32, side as u32, row)),
                    );
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        for &(slot, side, row) in &found {
            // A row stale already has its copy, under this layer or a lower one.
            if self.stale_above(slot, side as usize, row, depth) {
                continue;
            }
            self.mark_stale(slot, side as usize, row);
            out.push((slot, side, row));
        }
    }

    fn mark_stale(&mut self, slot: u32, side: usize, row: u32) {
        let layer = self.layers.last_mut().expect("an open layer");
        if layer.stale.len() <= side {
            layer.stale.resize_with(side + 1, Vec::new);
        }
        let slots = &mut layer.stale[side];
        if slots.len() <= slot as usize {
            slots.resize_with(slot as usize + 1, Vec::new);
        }
        let bits = &mut slots[slot as usize];
        let word = row as usize / 64;
        if bits.len() <= word {
            bits.resize(word + 1, 0);
        }
        bits[word] |= 1 << (row % 64);
    }

    fn rekeyed(
        &self,
        space: SpaceId,
        slot: u32,
        side: u32,
        row: u32,
        dissolve: &mut Option<Dissolve<'_, C>>,
    ) -> Copied {
        let depth = self.layers.len();
        let table = self.side_table(slot, side as usize).expect("a side");
        let arity = table.arity();
        let plain = table.plain_columns();
        let mut key: SmallVec<[u32; 8]> = SmallVec::new();
        let mut weights: SmallVec<[u64; 8]> = SmallVec::new();
        let (mut value, mut value_weight) =
            (table.values()[row as usize], table.weight(arity, row));
        let found = self.find(space, Ref::new(value, value_weight));
        (value, value_weight) = (found.id, found.offset);
        let mut weighted = value_weight != 0;
        let mut key_moved = false;
        for column in 0..arity {
            let cell = table.column(column)[row as usize];
            let weight = table.weight(column, row);
            if column < plain {
                key.push(cell);
                weights.push(weight);
                continue;
            }
            let found = self.find(space, Ref::new(cell, 0));
            key_moved |= found.id != cell;
            key.push(found.id);
            match table.coefficient(column) {
                0 => {
                    let mask = self.mask_below(space, found.id, depth);
                    let weight = weight.wrapping_add(found.offset) & mask;
                    weights.push(weight);
                    weighted |= weight != 0;
                }
                coefficient => {
                    // op(.., to + delta, ..) == value, so op(.., to, ..) is
                    // value minus coefficient times delta.
                    let moved = (coefficient as u64).wrapping_mul(found.offset);
                    let mask = self.mask_below(space, value, depth);
                    value_weight = value_weight.wrapping_sub(moved) & mask;
                    weighted |= value_weight != 0;
                    weights.push(0);
                }
            }
        }
        table.order(&mut key, &mut weights);
        let tag = table.tags()[row as usize];
        let settled = dissolve.as_mut().is_some_and(|dissolve| {
            let view = RowView {
                cells: &key,
                weights: &weights,
                value: (value, value_weight),
                tag,
            };
            dissolve(&self.placement(space), &self.state.client, view)
        });
        Copied {
            slot,
            key,
            weights,
            weighted,
            value: (value, value_weight),
            tag,
            key_moved,
            settled,
        }
    }

    fn copy_row(&mut self, copy: Copied, epoch: u32, report: &mut Repair) {
        let Copied {
            slot,
            key,
            weights,
            weighted,
            value,
            tag,
            key_moved,
            settled,
        } = copy;
        // A row only whose value moved reads the same, as the base repair
        // has it: re-stamped, not re-keyed.
        if key_moved {
            report.rekeyed.push(value.0);
        }
        if settled {
            return;
        }
        let weights_or_none: &[u64] = if weighted { &weights } else { &[] };
        if let Some(kept) = self.lookup(slot, &key, weights_or_none) {
            report.collisions.push(Collision {
                kept: (kept.id, kept.offset),
                removed: value,
                tag,
            });
            return;
        }
        self.own_table(slot)
            .insert(&key, weights_or_none, value, tag, epoch);
        self.version += 1;
    }

    fn own_table(&mut self, slot: u32) -> &mut Table {
        let depth = self.layers.len();
        let base = &self.state.tables[slot as usize];
        let spare = &mut self.spare;
        let layer = &mut self.layers[depth - 1];
        if layer.own.len() <= slot as usize {
            layer.own.resize_with(slot as usize + 1, || None);
        }
        layer.own[slot as usize].get_or_insert_with(|| {
            spare
                .get_mut(slot as usize)
                .and_then(Vec::pop)
                .unwrap_or_else(|| base.like())
        })
    }

    fn collide_with_lower(
        &mut self,
        space: SpaceId,
        slot: u32,
        before: u64,
        epoch: u32,
        report: &mut Repair,
    ) {
        let depth = self.layers.len();
        let own = self.layers[depth - 1].own[slot as usize]
            .take()
            .expect("an own table");
        let arity = own.arity();
        let mut key: SmallVec<[u32; 8]> = SmallVec::new();
        let mut weights: SmallVec<[u64; 8]> = SmallVec::new();
        let moved: SmallVec<[u32; 16]> = match own.moved_since(before) {
            Some((rows, _)) => {
                let mut rows: SmallVec<[u32; 16]> = rows.iter().copied().collect();
                rows.sort_unstable();
                rows.dedup();
                rows.retain(|&mut row| (row as usize) < own.len());
                rows
            }
            None => (0..own.len() as u32)
                .filter(|&row| own.stamps()[row as usize] == epoch)
                .collect(),
        };
        for row in moved {
            key.clear();
            weights.clear();
            for column in 0..arity {
                key.push(own.column(column)[row as usize]);
                weights.push(own.weight(column, row));
            }
            let weights_or_none: &[u64] = if own.weighted() { &weights } else { &[] };
            let Some(kept) = self.lookup_below(space, slot, depth - 1, &key, weights_or_none)
            else {
                continue;
            };
            report.collisions.push(Collision {
                kept: (kept.id, kept.offset),
                removed: (own.values()[row as usize], own.weight(arity, row)),
                tag: own.tags()[row as usize],
            });
        }
        self.layers[depth - 1].own[slot as usize] = Some(own);
    }

    // ---- tables -----------------------------------------------------------

    /// The base's tables.
    #[inline]
    pub fn tables(&self) -> &[Table] {
        &self.state.tables
    }

    /// The base's table at `slot`.
    #[inline]
    pub fn table(&self, slot: u32) -> Option<&Table> {
        self.state.tables.get(slot as usize)
    }

    // The base's side first, then each open layer's.
    pub(crate) fn sides(&self, slot: u32) -> impl Iterator<Item = (usize, &Table)> {
        let base = self.table(slot).map(|table| (0, table));
        let layers = self
            .layers
            .iter()
            .enumerate()
            .filter_map(move |(at, layer)| {
                let own = layer.own.get(slot as usize)?.as_ref()?;
                Some((at + 1, own))
            });
        base.into_iter().chain(layers)
    }

    fn side_table(&self, slot: u32, side: usize) -> Option<&Table> {
        match side {
            0 => self.table(slot),
            layer => self.layers[layer - 1].own.get(slot as usize)?.as_ref(),
        }
    }

    // Stale as of that layer's last canonicalization.
    #[inline]
    pub(crate) fn stale(&self, slot: u32, side: usize, row: u32) -> bool {
        self.stale_above(slot, side, row, self.layers.len())
    }

    #[inline]
    fn stale_above(&self, slot: u32, side: usize, row: u32, depth: usize) -> bool {
        let (word, bit) = (row as usize / 64, row % 64);
        self.layers[side..depth].iter().any(|layer| {
            layer
                .stale
                .get(side)
                .and_then(|slots| slots.get(slot as usize))
                .and_then(|bits| bits.get(word))
                .is_some_and(|bits| bits >> bit & 1 == 1)
        })
    }

    /// Whether some layer is open.
    #[inline]
    pub fn layered(&self) -> bool {
        !self.layers.is_empty()
    }

    // A layer's own rows are not recorded, so with one open every table may.
    #[inline]
    pub(crate) fn naming(&self, space: SpaceId, id: u32) -> u64 {
        let holding = &self.state.spaces[space.0 as usize].holding;
        match self.layered() {
            true => u64::MAX,
            false => holding.get(id as usize).copied().unwrap_or(0),
        }
    }

    /// The reference of the row holding `key` at `weights` in the table at
    /// `slot`, where a read of the current layer sees one.
    pub fn lookup(&self, slot: u32, key: &[u32], weights: &[u64]) -> Option<Ref> {
        self.table(slot)?;
        self.lookup_below(
            self.state.table_space[slot as usize],
            slot,
            self.layers.len(),
            key,
            weights,
        )
    }

    fn lookup_below(
        &self,
        space: SpaceId,
        slot: u32,
        depth: usize,
        key: &[u32],
        weights: &[u64],
    ) -> Option<Ref> {
        for side in (1..=depth).rev() {
            let Some(own) = self.side_table(slot, side) else {
                continue;
            };
            if let Some(row) = own.get(key, weights)
                && !self.stale_above(slot, side, row, depth)
            {
                return Some(self.value_of(space, own, row));
            }
        }
        let table = self.table(slot)?;
        let row = table.get(key, weights)?;
        if self.stale_above(slot, 0, row, depth) {
            return None;
        }
        Some(self.value_of(space, table, row))
    }

    #[inline]
    fn value_of(&self, space: SpaceId, table: &Table, row: u32) -> Ref {
        let arity = table.arity();
        let value = Ref::new(table.values()[row as usize], table.weight(arity, row));
        self.find(space, value)
    }

    /// Insert a row into the table at `slot`, stamped with the current epoch,
    /// among the current layer's own rows; see [`Table::insert`].
    #[inline]
    pub fn insert(
        &mut self,
        slot: u32,
        key: &[u32],
        weights: &[u64],
        value: (u32, u64),
        tag: u32,
    ) -> u32 {
        let epoch = self.state.epoch;
        self.version += 1;
        if self.layers.is_empty() {
            self.based = self.version;
            let table = &mut self.state.tables[slot as usize];
            let space = &mut self.state.spaces[self.state.table_space[slot as usize].0 as usize];
            let named = key[table.plain_columns()..].iter().chain([&value.0]);
            space.hold(slot, named.copied());
            return table.insert(key, weights, value, tag, epoch);
        }
        self.own_table(slot).insert(key, weights, value, tag, epoch)
    }

    /// Look a batch of keys up in the table at `slot` as the current layer
    /// reads it and store the ones no row holds among the layer's own rows,
    /// stamped with the current epoch; see [`Table::probe_or_insert`].
    /// `found[index]` is the value of the row holding key `index`, as stored,
    /// or `None` for a key stored.
    pub fn insert_rows(
        &mut self,
        slot: u32,
        keys: &[&[u32]],
        weights: &[&[u64]],
        values: Values<'_>,
        tag: u32,
        found: &mut Vec<Option<Ref>>,
    ) {
        let epoch = self.state.epoch;
        let depth = self.layers.len();
        let mut rows = std::mem::take(&mut self.probed);
        self.version += 1;
        self.stats.batches += 1;
        self.stats.batched += keys.first().map_or(0, |column| column.len());
        found.clear();
        if depth == 0 {
            self.based = self.version;
            let table = &mut self.state.tables[slot as usize];
            let before = table.len();
            table.probe_or_insert(keys, weights, values, tag, epoch, &mut rows);
            found.extend(rows.iter().map(|&row| held(table, row)));
            let space = &mut self.state.spaces[self.state.table_space[slot as usize].0 as usize];
            for column in table.plain_columns()..=table.arity() {
                space.hold(slot, table.column(column)[before..].iter().copied());
            }
            self.probed = rows;
            return;
        }
        // A key a lower side holds in a row that is not stale is settled
        // there; the rest go to the own rows together.
        found.resize(keys.first().map_or(0, |column| column.len()), None);
        for side in 0..depth {
            let Some(table) = self.side_table(slot, side) else {
                continue;
            };
            table.probe(keys, weights, &mut rows);
            for (found, &row) in found.iter_mut().zip(&rows) {
                if found.is_none() && row != u32::MAX && !self.stale_above(slot, side, row, depth) {
                    *found = held(table, row);
                }
            }
        }
        let open: Vec<u32> = (0..found.len() as u32)
            .filter(|&index| found[index as usize].is_none())
            .collect();
        let pick = |column: &[u32]| -> Vec<u32> {
            open.iter().map(|&index| column[index as usize]).collect()
        };
        let own_keys: Vec<Vec<u32>> = keys.iter().map(|column| pick(column)).collect();
        let own_weights: Vec<Vec<u64>> = weights
            .iter()
            .map(|column| open.iter().map(|&index| column[index as usize]).collect())
            .collect();
        let own_values = match values {
            Values::Each(values) => pick(values),
            Values::From(_) => Vec::new(),
        };
        let values = match values {
            Values::Each(_) => Values::Each(&own_values),
            from => from,
        };
        let own_keys: SmallVec<[&[u32]; 8]> = own_keys.iter().map(Vec::as_slice).collect();
        let own_weights: SmallVec<[&[u64]; 8]> = own_weights.iter().map(Vec::as_slice).collect();
        let own = self.own_table(slot);
        own.probe_or_insert(&own_keys, &own_weights, values, tag, epoch, &mut rows);
        for (&index, &row) in open.iter().zip(&rows) {
            found[index as usize] = held(own, row);
        }
        self.probed = rows;
    }

    /// Give `row` of the base table at `slot` the tag `tag`.
    pub fn set_tag(&mut self, slot: u32, row: u32, tag: u32) {
        self.state.tables[slot as usize].set_tag(row, tag);
    }

    /// Whether the epoch before wrote a row of the table at `slot`, the
    /// base's or a layer's own: the tables the epoch's log names.
    pub fn wrote(&self, slot: u32) -> bool {
        let epoch = self.state.epoch - 1;
        self.sides(slot).any(|(_, table)| table.stamped() >= epoch)
    }

    // With a layer open, only until the layer's first round closes.
    #[inline]
    pub(crate) fn base_new(&self) -> bool {
        self.layers
            .first()
            .is_none_or(|layer| self.state.epoch - 1 <= layer.epoch)
    }

    #[inline]
    pub(crate) fn side_new(&self, side: usize) -> bool {
        match side {
            0 => self.base_new(),
            side => self
                .layers
                .get(side)
                .is_none_or(|above| self.state.epoch - 1 <= above.epoch),
        }
    }

    // ---- client state -----------------------------------------------------

    /// The client's own state.
    #[inline]
    pub fn client(&self) -> &C {
        &self.state.client
    }

    #[inline]
    pub fn client_mut(&mut self) -> &mut C {
        &mut self.state.client
    }

    // ---- lattice columns --------------------------------------------------

    #[inline]
    pub fn lattice(&self, lattice: LatticeId) -> &Column<u64> {
        &self.lattices[lattice.0 as usize].column
    }

    /// Join `value` onto what canonical `key` is known to be; see
    /// [`Column::raise`]. A rise is logged against the key.
    #[inline]
    pub fn raise(&mut self, lattice: LatticeId, key: u32, value: u64) -> bool {
        self.write(lattice, key, Fact::Known(value), false, true)
    }

    /// [`Self::raise`] for the value `key` is minted with, which
    /// [`Self::risen`] keeps apart.
    #[inline]
    pub fn seed(&mut self, lattice: LatticeId, key: u32, value: u64) -> bool {
        self.write(lattice, key, Fact::Known(value), true, true)
    }

    /// Join `fact` onto `key`'s entry.
    fn write(
        &mut self,
        lattice: LatticeId,
        key: u32,
        fact: Fact<u64>,
        minted: bool,
        counted: bool,
    ) -> bool {
        let epoch = self.state.epoch;
        let moved = self.lattices[lattice.0 as usize]
            .column
            .write(key, fact, epoch);
        if moved {
            self.log_rise(lattice, key, minted);
            self.stats.raises += usize::from(counted);
        }
        moved
    }

    /// Join `absorbed`'s entry onto `survivor`'s, for ids just equated.
    fn merge(&mut self, lattice: LatticeId, absorbed: u32, survivor: u32) {
        let entry = &mut self.lattices[lattice.0 as usize];
        if let Some(fact) = entry.column.detach(absorbed) {
            self.write(lattice, survivor, fact, false, false);
        }
    }

    /// What `key` is known to be in `lattice`. `None` for unknown and for a
    /// conflict.
    pub fn read(&self, lattice: LatticeId, key: u32) -> Option<u64> {
        let entry = &self.lattices[lattice.0 as usize];
        entry.column.get(self.root(entry.space, key))
    }

    /// Whether `key`'s entry in `lattice` rose during the epoch the last
    /// [`Self::advance`] closed.
    pub fn is_new(&self, lattice: LatticeId, key: u32) -> bool {
        let entry = &self.lattices[lattice.0 as usize];
        entry
            .column
            .is_new(self.root(entry.space, key), self.state.epoch)
    }

    /// The keys whose value rose during the epoch the last [`Self::advance`]
    /// closed, canonical and ascending: on an id that already existed, or with
    /// `minted` on one minted with it.
    #[inline]
    pub fn risen(&self, lattice: LatticeId, minted: bool) -> &[u32] {
        &self.state.rises[lattice.0 as usize].risen[usize::from(minted)]
    }

    /// The lattice columns [`Self::risen`] is not empty for, as a mask over
    /// their ids.
    pub fn rose(&self, minted: bool) -> u64 {
        let rises = self.state.rises.iter().enumerate();
        rises
            .filter(|(_, rises)| !rises.risen[usize::from(minted)].is_empty())
            .fold(0, |mask, (at, _)| mask | 1 << at)
    }

    /// Log that `key`'s value in `lattice` moved.
    fn log_rise(&mut self, lattice: LatticeId, key: u32, minted: bool) {
        self.state.rises[lattice.0 as usize].rising[usize::from(minted)].push(key);
        self.log_change(self.lattices[lattice.0 as usize].space, key);
    }

    // ---- epochs and the change log ----------------------------------------

    /// The stamp of the rows written now; a row stamped one below it was
    /// written in the epoch before.
    #[inline]
    pub fn epoch(&self) -> u32 {
        self.state.epoch
    }

    /// Close the epoch: what it wrote becomes what [`Self::prepare_new_rows`]
    /// and [`Self::risen`] read.
    pub fn advance(&mut self) {
        self.state.epoch += 1;
        for space in &mut self.state.spaces {
            // What is still to be canonicalized names ids of the epoch that
            // just closed.
            space.aged |= space.unclean != 0;
            space.fresh = space.uf.len() as u32;
        }
        for table in &mut self.state.tables {
            table.mark();
        }
        for lattice in 0..self.lattices.len() {
            let space = self.lattices[lattice].space;
            for list in 0..2 {
                let mut risen = std::mem::take(&mut self.state.rises[lattice].rising[list]);
                for key in &mut risen {
                    *key = self.root(space, *key);
                }
                risen.sort_unstable();
                risen.dedup();
                // The drained list's allocation becomes the next epoch's.
                let rises = &mut self.state.rises[lattice];
                std::mem::swap(&mut rises.risen[list], &mut risen);
                risen.clear();
                rises.rising[list] = risen;
            }
        }
    }

    /// Drain `space`'s change log: the canonical ids changed since the previous
    /// call, ascending; `None` means "every id".
    pub fn take_changed(&mut self, space: SpaceId) -> Option<Vec<u32>> {
        let state = &mut self.state.spaces[space.0 as usize];
        let all = std::mem::replace(&mut state.changed_all, false);
        let mut changed = std::mem::take(&mut state.changed);
        // Every mark of the drained log goes stale at once.
        state.changed_epoch += 1;
        if all {
            return None;
        }
        for id in &mut changed {
            *id = self.root(space, *id);
        }
        changed.sort_unstable();
        changed.dedup();
        Some(changed)
    }

    /// Report "everything" from the next [`Self::take_changed`] of `space`.
    pub fn mark_all_changed(&mut self, space: SpaceId) {
        self.state.spaces[space.0 as usize].changed_all = true;
    }

    #[inline]
    fn log_change(&mut self, space: SpaceId, id: u32) {
        let state = &mut self.state.spaces[space.0 as usize];
        let at = id as usize;
        if state.changed_at.len() <= at {
            // Ids are minted one at a time; growing by one each time would
            // make every mint a resize.
            let len = (at + 1).max(state.changed_at.len() * 2);
            state.changed_at.resize(len, 0);
        }
        if state.changed_at[at] != state.changed_epoch {
            state.changed_at[at] = state.changed_epoch;
            state.changed.push(id);
        }
    }

    // ---- reading upward ---------------------------------------------------

    fn own_edges(&self, space: SpaceId) -> Vec<(u32, u32)> {
        let mut edges = Vec::new();
        for layer in &self.layers {
            for (slot, own) in layer.own.iter().enumerate() {
                let Some(own) = own else {
                    continue;
                };
                if self.state.table_space[slot] != space {
                    continue;
                }
                for column in own.plain_columns()..own.arity() {
                    edges.extend(
                        own.column(column)
                            .iter()
                            .zip(own.values())
                            .map(|(&c, &p)| (c, p)),
                    );
                }
            }
        }
        edges
    }

    /// How many layers are open.
    #[inline]
    pub fn depth(&self) -> usize {
        self.layers.len()
    }

    /// The ids of `space` minted, equated or re-keyed under the open layers
    /// from the `depth`th inward, in no particular order and possibly
    /// repeated.
    pub fn layer_touched(&self, space: SpaceId, depth: usize) -> impl Iterator<Item = u32> + '_ {
        self.layers[depth..]
            .iter()
            .flat_map(move |layer| &layer.touched[space.0 as usize])
            .copied()
    }

    /// Work done since the database was made.
    #[inline]
    pub fn stats(&self) -> Stats {
        self.stats
    }

    // ---- layers -----------------------------------------------------------

    /// Open a layer: everything until the matching [`Self::pop_layer`] is
    /// undone by it. The base is canonical by then: a layer places its
    /// representatives.
    pub fn push_layer(&mut self) {
        debug_assert!(
            (0..SPACES).all(|at| !self.state.spaces[at].dirty),
            "a layer opens on a canonical base"
        );
        let saved = std::array::from_fn(|at| {
            let state = &mut self.state.spaces[at];
            Saved {
                live: state.live,
                dirty: state.dirty,
                contradicted: state.contradicted,
                changed: state.changed.clone(),
                changed_all: state.changed_all,
            }
        });
        self.layers.push(Layer {
            overlay: std::array::from_fn(|_| Overlay::default()),
            own: Vec::new(),
            saved,
            rises: self.state.rises.clone(),
            epoch: self.state.epoch,
            ids: std::array::from_fn(|at| self.state.spaces[at].uf.len()),
            touched: vec![Vec::new(); SPACES],
            stale: Vec::new(),
        });
        for Lattice { column, .. } in &mut self.lattices {
            column.push_scope();
        }
    }

    /// Close the layer, dropping its rows and equalities and putting back the
    /// counters, lattice columns and change log it found. Ids minted under it
    /// stay minted, each in a set of its own, so a caller still holding one
    /// can ask about it.
    pub fn pop_layer(&mut self) {
        let layer = self.layers.pop().expect("an open layer");
        for Lattice { column, .. } in &mut self.lattices {
            column.pop_scope();
        }
        for (at, saved) in layer.saved.into_iter().enumerate() {
            let state = &mut self.state.spaces[at];
            state.live = saved.live;
            state.dirty = saved.dirty;
            state.contradicted = saved.contradicted;
            state.changed = saved.changed;
            state.changed_all = saved.changed_all;
            // Every mark of the layer's log goes stale at once, and the log
            // put back marks its own ids again.
            state.changed_epoch += 1;
            for at in 0..state.changed.len() {
                let id = state.changed[at] as usize;
                state.changed_at[id] = state.changed_epoch;
            }
        }
        self.state.rises = layer.rises;
        self.state.epoch = layer.epoch;
        if self.spare.len() < layer.own.len() {
            self.spare.resize_with(layer.own.len(), Vec::new);
        }
        for (slot, own) in layer.own.into_iter().enumerate() {
            if let Some(mut own) = own {
                own.clear();
                self.spare[slot].push(own);
            }
        }
        self.version += 1;
    }
}

impl<C, const SPACES: usize> Database<C, SPACES> {
    /// Rewrite every table of `space` through its partition, carrying the
    /// offsets the equates put ids at, if anything was equated since the last
    /// call; reports whether it was. Under a layer only the layer's own rows
    /// are rewritten, and a lower row that holds an id the layer absorbed gets
    /// a re-keyed copy among them.
    ///
    /// `report` is cleared, then receives the value of every re-keyed row,
    /// already logged as changed, and every pair of rows that came to share a
    /// key, the later one removed. Equating those pairs' values is how a client
    /// restores a functional dependency, and makes the space need this again.
    /// A re-keyed row `dissolve` answers true for is removed instead.
    pub fn canonicalize(
        &mut self,
        space: SpaceId,
        dissolve: Option<Dissolve<'_, C>>,
        report: &mut Repair,
    ) -> bool {
        let at = space.0 as usize;
        let Some(layer) = self.core.layers.last() else {
            return self.core.canonicalize_base(space, dissolve, report);
        };
        // Only a layer that absorbed an id reads the base's rows by id.
        if !self.core.state.spaces[at].dirty || layer.overlay[at].pending.is_empty() {
            let none = Csr::default();
            return self.core.canonicalize_layer(space, dissolve, report, &none);
        }
        let holders = Self::holders(&self.holders, &self.core, space);
        self.core
            .canonicalize_layer(space, dissolve, report, &holders)
    }

    /// Run `visit` on every row of the tables at `slot` holding `cell` in
    /// `column`, the base's rows that are not stale and then each layer's own.
    pub fn each_row(
        &self,
        slot: u32,
        column: usize,
        cell: u32,
        mut visit: impl FnMut(&Table, u32),
    ) {
        self.prepare_group(slot, column);
        let groups = self.groups.borrow();
        for (side, table) in self.sides(slot) {
            if side == 0 {
                for &row in groups.get(slot, column).rows(cell) {
                    if !self.stale(slot, 0, row) {
                        visit(table, row);
                    }
                }
                continue;
            }
            let cells = table.column(column);
            for row in 0..table.len() as u32 {
                if cells[row as usize] == cell && !self.stale(slot, side, row) {
                    visit(table, row);
                }
            }
        }
    }

    /// Group the rows of the base's table at `slot` by `column`, or bring the
    /// grouping up to date. Read through [`Self::groups`].
    pub fn prepare_group(&self, slot: u32, column: usize) {
        let Some(table) = self.table(slot) else {
            return;
        };
        let mut groups = self.groups.borrow_mut();
        if groups.built.len() <= slot as usize {
            groups.built.resize_with(slot as usize + 1, Vec::new);
        }
        let columns = &mut groups.built[slot as usize];
        if columns.len() <= column {
            columns.resize_with(column + 1, Default::default);
        }
        let ids = self.ids(self.state.table_space[slot as usize]);
        columns[column].refresh(table, column, ids);
    }

    /// Order the rows of the base's table at `slot` by `column` and then by
    /// tag, or bring the order up to date. Read through [`Self::groups`].
    pub fn prepare_tag_order(&self, slot: u32, column: usize) {
        let Some(table) = self.table(slot) else {
            return;
        };
        let mut groups = self.groups.borrow_mut();
        if groups.tag_orders.len() <= slot as usize {
            groups.tag_orders.resize_with(slot as usize + 1, Vec::new);
        }
        let columns = &mut groups.tag_orders[slot as usize];
        if columns.len() <= column {
            columns.resize_with(column + 1, Default::default);
        }
        columns[column].refresh(table, column);
    }

    /// List the rows of the base's table at `slot` the epoch before wrote, by
    /// their cell in `column`, unless that is already done for the table as
    /// it is. Read through [`Self::groups`].
    pub fn prepare_new_rows(&self, slot: u32, column: usize) {
        let Some(table) = self.table(slot) else {
            return;
        };
        let mut groups = self.groups.borrow_mut();
        let groups = &mut *groups;
        if groups.new_rows.len() <= slot as usize {
            groups.new_rows.resize_with(slot as usize + 1, Vec::new);
        }
        let columns = &mut groups.new_rows[slot as usize];
        if columns.len() <= column {
            columns.resize_with(column + 1, Default::default);
        }
        let NewRows { made, rows } = &mut columns[column];
        let now = (table.version() + 1, self.state.epoch);
        if *made == now {
            return;
        }
        *made = now;
        rows.clear();
        if !self.base_new() || table.stamped() + 1 < self.state.epoch {
            return;
        }
        groups.scratch.clear();
        tir_adt::simd::select_eq(table.stamps(), self.state.epoch - 1, &mut groups.scratch);
        let cells = table.column(column);
        rows.extend(groups.scratch.iter().map(|&row| (cells[row as usize], row)));
        rows.sort_unstable();
    }

    /// Run `visit` on the cell in `column` of each row at `slot` the epoch
    /// before wrote, the base's and then each layer's, sorted within each.
    pub fn each_new_cell(&self, slot: u32, column: usize, mut visit: impl FnMut(u32)) {
        self.prepare_new_rows(slot, column);
        for &(cell, _) in self.groups.borrow().new_rows(slot, column) {
            visit(cell);
        }
        let epoch = self.state.epoch - 1;
        for (side, table) in self.sides(slot) {
            if side == 0 || !self.side_new(side) || table.stamped() < epoch {
                continue;
            }
            let mut cells: Vec<u32> = (0..table.len() as u32)
                .filter(|&row| table.stamps()[row as usize] == epoch)
                .map(|row| table.column(column)[row as usize])
                .collect();
            cells.sort_unstable();
            cells.into_iter().for_each(&mut visit);
        }
    }

    /// Index the rows of the base's table at `slot` as a trie over `columns`,
    /// or bring the trie up to date. Read through [`Self::groups`].
    pub fn prepare_trie(&self, slot: u32, columns: &[u8]) {
        debug_assert!(self.layers.is_empty(), "a trie holds the base's rows alone");
        let Some(table) = self.table(slot) else {
            return;
        };
        let mut groups = self.groups.borrow_mut();
        if groups.tries.len() <= slot as usize {
            groups.tries.resize_with(slot as usize + 1, Vec::new);
        }
        let tries = &mut groups.tries[slot as usize];
        let at = match tries.iter().position(|trie| trie.columns() == columns) {
            Some(at) => at,
            None => {
                tries.push(Trie::new(columns));
                tries.len() - 1
            }
        };
        tries[at].refresh(table);
    }

    #[inline]
    pub fn groups(&self) -> cell::Ref<'_, Groups> {
        self.groups.borrow()
    }

    /// Fresh marks over the ids of `space`.
    #[inline]
    pub fn marks(&self, space: SpaceId) -> RefMut<'_, Marks> {
        let mut marks = self.marks.borrow_mut();
        marks.begin(self.ids(space));
        marks
    }

    /// `seeds`, canonicalized, and every id reachable from them in at most
    /// `levels` steps (unbounded when `None`), ascending. A step goes from an
    /// id to the value of every row of the space's tables holding it in a key
    /// column, and never from a pinned id.
    pub fn close_upward(
        &self,
        space: SpaceId,
        seeds: impl IntoIterator<Item = u32>,
        levels: Option<usize>,
    ) -> Vec<u32> {
        let reverse = self.reverse(space);
        let own = self.own_edges(space);
        // A layer's representative stands for the ids the layer absorbed
        // since its last canonicalization, whose lower rows are its own until
        // that copies them.
        let mut absorbed_into: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for layer in &self.layers {
            for &id in &layer.overlay[space.0 as usize].pending {
                absorbed_into
                    .entry(self.root(space, id))
                    .or_default()
                    .push(id);
            }
        }
        let mut seen = self.marks(space);
        let mut frontier: Vec<u32> = seeds
            .into_iter()
            .map(|id| self.root(space, id))
            .filter(|&id| seen.insert(id as usize))
            .collect();
        let mut closure = frontier.clone();
        let mut level = 0;
        while !frontier.is_empty() && levels.is_none_or(|max| level < max) {
            let mut next = Vec::new();
            for id in frontier.drain(..) {
                // A zero's users are every row with that carrier's constant
                // in it. An id equated with it reaches its own users through
                // the rows the equate re-keyed, which are logged themselves.
                if self.is_zero(space, id) {
                    continue;
                }
                let members = absorbed_into.get(&id).map_or(&[][..], Vec::as_slice);
                let base = std::iter::once(&id)
                    .chain(members)
                    .flat_map(|&member| reverse.get(member).iter().copied());
                let layered = own
                    .iter()
                    .filter(|(child, _)| *child == id)
                    .map(|&(_, parent)| parent);
                for parent in base.chain(layered) {
                    let parent = self.root(space, parent);
                    if seen.insert(parent as usize) {
                        closure.push(parent);
                        next.push(parent);
                    }
                }
            }
            frontier = next;
            level += 1;
        }
        closure.sort_unstable();
        closure
    }

    /// Id -> the values of the rows of the base holding it in a key column,
    /// canonical in the base, built once per base version. The tables are
    /// keyed the other way, so an upward walk reads this instead.
    fn reverse(&self, space: SpaceId) -> cell::Ref<'_, Csr> {
        let at = space.0 as usize;
        let stale = self
            .reverse
            .borrow()
            .get(at)
            .is_none_or(|(built, _)| *built != self.based);
        if stale {
            let uf = &self.state.spaces[at].uf;
            // A canonical table already names representatives.
            let canonical = self.is_canonical(space) || self.layered();
            let cell = |cell: u32| match canonical {
                true => cell,
                false => uf.root(cell),
            };
            let csr = Csr::build_with(uf.len(), |edge| {
                let tables = self.state.tables.iter().zip(&self.state.table_space);
                for (table, _) in tables.filter(|(_, over)| **over == space) {
                    for column in table.plain_columns()..table.arity() {
                        for (&child, &parent) in table.column(column).iter().zip(table.values()) {
                            edge(cell(child), cell(parent));
                        }
                    }
                }
            });
            let mut reverse = self.reverse.borrow_mut();
            if reverse.len() <= at {
                reverse.resize_with(at + 1, Default::default);
            }
            reverse[at] = (self.based, csr);
        }
        cell::Ref::map(self.reverse.borrow(), |reverse| &reverse[at].1)
    }

    fn holders<'a>(
        holders: &'a RefCell<Vec<(u64, Csr)>>,
        core: &Core<C, SPACES>,
        space: SpaceId,
    ) -> cell::Ref<'a, Csr> {
        let at = space.0 as usize;
        let stale = holders
            .borrow()
            .get(at)
            .is_none_or(|(built, _)| *built != core.based);
        if stale {
            let ids = core.state.spaces[at].uf.len();
            let csr = Csr::build_with(ids, |edge| {
                let tables = core.state.tables.iter().zip(&core.state.table_space);
                for (slot, (table, _)) in
                    tables.enumerate().filter(|(_, (_, over))| **over == space)
                {
                    for column in table.plain_columns()..=table.arity() {
                        for (row, &cell) in table.column(column).iter().enumerate() {
                            edge(cell, pack(slot as u32, row as u32));
                        }
                    }
                }
            });
            let mut holders = holders.borrow_mut();
            if holders.len() <= at {
                holders.resize_with(at + 1, Default::default);
            }
            holders[at] = (core.based, csr);
        }
        cell::Ref::map(holders.borrow(), |holders| &holders[at].1)
    }
}

// `None` for the `u32::MAX` a batch reports a key it stored as.
fn held(table: &Table, row: u32) -> Option<Ref> {
    (row != u32::MAX).then(|| {
        Ref::new(
            table.values()[row as usize],
            table.weight(table.arity(), row),
        )
    })
}

fn pack(slot: u32, row: u32) -> u32 {
    assert!(slot < 1 << 12 && row < 1 << 20, "a packed holder");
    slot << 20 | row
}

fn unpack(packed: u32) -> (u32, u32) {
    (packed >> 20, packed & ((1 << 20) - 1))
}

/// The partition of a space as the current layer reads it.
pub struct Placed<'a, C, const SPACES: usize> {
    db: &'a Core<C, SPACES>,
    space: SpaceId,
}

impl<C, const SPACES: usize> Placement for Placed<'_, C, SPACES> {
    fn is_zero(&self, id: u32) -> bool {
        self.db.is_zero(self.space, id)
    }

    fn mask(&self, id: u32) -> u64 {
        self.db.mask(self.space, id)
    }
}

// Over cells that are representatives of what lies below the layer.
struct Composed<'a, C, const SPACES: usize> {
    db: &'a Core<C, SPACES>,
    space: SpaceId,
    depth: usize,
}

impl<C, const SPACES: usize> Composed<'_, C, SPACES> {
    #[inline]
    fn overlay(&self) -> &Overlay {
        &self.db.layers[self.depth - 1].overlay[self.space.0 as usize]
    }
}

impl<C, const SPACES: usize> Rename for Composed<'_, C, SPACES> {
    fn select_moved(&self, column: &[u32], out: &mut Vec<u32>) {
        let overlay = self.overlay();
        out.extend(
            column
                .iter()
                .enumerate()
                .filter(|(_, cell)| overlay.absorbed(**cell))
                .map(|(row, _)| row as u32),
        );
    }

    #[inline]
    fn to(&self, cell: u32) -> u32 {
        self.overlay()
            .find(cell.into(), |id| {
                self.db.mask_below(self.space, id, self.depth)
            })
            .id
    }

    #[inline]
    fn shift(&self, cell: u32) -> Option<(u64, u64)> {
        let overlay = self.overlay();
        if !overlay.shifted {
            return None;
        }
        let found = overlay.find(cell.into(), |id| {
            self.db.mask_below(self.space, id, self.depth)
        });
        Some((
            found.offset,
            self.db.mask_below(self.space, found.id, self.depth),
        ))
    }
}

/// Per table and column, the rows grouped by that column, and ordered by it
/// and their tag; per table, the rows the epoch before wrote; and per table
/// and column order, the trie a join reads. All over the base's rows.
#[derive(Default)]
pub struct Groups {
    built: Vec<Vec<Group>>,
    tag_orders: Vec<Vec<TagOrder>>,
    new_rows: Vec<Vec<NewRows>>,
    tries: Vec<Vec<Trie>>,
    scratch: Vec<u32>,
}

/// The rows of one table the epoch before wrote, as `(cell, row)` for the
/// cell of one column, ascending.
#[derive(Default)]
struct NewRows {
    /// The table version and epoch the list was made at.
    made: (u64, u32),
    rows: Vec<(u32, u32)>,
}

impl Groups {
    /// The grouping [`Database::prepare_group`] built.
    pub fn get(&self, slot: u32, column: usize) -> &Group {
        &self.built[slot as usize][column]
    }

    /// The order [`Database::prepare_tag_order`] built.
    pub fn tag_order(&self, slot: u32, column: usize) -> &TagOrder {
        &self.tag_orders[slot as usize][column]
    }

    /// How many rows of the table at `slot` hold one of `cells` in `column`,
    /// counting none for a table not grouped by it.
    pub fn count(&self, slot: u32, column: usize, cells: &[u32]) -> usize {
        let group = self.built.get(slot as usize);
        group
            .and_then(|columns| columns.get(column))
            .map_or(0, |group| {
                cells.iter().map(|&cell| group.rows(cell).len()).sum()
            })
    }

    /// The rows of the table at `slot` that the epoch before wrote, as
    /// `(cell in column, row)`, ascending; prepared by
    /// [`Database::prepare_new_rows`].
    pub fn new_rows(&self, slot: u32, column: usize) -> &[(u32, u32)] {
        self.new_rows
            .get(slot as usize)
            .and_then(|columns| columns.get(column))
            .map_or(&[], |new| new.rows.as_slice())
    }

    /// The trie [`Database::prepare_trie`] built over `columns`.
    pub(crate) fn trie(&self, slot: u32, columns: &[u8]) -> &Trie {
        let tries = &self.tries[slot as usize];
        let found = tries.iter().find(|trie| trie.columns() == columns);
        found.expect("a prepared trie")
    }

    /// The part of [`Self::new_rows`] whose cell is `cell`.
    pub fn new_rows_with(&self, slot: u32, column: usize, cell: u32) -> &[(u32, u32)] {
        let rows = self.new_rows(slot, column);
        let from = rows.partition_point(|&(other, _)| other < cell);
        let to = rows.partition_point(|&(other, _)| other <= cell);
        &rows[from..to]
    }
}

/// Epoch-stamped membership marks over the ids of a space: `begin` costs
/// nothing per id, so a sweep pays only for what it visits.
#[derive(Default)]
pub struct Marks {
    stamp: Vec<u32>,
    epoch: u32,
}

impl Marks {
    fn begin(&mut self, ids: usize) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.fill(0);
            self.epoch = 1;
        }
        if self.stamp.len() < ids {
            self.stamp.resize(ids, 0);
        }
    }

    /// Mark `id`, reporting whether this sweep had not seen it.
    pub fn insert(&mut self, id: usize) -> bool {
        let slot = &mut self.stamp[id];
        std::mem::replace(slot, self.epoch) != self.epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: SpaceId = SpaceId::nth(0);

    fn two_tables() -> (Database<(), 1>, [u32; 2], Vec<u32>) {
        let mut db: Database<(), 1> = Database::new();
        let slots = [Table::new(1), Table::new(1)].map(|table| db.add_table(table, IDS));
        let ids = (0..6).map(|_| db.mint(IDS, 0, false)).collect();
        (db, slots, ids)
    }

    #[test]
    fn a_table_is_rewritten_for_an_id_its_rows_came_to_name() {
        let (mut db, [other, table], ids) = two_tables();
        db.insert(other, &[ids[0]], &[], (ids[5], 0), 0);
        db.insert(table, &[ids[3]], &[], (ids[4], 0), 1);
        let mut report = Repair::default();
        // The row names 3, then 2 once 2 absorbs 3, then 1: an id no row was
        // stored under still says which tables name it.
        for (absorbed, survivor) in [(3, 2), (2, 1)] {
            db.equate(IDS, ids[absorbed].into(), ids[survivor].into());
            assert!(db.canonicalize(IDS, None, &mut report));
            let found = db.lookup(table, &[ids[survivor]], &[]);
            assert_eq!(found, Some(Ref::new(ids[4], 0)));
        }
        assert_eq!(db.lookup(other, &[ids[0]], &[]), Some(Ref::new(ids[5], 0)));
    }

    proptest::proptest! {
        #[test]
        fn a_canonical_space_names_representatives_alone(
            steps in proptest::collection::vec((0u8..5, 0u32..40, 0u32..40, 0u32..40), 1..60),
        ) {
            let (mut db, slots, _) = two_tables();
            let mut report = Repair::default();
            let canonicalize = |db: &mut Database<(), 1>, report: &mut Repair| {
                while db.canonicalize(IDS, None, report) {
                    for pair in std::mem::take(&mut report.collisions) {
                        db.equate(IDS, Ref::from(pair.kept.0), Ref::from(pair.removed.0));
                    }
                }
            };
            for (step, a, b, c) in steps {
                let ids = db.ids(IDS) as u32;
                let (a, b, c) = (a % ids, b % ids, c % ids);
                match step {
                    0 => {
                        db.mint(IDS, 0, false);
                    }
                    1 => {
                        let slot = slots[(c % 2) as usize];
                        if db.lookup(slot, &[a], &[]).is_none() {
                            db.insert(slot, &[a], &[], (b, 0), 0);
                        }
                    }
                    2 => {
                        db.equate(IDS, Ref::from(a), Ref::from(b));
                    }
                    3 => db.advance(),
                    _ => canonicalize(&mut db, &mut report),
                }
            }
            // A row stored naming an id absorbed before the space was last
            // canonical waits for the next equate.
            let last = [(); 2].map(|()| Ref::from(db.mint(IDS, 0, false)));
            db.equate(IDS, last[0], last[1]);
            canonicalize(&mut db, &mut report);
            for table in db.tables() {
                for column in 0..=table.arity() {
                    for &cell in table.column(column) {
                        proptest::prop_assert_eq!(db.root(IDS, cell), cell);
                    }
                }
            }
        }
    }

    #[test]
    fn a_cleared_database_answers_like_a_new_one() {
        fn schema() -> (Database<(), 1>, LatticeId) {
            let mut db = Database::new();
            for _ in 0..2 {
                db.add_table(Table::new(1), IDS);
            }
            let lattice = db.add_lattice(IDS, Join::Agree);
            (db, lattice)
        }
        let session = |db: &mut Database<(), 1>, lattice: LatticeId| {
            let changed_before = db.take_changed(IDS);
            let ids = [(); 3].map(|()| db.mint(IDS, 0, false));
            db.insert(0, &[ids[0]], &[], (ids[1], 0), 0);
            db.raise(lattice, ids[2], 5);
            db.advance();
            (
                changed_before,
                ids,
                [db.lookup(0, &[ids[0]], &[]), db.lookup(1, &[ids[0]], &[])],
                db.read(lattice, ids[2]),
                db.risen(lattice, false).to_vec(),
                db.take_changed(IDS),
                db.epoch(),
            )
        };
        let (mut new, lattice) = schema();
        let (mut cleared, _) = schema();
        session(&mut cleared, lattice);
        cleared.clear();
        assert_eq!(session(&mut cleared, lattice), session(&mut new, lattice));
    }

    #[test]
    fn an_epoch_names_the_tables_it_wrote() {
        let (mut db, [written, idle], ids) = two_tables();
        db.insert(written, &[ids[0]], &[], (ids[1], 0), 0);
        db.advance();
        assert!(db.wrote(written) && !db.wrote(idle));
        let mut cells = Vec::new();
        db.each_new_cell(written, 0, |cell| cells.push(cell));
        assert_eq!(cells, [ids[0]]);
        db.advance();
        assert!(!db.wrote(written));
        cells.clear();
        db.each_new_cell(written, 0, |cell| cells.push(cell));
        assert_eq!(cells, []);
    }
}
