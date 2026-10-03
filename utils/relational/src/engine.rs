use std::cell::{Ref, RefCell};

use smallvec::SmallVec;

use crate::column::{Column, Fact, Join};
use crate::label::{FxHashMap, Labels};
use crate::saturate::{RuleCache, RuleIndex};
use crate::store::{Group, Repair, Table};
use crate::unionfind::UnionFind;
use crate::{ClassId, ColumnId, Csr, Label, LabelId, RowId};

/// Empty link in an intrusive list.
const NONE: u32 = u32::MAX;

/// A class read as a distance from another class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Object {
    base: ClassId,
    offset: i64,
}

/// The e-graph: equivalence classes of terms, over the tables of
/// [`crate::store`].
///
/// Each label has a table keyed on the child classes and valued by the class
/// the node belongs to, so hash-consing is a key lookup and congruence is the
/// table's functional dependency: a rebuild rewrites every table through the
/// union-find and merges the classes of the rows that come to share a key. A
/// rule's atoms are joins over those tables.
///
/// What the tables do not hold is the term itself. A row is tagged with the
/// e-node it was interned as, and `node` keeps that term verbatim, provenance
/// included. Its inline children are canonical *as of the insert* and drift
/// afterwards, so every reader passes them through [`Self::find`]. A class
/// lists its e-nodes in insertion order, a union appending the absorbed class's
/// after the survivor's; an e-node a rebuild found congruent to an older one
/// leaves its table but stays on that list, since it may carry provenance the
/// older one does not.
pub struct Engine<L: Label> {
    labels: Labels<L>,
    graph: Graph,

    /// The term each e-node was interned as, and its label.
    node: Vec<L>,
    node_label: Vec<LabelId>,
    /// [`Label::op_key`] -> the labels under it, in the order they were first
    /// seen.
    op_labels: FxHashMap<u64, Vec<LabelId>>,
    /// Per label, the table its rows live in and whether it is a commutative
    /// binary operator.
    label_slot: Vec<u32>,
    label_commutes: Vec<bool>,
    label_op: Vec<u64>,

    /// Bumped whenever the graph changes, so a derived index knows it is stale.
    version: u64,
    groups: RefCell<Groups>,
    /// Class -> the classes of the rows naming it as a child, and the version
    /// it was built at.
    parents: RefCell<(u64, Csr)>,
    /// Reusable per-class "already seen" marks for the read-side sweeps, so a
    /// query that visits a handful of classes does not first zero an array the
    /// size of the graph.
    marks: RefCell<Marks>,
    repair: Repair,
    /// Per plan, what its searches keep: the labels its atoms read and the
    /// labels its head builds. Labels only accumulate, so an entry stays
    /// valid for the engine's life.
    caches: RefCell<FxHashMap<u64, RuleCache>>,
    /// Which rules a round visits, for the rule set last saturated with.
    pub(crate) rule_index: Option<std::sync::Arc<RuleIndex>>,
    /// Scratch for the variables a rule head binds.
    pub(crate) head_bound: Vec<Option<ClassId>>,
    stats: Stats,

    scopes: Vec<Scope>,
    /// The constant a class is known to be: seeded by every literal row, raised
    /// by a scope's assumption, joined by a union.
    consts: Column<LabelId>,
    /// The type a class's terms carry, seeded by every typed row. Congruence
    /// already forces a class's rows to agree on it, so the first row to say
    /// wins and a merge does not make the answer depend on merge order.
    types: Column<u64>,
    /// The class a class is derived from and the distance to it: pointer
    /// provenance, for the vocabulary that has any. A fact naming a class is
    /// canonicalized like a child, and two derivations that disagree conflict —
    /// which reads back as "derived from nothing known", the conservative
    /// answer.
    objects: Column<Object>,
}

/// The part of the engine a scope undoes: copied when one opens, put back when
/// it closes.
#[derive(Clone)]
struct Graph {
    uf: UnionFind,
    /// `(label, children) -> class`, tagged with the e-node: one table per
    /// arity, and beside it one for the labels of that arity that never
    /// hash-cons. A table past the end has no rows.
    tables: Vec<Table>,
    /// Next e-node of the same class, or [`NONE`].
    node_next: Vec<u32>,
    class_head: Vec<u32>,
    class_tail: Vec<u32>,
    class_len: Vec<u32>,
    /// Whether a union happened since the last rebuild.
    dirty: bool,
    total_nodes: usize,
    num_classes: usize,
    /// The stamp a row written now gets. Bumped by every change-log drain, so
    /// `stamp == epoch - 1` reads as "written during the round that just ended".
    epoch: u32,
    /// Classes changed since the last [`Engine::take_changed`], possibly
    /// non-canonical — semi-naive saturation's frontier. A round touches the
    /// same class many times, so entries are deduplicated as they are logged.
    changed: Vec<ClassId>,
    /// Per class, the `changed_epoch` it was last logged in.
    changed_at: Vec<u32>,
    changed_epoch: u32,
    changed_all: bool,
    /// The facts that rose since the last [`Engine::take_changed`], as column
    /// and class: on a class that already existed, and on a class minted with
    /// the fact. The two are kept apart because a class minted with a fact has
    /// only new rows around it, which a round searches from anyway.
    rising: [Vec<(u8, ClassId)>; 2],
    /// The same for the round that drain closed, canonical and ascending.
    risen: [Vec<(u8, ClassId)>; 2],
}

/// An open assumption scope.
struct Scope {
    /// The graph as the scope found it.
    saved: Graph,
    /// E-nodes interned before the scope opened; the rest go with it.
    nodes: usize,
    /// The classes the scope found distinct, grouped under the class it merged
    /// them into; merged groups only. Starts as the enclosing scope's, so a
    /// nested scope keeps naming base classes.
    members: FxHashMap<ClassId, Vec<ClassId>>,
    /// The classes the scope minted or merged: the seeds of
    /// [`Engine::scope_dirty`].
    dirt: Vec<ClassId>,
}

/// The labels an atom reads, by the table they live in; ascending within one.
pub(crate) type Candidates = SmallVec<[(u32, SmallVec<[LabelId; 4]>); 2]>;

