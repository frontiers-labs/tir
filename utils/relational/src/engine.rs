use std::cell::RefCell;
use std::hash::{Hash, Hasher};

use tir_adt::FxHasher;

use crate::column::{Column, Join};
use crate::label::{FxHashMap, Labels};
use crate::{ClassId, ColumnId, Label, LabelId, RowId, UnionFind};

/// Empty link in an intrusive list.
const NONE: u32 = u32::MAX;

/// The e-graph as a database.
///
/// One row per e-node, in columns: its label (interned, so congruence compares a
/// `u32`), the class it belongs to, and its child classes in one flat array
/// sliced by [`Self::children`]. No `Vec` per class, per node, or per parent
/// list — class membership and parent back-edges are intrusive lists over the
/// row arrays, spliced in `O(1)` by a union.
///
/// `node` is the reconstruction column: the term a row was interned as, kept
/// verbatim. Its inline children are canonical *as of the insert* and drift
/// afterwards, exactly as the scalar engine's class node lists did, so every
/// reader passes them through [`Self::find`]. The `u32` columns are the graph;
/// `node` is only what a caller reading e-nodes back gets handed.
pub struct Engine<L: Label> {
    labels: Labels<L>,
    uf: UnionFind,

    row_label: Vec<LabelId>,
    row_class: Vec<ClassId>,
    /// `rows + 1` offsets into `children`.
    row_start: Vec<u32>,
    children: Vec<ClassId>,
    node: Vec<L>,
    /// Next row of the same class, or [`NONE`].
    row_next: Vec<u32>,
    /// The [`Self::row_epoch`] a row was appended or re-canonicalized in.
    row_stamp: Vec<u32>,
    /// Bumped by every change-log drain, so `row_stamp == row_epoch - 1` reads as
    /// "touched during the round that just ended".
    row_epoch: u32,

    class_head: Vec<u32>,
    class_tail: Vec<u32>,
    class_len: Vec<u32>,
    /// Head/tail of the class's parent back-edge list.
    parent_head: Vec<u32>,
    parent_tail: Vec<u32>,

    /// Parent back-edges: `edge_row[e]` has the owning class among its children.
    edge_row: Vec<u32>,
    edge_next: Vec<u32>,

    /// Hash-cons: `(label, children)` bucket -> the rows in it. A collision only
    /// shares a bucket; identity is the label id plus equal child slices.
    memo: FxHashMap<u64, Vec<RowId>>,
    /// One hash-cons layer per open scope, innermost last. A scope never writes
    /// the base table, so a pop restores it by dropping the layer.
    scope_memo: Vec<FxHashMap<u64, Vec<RowId>>>,
    /// [`Label::op_key`] bucket -> the rows that minted a class under it, in
    /// minting order. Over-approximates the classes holding the operator, never
    /// misses one.
    op_rows: FxHashMap<u64, Vec<RowId>>,

    /// Reusable per-class "already seen" marks for the read-side sweeps, so a
    /// query that visits a handful of classes does not first zero an array the
    /// size of the graph.
    marks: RefCell<Marks>,
    /// Scratch for walks that write back through `&mut self`.
    edge_scratch: Vec<u32>,
    /// Scratch for the back-edges one rebuild pass detaches and puts back.
    taken_parents: Vec<(ClassId, Vec<u32>)>,
    /// Rows a rebuild pass found to be duplicates of a lower row.
    dead_rows: Marks,
    /// Classes a union touched, awaiting congruence repair.
    pending: Vec<ClassId>,
    stats: Stats,
    total_nodes: usize,
    num_classes: usize,

    /// Classes changed since the last [`Self::take_changed`], possibly
    /// non-canonical — semi-naive saturation's frontier. A round touches the
    /// same class many times, so entries are deduplicated as they are logged.
    changed: Vec<ClassId>,
    /// Per class, the [`Self::changed_epoch`] it was last logged in.
    changed_at: Vec<u32>,
    changed_epoch: u32,
    changed_all: bool,

    scopes: Vec<Frame>,
    /// Per open scope, the classes minted inside it. A popped scope's classes
    /// keep their ids but stop being the scope's business, so this cannot be
    /// read off the id range.
    minted: Vec<Vec<ClassId>>,
    undo: Vec<Undo>,
    /// The constant a class is known to be: seeded by every literal row, raised
    /// by a scope's assumption, joined by a union.
    consts: Column<LabelId>,
    /// The type a class's terms carry, seeded by every typed row. Congruence
    /// already forces a class's rows to agree on it, so the first row to say
    /// wins and a merge does not make the answer depend on merge order.
    types: Column<u64>,
    /// Per open scope, the base reps grouped under their scoped rep; merged
    /// groups only. Cloned on push, so a nested frame keeps naming base reps.
    scope_members: Vec<FxHashMap<ClassId, Vec<ClassId>>>,
    /// The read view of the innermost scope: [`Self::scope_members`] as of the
    /// last refresh, which is a scope push, pop, or rebuild. Reading e-nodes
    /// through a snapshot rather than the live partition is what makes a class
    /// hold still while a round applies its matches — the order in which a
    /// hypothesis's classes are then re-searched is observable, so the snapshot
    /// is part of the contract, not an artifact of how it was aggregated.
    view: FxHashMap<ClassId, Vec<ClassId>>,
}

/// Cumulative engine work, for the saturation counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub merges: usize,
    pub adds: usize,
    pub repairs: usize,
}

/// What a scope's state was when it opened; everything after it is truncated.
struct Frame {
    rows: usize,
    classes: usize,
    edges: usize,
    undo: usize,
    pending: Vec<ClassId>,
    total_nodes: usize,
    num_classes: usize,
    /// A scope leaves the base graph structurally identical, so what changed
    /// since the last drain is, at the pop, what it was at the push.
    changed: Vec<ClassId>,
    changed_all: bool,
}