/// Per table and column, the rows grouped by that column and the table version
/// the grouping was built at.
#[derive(Default)]
pub(crate) struct Groups {
    built: Vec<Vec<(u64, Group)>>,
    /// Per table, the rows the previous round wrote.
    new_rows: Vec<NewRows>,
    scratch: Vec<u32>,
}

/// The rows of one table the previous round wrote, as `(label, row)`,
/// ascending.
#[derive(Default)]
struct NewRows {
    /// The table version and epoch the list was made at.
    made: (u64, u32),
    rows: Vec<(u32, u32)>,
}

impl Groups {
    pub(crate) fn get(&self, slot: u32, column: usize) -> &Group {
        &self.built[slot as usize][column].1
    }

    /// The rows of `label`, in the table at `slot`, that the previous round
    /// wrote, as `(label, row)`, ascending by row.
    pub(crate) fn new_rows(&self, slot: u32, label: LabelId) -> &[(u32, u32)] {
        let Some(NewRows { rows, .. }) = self.new_rows.get(slot as usize) else {
            return &[];
        };
        let from = rows.partition_point(|&(other, _)| other < label.0);
        let to = rows.partition_point(|&(other, _)| other <= label.0);
        &rows[from..to]
    }
}

/// Cumulative engine work, for the saturation counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub merges: usize,
    pub adds: usize,
    /// Rows a rebuild re-keyed.
    pub repairs: usize,
    /// Column entries that rose. A round that only raised a fact changed
    /// nothing the class and node counts see, and is not a fixpoint.
    pub raises: usize,
}

impl<L: Label> Default for Engine<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L: Label> Engine<L> {
    pub fn new() -> Self {
        Self {
            labels: Labels::default(),
            graph: Graph {
                uf: UnionFind::new(),
                tables: Vec::new(),
                node_next: Vec::new(),
                class_head: Vec::new(),
                class_tail: Vec::new(),
                class_len: Vec::new(),
                dirty: false,
                total_nodes: 0,
                num_classes: 0,
                epoch: 1,
                changed: Vec::new(),
                changed_at: Vec::new(),
                changed_epoch: 1,
                changed_all: true,
                rising: Default::default(),
                risen: Default::default(),
            },
            node: Vec::new(),
            node_label: Vec::new(),
            op_labels: FxHashMap::default(),
            label_slot: Vec::new(),
            label_commutes: Vec::new(),
            label_op: Vec::new(),
            version: 1,
            groups: RefCell::default(),
            parents: RefCell::default(),
            marks: RefCell::default(),
            repair: Repair::default(),
            caches: RefCell::default(),
            rule_index: None,
            head_bound: Vec::new(),
            stats: Stats::default(),
            scopes: Vec::new(),
            consts: Column::new(Join::Agree),
            types: Column::new(Join::First),
            objects: Column::new(Join::Agree),
        }
    }

    // ---- reading ----------------------------------------------------------

    pub fn find(&self, id: ClassId) -> ClassId {
        self.graph.uf.find(id)
    }

    pub fn connected(&self, a: ClassId, b: ClassId) -> bool {
        self.find(a) == self.find(b)
    }

    pub fn is_empty(&self) -> bool {
        self.graph.num_classes == 0
    }

    /// Total e-nodes across all classes. An e-node found congruent to another
    /// still counts, so this only ever grows within a context — the fixpoint
    /// test both saturation drivers use reads it.
    pub fn total_size(&self) -> usize {
        self.graph.total_nodes
    }

    pub fn num_classes(&self) -> usize {
        self.graph.num_classes
    }

    /// One past the highest class id ever minted, live or not — the size a table
    /// indexed by class id needs.
    pub fn class_count(&self) -> usize {
        self.graph.uf.len()
    }

    /// Bytes the e-nodes, the tables and the per-class arrays hold, without the
    /// tables' key indexes. An estimate for ranking, not an allocator total.
    pub fn approx_bytes(&self) -> usize {
        let cells: usize = self
            .graph
            .tables
            .iter()
            .map(|table| table.len() * (table.arity() + 3))
            .sum();
        self.node.len() * (size_of::<L>() + 2 * size_of::<u32>())
            + cells * size_of::<u32>()
            + self.graph.uf.len() * 5 * size_of::<u32>()
            + self.labels.len() * size_of::<L>()
    }