/// A field a scope overwrote, with the value to put back. Only logged while a
/// scope is open.
enum Undo {
    ParentList { class: u32, head: u32, tail: u32 },
    EdgeNext { edge: u32, next: u32 },
    OpBucket { op: u64 },
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
            uf: UnionFind::new(),
            row_label: Vec::new(),
            row_class: Vec::new(),
            row_start: vec![0],
            children: Vec::new(),
            node: Vec::new(),
            row_next: Vec::new(),
            row_stamp: Vec::new(),
            row_epoch: 1,
            class_head: Vec::new(),
            class_tail: Vec::new(),
            class_len: Vec::new(),
            parent_head: Vec::new(),
            parent_tail: Vec::new(),
            edge_row: Vec::new(),
            edge_next: Vec::new(),
            memo: FxHashMap::default(),
            scope_memo: Vec::new(),
            op_rows: FxHashMap::default(),
            marks: RefCell::default(),
            edge_scratch: Vec::new(),
            taken_parents: Vec::new(),
            dead_rows: Marks::default(),
            pending: Vec::new(),
            stats: Stats::default(),
            total_nodes: 0,
            num_classes: 0,
            changed: Vec::new(),
            changed_at: Vec::new(),
            changed_epoch: 1,
            changed_all: true,
            scopes: Vec::new(),
            minted: Vec::new(),
            undo: Vec::new(),
            consts: Column::new(Join::Agree),
            types: Column::new(Join::First),
            scope_members: Vec::new(),
            view: FxHashMap::default(),
        }
    }

    // ---- reading ----------------------------------------------------------

    pub fn find(&self, id: ClassId) -> ClassId {
        self.uf.find(id)
    }

    pub fn connected(&self, a: ClassId, b: ClassId) -> bool {
        self.find(a) == self.find(b)
    }

    pub fn is_empty(&self) -> bool {
        self.num_classes == 0
    }

    /// Total e-nodes across all classes. Congruent duplicates are never removed,
    /// so this only ever grows within a context — the fixpoint test both
    /// saturation drivers use reads it.
    pub fn total_size(&self) -> usize {
        self.total_nodes
    }

    pub fn num_classes(&self) -> usize {
        self.num_classes
    }

    /// One past the highest class id ever minted, live or not — the size a table
    /// indexed by class id needs.
    pub fn class_count(&self) -> usize {
        self.uf.len()
    }

    /// Bytes the columns hold: the row arrays, the child array, the per-class
    /// arrays and the interned labels. Excludes the hash-cons and the operator
    /// index, which are rebuilt rather than owned. An estimate for ranking, not
    /// an allocator total.
    pub fn approx_bytes(&self) -> usize {
        let rows = self.node.len();
        let per_row = size_of::<L>() + 4 * size_of::<u32>();
        rows * per_row
            + self.children.len() * size_of::<u32>()
            + self.uf.len() * 7 * size_of::<u32>()
            + self.edge_row.len() * 2 * size_of::<u32>()
            + self.labels.len() * size_of::<L>()
    }

    /// Work done since the engine was built. A saturation round reads the
    /// difference across it; a scope does not roll these back, since they count
    /// work, not state.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn in_scope(&self) -> bool {
        !self.scopes.is_empty()
    }

    /// Child classes of a row, canonical as of its insert.
    pub fn children(&self, row: RowId) -> &[ClassId] {
        let start = self.row_start[row.index()] as usize;
        let end = self.row_start[row.index() + 1] as usize;
        &self.children[start..end]
    }

    pub fn label(&self, row: RowId) -> LabelId {
        self.row_label[row.index()]
    }

    pub fn node(&self, row: RowId) -> &L {
        &self.node[row.index()]
    }

    /// The rows of `id`'s class, in the order the scalar engine held its nodes:
    /// insertion order, a union appending the absorbed class's rows after the
    /// survivor's. Extraction and the matcher break ties by this order, so it is
    /// part of the contract — a rebuild must not re-sort it.
    pub fn rows(&self, id: ClassId) -> Rows<'_, L> {
        let root = self.find(id);
        // A scope leaves the base class lists alone, so a scoped class is the
        // concatenation of its members' lists, ascending.
        self.rows_of(root, self.viewed_members(root))
    }

    /// The rows a class holds under the live partition, without canonicalizing
    /// it first — what a class that is about to stop being a root still owns.
    fn raw_rows(&self, class: ClassId) -> Rows<'_, L> {
        self.rows_of(class, self.scope_members(class))
    }

    fn rows_of<'a>(&'a self, class: ClassId, members: &'a [ClassId]) -> Rows<'a, L> {
        Rows {
            engine: self,
            cursor: self.class_head[members.first().copied().unwrap_or(class).index()],
            members,
            next_member: 1,
        }
    }

    /// E-nodes of `id`'s class; child ids may be non-canonical — resolve with
    /// [`Self::find`].
    pub fn nodes(&self, id: ClassId) -> impl Iterator<Item = &L> + Clone {
        self.rows(id).map(|row| &self.node[row.index()])
    }

    pub fn class_len(&self, id: ClassId) -> usize {
        let root = self.find(id);
        match self.viewed_members(root) {
            [] => self.class_len[root.index()] as usize,
            members => members
                .iter()
                .map(|m| self.class_len[m.index()] as usize)
                .sum(),
        }
    }

    /// Every live class, at the position of its lowest member: ascending id in
    /// the base graph, and under a scope the position of the group's lowest base
    /// rep. Extraction and bare-variable-rooted patterns walk classes in this
    /// order and break ties by it, so it is part of the contract.
    pub fn class_ids(&self) -> impl Iterator<Item = ClassId> + '_ {
        (0..self.uf.len() as u32).map(ClassId).filter_map(|id| {
            let root = self.find(id);
            if self.class_len[root.index()] == 0 {
                return None;
            }
            match self.viewed_members(root) {
                [] => (root == id).then_some(id),
                group => (Some(&id) == group.iter().min()).then_some(root),
            }
        })
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

    /// Canonical classes holding a node in the `op` bucket, each once, in
    /// minting order. Over-approximates — callers confirm with the label.
    pub fn classes_with_op(&self, op: u64) -> Vec<ClassId> {
        let Some(rows) = self.op_rows.get(&op) else {
            return Vec::new();
        };
        let mut seen = self.marks.borrow_mut();
        seen.begin(self.uf.len());
        let mut out = Vec::new();
        for &row in rows {
            let root = self.find(self.row_class[row.index()]);
            if seen.insert(root.index()) {
                out.push(root);
            }
        }
        out
    }

    /// Class of an already-interned `node`, or `None` (never inserts; always
    /// `None` for a unique node).
    pub fn lookup(&self, node: &L) -> Option<ClassId> {
        if node.is_unique() {
            return None;
        }
        let label = self.labels.get(node)?;
        let children: Vec<ClassId> = node.children().iter().map(|&c| self.find(c)).collect();
        self.memo_find(label, &children)
    }

    // ---- writing ----------------------------------------------------------

    /// Intern `node`, returning its class. A non-unique node equal to an
    /// existing one shares its class; otherwise a fresh class.
    pub fn add(&mut self, mut node: L) -> ClassId {
        for child in node.children_mut() {
            *child = self.uf.find(*child);
        }
        if !node.is_unique() {
            let label = self.labels.get(&node);
            if let Some(label) = label
                && let Some(class) = self.memo_find(label, node.children())
            {
                return class;
            }
        }
        self.make_class(node)
    }

    /// Merge the classes of `a` and `b`, returning the survivor. Congruence
    /// repair is deferred to [`Self::rebuild`]; the merge itself is visible
    /// immediately, so an applier that unions and then instantiates hash-conses
    /// against the result.
    ///
    /// A union inside a scope must be followed by [`Self::rebuild`] before the
    /// next [`Self::take_changed`]. Until that rebuild refreshes the read view,
    /// the merge is invisible to every query, so a drain that ran in between
    /// would retire the round that made those e-nodes readable and semi-naive
    /// would skip the matches they enable. Every caller obeys this; nothing
    /// enforces it.
    pub fn union(&mut self, a: ClassId, b: ClassId) -> ClassId {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        self.stats.merges += 1;
        let survivor = self.uf.union(ra, rb);
        if crate::trace_enabled() {
            eprintln!("U {} {} -> {}", ra.0, rb.0, survivor.0);
        }
        let absorbed = if survivor == ra { rb } else { ra };
        // A merge makes two sets of rows readable where they were not before:
        // the absorbed class's own rows, which a query reaching the survivor now
        // enumerates, and every row naming the absorbed class as a child, which
        // now names the survivor. Congruence repair rewrites the second set's
        // columns later (never, inside a scope), but the matcher may run first,
        // and semi-naive has to see both as new either way.
        self.mark_merged_new(absorbed);
        if self.consts.merge(absorbed, survivor, self.row_epoch)
            | self.types.merge(absorbed, survivor, self.row_epoch)
        {
            self.log_change(survivor);
        }
        if !self.in_scope() {
            self.splice_class(survivor, absorbed);
            self.splice_parents(survivor, absorbed);
        }
        if let Some(frame) = self.scope_members.last_mut() {
            let taken = frame.remove(&absorbed).unwrap_or_else(|| vec![absorbed]);
            frame
                .entry(survivor)
                .or_insert_with(|| vec![survivor])
                .extend(taken);
        }
        self.num_classes -= 1;
        self.pending.push(survivor);
        self.log_change(survivor);
        survivor
    }

    /// Restore congruence to a fixpoint after a batch of unions. Each round
    /// canonicalizes `pending` to reps and deduplicates first: without it a
    /// survivor queued many times would re-repair its growing parent list once
    /// per queueing, making rebuild quadratic.
    pub fn rebuild(&mut self) {
        if self.in_scope() {
            self.rebuild_scope();
            return;
        }
        while !self.pending.is_empty() {
            let todo = self.drain_pending();
            self.repair(&todo);
        }
        self.uf.flatten();
    }

    fn drain_pending(&mut self) -> Vec<ClassId> {
        let mut todo = std::mem::take(&mut self.pending);
        for id in &mut todo {
            *id = self.find(*id);
        }
        todo.sort_unstable();
        todo.dedup();
        todo
    }

    /// Congruence repair as bulk passes over the columns: canonicalize, sort,
    /// group.
    ///
    /// The rows that name a repaired class as a child are gathered, their child
    /// and class columns rewritten through the union-find in one loop, then
    /// sorted by `(label, children)` so congruent rows land in a run. Each run
    /// keeps its lowest row and merges the classes of the rest.
    ///
    /// Nothing here depends on the order the runs are visited in, or the order
    /// within one: the class a merge leaves standing is the smallest id in the
    /// set whichever way the merges are grouped, and the row a run keeps is its
    /// smallest. That is what a worklist walking one class's parent list at a
    /// time could not offer, and what a partitioned or parallel pass needs.
    fn repair(&mut self, classes: &[ClassId]) {
        let mut taken = std::mem::take(&mut self.taken_parents);
        taken.clear();
        for &class in classes {
            self.stats.repairs += 1;
            let edges = self.take_parents(class);
            taken.push((class, edges));
        }

        // The hash-cons is keyed on the children a row was interned with, so the
        // stale entries have to go before those children move.
        // By row, not by back-edge: a row with two distinct child classes sits on
        // both their lists, and a run that saw it twice would read it as
        // congruent to itself and retire it.
        let mut order: Vec<RowId> = taken
            .iter()
            .flat_map(|(_, edges)| edges)
            .map(|&edge| self.row(edge))
            .collect();
        order.sort_unstable();
        order.dedup();

        for &row in &order {
            if !self.node[row.index()].is_unique() {
                self.memo_remove(row);
            }
        }
        for &row in &order {
            if self.canonicalize_row(row) {
                self.log_change(self.row_class[row.index()]);
            }
            self.row_class[row.index()] = self.find(self.row_class[row.index()]);
        }
        order.retain(|&row| !self.node[row.index()].is_unique());
        order.sort_unstable_by(|&a, &b| {
            self.row_label[a.index()]
                .cmp(&self.row_label[b.index()])
                .then_with(|| self.children(a).cmp(self.children(b)))
                .then(a.cmp(&b))
        });

        self.dead_rows.begin(self.node.len());
        let mut run = 0;
        while run < order.len() {
            let first = order[run];
            let mut next = run + 1;
            while next < order.len() && self.rows_congruent(first, order[next]) {
                let other = order[next];
                self.dead_rows.insert(other.index());
                let (a, b) = (self.row_class[first.index()], self.row_class[other.index()]);
                self.union(a, b);
                next += 1;
            }
            let key = self.row_hash(first);
            self.memo_insert(key, first);
            run = next;
        }

        // Put each class's back-edges back, minus the rows a run absorbed, and
        // onto whatever class it is part of now. Extend rather than assign: a
        // union above may have spliced parents onto it already.
        for (class, edges) in taken.iter_mut() {
            edges.retain(|&edge| !self.dead_rows.contains(self.row(edge).index()));
            let root = self.find(*class);
            let edges = std::mem::take(edges);
            self.append_parents(root, edges);
        }
        self.taken_parents = taken;
    }

    fn row(&self, edge: u32) -> RowId {
        RowId(self.edge_row[edge as usize])
    }

    /// Congruence repair inside a scope. The base rows, hash-cons and columns
    /// stay read-only: a scoped survivor is a base class id, so writing scoped
    /// canonical ids into a base row would leave it naming a different class
    /// once the scope pops. Canonicalization happens in a scratch buffer against
    /// a hash-cons that lives only as long as the rebuild.
    fn rebuild_scope(&mut self) {
        let mut memo: FxHashMap<u64, Vec<(LabelId, Vec<ClassId>, ClassId)>> = FxHashMap::default();
        let mut scratch: Vec<ClassId> = Vec::new();
        while !self.pending.is_empty() {
            for rep in self.drain_pending() {
                let rep = self.find(rep);
                let edges: Vec<u32> = self.parent_edges(rep).collect();
                for edge in edges {
                    let row = RowId(self.edge_row[edge as usize]);
                    if self.node[row.index()].is_unique() {
                        continue;
                    }
                    scratch.clear();
                    scratch.extend(self.children(row).iter().map(|&c| self.uf.find(c)));
                    let label = self.row_label[row.index()];
                    let class = self.find(self.row_class[row.index()]);
                    let key = hash_row(label, &scratch);
                    let bucket = memo.entry(key).or_default();
                    let congruent = bucket
                        .iter()
                        .find(|(l, c, _)| *l == label && c == &scratch)
                        .map(|&(_, _, id)| id);
                    match congruent {
                        Some(other) => {
                            let other = self.find(other);
                            if other != class {
                                self.union(other, class);
                            }
                        }
                        None => bucket.push((label, scratch.clone(), class)),
                    }
                }
            }
        }
        self.uf.flatten();
        self.refresh_view();
    }

    /// Re-aggregate the read view from the innermost scope frame, whose groups
    /// it sorts in place.
    fn refresh_view(&mut self) {
        self.view.clear();
        let Some(frame) = self.scope_members.last_mut() else {
            return;
        };
        for (&rep, members) in frame.iter_mut() {
            members.sort_unstable();
            members.dedup();
            self.view.insert(rep, members.clone());
        }
    }

    // ---- rows -------------------------------------------------------------

    fn make_class(&mut self, node: L) -> ClassId {
        let op_key = node.op_key();
        let unique = node.is_unique();
        let constant = node.is_constant();
        let type_key = node.type_key();
        let row = self.push_row(node);
        let class = self.uf.push();
        self.class_head.push(row.0);
        self.class_tail.push(row.0);
        self.class_len.push(1);
        self.parent_head.push(NONE);
        self.parent_tail.push(NONE);
        self.row_class[row.index()] = class;

        let mut seen: Vec<ClassId> = Vec::new();
        for i in 0..self.children(row).len() {
            let child = self.find(self.children(row)[i]);
            if !seen.contains(&child) {
                seen.push(child);
                self.push_edge(child, row);
            }
        }
        if !unique {
            let key = self.row_hash(row);
            self.memo_insert(key, row);
        }
        self.op_rows.entry(op_key).or_default().push(row);
        if let Some(minted) = self.minted.last_mut() {
            minted.push(class);
            self.undo.push(Undo::OpBucket { op: op_key });
        }
        if constant {
            let label = self.row_label[row.index()];
            self.consts.raise(class, label, self.row_epoch);
        }
        if let Some(key) = type_key {
            self.types.raise(class, key, self.row_epoch);
        }
        self.total_nodes += 1;
        self.num_classes += 1;
        self.stats.adds += 1;
        if crate::trace_enabled() {
            eprintln!(
                "A {} {:?} {:?}",
                class.0,
                self.node(row),
                self.children(row).iter().map(|c| c.0).collect::<Vec<_>>()
            );
        }
        self.log_change(class);
        class
    }

    /// Append a row for `node`, whose children are already canonical.
    fn push_row(&mut self, node: L) -> RowId {
        let row = RowId(self.row_label.len() as u32);
        self.row_label.push(self.labels.intern(&node));
        self.row_class.push(ClassId(0));
        self.children.extend_from_slice(node.children());
        self.row_start.push(self.children.len() as u32);
        self.row_next.push(NONE);
        self.row_stamp.push(self.row_epoch);
        self.node.push(node);
        row
    }

    /// Rewrite a row's child column to canonical ids; reports whether any moved.
    fn canonicalize_row(&mut self, row: RowId) -> bool {
        let start = self.row_start[row.index()] as usize;
        let end = self.row_start[row.index() + 1] as usize;
        let mut moved = false;
        for i in start..end {
            let root = self.uf.find(self.children[i]);
            if root != self.children[i] {
                self.children[i] = root;
                moved = true;
            }
        }
        if moved {
            self.row_stamp[row.index()] = self.row_epoch;
        }
        moved
    }

    fn row_hash(&self, row: RowId) -> u64 {
        hash_row(self.row_label[row.index()], self.children(row))
    }

    fn rows_congruent(&self, a: RowId, b: RowId) -> bool {
        self.row_label[a.index()] == self.row_label[b.index()]
            && self.children(a) == self.children(b)
    }

    // ---- hash-cons --------------------------------------------------------

    fn memo_find(&self, label: LabelId, children: &[ClassId]) -> Option<ClassId> {
        let key = hash_row(label, children);
        let hit =
            |table: &FxHashMap<u64, Vec<RowId>>| {
                table.get(&key)?.iter().copied().find(|&row| {
                    self.row_label[row.index()] == label && self.children(row) == children
                })
            };
        for table in self.scope_memo.iter().rev() {
            if let Some(row) = hit(table) {
                return Some(self.find(self.row_class[row.index()]));
            }
        }
        hit(&self.memo).map(|row| self.find(self.row_class[row.index()]))
    }

    fn memo_insert(&mut self, key: u64, row: RowId) {
        let table = self.scope_memo.last_mut().unwrap_or(&mut self.memo);
        let bucket = table.entry(key).or_default();
        if !bucket.contains(&row) {
            bucket.push(row);
        }
    }

    /// Drop `row`'s (possibly stale) base hash-cons entry. Only ever called
    /// outside a scope — a scoped rebuild never touches the base table.
    fn memo_remove(&mut self, row: RowId) {
        let key = self.row_hash(row);
        let Some(bucket) = self.memo.get_mut(&key) else {
            return;
        };
        if let Some(pos) = bucket.iter().position(|&r| r == row) {
            bucket.swap_remove(pos);
        }
        if bucket.is_empty() {
            self.memo.remove(&key);
        }
    }

    // ---- intrusive lists --------------------------------------------------

    /// Move the absorbed class's rows onto the end of the survivor's list, which
    /// is where the scalar engine's `nodes.append` put them. Only outside a
    /// scope: a scope leaves the base lists alone and aggregates over members.
    fn splice_class(&mut self, survivor: ClassId, absorbed: ClassId) {
        let (head, tail, len) = (
            self.class_head[absorbed.index()],
            self.class_tail[absorbed.index()],
            self.class_len[absorbed.index()],
        );
        if head != NONE {
            let survivor_tail = self.class_tail[survivor.index()];
            if survivor_tail == NONE {
                self.class_head[survivor.index()] = head;
            } else {
                self.row_next[survivor_tail as usize] = head;
            }
            self.class_tail[survivor.index()] = tail;
            self.class_len[survivor.index()] += len;
        }
        self.class_head[absorbed.index()] = NONE;
        self.class_tail[absorbed.index()] = NONE;
        self.class_len[absorbed.index()] = 0;
    }

    fn splice_parents(&mut self, survivor: ClassId, absorbed: ClassId) {
        let (head, tail) = (
            self.parent_head[absorbed.index()],
            self.parent_tail[absorbed.index()],
        );
        if head != NONE {
            let survivor_tail = self.parent_tail[survivor.index()];
            if survivor_tail == NONE {
                self.parent_head[survivor.index()] = head;
            } else {
                self.edge_next[survivor_tail as usize] = head;
            }
            self.parent_tail[survivor.index()] = tail;
        }
        self.parent_head[absorbed.index()] = NONE;
        self.parent_tail[absorbed.index()] = NONE;
    }

    fn push_edge(&mut self, class: ClassId, row: RowId) {
        let edge = self.edge_row.len() as u32;
        self.edge_row.push(row.0);
        self.edge_next.push(NONE);
        self.append_edge(class, edge, edge);
    }

    fn append_edge(&mut self, class: ClassId, head: u32, tail: u32) {
        if self.in_scope() {
            self.undo.push(Undo::ParentList {
                class: class.0,
                head: self.parent_head[class.index()],
                tail: self.parent_tail[class.index()],
            });
        }
        let old_tail = self.parent_tail[class.index()];
        if old_tail == NONE {
            self.parent_head[class.index()] = head;
        } else {
            if self.in_scope() {
                self.undo.push(Undo::EdgeNext {
                    edge: old_tail,
                    next: self.edge_next[old_tail as usize],
                });
            }
            self.edge_next[old_tail as usize] = head;
        }
        self.parent_tail[class.index()] = tail;
    }

    fn parent_edges(&self, class: ClassId) -> Edges<'_, L> {
        let members = self.scope_members(class);
        Edges {
            engine: self,
            cursor: self.parent_head[members.first().copied().unwrap_or(class).index()],
            members,
            next_member: 1,
        }
    }

    /// Detach and return `class`'s parent back-edges, so a repair can rebuild
    /// the list while [`Self::union`] appends to it.
    fn take_parents(&mut self, class: ClassId) -> Vec<u32> {
        debug_assert!(!self.in_scope(), "repair never runs under a scope");
        let edges: Vec<u32> = self.parent_edges(class).collect();
        self.parent_head[class.index()] = NONE;
        self.parent_tail[class.index()] = NONE;
        edges
    }

    fn append_parents(&mut self, class: ClassId, edges: Vec<u32>) {
        debug_assert!(!self.in_scope(), "repair never runs under a scope");
        for edge in edges {
            self.edge_next[edge as usize] = NONE;
            self.append_edge(class, edge, edge);
        }
    }

    // ---- change log -------------------------------------------------------

    fn log_change(&mut self, id: ClassId) {
        if self.changed_at.len() <= id.index() {
            self.changed_at.resize(id.index() + 1, 0);
        }
        if self.changed_at[id.index()] != self.changed_epoch {
            self.changed_at[id.index()] = self.changed_epoch;
            self.changed.push(id);
        }
    }

    /// Whether `row` was appended or re-canonicalized during the round the last
    /// [`Self::take_changed`] closed. A match all of whose rows are older than
    /// that already existed when the round before it ran, so a rule whose match
    /// predicate reads nothing else has applied it already.
    pub fn row_is_new(&self, row: RowId) -> bool {
        self.row_stamp[row.index()] + 1 == self.row_epoch
    }

    fn mark_merged_new(&mut self, absorbed: ClassId) {
        let mut scratch = std::mem::take(&mut self.edge_scratch);
        scratch.clear();
        scratch.extend(
            self.parent_edges(absorbed)
                .map(|e| self.edge_row[e as usize]),
        );
        scratch.extend(self.raw_rows(absorbed).map(|row| row.0));
        for &row in &scratch {
            self.row_stamp[row as usize] = self.row_epoch;
        }
        self.edge_scratch = scratch;
    }

    /// Drain the change log: the canonical classes changed since the previous
    /// call, ascending and deduplicated; `None` means "every class".
    pub fn take_changed(&mut self) -> Option<Vec<ClassId>> {
        self.row_epoch = self.row_epoch.wrapping_add(1);
        if self.row_epoch <= 1 {
            self.row_stamp.fill(0);
            self.row_epoch = 2;
        }
        let all = std::mem::replace(&mut self.changed_all, false);
        let mut changed = std::mem::take(&mut self.changed);
        self.bump_epoch();
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
        self.changed_all = true;
    }

    /// Every stamp of the current epoch becomes stale. Wrapping past an epoch a
    /// stamp still holds would resurrect it, so the wrap clears them instead.
    fn bump_epoch(&mut self) {
        self.changed_epoch = self.changed_epoch.wrapping_add(1);
        if self.changed_epoch == 0 {
            self.changed_at.fill(0);
            self.changed_epoch = 1;
        }
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
        let label = self.labels.intern(&node);
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

    /// Whether `class` was proven two different constants — a refuted scope.
    pub fn const_conflicted(&self, class: ClassId) -> bool {
        self.consts.is_conflicted(self.find(class))
    }

    /// The classes known to be `node`.
    pub fn classes_with_const<'a>(&'a self, node: &L) -> impl Iterator<Item = ClassId> + 'a {
        self.labels
            .get(node)
            .into_iter()
            .flat_map(|label| self.consts.classes_with(label))
    }

    /// The type every term of `class` carries, as the language spells it.
    /// `None` when no row of the class is typed.
    pub fn type_of(&self, class: ClassId) -> Option<u64> {
        self.types.get(self.find(class))
    }

    /// The node interned under `label`.
    pub fn label_node(&self, label: LabelId) -> Option<&L> {
        (label.index() < self.labels.len()).then(|| self.labels.node(label))
    }

    /// `class`'s value in `column`, as one word.
    pub fn fact(&self, column: ColumnId, class: ClassId) -> Option<u64> {
        let class = self.find(class);
        match column {
            ColumnId::Const => self.consts.get(class).map(|label| label.0 as u64),
            ColumnId::Type => self.types.get(class),
        }
    }

    /// Whether `class`'s value in `column` rose during the round the last
    /// [`Self::take_changed`] closed — the fact-level [`Self::row_is_new`].
    pub fn fact_is_new(&self, column: ColumnId, class: ClassId) -> bool {
        let class = self.find(class);
        match column {
            ColumnId::Const => self.consts.is_new(class, self.row_epoch),
            ColumnId::Type => self.types.is_new(class, self.row_epoch),
        }
    }

    fn raise_const(&mut self, class: ClassId, label: LabelId) {
        if self.consts.raise(class, label, self.row_epoch) {
            self.log_change(class);
        }
    }

    // ---- scopes -----------------------------------------------------------

    /// Enter an assumption scope: everything until the matching
    /// [`Self::pop_context`] is undone by it.
    pub fn push_context(&mut self) {
        let members = self.scope_members.last().cloned().unwrap_or_default();
        self.scopes.push(Frame {
            rows: self.row_label.len(),
            classes: self.uf.len(),
            edges: self.edge_row.len(),
            undo: self.undo.len(),
            pending: self.pending.clone(),
            total_nodes: self.total_nodes,
            num_classes: self.num_classes,
            changed: self.changed.clone(),
            changed_all: self.changed_all,
        });
        self.uf.push_scope();
        self.scope_memo.push(FxHashMap::default());
        self.consts.push_scope();
        self.types.push_scope();
        self.scope_members.push(members);
        self.minted.push(Vec::new());
        self.refresh_view();
    }

    /// Leave the scope, discarding its unions, its rows, and its assumptions;
    /// the enclosing scope (or the base graph) is restored without a rebuild.
    pub fn pop_context(&mut self) {
        let frame = self.scopes.pop().expect("open scope");
        self.consts.pop_scope();
        self.types.pop_scope();
        for entry in self.undo.drain(frame.undo..).rev() {
            match entry {
                Undo::ParentList { class, head, tail } => {
                    self.parent_head[class as usize] = head;
                    self.parent_tail[class as usize] = tail;
                }
                Undo::EdgeNext { edge, next } => self.edge_next[edge as usize] = next,
                Undo::OpBucket { op } => {
                    if let Some(bucket) = self.op_rows.get_mut(&op) {
                        bucket.pop();
                        if bucket.is_empty() {
                            self.op_rows.remove(&op);
                        }
                    }
                }
            }
        }
        self.uf.pop_scope();
        self.row_label.truncate(frame.rows);
        self.row_class.truncate(frame.rows);
        self.row_start.truncate(frame.rows + 1);
        self.children.truncate(self.row_start[frame.rows] as usize);
        self.node.truncate(frame.rows);
        self.row_next.truncate(frame.rows);
        self.row_stamp.truncate(frame.rows);
        self.edge_row.truncate(frame.edges);
        self.edge_next.truncate(frame.edges);
        // The classes the scope minted keep their ids, so a caller still holding
        // one finds it, but they lose every row the scope gave them and stop
        // being classes: `class_ids` skips a root with no rows.
        for class in frame.classes..self.uf.len() {
            self.class_head[class] = NONE;
            self.class_tail[class] = NONE;
            self.class_len[class] = 0;
            self.parent_head[class] = NONE;
            self.parent_tail[class] = NONE;
        }
        self.scope_memo.pop();
        self.scope_members.pop();
        self.minted.pop();
        self.pending = frame.pending;
        self.total_nodes = frame.total_nodes;
        self.num_classes = frame.num_classes;
        self.refresh_view();
        self.restore_changed(frame.changed, frame.changed_all);
    }

    /// Put the change log back the way the just-popped scope found it. A fresh
    /// epoch invalidates the scope's stamps in one step; the restored ids are
    /// then re-stamped, so log and stamps agree again.
    fn restore_changed(&mut self, changed: Vec<ClassId>, all: bool) {
        self.bump_epoch();
        for id in &changed {
            self.changed_at[id.index()] = self.changed_epoch;
        }
        self.changed = changed;
        self.changed_all = all;
    }

    /// The groups of the read view: [`Self::scope_members`] as of the last
    /// refresh. Empty for a class the view does not merge, which is then read
    /// straight from its own row list.
    fn viewed_members(&self, id: ClassId) -> &[ClassId] {
        self.view.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The base class ids the open scope groups under the scoped-canonical `id`.
    /// Empty when no scope is open or `id` is not a scoped representative — then
    /// `id` is itself the base rep. Side tables built against the base graph are
    /// keyed by base reps, so a query made under a scope aggregates over this.
    pub fn scope_members(&self, id: ClassId) -> &[ClassId] {
        self.scope_members
            .last()
            .and_then(|frame| frame.get(&id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Canonical classes the open scopes changed: the ones their unions merged,
    /// the ones minted inside them, and transitively every class holding a node
    /// with such a child. Ascending id. Empty with no scope open.
    pub fn scope_dirty(&self) -> Vec<ClassId> {
        if !self.in_scope() {
            return Vec::new();
        }
        let seeds: Vec<ClassId> = self
            .minted
            .iter()
            .flatten()
            .copied()
            .chain(self.consts.scoped_keys())
            .chain(
                self.scope_members
                    .last()
                    .into_iter()
                    .flat_map(|frame| frame.keys().copied()),
            )
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

    /// `seeds` and everything reachable upward from them over parent edges in at
    /// most `levels` steps (unbounded when `None`), ascending.
    fn close_upward(&self, seeds: Vec<ClassId>, levels: Option<usize>) -> Vec<ClassId> {
        let mut seen = self.marks.borrow_mut();
        seen.begin(self.uf.len());
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
                for edge in self.parent_edges(id) {
                    let row = RowId(self.edge_row[edge as usize]);
                    let parent = self.find(self.row_class[row.index()]);
                    if seen.insert(parent.index()) {
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
}

/// Epoch-stamped membership marks over the class ids: `begin` costs nothing per
/// class, so a sweep pays only for what it visits.
#[derive(Default)]
struct Marks {
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
    fn insert(&mut self, id: usize) -> bool {
        let slot = &mut self.stamp[id];
        std::mem::replace(slot, self.epoch) != self.epoch
    }

    fn contains(&self, id: usize) -> bool {
        self.stamp[id] == self.epoch
    }
}

fn hash_row(label: LabelId, children: &[ClassId]) -> u64 {
    let mut h = FxHasher::default();
    label.0.hash(&mut h);
    children.hash(&mut h);
    h.finish()
}

/// A class, read through the columns. Not a struct the engine owns: an e-class
/// is a set of rows, and this is the cursor into it.
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

/// Walks a class's rows: its own intrusive membership list, or, under a scope
/// that merged it, its members' lists back to back.
pub struct Rows<'a, L: Label> {
    engine: &'a Engine<L>,
    cursor: u32,
    members: &'a [ClassId],
    next_member: usize,
}

impl<L: Label> Clone for Rows<'_, L> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine,
            cursor: self.cursor,
            members: self.members,
            next_member: self.next_member,
        }
    }
}

impl<L: Label> Iterator for Rows<'_, L> {
    type Item = RowId;

    fn next(&mut self) -> Option<RowId> {
        while self.cursor == NONE {
            let member = *self.members.get(self.next_member)?;
            self.next_member += 1;
            self.cursor = self.engine.class_head[member.index()];
        }
        let row = RowId(self.cursor);
        self.cursor = self.engine.row_next[row.index()];
        Some(row)
    }
}

/// The same walk over parent back-edges.
struct Edges<'a, L: Label> {
    engine: &'a Engine<L>,
    cursor: u32,
    members: &'a [ClassId],
    next_member: usize,
}

impl<L: Label> Iterator for Edges<'_, L> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        while self.cursor == NONE {
            let member = *self.members.get(self.next_member)?;
            self.next_member += 1;
            self.cursor = self.engine.parent_head[member.index()];
        }
        let edge = self.cursor;
        self.cursor = self.engine.edge_next[edge as usize];
        Some(edge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Term;
    use proptest::prelude::*;

    /// `f(g(x), y)` over fresh leaves, returning every class by name.
    fn seed() -> (Engine<Term>, [ClassId; 4]) {
        let mut eg = Engine::new();
        let x = eg.add(Term::leaf("x"));
        let y = eg.add(Term::leaf("y"));
        let g = eg.add(Term::op("g", &[x]));
        let f = eg.add(Term::op("f", &[g, y]));
        (eg, [x, y, g, f])
    }

    #[test]
    fn equal_terms_share_a_class() {
        let mut eg = Engine::new();
        let x = eg.add(Term::leaf("x"));
        assert_eq!(eg.add(Term::leaf("x")), x);
        let a = eg.add(Term::op("g", &[x]));
        assert_eq!(eg.add(Term::op("g", &[x])), a);
        assert_eq!(eg.num_classes(), 2);
        assert_eq!(eg.total_size(), 2);
    }

    #[test]
    fn unique_terms_never_share_a_class() {
        let mut eg = Engine::new();
        let a = eg.add(Term::unique("effect", &[]));
        let b = eg.add(Term::unique("effect", &[]));
        assert_ne!(a, b);
        assert_eq!(eg.lookup(&Term::unique("effect", &[])), None);
    }

    #[test]
    fn lookup_finds_an_interned_term_and_nothing_else() {
        let (eg, [x, _, g, _]) = seed();
        assert_eq!(eg.lookup(&Term::op("g", &[x])), Some(g));
        assert_eq!(eg.lookup(&Term::op("h", &[x])), None);
    }

    #[test]
    fn a_union_concatenates_the_classes_in_survivor_order() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let survivor = eg.union(a, b);
        assert_eq!(survivor, a, "the smaller id represents the merged set");
        let names: Vec<&str> = eg.nodes(survivor).map(|n| n.op.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(eg.num_classes(), 1);
        assert_eq!(eg.total_size(), 2);
    }

    #[test]
    fn rebuild_merges_congruent_parents() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let fa = eg.add(Term::op("f", &[a]));
        let fb = eg.add(Term::op("f", &[b]));
        assert_ne!(eg.find(fa), eg.find(fb));
        eg.union(a, b);
        eg.rebuild();
        assert_eq!(eg.find(fa), eg.find(fb));
    }

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

    #[test]
    fn changes_are_logged_once_per_round_and_drained() {
        let mut eg = Engine::new();
        assert_eq!(eg.take_changed(), None, "a fresh graph changed everything");
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        assert_eq!(eg.take_changed(), Some(vec![a, b]));
        assert_eq!(eg.take_changed(), Some(vec![]));
        let survivor = eg.union(a, b);
        assert_eq!(eg.take_changed(), Some(vec![survivor]));
    }

    #[test]
    fn a_repaired_parent_is_a_change() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let fb = eg.add(Term::op("f", &[b]));
        eg.take_changed();
        eg.union(b, a);
        eg.rebuild();
        let changed = eg.take_changed().expect("a named change");
        assert!(
            changed.contains(&eg.find(fb)),
            "f(b) re-canonicalized to f(a)"
        );
    }

    #[test]
    fn delta_closes_upward_by_height() {
        let mut eg = Engine::new();
        let x = eg.add(Term::leaf("x"));
        let h = eg.add(Term::op("h", &[x]));
        let g = eg.add(Term::op("g", &[h]));
        let f = eg.add(Term::op("f", &[g]));
        assert_eq!(eg.delta(&[x], 0), vec![x]);
        assert_eq!(eg.delta(&[x], 1), vec![x, h]);
        assert_eq!(eg.delta(&[x], 3), vec![x, h, g, f]);
    }

    #[test]
    fn a_scope_leaves_no_trace() {
        let (mut eg, [x, y, g, f]) = seed();
        eg.rebuild();
        let before = state(&eg);
        eg.push_context();
        eg.union(x, y);
        eg.add(Term::op("k", &[f]));
        eg.assume_const(g, Term::int(7));
        eg.rebuild();
        assert!(eg.connected(x, y));
        eg.pop_context();
        assert_eq!(state(&eg), before);
        assert!(!eg.connected(x, y));
        assert_eq!(eg.const_of(g), None);
    }

    #[test]
    fn a_scoped_lookup_stays_as_incomplete_as_the_base_hash_cons() {
        let mut eg = Engine::new();
        // `b` first, so the merge canonicalizes `a` onto it and the probe for
        // `f(a)` no longer spells the children `f(a)` was interned with.
        let b = eg.add(Term::leaf("b"));
        let a = eg.add(Term::leaf("a"));
        let fa = eg.add(Term::op("f", &[a]));
        eg.rebuild();
        eg.push_context();
        eg.union(a, b);
        eg.rebuild();
        // Under the hypothesis `f(a)` and `f(b)` are the same term, but the
        // base hash-cons is keyed by the children `f(a)` was interned with and
        // a scope never rewrites it — so both probes, canonicalized through the
        // scoped union-find, miss. That incompleteness is the scalar engine's,
        // and consumers are written against it.
        assert_eq!(eg.lookup(&Term::op("f", &[b])), None);
        assert_eq!(eg.lookup(&Term::op("f", &[a])), None);
        eg.pop_context();
        assert_eq!(eg.lookup(&Term::op("f", &[a])), Some(eg.find(fa)));
    }

    #[test]
    fn scope_members_name_base_reps_through_nesting() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let c = eg.add(Term::leaf("c"));
        eg.push_context();
        let outer = eg.union(a, b);
        eg.push_context();
        let inner = eg.union(outer, c);
        let mut members = eg.scope_members(inner).to_vec();
        members.sort_unstable();
        assert_eq!(members, vec![a, b, c]);
        eg.pop_context();
        let mut members = eg.scope_members(outer).to_vec();
        members.sort_unstable();
        assert_eq!(members, vec![a, b]);
        eg.pop_context();
        assert!(eg.scope_members(a).is_empty());
    }

    /// Everything a caller can observe, for the scope round-trip test.
    fn state(eg: &Engine<Term>) -> Vec<(u32, Vec<String>, Vec<Vec<u32>>)> {
        eg.class_ids()
            .map(|class| {
                (
                    class.0,
                    eg.nodes(class).map(|n| n.op.clone()).collect(),
                    eg.rows(class)
                        .map(|row| eg.children(row).iter().map(|c| eg.find(*c).0).collect())
                        .collect(),
                )
            })
            .collect()
    }

    /// A term built from a random program: `ops[i]` is applied to earlier ids.
    fn programs() -> impl Strategy<Value = Vec<(usize, Vec<usize>)>> {
        prop::collection::vec((0usize..4, prop::collection::vec(0usize..12, 0..3)), 1..24)
    }

    fn build(eg: &mut Engine<Term>, program: &[(usize, Vec<usize>)]) -> Vec<ClassId> {
        const OPS: [&str; 4] = ["a", "f", "g", "h"];
        let mut ids: Vec<ClassId> = Vec::new();
        for (op, args) in program {
            let children: Vec<ClassId> = args
                .iter()
                .filter_map(|&i| ids.get(i % ids.len().max(1)).copied())
                .collect();
            ids.push(eg.add(Term::op(OPS[*op], &children)));
        }
        ids
    }

    proptest! {
        #[test]
        fn rebuild_restores_the_functional_dependency(
            program in programs(),
            merges in prop::collection::vec((0usize..24, 0usize..24), 0..8),
        ) {
            let mut eg: Engine<Term> = Engine::new();
            let ids = build(&mut eg, &program);
            eg.rebuild();
            for (a, b) in merges {
                eg.union(ids[a % ids.len()], ids[b % ids.len()]);
            }
            eg.rebuild();
            // No two live rows share a label and canonical children in
            // different classes.
            let mut seen: std::collections::HashMap<(u32, Vec<u32>), u32> = Default::default();
            for class in eg.class_ids() {
                for row in eg.rows(class) {
                    if eg.node(row).is_unique() {
                        continue;
                    }
                    let key = (
                        eg.label(row).0,
                        eg.children(row).iter().map(|c| eg.find(*c).0).collect(),
                    );
                    let owner = eg.find(eg.row_class[row.index()]).0;
                    prop_assert_eq!(*seen.entry(key).or_insert(owner), owner);
                }
            }
        }

        #[test]
        fn the_same_program_builds_the_same_ids(program in programs()) {
            let mut one: Engine<Term> = Engine::new();
            let mut two: Engine<Term> = Engine::new();
            let a = build(&mut one, &program);
            let b = build(&mut two, &program);
            one.rebuild();
            two.rebuild();
            prop_assert_eq!(a, b);
            prop_assert_eq!(state(&one), state(&two));
        }

        #[test]
        fn a_scope_round_trip_restores_every_column(
            program in programs(),
            merges in prop::collection::vec((0usize..24, 0usize..24), 0..8),
        ) {
            let mut eg: Engine<Term> = Engine::new();
            let ids = build(&mut eg, &program);
            eg.rebuild();
            let before = state(&eg);
            eg.push_context();
            for (a, b) in merges {
                eg.union(ids[a % ids.len()], ids[b % ids.len()]);
            }
            eg.add(Term::op("f", &[ids[0]]));
            eg.rebuild();
            eg.pop_context();
            prop_assert_eq!(state(&eg), before);
        }
    }
}