    /// Work done since the engine was built. A saturation round reads the
    /// difference across it; a scope does not roll these back, since they count
    /// work, not state.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn in_scope(&self) -> bool {
        !self.scopes.is_empty()
    }

    /// Child classes of an e-node, canonical as of now.
    pub fn children(&self, row: RowId) -> SmallVec<[ClassId; 4]> {
        self.node[row.index()]
            .children()
            .iter()
            .map(|&child| self.find(child))
            .collect()
    }

    pub fn label(&self, row: RowId) -> LabelId {
        self.node_label[row.index()]
    }

    pub fn node(&self, row: RowId) -> &L {
        &self.node[row.index()]
    }

    /// The e-nodes of `id`'s class in insertion order, a union appending the
    /// absorbed class's after the survivor's. Extraction breaks ties by this
    /// order, so it is part of the contract — a rebuild must not re-sort it.
    pub fn rows(&self, id: ClassId) -> Rows<'_, L> {
        Rows {
            engine: self,
            cursor: self.graph.class_head[self.find(id).index()],
        }
    }

    /// E-nodes of `id`'s class; child ids may be non-canonical — resolve with
    /// [`Self::find`].
    pub fn nodes(&self, id: ClassId) -> impl Iterator<Item = &L> + Clone {
        self.rows(id).map(|row| &self.node[row.index()])
    }

    fn class_len(&self, id: ClassId) -> usize {
        self.graph.class_len[self.find(id).index()] as usize
    }

    /// Every live class, ascending. A class is named by its lowest member, so
    /// under a scope this is the position of the group's lowest base class.
    /// Extraction and bare-variable-rooted patterns walk classes in this order
    /// and break ties by it, so it is part of the contract.
    pub fn class_ids(&self) -> impl Iterator<Item = ClassId> + '_ {
        (0..self.graph.uf.len() as u32)
            .map(ClassId)
            .filter(|&id| self.find(id) == id && self.graph.class_len[id.index()] != 0)
    }

    /// [`Self::class_ids`] as readable classes.
    pub fn classes(&self) -> impl Iterator<Item = ClassRef<'_, L>> + '_ {
        self.class_ids().map(|id| self.class(id))
    }

    pub fn class(&self, id: ClassId) -> ClassRef<'_, L> {
        ClassRef {
            engine: self,
            id: self.find(id),
        }
    }

    /// The table `label`'s rows live in, or `None` while that table has never
    /// had a row. Labels of one arity share a table, keyed on the label and
    /// then the children.
    pub(crate) fn table(&self, label: LabelId) -> Option<&Table> {
        self.slot_table(*self.label_slot.get(label.index())?)
    }

    pub(crate) fn slot_table(&self, slot: u32) -> Option<&Table> {
        self.graph.tables.get(slot as usize)
    }

    /// Whether `label` is a commutative binary operator.
    pub(crate) fn commutes(&self, label: LabelId) -> bool {
        self.label_commutes[label.index()]
    }

    /// The labels whose operator bucket is `op`, in the order first seen.
    pub(crate) fn labels_with_op(&self, op: u64) -> &[LabelId] {
        self.op_labels.get(&op).map_or(&[], Vec::as_slice)
    }

    /// Add to `reads` the labels in the `op` bucket that `template` matches at
    /// its arity, or every label in the bucket without one. With `since`, only
    /// the labels met from that count on are looked at, and `reads` already
    /// holds the rest.
    pub(crate) fn labels_matching(
        &self,
        op: u64,
        template: Option<(&L, usize)>,
        since: Option<usize>,
        reads: &mut Candidates,
    ) {
        let matches = |label: LabelId| {
            let node = self.labels.node(label);
            template.is_none_or(|(template, arity)| {
                node.children().len() == arity && template.matches_template(node)
            })
        };
        let mut add = |label: LabelId| {
            let slot = self.label_slot[label.index()];
            // Labels arrive ascending, so each table's list stays sorted.
            match reads.iter_mut().find(|(known, _)| *known == slot) {
                Some((_, labels)) => labels.push(label),
                None => reads.push((slot, SmallVec::from_slice(&[label]))),
            }
        };
        match since {
            None => {
                for &label in self.labels_with_op(op) {
                    if matches(label) {
                        add(label);
                    }
                }
            }
            Some(since) => {
                for index in since..self.label_op.len() {
                    let label = LabelId(index as u32);
                    if self.label_op[index] == op && matches(label) {
                        add(label);
                    }
                }
            }
        }
    }

    /// How many labels the graph has met. What a plan's atoms read is fixed
    /// between two readings that agree.
    pub(crate) fn labels_len(&self) -> usize {
        self.labels.len()
    }

    /// The columns, as a mask of [`column_bit`], in which a fact rose during
    /// the round the last [`Self::take_changed`] closed: on a class that
    /// already existed, or with `minted` on a class minted with it.
    pub(crate) fn facts_rose(&self, minted: bool) -> u8 {
        self.graph.risen[usize::from(minted)]
            .iter()
            .fold(0, |mask, &(column, _)| mask | 1 << column)
    }

    /// The classes whose fact in `column` rose during the round the last
    /// [`Self::take_changed`] closed; `minted` as for [`Self::facts_rose`].
    pub(crate) fn risen(&self, column: ColumnId, minted: bool) -> impl Iterator<Item = ClassId> {
        let column = column_bit(column) as u8;
        self.graph.risen[usize::from(minted)]
            .iter()
            .filter(move |&&(other, _)| other == column)
            .map(|&(_, class)| class)
    }

    /// The stamp of the rows the round now running writes; a row stamped one
    /// below it was written by the round before.
    pub(crate) fn epoch(&self) -> u32 {
        self.graph.epoch
    }

    /// Group the rows of the table at `slot` by `column`, unless that is
    /// already done for the table as it is. A join looks rows up through
    /// [`Self::groups`].
    pub(crate) fn prepare_group(&self, slot: u32, column: usize) {
        let Some(table) = self.slot_table(slot) else {
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
        let version = table.version() + 1;
        let (built, group) = &mut columns[column];
        if *built != version {
            *built = version;
            // Column zero holds labels; every other one holds classes.
            let cells = if column == 0 {
                self.labels.len()
            } else {
                self.graph.uf.len()
            };
            group.build(table.column(column), cells);
        }
    }

    /// List the rows of the table at `slot` the previous round wrote, by
    /// label, unless that is already done for the table as it is; read through
    /// [`Self::groups`].
    pub(crate) fn prepare_new_rows(&self, slot: u32) {
        let Some(table) = self.slot_table(slot) else {
            return;
        };
        let mut groups = self.groups.borrow_mut();
        let groups = &mut *groups;
        if groups.new_rows.len() <= slot as usize {
            groups
                .new_rows
                .resize_with(slot as usize + 1, Default::default);
        }
        let NewRows { made, rows } = &mut groups.new_rows[slot as usize];
        let now = (table.version() + 1, self.graph.epoch);
        if *made != now {
            *made = now;
            groups.scratch.clear();
            tir_adt::simd::select_eq(table.stamps(), self.graph.epoch - 1, &mut groups.scratch);
            rows.clear();
            let labels = table.column(0);
            rows.extend(
                groups
                    .scratch
                    .iter()
                    .map(|&row| (labels[row as usize], row)),
            );
            rows.sort_unstable();
        }
    }

    /// A fresh set of per-class marks: `insert` reports whether a class was
    /// unmarked.
    pub(crate) fn marks(&self) -> std::cell::RefMut<'_, Marks> {
        let mut marks = self.marks.borrow_mut();
        marks.begin(self.graph.uf.len());
        marks
    }

    /// Take the cache of plan `id` out for a search; [`Self::put_cache`] puts
    /// it back. Plan zero has none.
    pub(crate) fn take_cache(&self, id: u64) -> RuleCache {
        self.caches.borrow_mut().remove(&id).unwrap_or_default()
    }

    pub(crate) fn put_cache(&self, id: u64, cache: RuleCache) {
        if id != 0 {
            self.caches.borrow_mut().insert(id, cache);
        }
    }

    /// Every plan's cache, for a saturation to use and hand back.
    pub(crate) fn take_caches(&mut self) -> FxHashMap<u64, RuleCache> {
        std::mem::take(self.caches.get_mut())
    }

    pub(crate) fn put_caches(&mut self, caches: FxHashMap<u64, RuleCache>) {
        *self.caches.get_mut() = caches;
    }

    /// The operator buckets of the rows the previous round wrote, each once.
    pub(crate) fn new_ops(&self) -> Vec<u64> {
        for slot in 0..self.graph.tables.len() as u32 {
            self.prepare_new_rows(slot);
        }
        let groups = self.groups();
        let mut ops = Vec::new();
        for NewRows { rows, .. } in &groups.new_rows {
            let mut last = None;
            // The rows are sorted by label, so each label shows up as a run.
            for &(label, _) in rows {
                if last.replace(label) != Some(label) {
                    ops.push(self.label_op[label as usize]);
                }
            }
        }
        ops.sort_unstable();
        ops.dedup();
        ops
    }

    /// Whether every table names every class by its representative: no union
    /// is waiting for a rebuild.
    pub(crate) fn rebuilt(&self) -> bool {
        !self.graph.dirty
    }

    pub(crate) fn groups(&self) -> Ref<'_, Groups> {
        self.groups.borrow()
    }

    /// Canonical classes holding a node in the `op` bucket, each once, in the
    /// order their labels were first seen. Over-approximates — callers confirm
    /// with the label.
    pub fn classes_with_op(&self, op: u64) -> Vec<ClassId> {
        let labels = self.labels_with_op(op);
        for &label in labels {
            self.prepare_group(self.label_slot[label.index()], 0);
        }
        let groups = self.groups();
        let mut seen = self.marks.borrow_mut();
        seen.begin(self.graph.uf.len());
        let mut out = Vec::new();
        for &label in labels {
            let Some(table) = self.table(label) else {
                continue;
            };
            for &row in groups.get(self.label_slot[label.index()], 0).rows(label.0) {
                let root = self.find(ClassId(table.values()[row as usize]));
                if seen.insert(root.index()) {
                    out.push(root);
                }
            }
        }
        out
    }

    /// Class of an already-interned `node`, or `None` (never inserts; always
    /// `None` for a unique node).
    pub fn lookup(&self, node: &L) -> Option<ClassId> {
        let label = self.labels.get(node)?;
        let mut key: SmallVec<[u32; 8]> = SmallVec::from_slice(&[label.0]);
        key.extend(node.children().iter().map(|&child| self.find(child).0));
        self.memo_find(label, &key)
    }

    /// The class of the row with `key`, which is `label` and then the canonical
    /// children.
    fn memo_find(&self, label: LabelId, key: &[u32]) -> Option<ClassId> {
        let table = self.table(label)?;
        let row = table.get(key)?;
        Some(self.find(ClassId(table.values()[row as usize])))
    }

    // ---- writing ----------------------------------------------------------

    /// Intern `node`, returning its class. A non-unique node equal to an
    /// existing one shares its class; otherwise a fresh class.
    pub fn add(&mut self, mut node: L) -> ClassId {
        for child in node.children_mut() {
            *child = self.graph.uf.find(*child);
        }
        let label = self.intern(&node);
        let mut key: SmallVec<[u32; 8]> = SmallVec::new();
        key.push(label.0);
        key.extend(node.children().iter().map(|child| child.0));
        self.add_labelled(label, &key, || node)
    }

    /// Whether `label` is `node`'s label.
    pub(crate) fn label_is(&self, label: LabelId, node: &L) -> bool {
        let known = self.labels.node(label);
        known.children().len() == node.children().len() && known.matches(node)
    }

    /// The id of `node`'s label, for [`Self::add_labelled`].
    pub(crate) fn intern(&mut self, node: &L) -> LabelId {
        let label = self.labels.intern(node);
        // Whatever the label table grew by gets its table and operator bucket.
        for index in self.label_slot.len()..self.labels.len() {
            let node = self.labels.node(LabelId(index as u32));
            let arity = node.children().len();
            self.label_slot
                .push(2 * arity as u32 + u32::from(node.is_unique()));
            self.label_commutes.push(arity == 2 && node.commutative());
            let op = node.op_key();
            self.label_op.push(op);
            self.op_labels
                .entry(op)
                .or_default()
                .push(LabelId(index as u32));
        }
        label
    }

    /// [`Self::add`] for a caller that already has the key, which is the label
    /// and then the canonical children: `node` is built only if the graph does
    /// not hold the node yet.
    pub(crate) fn add_labelled(
        &mut self,
        label: LabelId,
        key: &[u32],
        node: impl FnOnce() -> L,
    ) -> ClassId {
        match self.memo_find(label, key) {
            Some(class) => class,
            None => self.make_class(node(), label, key),
        }
    }

    /// Merge the classes of `a` and `b`, returning the survivor. Congruence
    /// repair is deferred to [`Self::rebuild`], and so is everything a query
    /// reads: the tables keep naming the absorbed class until then, so a search
    /// needs a rebuild first. The merge
    /// itself is visible immediately to [`Self::find`] and [`Self::nodes`], so
    /// an applier that unions and then instantiates hash-conses against the
    /// result.
    pub fn union(&mut self, a: ClassId, b: ClassId) -> ClassId {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        self.stats.merges += 1;
        self.version += 1;
        let survivor = self.graph.uf.union(ra, rb);
        if crate::trace_enabled() {
            eprintln!("U {} {} -> {}", ra.0, rb.0, survivor.0);
        }
        let absorbed = if survivor == ra { rb } else { ra };
        let epoch = self.graph.epoch;
        let moved = [
            self.consts.merge(absorbed, survivor, epoch),
            self.types.merge(absorbed, survivor, epoch),
            !self.objects.is_empty() && self.merge_object(absorbed, survivor),
        ];
        for (column, moved) in moved.into_iter().enumerate() {
            if moved {
                self.graph.rising[0].push((column as u8, survivor));
            }
        }
        if moved.contains(&true) {
            self.log_change(survivor);
        }
        self.splice_class(survivor, absorbed);
        if let Some(scope) = self.scopes.last_mut() {
            let taken = scope
                .members
                .remove(&absorbed)
                .unwrap_or_else(|| vec![absorbed]);
            scope
                .members
                .entry(survivor)
                .or_insert_with(|| vec![survivor])
                .extend(taken);
            scope.dirt.push(survivor);
        }
        self.graph.num_classes -= 1;
        self.graph.dirty = true;
        self.log_change(survivor);
        survivor
    }

    /// Restore congruence to a fixpoint after a batch of unions.
    ///
    /// Each pass rewrites every table through the union-find and merges the
    /// classes of the rows that came to share a key; those merges are what the
    /// next pass rewrites. Nothing depends on the order the tables are visited
    /// in or the rows within one: the class a merge leaves standing is the
    /// smallest id in the set whichever way the merges are grouped.
    pub fn rebuild(&mut self) {
        while std::mem::take(&mut self.graph.dirty) {
            self.version += 1;
            self.graph.uf.flatten();
            let mut report = std::mem::take(&mut self.repair);
            let epoch = self.graph.epoch;
            for table in &mut self.graph.tables {
                table.repair(self.graph.uf.parents(), epoch, &mut report);
            }
            self.stats.repairs += report.rekeyed.len();
            // A row with a re-keyed child reads differently, and semi-naive has
            // to see its class as changed.
            for class in report.rekeyed.drain(..) {
                self.log_change(ClassId(class));
            }
            for collision in report.collisions.drain(..) {
                self.union(ClassId(collision.kept), ClassId(collision.removed));
            }
            self.repair = report;
        }
        self.graph.uf.flatten();
        if let Some(scope) = self.scopes.last_mut() {
            for members in scope.members.values_mut() {
                members.sort_unstable();
                members.dedup();
            }
        }
    }

    // ---- rows -------------------------------------------------------------

    /// The table `label`'s rows live in, created with every table below it if
    /// this is the first row it gets.
    fn table_mut(&mut self, label: LabelId) -> &mut Table {
        let slot = self.label_slot[label.index()] as usize;
        while self.graph.tables.len() <= slot {
            // The label is a key column the union-find has no say over.
            let columns = 1 + self.graph.tables.len() / 2;
            self.graph.tables.push(if self.graph.tables.len() % 2 == 1 {
                Table::bag(columns).plain(1)
            } else {
                Table::new(columns).plain(1)
            });
        }
        &mut self.graph.tables[slot]
    }

    fn make_class(&mut self, node: L, label: LabelId, key: &[u32]) -> ClassId {
        let constant = node.constant();
        let type_key = node.type_key();
        let row = RowId(self.node.len() as u32);
        let class = self.graph.uf.push();
        let epoch = self.graph.epoch;
        self.table_mut(label).insert(key, class.0, row.0, epoch);
        self.version += 1;
        self.graph.node_next.push(NONE);
        self.graph.class_head.push(row.0);
        self.graph.class_tail.push(row.0);
        self.graph.class_len.push(1);
        if let Some(scope) = self.scopes.last_mut() {
            scope.dirt.push(class);
        }
        if crate::trace_enabled() {
            eprintln!("A {} {:?} {:?}", class.0, node, &key[1..]);
        }
        self.node.push(node);
        self.node_label.push(label);
        if let Some(constant) = constant {
            let label = self.intern(&constant);
            if self.consts.raise(class, label, epoch) {
                self.stats.raises += 1;
                self.graph.rising[1].push((column_bit(ColumnId::Const) as u8, class));
            }
        }
        if let Some(key) = type_key {
            self.types.raise(class, key, epoch);
            self.graph.rising[1].push((column_bit(ColumnId::Type) as u8, class));
        }
        self.graph.total_nodes += 1;
        self.graph.num_classes += 1;
        self.stats.adds += 1;
        self.log_change(class);
        class
    }

    /// Move the absorbed class's e-nodes onto the end of the survivor's list.
    fn splice_class(&mut self, survivor: ClassId, absorbed: ClassId) {
        let graph = &mut self.graph;
        let (head, tail, len) = (
            graph.class_head[absorbed.index()],
            graph.class_tail[absorbed.index()],
            graph.class_len[absorbed.index()],
        );
        if head != NONE {
            let survivor_tail = graph.class_tail[survivor.index()];
            if survivor_tail == NONE {
                graph.class_head[survivor.index()] = head;
            } else {
                graph.node_next[survivor_tail as usize] = head;
            }
            graph.class_tail[survivor.index()] = tail;
            graph.class_len[survivor.index()] += len;
        }
        graph.class_head[absorbed.index()] = NONE;
        graph.class_tail[absorbed.index()] = NONE;
        graph.class_len[absorbed.index()] = 0;
    }

    // ---- change log -------------------------------------------------------

    fn log_change(&mut self, id: ClassId) {
        let graph = &mut self.graph;
        if graph.changed_at.len() <= id.index() {
            // Classes are minted one at a time; growing by one each time would
            // make every mint a resize.
            let len = (id.index() + 1).max(graph.changed_at.len() * 2);
            graph.changed_at.resize(len, 0);
        }
        if graph.changed_at[id.index()] != graph.changed_epoch {
            graph.changed_at[id.index()] = graph.changed_epoch;
            graph.changed.push(id);
        }
    }

    /// Drain the change log: the canonical classes changed since the previous
    /// call, ascending and deduplicated; `None` means "every class".
    pub fn take_changed(&mut self) -> Option<Vec<ClassId>> {
        self.graph.epoch += 1;
        let all = std::mem::replace(&mut self.graph.changed_all, false);
        let mut changed = std::mem::take(&mut self.graph.changed);
        for list in 0..2 {
            let mut risen = std::mem::take(&mut self.graph.rising[list]);
            for (_, class) in &mut risen {
                *class = self.find(*class);
            }
            risen.sort_unstable();
            risen.dedup();
            // The drained list's allocation becomes the next round's.
            std::mem::swap(&mut self.graph.risen[list], &mut risen);
            risen.clear();
            self.graph.rising[list] = risen;
        }
        // Every mark of the drained log goes stale at once.
        self.graph.changed_epoch += 1;
        if all {
            return None;
        }
        for id in &mut changed {
            *id = self.find(*id);
        }
        changed.sort_unstable();
        changed.dedup();
        Some(changed)
    }

    /// Report "everything" from the next [`Self::take_changed`]. A driver that
    /// stopped on a limit rather than at a fixpoint calls this: the matches it
    /// never reached are not named by the change log.
    pub fn mark_all_changed(&mut self) {
        self.graph.changed_all = true;
    }

    // ---- facts ------------------------------------------------------------

    /// Raise, inside the current scope, that `class` evaluates to constant
    /// `node`. A fact, not a merge: the class keeps its identity and parents, so
    /// only its users see a change. Panics with no scope open — an unscoped
    /// assumption would never be popped.
    pub fn assume_const(&mut self, class: ClassId, node: L) {
        assert!(
            self.in_scope(),
            "an assumption needs a scope to be undone by"
        );
        let label = self.intern(&node);
        self.raise_const(self.find(class), label);
    }

    /// The constant `class` is known to be — its own literal row, or what an
    /// open scope assumed of it. `None` when nothing is known and when two
    /// values were proven, which a refuted hypothesis reads as "unknown".
    pub fn const_of(&self, class: ClassId) -> Option<&L> {
        self.consts
            .get(self.find(class))
            .map(|label| self.labels.node(label))
    }

    /// The constant an *open scope* assumed of `class`, as opposed to the one
    /// the class states about itself. A reader that acts on a hypothesis rather
    /// than on the program asks this.
    pub fn assumed_const(&self, class: ClassId) -> Option<&L> {
        let class = self.find(class);
        self.consts
            .written_in_scope(class)
            .then(|| self.const_of(class))
            .flatten()
    }

    /// The class that holds `node` as its own literal, whatever type the row
    /// carries. The hash-cons cannot answer this: a class is known to be a
    /// *value*, and the row saying so may spell it at a type the caller has no
    /// way to name.
    pub fn const_class(&self, node: &L) -> Option<ClassId> {
        self.classes_with_const(node)
            .find(|&class| !self.consts.written_in_scope(class))
    }

    /// The classes an open scope assumed to be `node`.
    pub fn classes_assumed_const<'a>(&'a self, node: &L) -> impl Iterator<Item = ClassId> + 'a {
        self.classes_with_const(node)
            .filter(|&class| self.consts.written_in_scope(class))
    }

    /// Whether `class` was proven two different constants — a refuted scope.
    pub fn const_conflicted(&self, class: ClassId) -> bool {
        self.consts.is_conflicted(self.find(class))
    }

    /// The classes known to be `node`.
    pub fn classes_with_const<'a>(&'a self, node: &L) -> impl Iterator<Item = ClassId> + 'a {
        node.constant()
            .and_then(|constant| self.labels.get(&constant))
            .into_iter()
            .flat_map(|label| self.consts.classes_with(label))
    }

    /// The node interned under `label`.
    pub fn label_node(&self, label: LabelId) -> Option<&L> {
        (label.index() < self.labels.len()).then(|| self.labels.node(label))
    }

    /// Raise "`class` is `base` plus `offset`". The value is resolved to the end
    /// of its chain first, so a rule that learns the same location by two routes
    /// — `q` from `p + 4`, then `q` from `(p + 4) + 0` — states one fact rather
    /// than two that disagree. Reports whether the column moved.
    pub fn raise_object(&mut self, class: ClassId, base: ClassId, offset: i64) -> bool {
        let class = self.find(class);
        let raised = match self.resolve(base, offset) {
            // A class derived from itself at no distance is not derived at all;
            // at a distance it is a contradiction.
            Some((base, 0)) if base == class => return false,
            Some((base, offset)) => Fact::Known(Object { base, offset }),
            None => Fact::Conflict,
        };
        // Both sides are read back to where they land before they are compared.
        // What is stored may name a class since absorbed, or a base whose own
        // derivation deepened after it was written, and neither is a second
        // opinion — it is the same one, spelled at the time it was learned.
        let stored = self.resolved_object(class);
        let joined = match stored {
            Some(stored) => stored.join(raised, Join::Agree),
            None => raised,
        };
        if stored == Some(joined) {
            return false;
        }
        self.objects.put(class, joined, self.graph.epoch);
        self.stats.raises += 1;
        self.rose(ColumnId::Object, class);
        self.log_change(class);
        true
    }

    /// What `class` is derived from: the end of its derivation chain, or itself
    /// at offset zero when nothing derived it. `None` once two derivations have
    /// disagreed, and for a chain that does not end — a class derived from
    /// itself is a contradiction, and "nowhere known" is the conservative
    /// reading of one.
    pub fn object_of(&self, class: ClassId) -> Option<(ClassId, i64)> {
        self.resolve(class, 0)
    }

    /// Follow the chain from `base` to the class nothing derives, adding the
    /// distances. Bounded: the chain is flattened as it is written, so anything
    /// this long is a cycle.
    fn resolve(&self, base: ClassId, offset: i64) -> Option<(ClassId, i64)> {
        const CHAIN_LIMIT: usize = 64;
        let mut base = self.find(base);
        let mut offset = offset;
        for _ in 0..CHAIN_LIMIT {
            match self.canonical_object(base) {
                // Nothing derives it: the end of the chain.
                None => return Some((base, offset)),
                // A class that derives itself ends the chain when it adds
                // nothing, and contradicts itself when it adds something.
                Some(Fact::Known(object)) if object.base == base => {
                    return (object.offset == 0).then_some((base, offset));
                }
                Some(Fact::Known(object)) => {
                    base = object.base;
                    offset = offset.checked_add(object.offset)?;
                }
                Some(Fact::Conflict) => return None,
            }
        }
        None
    }

    /// The stored entry with its base read through the union-find, or `None`
    /// when nothing derived the class. A class the entry names may have been
    /// absorbed since it was written.
    fn canonical_object(&self, class: ClassId) -> Option<Fact<Object>> {
        Some(match self.objects.entry(class)? {
            Fact::Known(object) => Fact::Known(Object {
                base: self.find(object.base),
                offset: object.offset,
            }),
            Fact::Conflict => Fact::Conflict,
        })
    }

    /// The stored entry read all the way back to where it lands.
    fn resolved_object(&self, class: ClassId) -> Option<Fact<Object>> {
        Some(match self.canonical_object(class)? {
            Fact::Known(object) => match self.resolve(object.base, object.offset) {
                Some((base, offset)) => Fact::Known(Object { base, offset }),
                None => Fact::Conflict,
            },
            Fact::Conflict => Fact::Conflict,
        })
    }

    fn merge_object(&mut self, absorbed: ClassId, survivor: ClassId) -> bool {
        let Some(fact) = self.objects.detach(absorbed) else {
            return false;
        };
        match fact {
            Fact::Known(object) => self.raise_object(survivor, object.base, object.offset),
            Fact::Conflict => self.objects.put(survivor, Fact::Conflict, self.graph.epoch),
        }
    }

    /// `class`'s value in `column`, as one word.
    pub fn fact(&self, column: ColumnId, class: ClassId) -> Option<u64> {
        let class = self.find(class);
        match column {
            ColumnId::Const => self.consts.get(class).map(|label| label.0 as u64),
            ColumnId::Type => self.types.get(class),
            // Not a word: a derivation is a class and a distance, which
            // [`crate::Atom::Object`] binds as a variable and a scalar.
            ColumnId::Object => None,
        }
    }

    /// Raise the type `class`'s terms carry, for a term whose type the language
    /// keeps outside the node.
    pub fn raise_type(&mut self, class: ClassId, key: u64) {
        let class = self.find(class);
        if self.types.raise(class, key, self.graph.epoch) {
            self.stats.raises += 1;
            self.rose(ColumnId::Type, class);
            self.log_change(class);
        }
    }

    /// Whether `class`'s value in `column` rose during the round the last
    /// [`Self::take_changed`] closed — the fact-level [`Self::row_is_new`].
    pub fn fact_is_new(&self, column: ColumnId, class: ClassId) -> bool {
        let class = self.find(class);
        match column {
            ColumnId::Const => self.consts.is_new(class, self.graph.epoch),
            ColumnId::Type => self.types.is_new(class, self.graph.epoch),
            ColumnId::Object => self.objects.is_new(class, self.graph.epoch),
        }
    }

    /// Note that `class`, which already existed, gained a fact in `column`.
    fn rose(&mut self, column: ColumnId, class: ClassId) {
        self.graph.rising[0].push((column_bit(column) as u8, class));
    }

    fn raise_const(&mut self, class: ClassId, label: LabelId) {
        if self.consts.raise(class, label, self.graph.epoch) {
            self.stats.raises += 1;
            self.rose(ColumnId::Const, class);
            self.log_change(class);
        }
    }

    // ---- scopes -----------------------------------------------------------

    /// Enter an assumption scope: everything until the matching
    /// [`Self::pop_context`] is undone by it.
    pub fn push_context(&mut self) {
        let members = self
            .scopes
            .last()
            .map(|scope| scope.members.clone())
            .unwrap_or_default();
        self.scopes.push(Scope {
            saved: self.graph.clone(),
            nodes: self.node.len(),
            members,
            dirt: Vec::new(),
        });
        self.consts.push_scope();
        self.types.push_scope();
        self.objects.push_scope();
    }

    /// Leave the scope, discarding its unions, its rows, and its assumptions;
    /// the enclosing scope (or the base graph) is restored without a rebuild.
    pub fn pop_context(&mut self) {
        let scope = self.scopes.pop().expect("open scope");
        self.consts.pop_scope();
        self.types.pop_scope();
        self.objects.pop_scope();
        // The classes the scope minted keep their ids, so a caller still holding
        // one finds it, but they lose every e-node the scope gave them and stop
        // being classes: `class_ids` skips a class with none.
        let classes = self.graph.uf.len();
        self.graph = scope.saved;
        self.graph.uf.extend_to(classes);
        self.graph.class_head.resize(classes, NONE);
        self.graph.class_tail.resize(classes, NONE);
        self.graph.class_len.resize(classes, 0);
        self.node.truncate(scope.nodes);
        self.node_label.truncate(scope.nodes);
        self.version += 1;
        // Table versions restart from the scope's copy, so a grouping built
        // inside the scope could pass for one of a later table.
        let groups = self.groups.get_mut();
        groups.built.clear();
        groups.new_rows.clear();
    }

    /// The classes the open scope found distinct and grouped under `id`, as of
    /// the last rebuild, ascending. Empty when no scope is open or the scope did
    /// not merge `id` with anything. Side tables built against the graph the
    /// scope opened on are keyed by those, so a query made under a scope
    /// aggregates over this.
    pub fn scope_members(&self, id: ClassId) -> &[ClassId] {
        self.scopes
            .last()
            .and_then(|scope| scope.members.get(&id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Canonical classes the open scopes changed: the ones their unions merged,
    /// the ones minted inside them, and transitively every class holding a node
    /// with such a child. Ascending id. Empty with no scope open.
    pub fn scope_dirty(&self) -> Vec<ClassId> {
        self.dirty_since(0)
    }

    /// The same, counted from the innermost scope alone: what changed since the
    /// scope enclosing it, so a caller that already answered for that one has
    /// only this to redo. Ascending id. Empty with no scope open.
    pub fn innermost_dirty(&self) -> Vec<ClassId> {
        self.dirty_since(self.scopes.len().saturating_sub(1))
    }

    /// Everything the scopes from `depth` outward-in changed, closed upward.
    fn dirty_since(&self, depth: usize) -> Vec<ClassId> {
        if !self.in_scope() {
            return Vec::new();
        }
        let seeds: Vec<ClassId> = self.scopes[depth..]
            .iter()
            .flat_map(|scope| &scope.dirt)
            .copied()
            .chain(self.consts.scoped_keys_from(depth))
            .collect();
        self.close_upward(seeds, None)
    }

    /// `changed` closed upward `height` times over parent edges, ascending: the
    /// classes a pattern of that height can newly match at. A class outside it
    /// has an unchanged downward cone to that depth, so its matches are the
    /// previous round's and were applied then.
    pub fn delta(&self, changed: &[ClassId], height: usize) -> Vec<ClassId> {
        self.close_upward(changed.to_vec(), Some(height))
    }

    /// Class -> the classes of the rows naming it as a child, grouped once for
    /// the graph as it is. The tables are keyed the other way, so an upward walk
    /// reads this instead.
    fn parents(&self) -> Ref<'_, (u64, Csr)> {
        if self.parents.borrow().0 != self.version {
            // A rebuilt table already names representatives.
            let rebuilt = self.rebuilt();
            let class = |cell: u32| match rebuilt {
                true => cell,
                false => self.find(ClassId(cell)).0,
            };
            let csr = Csr::build_with(self.graph.uf.len(), |edge| {
                for table in &self.graph.tables {
                    for column in 1..table.arity() {
                        for (&child, &parent) in table.column(column).iter().zip(table.values()) {
                            edge(class(child), class(parent));
                        }
                    }
                }
            });
            *self.parents.borrow_mut() = (self.version, csr);
        }
        self.parents.borrow()
    }

    /// `seeds` and everything reachable upward from them over parent edges in at
    /// most `levels` steps (unbounded when `None`), ascending.
    fn close_upward(&self, seeds: Vec<ClassId>, levels: Option<usize>) -> Vec<ClassId> {
        let parents = self.parents();
        let mut seen = self.marks.borrow_mut();
        seen.begin(self.graph.uf.len());
        let mut frontier: Vec<ClassId> = seeds
            .into_iter()
            .map(|id| self.find(id))
            .filter(|&id| seen.insert(id.index()))
            .collect();
        let mut closure = frontier.clone();
        let mut level = 0;
        while !frontier.is_empty() && levels.is_none_or(|max| level < max) {
            let mut next = Vec::new();
            for id in frontier.drain(..) {
                for &parent in parents.1.get(id.0) {
                    if seen.insert(parent as usize) {
                        closure.push(ClassId(parent));
                        next.push(ClassId(parent));
                    }
                }
            }
            frontier = next;
            level += 1;
        }
        closure.sort_unstable();
        closure
    }
}

/// The position of `column` in a mask of columns.
pub(crate) fn column_bit(column: ColumnId) -> usize {
    match column {
        ColumnId::Const => 0,
        ColumnId::Type => 1,
        ColumnId::Object => 2,
    }
}

/// Epoch-stamped membership marks over the class ids: `begin` costs nothing per
/// class, so a sweep pays only for what it visits.
#[derive(Default)]
pub(crate) struct Marks {
    stamp: Vec<u32>,
    epoch: u32,
}

impl Marks {
    fn begin(&mut self, classes: usize) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.fill(0);
            self.epoch = 1;
        }
        if self.stamp.len() < classes {
            self.stamp.resize(classes, 0);
        }
    }

    /// Mark `id`, reporting whether this sweep had not seen it.
    pub(crate) fn insert(&mut self, id: usize) -> bool {
        let slot = &mut self.stamp[id];
        std::mem::replace(slot, self.epoch) != self.epoch
    }
}

/// A class, read through the engine. Not a struct the engine owns: an e-class
/// is a set of e-nodes, and this is the cursor into it.
pub struct ClassRef<'a, L: Label> {
    engine: &'a Engine<L>,
    id: ClassId,
}

impl<L: Label> Clone for ClassRef<'_, L> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<L: Label> Copy for ClassRef<'_, L> {}

impl<'a, L: Label> ClassRef<'a, L> {
    pub fn id(self) -> ClassId {
        self.id
    }

    pub fn rows(self) -> Rows<'a, L> {
        self.engine.rows(self.id)
    }

    pub fn nodes(self) -> impl Iterator<Item = &'a L> + Clone {
        self.engine.nodes(self.id)
    }

    pub fn len(self) -> usize {
        self.engine.class_len(self.id)
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// Walks a class's e-nodes along its intrusive list.
pub struct Rows<'a, L: Label> {
    engine: &'a Engine<L>,
    cursor: u32,
}

impl<L: Label> Clone for Rows<'_, L> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine,
            cursor: self.cursor,
        }
    }
}

impl<L: Label> Iterator for Rows<'_, L> {
    type Item = RowId;

    fn next(&mut self) -> Option<RowId> {
        if self.cursor == NONE {
            return None;
        }
        let row = RowId(self.cursor);
        self.cursor = self.engine.graph.node_next[row.index()];
        Some(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Term;

    #[test]
    fn commutative_operands_keep_the_order_they_were_written_in() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let ab = eg.add(Term::comm("add", &[a, b]));
        let ba = eg.add(Term::comm("add", &[b, a]));
        assert_ne!(ab, ba);
        assert_eq!(eg.nodes(ba).next().unwrap().children, vec![b, a]);
    }

    #[test]
    fn classes_with_op_reports_each_class_once_in_minting_order() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let key = Term::leaf("a").op_key();
        assert_eq!(eg.classes_with_op(key), vec![a]);
        eg.union(a, b);
        assert_eq!(eg.classes_with_op(key), vec![eg.find(a)]);
        assert!(eg.classes_with_op(Term::leaf("zzz").op_key()).is_empty());
    }

    /// A scope is the same graph with more merges, so a lookup under it sees
    /// what congruence over those merges proves, and the pop takes it back.
    #[test]
    fn a_scoped_lookup_sees_the_scopes_congruence() {
        let mut eg = Engine::new();
        let b = eg.add(Term::leaf("b"));
        let a = eg.add(Term::leaf("a"));
        let fa = eg.add(Term::op("f", &[a]));
        eg.rebuild();
        assert_eq!(eg.lookup(&Term::op("f", &[b])), None);
        eg.push_context();
        eg.union(a, b);
        eg.rebuild();
        assert_eq!(eg.lookup(&Term::op("f", &[b])), Some(eg.find(fa)));
        eg.pop_context();
        assert_eq!(eg.lookup(&Term::op("f", &[b])), None);
        assert_eq!(eg.lookup(&Term::op("f", &[a])), Some(eg.find(fa)));
    }
}
