use std::collections::{BTreeMap, HashSet};
use std::marker::PhantomData;
use std::sync::Mutex;

use smallvec::SmallVec;

use tir_relational::query::Resolution;
use tir_relational::rule::Key;
use tir_relational::saturate::Caches;
use tir_relational::store::{Collision, Repair, RowView, Table, Values};
use tir_relational::unionfind::Merge;
use tir_relational::{Database, Join, LatticeId, Placement, SpaceId, Stats};

use crate::label::{Carrier, Labels, Laws};
use crate::{ClassId, ColumnId, FxHashMap, Label, LabelId, Ref, RowId};

/// Empty link in an intrusive list.
const NONE: u32 = u32::MAX;

/// A label bit: a commutative binary operator.
const COMMUTES: u8 = 1;
/// A label bit: no carrier, so no laws, and a key of classes alone.
const PLAIN: u8 = 2;
/// A label bit: a leaf integer constant of a carrier, which is a reference
/// and never a row.
const CONSTANT: u8 = 4;

/// The id space classes are minted in, the database's only one.
pub(crate) const CLASSES: SpaceId = SpaceId::nth(0);

/// The constant a class is known to be, as a label: seeded by every literal
/// row outside a carrier, raised by a scope's assumption, joined by a union.
///
/// The lattice columns are declared in the order a search from a risen fact
/// tries them, which is how rarely they rise: a constant, then a type, which
/// nearly every new class is minted with.
pub(crate) const CONSTS: LatticeId = LatticeId::nth(0);
/// The type a class's terms carry, seeded by every typed row. Congruence
/// already forces a class's rows to agree on it, so the first row to say wins
/// and a merge does not make the answer depend on merge order.
pub(crate) const TYPES: LatticeId = LatticeId::nth(1);
// The engine's lattice columns come first; a client's come after.
const OWN_COLUMNS: u32 = 2;

/// A union of two references that name one class at different offsets, or
/// two carriers' zeros. Nothing was merged; [`Engine::contradicted`] reports
/// it until the scope it happened in is popped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contradiction;

/// What a node comes to before its table is probed.
pub(crate) enum Shaped {
    /// Its laws settle it without a row.
    Ref(Ref),
    /// The key is built; `shift` is what the row's value is shifted by, and
    /// `exchanged` whether a commutative label's operands are keyed in the
    /// other order than given.
    Key { shift: u64, exchanged: bool },
}

/// `node`'s carrier and value, when it is an integer constant of one.
pub(crate) fn leaf_constant<L: Label>(node: &L) -> Option<(Carrier, u64)> {
    let carrier = node.carrier()?;
    let value = node.int_value()?;
    Some((
        carrier,
        value & tir_relational::unionfind::mask(carrier.width),
    ))
}

/// What a node with `laws` over `operands` is without a row, if its laws
/// settle it: an identity element at one operand makes it the other, and a
/// linear node over constants is the constant the coefficients sum them to.
/// `operands` are canonical; one with a coefficient may carry its offset or
/// have had it moved out, as long as the caller applies the same convention to
/// the answer.
fn fold<P: Placement + ?Sized>(
    placed: &P,
    zeros: &[(u64, ClassId)],
    laws: &Laws,
    operands: &[Ref],
) -> Option<Ref> {
    for (at, identity) in laws.identity.iter().enumerate() {
        let (Some(identity), Some(&element)) = (identity, operands.get(at)) else {
            continue;
        };
        if !placed.is_zero(element.class.0) {
            continue;
        }
        // op(x, zero + identity) == x, and a coefficient carries the rest.
        let shift = match laws.coefficient(at) {
            0 if element.offset == *identity => 0,
            0 => continue,
            coefficient => {
                (coefficient as u64).wrapping_mul(element.offset.wrapping_sub(*identity))
            }
        };
        let other = operands[1 - at];
        // An operand of another carrier cannot take the shift.
        if placed.mask(other.class.0) == laws.mask {
            return Some(Ref::new(
                other.class,
                other.offset.wrapping_add(shift) & laws.mask,
            ));
        }
    }
    if laws.linear()
        && operands
            .iter()
            .all(|operand| placed.is_zero(operand.class.0))
    {
        let key = laws.carrier?.key;
        let zero = zeros.iter().find(|&&(known, _)| known == key)?.1;
        let sum = operands
            .iter()
            .enumerate()
            .fold(laws.bias, |sum, (at, operand)| {
                sum.wrapping_add((laws.coefficient(at) as u64).wrapping_mul(operand.offset))
            });
        return Some(Ref::new(zero, sum & laws.mask));
    }
    None
}

/// What a table holds: rows of one arity, hash-consed or not, whose operands
/// carry these offset coefficients and, for commutative binary labels, are an
/// unordered pair. Labels that agree on it share a table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Shape {
    arity: usize,
    unique: bool,
    /// Empty when no operand has one.
    coefficients: SmallVec<[i64; 2]>,
    commutes: bool,
}

pub(crate) struct Terms<'a, L: Label> {
    db: &'a tir_relational::Core<Lists, 1>,
    nodes: &'a [L],
    labels: &'a Labels<L>,
    pub(crate) names: &'a L::Names,
}

impl<L: Label> Terms<'_, L> {
    pub(crate) fn node(&self, row: RowId) -> &L {
        &self.nodes[row.index()]
    }

    pub(crate) fn label_node(&self, label: LabelId) -> Option<&L> {
        (label.index() < self.labels.len()).then(|| self.labels.node(label))
    }

    pub(crate) fn is_literal(&self, reference: Ref, value: &L) -> Option<bool> {
        let (carrier, constant) = leaf_constant(value)?;
        let zeros = &self.db.client().zeros;
        let zero = zeros.iter().find(|&&(known, _)| known == carrier.key);
        let reference: tir_relational::Ref = reference.into();
        let found: Ref = self.db.find(CLASSES, reference).into();
        Some(zero.is_some_and(|&(_, zero)| found == Ref::new(zero, constant)))
    }
}

/// The labels a pass's graphs have met and what its saturations keep per
/// plan: what of an [`Engine`] is no one graph's. A pass owns it and lends it
/// to each function's engine, [`Engine::over`] taking it and
/// [`Engine::release`] handing it back, so a label has one id and one table
/// slot in every function of the pass, and a plan resolves a label once. A
/// label interned before the first function has that id in every copy of the
/// pass.
///
/// Ids and slots are handed out in the order the pass met them, so nothing
/// reads labels or tables in id order: a search reads labels by
/// [`Label::sort_key`] and tables by what they hold, and a graph's results
/// are those of a vocabulary that met its labels alone.
pub struct Vocabulary<L: Label> {
    names: L::Names,
    labels: Labels<L>,
    keyed: BTreeMap<L::SortKey, LabelId>,
    // Only how two ranks compare means anything: a label takes one between its
    // neighbours'.
    label_rank: Vec<u64>,
    /// [`Label::op_key`] -> the labels under it, by rank.
    op_labels: FxHashMap<u64, Vec<LabelId>>,
    /// Per label, the table its rows live in, the [`COMMUTES`], [`PLAIN`]
    /// and [`CONSTANT`] bits an insert branches on, and its offset laws.
    label_slot: Vec<u32>,
    label_flags: Vec<u8>,
    label_op: Vec<u64>,
    label_laws: Vec<Laws>,
    /// What each table slot holds, in the order first needed. Read in the
    /// order of the shapes themselves.
    shapes: Vec<Shape>,
    /// Whether some label has an identity or is linear, so a rebuild looks
    /// for rows its laws now settle.
    reshapes: bool,
    /// Per plan, what its searches keep, and which rules a round visits for
    /// the rule set last saturated with. Labels only accumulate, so an entry
    /// stays valid for the vocabulary's life.
    caches: Caches,
}

impl<L: Label> Default for Vocabulary<L>
where
    L::Names: Default,
{
    fn default() -> Self {
        Self::new(L::Names::default())
    }
}

/// The copy starts with the labels and none of the caches, which are scratch.
impl<L: Label> Clone for Vocabulary<L> {
    fn clone(&self) -> Self {
        Self {
            names: self.names.clone(),
            labels: self.labels.clone(),
            keyed: self.keyed.clone(),
            label_rank: self.label_rank.clone(),
            op_labels: self.op_labels.clone(),
            label_slot: self.label_slot.clone(),
            label_flags: self.label_flags.clone(),
            label_op: self.label_op.clone(),
            label_laws: self.label_laws.clone(),
            shapes: self.shapes.clone(),
            reshapes: self.reshapes,
            caches: Caches::default(),
        }
    }
}

impl<L: Label> Vocabulary<L> {
    /// No labels, ordered through `names`.
    pub fn new(names: L::Names) -> Self {
        Self {
            names,
            labels: Labels::default(),
            keyed: BTreeMap::new(),
            label_rank: Vec::new(),
            op_labels: FxHashMap::default(),
            label_slot: Vec::new(),
            label_flags: Vec::new(),
            label_op: Vec::new(),
            label_laws: Vec::new(),
            shapes: Vec::new(),
            reshapes: false,
            caches: Caches::default(),
        }
    }

    /// The node interned under `label`, bare of children it was met with.
    pub fn node(&self, label: LabelId) -> &L {
        self.labels.node(label)
    }

    /// What labels spell their interned fields through.
    pub fn names(&self) -> &L::Names {
        &self.names
    }

    /// The id of `node`'s label, the same in every engine over this
    /// vocabulary.
    pub fn intern(&mut self, node: &L) -> LabelId {
        let label = self.labels.intern(node);
        // Whatever the label table grew by gets its table and operator bucket.
        for index in self.label_slot.len()..self.labels.len() {
            let node = self.labels.node(LabelId(index as u32));
            let arity = node.children().len();
            let laws = Laws::of(node);
            let commutes = arity == 2 && node.commutative();
            let shape = Shape {
                arity,
                unique: node.is_unique(),
                coefficients: laws.coefficients.clone(),
                commutes,
            };
            let slot = match self.shapes.iter().position(|known| *known == shape) {
                Some(slot) => slot,
                None => {
                    self.shapes.push(shape);
                    self.shapes.len() - 1
                }
            };
            self.label_slot.push(slot as u32);
            let flags = [
                (commutes, COMMUTES),
                (laws.mask == 0, PLAIN),
                (laws.constant.is_some(), CONSTANT),
            ];
            self.label_flags.push(
                flags
                    .iter()
                    .fold(0, |bits, &(set, bit)| bits | if set { bit } else { 0 }),
            );
            let op = node.op_key();
            self.label_op.push(op);
            self.reshapes |= laws.identity.iter().any(Option::is_some) || laws.linear();
            self.label_laws.push(laws);
            let key = node.sort_key(&self.names);
            let rank = self.rank_between(&key);
            self.label_rank.push(rank);
            self.keyed.insert(key, LabelId(index as u32));
            let ranks = &self.label_rank;
            let bucket = self.op_labels.entry(op).or_default();
            let at = bucket.partition_point(|known| ranks[known.index()] < rank);
            bucket.insert(at, LabelId(index as u32));
        }
        label
    }

    fn rank_between(&mut self, key: &L::SortKey) -> u64 {
        // The ranks of the last label keyed at or below `key` and of the
        // first keyed above it.
        let bounds = |vocabulary: &Self| {
            let rank = |(_, label): (_, &LabelId)| vocabulary.label_rank[label.index()];
            let below = vocabulary.keyed.range(..=key).next_back().map(rank);
            let above = vocabulary
                .keyed
                .range(key..)
                .find(|(known, _)| *known != key);
            (below.unwrap_or(0), above.map(rank).unwrap_or(u64::MAX))
        };
        let (mut below, mut above) = bounds(self);
        if above - below < 2 {
            // No rank is left between the neighbours: spread them all out.
            let mut ranked: Vec<usize> = (0..self.label_rank.len()).collect();
            ranked.sort_unstable_by_key(|&label| self.label_rank[label]);
            let step = u64::MAX / (ranked.len() as u64 + 1);
            for (at, label) in ranked.into_iter().enumerate() {
                self.label_rank[label] = (at as u64 + 1) * step;
            }
            (below, above) = bounds(self);
        }
        below + (above - below) / 2
    }
}

impl Shape {
    fn table(&self) -> Table {
        let columns = 1 + self.arity;
        let table = match self.unique {
            true => Table::bag(columns),
            false => Table::new(columns),
        };
        // The label is a key column the union-find has no say over.
        let mut table = table.plain(1);
        if !self.coefficients.is_empty() {
            let mut coefficients = vec![0];
            coefficients.extend_from_slice(&self.coefficients);
            table = table.coefficients(&coefficients);
        }
        match self.commutes {
            true => table.unordered([1, 2]),
            false => table,
        }
    }
}

/// The e-graph: equivalence classes of terms, as a client of a [`Database`].
///
/// Classes are the ids of one space of the database, and a value is a
/// [`Ref`], a class plus the offset the database's union-find keeps. Each
/// label has a table keyed on the operands and valued by the reference the
/// node is, with the offsets the label's laws move to the result factored out
/// of the key, so hash-consing is a key lookup and congruence is the table's
/// functional dependency: a rebuild canonicalizes the tables and merges the
/// values of the rows that come to share a key. A commutative binary label's
/// operands are a multiset: its key holds the lesser operand first, at insert
/// and after every rebuild, so `add(a, b)` and `add(b, a)` are one row. A
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
    /// The tables, the class partition, the class lists and the facts:
    /// everything a scope undoes.
    pub(crate) db: Database<Lists, 1>,
    vocabulary: Vocabulary<L>,
    held: Vec<bool>,
    held_ops: HashSet<u64, tir_adt::FxBuildHasher>,

    /// The term each e-node was interned as, its label, and the class it was
    /// minted with: the node's value is that class's reference.
    node: Vec<L>,
    node_label: Vec<LabelId>,
    node_class: Vec<ClassId>,
    /// Per e-node, where its operands' offsets start in `operand_offsets`, or
    /// [`NONE`] when every one is zero. Only an operand without a coefficient
    /// keeps one.
    node_offsets: Vec<u32>,
    operand_offsets: Vec<u64>,
    /// Per open layer, the classes it found distinct, grouped under the class
    /// it merged them into; merged groups only. Starts as the enclosing
    /// layer's, so a nested layer keeps naming base classes. A class's e-node
    /// list is read through this under a layer: the lists are the base's.
    members: Vec<FxHashMap<ClassId, Vec<ClassId>>>,
    opened: Vec<Opened>,

    repair: Repair,
    // What `insert_rows` keeps allocated between batches.
    batch: Batch<L>,
    /// The vocabulary's caches while the engine holds it.
    pub(crate) caches: Mutex<Caches>,
}

// The rows of a batch that are looked up together: their key columns, the
// classes they are valued with, their nodes, and the rows found.
struct Batch<L> {
    columns: Vec<Vec<u32>>,
    classes: Vec<u32>,
    nodes: Vec<L>,
    found: Vec<Option<tir_relational::Ref>>,
}

impl<L> Default for Batch<L> {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            classes: Vec::new(),
            nodes: Vec::new(),
            found: Vec::new(),
        }
    }
}

struct Opened {
    nodes: usize,
    classes: usize,
    zeros: usize,
    offsets: usize,
}

/// The e-nodes of each class as an intrusive list and the carriers' zeros,
/// kept in the database beside the tables.
#[derive(Clone, Default)]
pub(crate) struct Lists {
    /// Per e-node, the next e-node of its class, or [`NONE`].
    next: Vec<u32>,
    /// Per class, its first and last e-node and how many it has.
    head: Vec<u32>,
    tail: Vec<u32>,
    size: Vec<u32>,
    /// Each carrier's zero class, by carrier key, in the order minted.
    zeros: Vec<(u64, ClassId)>,
}

impl<L: Label> Default for Engine<L>
where
    L::Names: Default,
{
    fn default() -> Self {
        Self::over(Vocabulary::default())
    }
}

impl<L: Label> Engine<L>
where
    L::Names: Default,
{
    pub fn new() -> Self {
        Self::default()
    }
}

impl<L: Label> Engine<L> {
    /// An empty graph whose labels are `vocabulary`'s, with a table for every
    /// shape it knows: a plan it resolved names them by slot.
    pub fn over(mut vocabulary: Vocabulary<L>) -> Self {
        let mut db = Database::new();
        let facts = [
            db.add_lattice(CLASSES, Join::Agree),
            db.add_lattice(CLASSES, Join::First),
        ];
        debug_assert_eq!(facts, [CONSTS, TYPES]);
        for shape in &vocabulary.shapes {
            db.add_table(shape.table(), CLASSES);
        }
        Self {
            db,
            caches: Mutex::new(std::mem::take(&mut vocabulary.caches)),
            vocabulary,
            held: Vec::new(),
            held_ops: HashSet::default(),
            node: Vec::new(),
            node_label: Vec::new(),
            node_class: Vec::new(),
            node_offsets: Vec::new(),
            operand_offsets: Vec::new(),
            members: Vec::new(),
            opened: Vec::new(),
            repair: Repair::default(),
            batch: Batch::default(),
        }
    }

    /// The vocabulary, for the next graph of the pass.
    pub fn release(self) -> Vocabulary<L> {
        let mut vocabulary = self.vocabulary;
        vocabulary.caches = self.caches.into_inner().expect("the plan caches");
        vocabulary
    }

    // ---- reading ----------------------------------------------------------

    /// `reference` as its class's representative plus an offset.
    #[inline]
    pub fn find(&self, reference: impl Into<Ref>) -> Ref {
        self.db.find(CLASSES, reference.into()).into()
    }

    /// The representative of `id`'s class, whatever offset `id` sits at from
    /// it.
    #[inline]
    pub fn root(&self, id: ClassId) -> ClassId {
        ClassId(self.db.root(CLASSES, id.0))
    }

    /// Whether `a` and `b` are the same value.
    pub fn connected(&self, a: impl Into<Ref>, b: impl Into<Ref>) -> bool {
        self.find(a) == self.find(b)
    }

    /// Whether a union since the outermost open scope (or ever, with none
    /// open) was a [`Contradiction`]: under an assumption, that the
    /// assumption is impossible.
    pub fn contradicted(&self) -> bool {
        self.db.contradicted(CLASSES)
    }

    pub fn is_empty(&self) -> bool {
        self.num_classes() == 0
    }

    /// Total e-nodes across all classes. An e-node found congruent to another
    /// still counts, so this only ever grows within a context — the fixpoint
    /// test both saturation drivers use reads it.
    pub fn total_size(&self) -> usize {
        self.node.len()
    }

    pub fn num_classes(&self) -> usize {
        self.db.live(CLASSES)
    }

    /// One past the highest class id ever minted, live or not — the size a table
    /// indexed by class id needs.
    pub fn class_count(&self) -> usize {
        self.db.ids(CLASSES)
    }

    /// Bytes the e-nodes, the tables and the per-class arrays hold, without the
    /// tables' key indexes. An estimate for ranking, not an allocator total.
    pub fn approx_bytes(&self) -> usize {
        let cells: usize = self
            .db
            .tables()
            .iter()
            .map(|table| table.len() * (table.arity() + 3))
            .sum();
        self.node.len() * (size_of::<L>() + 4 * size_of::<u32>())
            + cells * size_of::<u32>()
            + self.operand_offsets.len() * size_of::<u64>()
            + self.class_count() * (5 * size_of::<u32>() + 2 * size_of::<u64>() + 1)
            + self.vocabulary.labels.len() * size_of::<L>()
    }

    /// Work done since the engine was built. A saturation round reads the
    /// difference across it; a scope does not roll these back, since they count
    /// work, not state.
    pub fn stats(&self) -> Stats {
        self.db.stats()
    }

    fn in_scope(&self) -> bool {
        self.db.depth() != 0
    }

    /// The operands of an e-node, canonical as of now.
    pub fn children(&self, row: RowId) -> SmallVec<[Ref; 4]> {
        let offsets = self.node_offsets[row.index()];
        self.node[row.index()]
            .children()
            .iter()
            .enumerate()
            .map(|(operand, &child)| {
                let offset = match offsets {
                    NONE => 0,
                    at => self.operand_offsets[at as usize + operand],
                };
                self.find(Ref::new(child, offset))
            })
            .collect()
    }

    /// The value of an e-node: its class's representative plus the offset the
    /// node sits at from it.
    pub fn value(&self, row: RowId) -> Ref {
        self.find(self.node_class[row.index()])
    }

    /// The operator bucket of an e-node: its label's [`Label::op_key`].
    pub(crate) fn row_op(&self, row: RowId) -> u64 {
        self.vocabulary.label_op[self.node_label[row.index()].index()]
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
    /// Under a layer the class is its members' lists, in member order.
    pub fn rows(&self, id: ClassId) -> Rows<'_, L> {
        let lists = self.db.client();
        let root = self.root(id);
        let members = self.scope_members(root);
        let (first, rest) = match members.split_first() {
            Some((&first, rest)) => (first, rest),
            None => (root, &[][..]),
        };
        Rows {
            lists,
            cursor: lists.head.get(first.index()).copied().unwrap_or(NONE),
            members: rest.iter(),
            label: PhantomData,
        }
    }

    /// E-nodes of `id`'s class; child ids may be non-canonical — resolve with
    /// [`Self::find`].
    pub fn nodes(&self, id: ClassId) -> impl Iterator<Item = &L> + Clone {
        self.rows(id).map(|row| &self.node[row.index()])
    }

    fn class_len(&self, id: ClassId) -> usize {
        let root = self.root(id);
        let size = &self.db.client().size;
        match self.scope_members(root) {
            [] => size[root.index()] as usize,
            members => members
                .iter()
                .map(|member| size[member.index()] as usize)
                .sum(),
        }
    }

    /// Every live class, ascending. A class is named by its lowest member, so
    /// under a layer this is the position of the group's lowest base class.
    /// Extraction and bare-variable-rooted patterns walk classes in this order
    /// and break ties by it, so it is part of the contract.
    pub fn class_ids(&self) -> impl Iterator<Item = ClassId> + '_ {
        let size = &self.db.client().size;
        (0..self.class_count() as u32)
            .map(ClassId)
            .filter(move |&id| self.root(id) == id && size[id.index()] != 0)
    }

    /// [`Self::class_ids`] as readable classes.
    pub fn classes(&self) -> impl Iterator<Item = ClassRef<'_, L>> + '_ {
        self.class_ids().map(|id| self.class(id))
    }

    pub fn class(&self, id: ClassId) -> ClassRef<'_, L> {
        ClassRef {
            engine: self,
            id: self.root(id),
        }
    }

    /// Whether `label` is a commutative binary operator.
    pub(crate) fn commutes(&self, label: LabelId) -> bool {
        self.vocabulary.label_flags[label.index()] & COMMUTES != 0
    }

    /// The labels whose operator bucket is `op`, by rank.
    pub(crate) fn labels_with_op(&self, op: u64) -> &[LabelId] {
        self.vocabulary
            .op_labels
            .get(&op)
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn holds_op(&self, op: u64) -> bool {
        self.held_ops.contains(&op)
    }

    /// The labels of the `op` bucket, or with `None` of every bucket: all of
    /// them, or with `since` those met from that count on. In no one order.
    fn labels_in(&self, op: Option<u64>, since: Option<usize>, keep: impl FnMut(LabelId)) {
        let since = since.unwrap_or(0);
        let met = since..self.vocabulary.labels.len();
        let ops = &self.vocabulary.label_op;
        match op.map(|op| (op, self.labels_with_op(op))) {
            // A bucket is by rank, so the labels met since are anywhere in
            // it: they are picked out of whichever is the shorter read.
            Some((_, bucket)) if bucket.len() <= met.len() => {
                let bucket = bucket.iter().copied();
                bucket.filter(|label| label.index() >= since).for_each(keep)
            }
            Some((op, _)) => met
                .filter(|&index| ops[index] == op)
                .map(|index| LabelId(index as u32))
                .for_each(keep),
            None => met.map(|index| LabelId(index as u32)).for_each(keep),
        }
    }

    fn rank_in(&self, cells: &mut SmallVec<[u32; 4]>, label: LabelId) {
        let ranks = &self.vocabulary.label_rank;
        let rank = ranks[label.index()];
        let at = cells.partition_point(|&known| ranks[known as usize] < rank);
        cells.insert(at, label.0);
    }

    /// Add to `out` the labels in the `op` bucket, or with `None` in any,
    /// that `matches` holds of at `arity`, by the table their rows live in;
    /// with `commuting`, only the commutative binary ones. Tables come in the
    /// order of what they hold, and a scan reads a table's labels by rank,
    /// so neither order is one the labels were met in. With `since`, only
    /// the labels met from that count on are looked at, and `out` already
    /// holds the rest.
    pub(crate) fn labels_matching(
        &self,
        op: Option<u64>,
        arity: usize,
        matches: impl Fn(&L) -> bool,
        commuting: bool,
        since: Option<usize>,
        out: &mut Resolution,
    ) {
        let shapes = &self.vocabulary.shapes;
        // The tables that gain a label, and how many each held.
        let mut grown: SmallVec<[(u32, usize); 2]> = SmallVec::new();
        self.labels_in(op, since, |label| {
            let node = self.vocabulary.labels.node(label);
            let matches = node.children().len() == arity && matches(node);
            if !matches || commuting && !self.commutes(label) {
                return;
            }
            let slot = self.vocabulary.label_slot[label.index()];
            let known = out.tables.iter().position(|(known, _)| *known == slot);
            let at = known.unwrap_or_else(|| {
                let shape = &shapes[slot as usize];
                let before = |(known, _): &(u32, _)| shapes[*known as usize] < *shape;
                let at = out.tables.partition_point(before);
                out.tables.insert(at, (slot, SmallVec::new()));
                out.scans.insert(at, SmallVec::new());
                at
            });
            if grown.iter().all(|(known, _)| *known != slot) {
                grown.push((slot, out.tables[at].1.len()));
            }
            out.tables[at].1.push(label.0);
            out.scans[at].push(label.0);
        });
        // What a table gained is at the end of its lists. Those labels are
        // the highest it holds, and go by rank among the ones a scan had.
        let ranks = &self.vocabulary.label_rank;
        for (slot, old) in grown {
            let at = out.tables.iter().position(|(known, _)| *known == slot);
            let at = at.expect("a table that gained a label");
            out.tables[at].1[old..].sort_unstable();
            let scan = &mut out.scans[at];
            scan[old..].sort_unstable_by_key(|&cell| ranks[cell as usize]);
            for at in (old..scan.len()).filter(|_| old != 0) {
                let rank = ranks[scan[at] as usize];
                let to = scan[..at].partition_point(|&known| ranks[known as usize] < rank);
                scan[to..=at].rotate_right(1);
            }
        }
    }

    /// Add to `out` the labels in the `op` bucket that `value` matches, by
    /// rank; `since` as for [`Self::labels_matching`].
    pub(crate) fn labels_equal(
        &self,
        op: u64,
        value: &L,
        since: Option<usize>,
        out: &mut Resolution,
    ) {
        self.labels_in(Some(op), since, |label| {
            if value.matches(self.vocabulary.labels.node(label)) {
                self.rank_in(&mut out.cells, label);
            }
        });
    }

    /// The table `label`'s rows live in.
    pub(crate) fn label_slot(&self, label: LabelId) -> u32 {
        self.vocabulary.label_slot[label.index()]
    }

    /// How many labels the graph has met. What a plan's atoms read is fixed
    /// between two readings that agree.
    pub(crate) fn labels_len(&self) -> usize {
        self.vocabulary.labels.len()
    }

    /// Canonical classes holding a node in the `op` bucket, each once, label
    /// by label in rank order. Over-approximates — callers confirm with the
    /// label.
    pub fn classes_with_op(&self, op: u64) -> Vec<ClassId> {
        let labels = self.labels_with_op(op);
        let mut seen = self.db.marks(CLASSES);
        let mut out = Vec::new();
        for &label in labels {
            let slot = self.vocabulary.label_slot[label.index()];
            self.db.each_row(slot, 0, label.0, |table, row| {
                let root = self.root(ClassId(table.values()[row as usize]));
                if seen.insert(root.index()) {
                    out.push(root);
                }
            });
        }
        out
    }

    /// The value of an already-interned `node` over its children at offset
    /// zero, or `None` (never inserts; always `None` for a unique node).
    pub fn lookup(&self, node: &L) -> Option<Ref> {
        let operands: SmallVec<[Ref; 4]> = node.children().iter().map(|&c| c.into()).collect();
        self.lookup_at(node, &operands)
    }

    /// [`Self::lookup`] over `operands` in place of `node`'s children.
    pub fn lookup_at(&self, node: &L, operands: &[Ref]) -> Option<Ref> {
        if operands.is_empty()
            && let Some((carrier, value)) = leaf_constant(node)
        {
            return Some(Ref::new(self.zero(carrier.key)?, value));
        }
        let label = self.vocabulary.labels.get(node)?;
        let operands: SmallVec<[Ref; 4]> = operands.iter().map(|&o| self.find(o)).collect();
        let mut key = Key::default();
        match self.shape(label, &operands, &mut key) {
            Shaped::Ref(found) => Some(found),
            Shaped::Key { shift, .. } => {
                let found = self.probe(label, &key)?;
                Some(self.find(Ref::new(found.class, found.offset.wrapping_add(shift))))
            }
        }
    }

    /// The row holding `key` in `label`'s table, as the value it stores.
    fn probe(&self, label: LabelId, key: &Key) -> Option<Ref> {
        let slot = *self.vocabulary.label_slot.get(label.index())?;
        self.db
            .lookup(slot, &key.cells, key.weights())
            .map(Ref::from)
    }

    /// What `label` over `operands`, which are canonical, comes to before a
    /// table is probed: a reference its laws settle without a row, or the
    /// key it is stored under, written into `key`, and the offset to add to
    /// that row's value. A commutative label's key holds the lesser operand
    /// first, by class and then by the offset the key keeps.
    #[inline]
    pub(crate) fn shape(&self, label: LabelId, operands: &[Ref], key: &mut Key) -> Shaped {
        let laws = &self.vocabulary.label_laws[label.index()];
        if laws.mask != 0
            && let Some(found) = fold(
                &self.db.placement(CLASSES),
                &self.db.client().zeros,
                laws,
                operands,
            )
        {
            return Shaped::Ref(found);
        }
        key.cells.push(label.0);
        key.cells
            .extend(operands.iter().map(|operand| operand.class.0));
        let mut sum = 0u64;
        key.weights.push(0);
        for (at, operand) in operands.iter().enumerate() {
            match laws.coefficient(at) {
                0 => {
                    key.weights.push(operand.offset);
                    key.weighted |= operand.offset != 0;
                }
                coefficient => {
                    sum = sum.wrapping_add((coefficient as u64).wrapping_mul(operand.offset));
                    key.weights.push(0);
                }
            }
        }
        let exchanged =
            self.commutes(label) && (key.cells[1], key.weights[1]) > (key.cells[2], key.weights[2]);
        if exchanged {
            key.cells.swap(1, 2);
            key.weights.swap(1, 2);
        }
        Shaped::Key {
            shift: sum & laws.mask,
            exchanged,
        }
    }

    /// The zero class of the carrier `key`, if the graph has one.
    fn zero(&self, key: u64) -> Option<ClassId> {
        let zeros = &self.db.client().zeros;
        let found = zeros.iter().find(|&&(known, _)| known == key);
        found.map(|&(_, class)| class)
    }

    /// The zero class of `carrier`, minted from `node`'s
    /// [`Label::constant_of`] if the graph has none yet.
    fn zero_or_mint(&mut self, node: &L, carrier: Carrier) -> Option<ClassId> {
        if let Some(zero) = self.zero(carrier.key) {
            return Some(zero);
        }
        let zero = node.constant_of(0)?;
        let label = self.intern(&zero);
        Some(self.make_class(zero, label, &[label.0], &[], true))
    }

    // ---- writing ----------------------------------------------------------

    /// Intern `node` over its children at offset zero, returning its value.
    pub fn add(&mut self, mut node: L) -> Ref {
        if let Some(found) = self.int_constant_ref(&node) {
            return found;
        }
        if !self.in_scope() && self.db.partition(CLASSES).deltas().is_none() {
            // No class sits at an offset, so every child is its root at zero.
            for child in node.children_mut() {
                *child = ClassId(self.db.root(CLASSES, child.0));
            }
            let label = self.intern(&node);
            let operands: SmallVec<[Ref; 4]> = node.children().iter().map(|&c| c.into()).collect();
            return self.insert_canonical(label, node, &operands);
        }
        let mut operands: SmallVec<[Ref; 4]> = SmallVec::new();
        for child in node.children_mut() {
            let operand = self.find(*child);
            *child = operand.class;
            operands.push(operand);
        }
        let label = self.intern(&node);
        self.insert_canonical(label, node, &operands)
    }

    /// Intern `node` over `operands`, which stand in for its children,
    /// returning its value. A non-unique node equal to an existing one shares
    /// its class; otherwise a fresh class.
    ///
    /// The node's laws apply first: an integer constant of a carrier is its
    /// zero at the constant's value, an identity element returns the other
    /// operand, a linear node over constants is their sum, and an operand
    /// with a coefficient leaves its offset out of the key and adds it to the
    /// result instead. None of those stores a row.
    pub fn insert(&mut self, node: L, operands: &[Ref]) -> Ref {
        debug_assert_eq!(node.children().len(), operands.len());
        if let Some(found) = self.int_constant_ref(&node) {
            return found;
        }
        let mut node = node;
        let operands: SmallVec<[Ref; 4]> = operands
            .iter()
            .zip(node.children_mut())
            .map(|(&operand, child)| {
                let operand = self.find(operand);
                *child = operand.class;
                operand
            })
            .collect();
        let label = self.intern(&node);
        self.insert_canonical(label, node, &operands)
    }

    /// [`Self::insert`] of `node`, whose label is `label` and whose children
    /// are the classes of `operands`, which are canonical.
    fn insert_canonical(&mut self, label: LabelId, node: L, operands: &[Ref]) -> Ref {
        let mut key = Key::default();
        let (shift, exchanged) = match self.shape(label, operands, &mut key) {
            Shaped::Ref(found) => return found,
            Shaped::Key { shift, exchanged } => (shift, exchanged),
        };
        if let Some(found) = self.probe(label, &key) {
            return self.find(Ref::new(found.class, found.offset.wrapping_add(shift)));
        }
        let class = self.make_class(node, label, &key.cells, key.weights(), false);
        if exchanged {
            self.exchange_offsets();
        }
        Ref::new(class, shift)
    }

    // A commutative node keyed in the other order than written keeps its
    // offsets beside the children they were written on.
    fn exchange_offsets(&mut self) {
        match self.node_offsets.last() {
            None | Some(&NONE) => {}
            Some(&at) => self.operand_offsets.swap(at as usize, at as usize + 1),
        }
    }

    /// What `node` over `operands` is, ahead of its row: the reference its
    /// laws settle it to, or a class minted for it, at the offset its row's
    /// value is shifted by, and the node's label. [`Self::insert_rows`] takes
    /// the nodes that got a class, a label at a time, and looks their rows
    /// up then; until it has, such a class is equal to nothing.
    ///
    /// Classes minted in the order a program is walked keep that order
    /// whichever order the labels are stored in, so the representative of a
    /// set, its lowest class, is the node walked first.
    pub fn reserve_class(&mut self, node: &L, operands: &[Ref]) -> (Ref, Option<LabelId>) {
        if let Some(found) = self.int_constant_ref(node) {
            return (found, None);
        }
        let operands: SmallVec<[Ref; 4]> = operands.iter().map(|&o| self.find(o)).collect();
        let label = self.intern(node);
        match self.shape(label, &operands, &mut Key::default()) {
            Shaped::Ref(found) => (found, None),
            Shaped::Key { shift, .. } => {
                let class = self.mint_class(node, label, false);
                (Ref::new(class, shift), Some(label))
            }
        }
    }

    /// Store a batch of nodes of `label` that [`Self::reserve_class`] minted
    /// classes for: `operands[i]` is their `i`-th operand column and `values`
    /// the references reserved for them, in the order reserved. Each node is
    /// keyed over its operands as they are now. A node whose key a row
    /// holds, of the graph or of a node earlier in the batch, is united with
    /// that row and stored nowhere; so is one its laws now settle, with what
    /// it settles to; any other becomes a row of the class reserved for it.
    /// The keys that keep no offset are hashed and looked up as one batch.
    ///
    /// A row stored before a later batch unites one of its operands still
    /// names that operand, so the graph needs [`Self::rebuild_seeded`]
    /// before it is read. No layer is open.
    pub fn insert_rows(
        &mut self,
        label: LabelId,
        operands: &[&[Ref]],
        values: &[Ref],
        nodes: impl IntoIterator<Item = L>,
    ) {
        debug_assert!(!self.in_scope());
        let slot = self.table_slot(label);
        let unique = self.vocabulary.shapes[slot as usize].unique;
        let mask = self.vocabulary.label_laws[label.index()].mask;
        let mut batch = std::mem::take(&mut self.batch);
        if batch.columns.len() <= operands.len() {
            batch.columns.resize_with(operands.len() + 1, Vec::new);
        }
        let columns = &mut batch.columns[..=operands.len()];
        let mut canonical: SmallVec<[Ref; 4]> = SmallVec::new();
        for (row, (mut node, &value)) in nodes.into_iter().zip(values).enumerate() {
            canonical.clear();
            for (column, child) in operands.iter().zip(node.children_mut()) {
                let operand = self.find(column[row]);
                *child = operand.class;
                canonical.push(operand);
            }
            let mut key = Key::default();
            let (shift, exchanged) = match self.shape(label, &canonical, &mut key) {
                Shaped::Ref(found) => {
                    let _ = self.union(value, found);
                    continue;
                }
                Shaped::Key { shift, exchanged } => (shift, exchanged),
            };
            // The node is its row's value at the shift.
            let stored = Ref::new(value.class, value.offset.wrapping_sub(shift) & mask);
            if key.weighted || unique || stored.offset != 0 {
                self.insert_reserved(node, label, &key, stored, exchanged);
                continue;
            }
            for (column, &cell) in columns.iter_mut().zip(&key.cells) {
                column.push(cell);
            }
            batch.classes.push(stored.class.0);
            batch.nodes.push(node);
        }
        let first = self.node.len() as u32;
        {
            let keys: SmallVec<[&[u32]; 6]> = columns.iter().map(Vec::as_slice).collect();
            let values = Values::Each(&batch.classes);
            self.db
                .insert_rows(slot, &keys, &[], values, first, &mut batch.found);
        }
        for ((node, &class), &found) in batch.nodes.drain(..).zip(&batch.classes).zip(&batch.found)
        {
            let class = ClassId(class);
            match found {
                None => {
                    if tir_relational::trace_enabled() {
                        eprintln!("A {} {:?} {:?}", class.0, node, node.children());
                    }
                    self.list_node(node, label, class, self.root(class), &[]);
                }
                Some(holds) => {
                    let _ = self.union(class, Ref::from(holds));
                }
            }
        }
        columns.iter_mut().for_each(Vec::clear);
        batch.classes.clear();
        self.batch = batch;
    }

    fn insert_reserved(&mut self, node: L, label: LabelId, key: &Key, value: Ref, exchanged: bool) {
        if let Some(found) = self.probe(label, key) {
            let _ = self.union(value, found);
            return;
        }
        match value.offset {
            0 => {
                let list = self.root(value.class);
                self.store(node, label, &key.cells, key.weights(), value.class, list);
            }
            // The class reserved is the node, and the row is another.
            _ => {
                let class = self.make_class(node, label, &key.cells, key.weights(), false);
                let _ = self.union(value, class);
            }
        }
        if exchanged {
            self.exchange_offsets();
        }
    }

    /// `node` as a reference when it is a leaf integer constant of a carrier
    /// that can spell its zero. A typed constant's type is what its zero's
    /// terms carry, as a row's would be its class's.
    #[inline]
    pub(crate) fn int_constant_ref(&mut self, node: &L) -> Option<Ref> {
        if !node.children().is_empty() {
            return None;
        }
        let (carrier, value) = leaf_constant(node)?;
        let zero = self.zero_or_mint(node, carrier)?;
        if let Some(key) = node.type_key() {
            self.raise_type(zero, key);
        }
        Some(Ref::new(zero, value))
    }

    /// Whether `label` is a leaf integer constant of a carrier: a reference,
    /// never a row.
    pub(crate) fn is_int_constant(&self, label: LabelId) -> bool {
        self.vocabulary.label_flags[label.index()] & CONSTANT != 0
    }

    /// The id of `node`'s label.
    pub(crate) fn intern(&mut self, node: &L) -> LabelId {
        self.vocabulary.intern(node)
    }

    /// Record `a == b`, returning the representative. Congruence repair is
    /// deferred to [`Self::rebuild`], and so is everything a query reads: the
    /// tables keep naming the absorbed class until then, so a search needs a
    /// rebuild first. The merge itself is visible immediately to
    /// [`Self::find`] and [`Self::nodes`], so an applier that unions and then
    /// instantiates hash-conses against the result.
    ///
    /// `a` and `b` naming one class at different offsets is a
    /// [`Contradiction`]: nothing merges, the class is marked
    /// [`Self::conflicted`], and [`Self::contradicted`] reports it.
    pub fn union(
        &mut self,
        a: impl Into<Ref>,
        b: impl Into<Ref>,
    ) -> Result<ClassId, Contradiction> {
        let (a, b) = (a.into(), b.into());
        let (survivor, absorbed) = match self.db.equate(CLASSES, a.into(), b.into()) {
            Merge::Same => return Ok(self.root(a.class)),
            Merge::Contradiction => return Err(Contradiction),
            Merge::Merged { survivor, absorbed } => (survivor, absorbed),
        };
        // The lists are the base's: a layer reads a class through its members.
        if !self.in_scope() {
            self.splice_class(survivor as usize, absorbed as usize);
        }
        let (survivor, absorbed) = (ClassId(survivor), ClassId(absorbed));
        if let Some(members) = self.members.last_mut() {
            let taken = members.remove(&absorbed).unwrap_or_else(|| vec![absorbed]);
            members
                .entry(survivor)
                .or_insert_with(|| vec![survivor])
                .extend(taken);
        }
        Ok(survivor)
    }

    /// Restore congruence to a fixpoint after a batch of unions.
    ///
    /// Each pass canonicalizes every table, carrying the offsets the merges
    /// put classes at, and merges the values of the rows that came to share a
    /// key. A re-keyed row whose laws now settle it without a row (an operand
    /// became an identity element, or every operand of a linear node a
    /// constant) leaves its table and its value is united with what it
    /// settles to. Those merges are what the next pass rewrites. The class a
    /// merge leaves standing is a function of the set whichever way the
    /// merges are grouped, but the order of its e-nodes is not, and the
    /// tables are visited in slot order, which is the order the pass met
    /// their shapes in: the merges of a pass go table by table in the order
    /// of the shapes themselves.
    pub fn rebuild(&mut self) {
        self.rebuild_listing(None);
    }

    /// [`Self::rebuild`] for a graph [`Self::insert_rows`] is seeding. An
    /// e-node this finds congruent to another, or settled by its laws, was
    /// stored only because its operands were united after it: it leaves its
    /// class's list with its table. Of two congruent e-nodes the one minted
    /// with the lower class stays, as the row's e-node too, so every class
    /// lists what inserting the nodes one at a time in the order their
    /// classes were reserved would have. No layer is open.
    pub fn rebuild_seeded(&mut self) {
        debug_assert!(!self.in_scope());
        let mut left = Vec::new();
        self.rebuild_listing(Some(&mut left));
        self.unlist(left);
    }

    fn rebuild_listing(&mut self, mut left: Option<&mut Vec<RowId>>) {
        let mut report = std::mem::take(&mut self.repair);
        let mut settled: Vec<(u32, Ref, Ref, RowId)> = Vec::new();
        loop {
            let (laws, slots) = (&self.vocabulary.label_laws, &self.vocabulary.label_slot);
            let mut dissolve = |placed: &dyn Placement, lists: &Lists, row: RowView<'_>| {
                let laws = &laws[row.cells[0] as usize];
                let operands: SmallVec<[Ref; 4]> = row.cells[1..]
                    .iter()
                    .zip(&row.weights[1..])
                    .map(|(&class, &offset)| Ref::new(ClassId(class), offset))
                    .collect();
                let Some(settles) = fold(placed, &lists.zeros, laws, &operands) else {
                    return false;
                };
                let slot = slots[row.cells[0] as usize];
                let value = Ref::new(ClassId(row.value.0), row.value.1);
                settled.push((slot, value, settles, RowId(row.tag)));
                true
            };
            let dissolve = self
                .vocabulary
                .reshapes
                .then_some(&mut dissolve as tir_relational::Dissolve<'_, Lists>);
            if !self.db.canonicalize(CLASSES, dissolve, &mut report) {
                break;
            }
            let shapes = &self.vocabulary.shapes;
            let by_shape = |a: u32, b: u32| match a == b {
                true => std::cmp::Ordering::Equal,
                false => shapes[a as usize].cmp(&shapes[b as usize]),
            };
            let slot =
                |collision: &Collision| slots[self.node_label[collision.tag as usize].index()];
            report.collisions.sort_by(|a, b| by_shape(slot(a), slot(b)));
            settled.sort_by(|a, b| by_shape(a.0, b.0));
            for collision in report.collisions.drain(..) {
                let (kept, removed) = (collision.kept, collision.removed);
                let _ = self.union(
                    Ref::new(ClassId(kept.0), kept.1),
                    Ref::new(ClassId(removed.0), removed.1),
                );
                if let Some(left) = left.as_mut() {
                    left.push(RowId(collision.tag));
                }
            }
            for (_, value, settles, row) in settled.drain(..) {
                let _ = self.union(value, settles);
                if let Some(left) = left.as_mut() {
                    left.push(row);
                }
            }
        }
        self.repair = report;
    }

    // Of two congruent e-nodes the one minted with the lower class stays, as
    // the tag of the row that stayed too.
    fn unlist(&mut self, left: Vec<RowId>) {
        let mut gone: Vec<u32> = Vec::with_capacity(left.len());
        for row in left {
            let label = self.node_label[row.index()];
            let mut key = Key::default();
            let kept = match self.shape(label, &self.children(row), &mut key) {
                Shaped::Ref(_) => None,
                Shaped::Key { .. } => {
                    let slot = self.label_slot(label);
                    let table = &self.db.tables()[slot as usize];
                    let at = table.get(&key.cells, key.weights());
                    at.map(|at| (slot, at, RowId(table.tags()[at as usize])))
                }
            };
            match kept {
                Some((slot, at, kept))
                    if self.node_class[row.index()] < self.node_class[kept.index()] =>
                {
                    self.db.set_tag(slot, at, row.0);
                    gone.push(kept.0);
                }
                _ => gone.push(row.0),
            }
        }
        gone.sort_unstable();
        let mut classes: Vec<ClassId> = gone
            .iter()
            .map(|&row| self.root(self.node_class[row as usize]))
            .collect();
        classes.sort_unstable();
        classes.dedup();
        let lists = self.db.client_mut();
        for class in classes {
            let mut cursor = std::mem::replace(&mut lists.head[class.index()], NONE);
            let (mut last, mut size) = (NONE, 0);
            while cursor != NONE {
                let next = lists.next[cursor as usize];
                if gone.binary_search(&cursor).is_err() {
                    match last {
                        NONE => lists.head[class.index()] = cursor,
                        last => lists.next[last as usize] = cursor,
                    }
                    last = cursor;
                    size += 1;
                }
                cursor = next;
            }
            if last != NONE {
                lists.next[last as usize] = NONE;
            }
            lists.tail[class.index()] = last;
            lists.size[class.index()] = size;
        }
    }

    // ---- rows -------------------------------------------------------------

    /// Store `node` as the first row of a fresh class. `weights` is empty or
    /// one per cell of `key`; a `pinned` class is its carrier's zero.
    pub(crate) fn make_class(
        &mut self,
        node: L,
        label: LabelId,
        key: &[u32],
        weights: &[u64],
        pinned: bool,
    ) -> ClassId {
        let class = self.mint_class(&node, label, pinned);
        self.store(node, label, key, weights, class, class);
        class
    }

    fn mint_class(&mut self, node: &L, label: LabelId, pinned: bool) -> ClassId {
        let laws = &self.vocabulary.label_laws[label.index()];
        let (mask, carrier) = (laws.mask, laws.carrier);
        let class = self.db.mint(CLASSES, mask, pinned);
        let lists = self.db.client_mut();
        lists.head.push(NONE);
        lists.tail.push(NONE);
        lists.size.push(0);
        if pinned {
            let carrier = carrier.expect("a zero has a carrier");
            lists.zeros.push((carrier.key, ClassId(class)));
        }
        // A carrier's constants are offsets of its zero, not facts.
        if let Some(constant) = node.constant().filter(|_| !pinned) {
            let label = self.intern(&constant);
            self.db.seed(CONSTS, class, u64::from(label.0));
        }
        if let Some(key) = node.type_key() {
            self.db.seed(TYPES, class, key);
        }
        ClassId(class)
    }

    // A table is made with every table below it when it gets its first row.
    pub(crate) fn table_slot(&mut self, label: LabelId) -> u32 {
        let slot = self.vocabulary.label_slot[label.index()];
        while self.db.tables().len() <= slot as usize {
            let table = self.vocabulary.shapes[self.db.tables().len()].table();
            self.db.add_table(table, CLASSES);
        }
        slot
    }

    fn store(
        &mut self,
        node: L,
        label: LabelId,
        key: &[u32],
        weights: &[u64],
        class: ClassId,
        list: ClassId,
    ) {
        let slot = self.table_slot(label);
        self.db
            .insert(slot, key, weights, (class.0, 0), self.node.len() as u32);
        self.list_stored(node, label, key, weights, class, list);
    }

    // The row is stored already, valued with the next class and tagged with
    // the next e-node.
    pub(crate) fn list_class(
        &mut self,
        node: L,
        label: LabelId,
        key: &[u32],
        weights: &[u64],
    ) -> ClassId {
        let class = self.mint_class(&node, label, false);
        self.list_stored(node, label, key, weights, class, class);
        class
    }

    fn list_stored(
        &mut self,
        node: L,
        label: LabelId,
        key: &[u32],
        weights: &[u64],
        class: ClassId,
        list: ClassId,
    ) {
        if tir_relational::trace_enabled() {
            match weights.iter().any(|&weight| weight != 0) {
                true => eprintln!(
                    "A {} {:?} {:?} {:?}",
                    class.0,
                    node,
                    &key[1..],
                    &weights[1..]
                ),
                false => eprintln!("A {} {:?} {:?}", class.0, node, &key[1..]),
            }
        }
        self.list_node(node, label, class, list, weights.get(1..).unwrap_or(&[]));
    }

    fn list_node(
        &mut self,
        node: L,
        label: LabelId,
        class: ClassId,
        list: ClassId,
        offsets: &[u64],
    ) {
        if self.held.len() <= label.index() {
            self.held.resize(self.vocabulary.labels.len(), false);
        }
        if !std::mem::replace(&mut self.held[label.index()], true) {
            self.held_ops
                .insert(self.vocabulary.label_op[label.index()]);
        }
        let row = self.node.len() as u32;
        let lists = self.db.client_mut();
        lists.next.push(NONE);
        match std::mem::replace(&mut lists.tail[list.index()], row) {
            NONE => lists.head[list.index()] = row,
            tail => lists.next[tail as usize] = row,
        }
        lists.size[list.index()] += 1;
        self.node.push(node);
        self.node_label.push(label);
        self.node_class.push(class);
        match offsets.iter().any(|&offset| offset != 0) {
            true => {
                self.node_offsets.push(self.operand_offsets.len() as u32);
                self.operand_offsets.extend_from_slice(offsets);
            }
            false => self.node_offsets.push(NONE),
        }
    }

    /// Move the absorbed class's e-nodes onto the end of the survivor's list.
    fn splice_class(&mut self, survivor: usize, absorbed: usize) {
        let lists = self.db.client_mut();
        let head = std::mem::replace(&mut lists.head[absorbed], NONE);
        let tail = std::mem::replace(&mut lists.tail[absorbed], NONE);
        let len = std::mem::take(&mut lists.size[absorbed]);
        if head != NONE {
            match lists.tail[survivor] {
                NONE => lists.head[survivor] = head,
                survivor_tail => lists.next[survivor_tail as usize] = head,
            }
            lists.tail[survivor] = tail;
            lists.size[survivor] += len;
        }
    }

    // ---- change log -------------------------------------------------------

    /// Drain the change log: the canonical classes changed since the previous
    /// call, ascending and deduplicated; `None` means "every class".
    pub fn take_changed(&mut self) -> Option<Vec<ClassId>> {
        self.db.advance();
        let changed = self.db.take_changed(CLASSES)?;
        Some(changed.into_iter().map(ClassId).collect())
    }

    /// Report "everything" from the next [`Self::take_changed`]. A driver that
    /// stopped on a limit rather than at a fixpoint calls this: the matches it
    /// never reached are not named by the change log.
    pub fn mark_all_changed(&mut self) {
        self.db.mark_all_changed(CLASSES);
    }

    // ---- facts ------------------------------------------------------------

    /// Assume, inside the current scope, that `reference` evaluates to the
    /// constant `node`. For an integer constant of a carrier that is a union
    /// with the constant's reference, and a [`Contradiction`] refutes the
    /// scope. Any other constant is a fact in the constant column, which
    /// leaves the class's identity and parents alone. Panics with no scope
    /// open: an unscoped assumption would never be popped.
    pub fn assume_const(&mut self, reference: impl Into<Ref>, node: L) {
        assert!(
            self.in_scope(),
            "an assumption needs a scope to be undone by"
        );
        let reference = reference.into();
        if let Some(constant) = self.int_constant_ref(&node) {
            let _ = self.union(reference, constant);
            return;
        }
        let label = self.intern(&node);
        let class = self.root(reference.class).0;
        self.db.raise(CONSTS, class, u64::from(label.0));
    }

    /// The integer constant `reference` is, when its class is a carrier's
    /// zero.
    pub fn int_const(&self, reference: impl Into<Ref>) -> Option<u64> {
        let found = self.find(reference);
        self.db
            .is_zero(CLASSES, found.class.0)
            .then_some(found.offset)
    }

    /// The width of `id`'s carrier, `None` for a class without one.
    pub fn width(&self, id: ClassId) -> Option<u32> {
        match self.db.mask(CLASSES, id.0) {
            0 => None,
            mask => Some(mask.count_ones()),
        }
    }

    /// Whether a union tried to put `id`'s class at a non-zero offset from
    /// itself: two placements that disagree, which a reader takes as "placed
    /// nowhere". The class otherwise behaves as the first placement made it.
    pub fn conflicted(&self, id: ClassId) -> bool {
        self.db.is_conflicted(CLASSES, id.0)
    }

    /// The constant `reference` is known to be, spelled as a node — its
    /// carrier's zero at an offset, its own literal, or what an open scope
    /// assumed of it. `None` when nothing is known and when two values were
    /// proven, which a refuted hypothesis reads as "unknown".
    pub fn const_of(&self, reference: impl Into<Ref>) -> Option<L> {
        let found = self.find(reference);
        if self.db.is_zero(CLASSES, found.class.0) {
            return self.spell_constant(found);
        }
        if found.offset != 0 {
            return None;
        }
        let label = self.db.lattice(CONSTS).get(found.class.0)?;
        Some(self.vocabulary.labels.node(LabelId(label as u32)).clone())
    }

    /// `constant`, whose class is a carrier's zero, spelled as a node. The
    /// zero's own node heads its class: it minted it, and a union appends.
    pub fn spell_constant(&self, constant: Ref) -> Option<L> {
        let found = self.find(constant);
        let head = self.db.client().head[found.class.index()];
        self.node.get(head as usize)?.constant_of(found.offset)
    }

    /// Whether the outermost open layer found `id` a carrier's zero already.
    fn zero_before_scopes(&self, id: ClassId) -> bool {
        // A layer places the base's representatives, so the base partition is
        // the one every open layer found; a zero minted under a layer is past
        // the ids it found.
        self.db
            .ids_before_layers(CLASSES)
            .is_some_and(|ids| id.index() < ids && self.db.partition(CLASSES).is_zero(id.0))
    }

    /// The constant an *open scope* assumed of `class`, as opposed to the one
    /// the class states about itself. A reader that acts on a hypothesis rather
    /// than on the program asks this.
    pub fn assumed_const(&self, class: ClassId) -> Option<L> {
        if !self.in_scope() {
            return None;
        }
        if self.int_const(class).is_some() {
            return (!self.zero_before_scopes(class))
                .then(|| self.const_of(class))
                .flatten();
        }
        let class = self.root(class);
        self.db
            .lattice(CONSTS)
            .written_in_scope(class.0)
            .then(|| self.const_of(class))
            .flatten()
    }

    /// The reference that holds `node` as its own literal, whatever type the
    /// row carries. The hash-cons cannot answer this for a constant outside a
    /// carrier: a class is known to be a *value*, and the row saying so may
    /// spell it at a type the caller has no way to name.
    pub fn const_class(&self, node: &L) -> Option<Ref> {
        if let Some((carrier, value)) = leaf_constant(node) {
            return Some(Ref::new(self.zero(carrier.key)?, value));
        }
        let consts = self.db.lattice(CONSTS);
        self.classes_with_const(node)
            .into_iter()
            .find(|&class| !consts.written_in_scope(class.0))
            .map(Ref::from)
    }

    /// The classes an open scope assumed to be `node`, ascending.
    pub fn classes_assumed_const(&self, node: &L) -> Vec<ClassId> {
        if let Some((carrier, value)) = leaf_constant(node) {
            let Some(zero) = self.zero(carrier.key) else {
                return Vec::new();
            };
            let constant = Ref::new(zero, value);
            // The layer's unions grouped what it merged under the zero.
            let mut out: Vec<ClassId> = self
                .scope_members(zero)
                .iter()
                .copied()
                .filter(|&member| !self.zero_before_scopes(member) && self.find(member) == constant)
                .collect();
            out.sort_unstable();
            return out;
        }
        let consts = self.db.lattice(CONSTS);
        self.classes_with_const(node)
            .into_iter()
            .filter(|&class| consts.written_in_scope(class.0))
            .collect()
    }

    /// Whether `class` was proven two different constants, or two different
    /// offsets from itself — a refuted scope.
    pub fn const_conflicted(&self, class: ClassId) -> bool {
        self.db.lattice(CONSTS).is_conflicted(self.root(class).0) || self.conflicted(class)
    }

    /// The classes known to be `node`. For an integer constant of a carrier
    /// these are every class id the union-find places at that offset from the
    /// zero, ascending; otherwise the canonical classes the constant column
    /// holds it for, in the order they first did.
    pub fn classes_with_const(&self, node: &L) -> Vec<ClassId> {
        if let Some((carrier, value)) = leaf_constant(node) {
            let Some(zero) = self.zero(carrier.key) else {
                return Vec::new();
            };
            let constant = Ref::new(zero, value);
            let mut out = Vec::new();
            match self.db.is_canonical(CLASSES) && !self.in_scope() {
                // A rebuilt union-find is flat: one scan finds the zero's set.
                true => {
                    tir_adt::simd::select_eq(self.db.partition(CLASSES).parents(), zero.0, &mut out)
                }
                false => out.extend(
                    (0..self.class_count() as u32).filter(|&id| self.root(ClassId(id)) == zero),
                ),
            }
            out.retain(|&id| self.find(ClassId(id)) == constant);
            return out.into_iter().map(ClassId).collect();
        }
        let consts = self.db.lattice(CONSTS);
        node.constant()
            .and_then(|constant| self.vocabulary.labels.get(&constant))
            .into_iter()
            .flat_map(|label| consts.keys_with(u64::from(label.0)))
            .map(ClassId)
            .collect()
    }

    /// Whether `reference` is the carrier constant `value`; `None` when
    /// `value` is not an integer constant of a carrier.
    pub(crate) fn is_literal(&self, reference: Ref, value: &L) -> Option<bool> {
        self.terms().is_literal(reference, value)
    }

    pub(crate) fn terms(&self) -> Terms<'_, L> {
        Terms {
            db: &self.db,
            nodes: &self.node,
            labels: &self.vocabulary.labels,
            names: &self.vocabulary.names,
        }
    }

    pub(crate) fn names(&self) -> &L::Names {
        self.vocabulary.names()
    }

    /// The node interned under `label`.
    pub fn label_node(&self, label: LabelId) -> Option<&L> {
        (label.index() < self.vocabulary.labels.len()).then(|| self.vocabulary.labels.node(label))
    }

    /// `class`'s value in `column`, as one word.
    pub fn fact(&self, column: ColumnId, class: ClassId) -> Option<u64> {
        self.db.lattice(lattice(column)).get(self.root(class).0)
    }

    /// Raise the type `class`'s terms carry, for a term whose type the language
    /// keeps outside the node.
    pub fn raise_type(&mut self, class: ClassId, key: u64) {
        let class = self.root(class);
        self.db.raise(TYPES, class.0, key);
    }

    /// Declare a column of the client's own, for facts its rules read
    /// through [`Atom::Fact`](crate::Atom::Fact). A plan names the column by
    /// number, so a pass declares its columns in one order on every graph.
    /// Declared with no layer open. `join` is how two values said of one
    /// class combine.
    pub fn add_column(&mut self, join: Join) -> ColumnId {
        let column = self.db.add_lattice(CLASSES, join);
        ColumnId::Client(column.index() as u32 - OWN_COLUMNS)
    }

    /// Say `value` of `class` in `column`, joined onto what the column
    /// holds for it.
    pub fn raise_fact(&mut self, column: ColumnId, class: ClassId, value: u64) {
        let class = self.root(class);
        self.db.raise(lattice(column), class.0, value);
    }

    /// Register `table` for the rules' [`Atom::Table`](crate::Atom::Table)
    /// atoms, which name it by the slot returned: tables number from zero in
    /// the order they are added. The table is shared, not copied.
    pub fn add_words(&mut self, table: std::sync::Arc<tir_relational::Words>) -> u32 {
        self.db.add_words(table)
    }

    /// Whether `class`'s value in `column` rose during the round the last
    /// [`Self::take_changed`] closed: the fact-level counterpart of a row in
    /// [`Groups::new_rows`](tir_relational::Groups::new_rows).
    pub fn fact_is_new(&self, column: ColumnId, class: ClassId) -> bool {
        self.db.is_new(lattice(column), class.0)
    }

    // ---- layers -----------------------------------------------------------

    /// Enter an assumption layer: everything until the matching
    /// [`Self::pop_context`] is undone by it. The graph is rebuilt first,
    /// since a layer places the base's representatives.
    pub fn push_context(&mut self) {
        self.rebuild();
        let members = self.members.last().cloned().unwrap_or_default();
        self.members.push(members);
        self.opened.push(Opened {
            nodes: self.node.len(),
            classes: self.class_count(),
            zeros: self.db.client().zeros.len(),
            offsets: self.operand_offsets.len(),
        });
        self.db.push_layer();
    }

    /// Leave the layer, discarding its unions, its rows, and its assumptions;
    /// the enclosing layer (or the base graph) is restored without a rebuild.
    pub fn pop_context(&mut self) {
        // The classes the layer minted keep their ids, so a caller still holding
        // one finds it, but they lose every e-node the layer gave them and stop
        // being classes: `class_ids` skips a class with none.
        self.db.pop_layer();
        self.members.pop();
        let opened = self.opened.pop().expect("an open layer");
        let classes = self.class_count();
        let lists = self.db.client_mut();
        // The base's lists were only appended to: the e-nodes and classes the
        // layer minted go, and the classes keep their ids with no e-node.
        lists.next.truncate(opened.nodes);
        lists.head.truncate(opened.classes);
        lists.tail.truncate(opened.classes);
        lists.size.truncate(opened.classes);
        lists.head.resize(classes, NONE);
        lists.tail.resize(classes, NONE);
        lists.size.resize(classes, 0);
        lists.zeros.truncate(opened.zeros);
        self.node.truncate(opened.nodes);
        self.node_label.truncate(opened.nodes);
        self.node_class.truncate(opened.nodes);
        self.node_offsets.truncate(opened.nodes);
        self.operand_offsets.truncate(opened.offsets);
    }

    /// The classes the open layer found distinct and grouped under `id`, in
    /// the order the layer merged them: the survivor's, then each absorbed
    /// group's. Empty when no layer is open or the layer did not merge `id`
    /// with anything. Side tables built against the graph the layer opened on
    /// are keyed by those, so a query made under a layer aggregates over this.
    pub fn scope_members(&self, id: ClassId) -> &[ClassId] {
        self.members
            .last()
            .and_then(|members| members.get(&id))
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
        self.dirty_since(self.db.depth().saturating_sub(1))
    }

    /// Everything the scopes from `depth` outward-in changed, closed upward.
    fn dirty_since(&self, depth: usize) -> Vec<ClassId> {
        if !self.in_scope() {
            return Vec::new();
        }
        let seeds = self
            .db
            .layer_touched(CLASSES, depth)
            .chain(self.db.lattice(CONSTS).scoped_keys_from(depth));
        class_ids(self.db.close_upward(CLASSES, seeds, None))
    }

    /// `changed` closed upward `height` times over parent edges, ascending: the
    /// classes a pattern of that height can newly match at. A class outside it
    /// has an unchanged downward cone to that depth, so its matches are the
    /// previous round's and were applied then.
    pub fn delta(&self, changed: &[ClassId], height: usize) -> Vec<ClassId> {
        let seeds = changed.iter().map(|id| id.0);
        class_ids(self.db.close_upward(CLASSES, seeds, Some(height)))
    }
}

fn class_ids(ids: Vec<u32>) -> Vec<ClassId> {
    ids.into_iter().map(ClassId).collect()
}

/// The lattice column of `column`.
pub(crate) fn lattice(column: ColumnId) -> LatticeId {
    match column {
        ColumnId::Const => CONSTS,
        ColumnId::Type => TYPES,
        ColumnId::Client(column) => LatticeId::nth(OWN_COLUMNS + column),
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

/// Walks a class's e-nodes along its intrusive list, then along each further
/// member's list under a layer.
pub struct Rows<'a, L: Label> {
    lists: &'a Lists,
    cursor: u32,
    members: std::slice::Iter<'a, ClassId>,
    label: PhantomData<&'a L>,
}

impl<L: Label> Clone for Rows<'_, L> {
    fn clone(&self) -> Self {
        Self {
            lists: self.lists,
            cursor: self.cursor,
            members: self.members.clone(),
            label: PhantomData,
        }
    }
}

impl<L: Label> Iterator for Rows<'_, L> {
    type Item = RowId;

    fn next(&mut self) -> Option<RowId> {
        while self.cursor == NONE {
            let member = self.members.next()?;
            self.cursor = self.lists.head.get(member.index()).copied().unwrap_or(NONE);
        }
        let row = RowId(self.cursor);
        self.cursor = self.lists.next[row.index()];
        Some(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Term;

    fn leaf(eg: &mut Engine<Term>, op: &str, width: u8) -> Ref {
        eg.add(Term::typed(op, width, &[]))
    }

    fn num(eg: &mut Engine<Term>, width: u8, value: u64) -> Ref {
        eg.add(Term::num(width, value))
    }

    fn op(eg: &mut Engine<Term>, op: &str, width: u8, operands: &[Ref]) -> Ref {
        let node = Term::typed(op, width, &vec![ClassId(0); operands.len()]);
        eg.insert(node, operands)
    }

    fn shifted(base: Ref, offset: u64, width: u8) -> Ref {
        Ref::new(
            base.class,
            base.offset.wrapping_add(offset) & tir_relational::unionfind::mask(width),
        )
    }

    #[test]
    fn commutative_operands_are_one_row_in_either_order() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a")).class;
        let b = eg.add(Term::leaf("b")).class;
        let ba = eg.add(Term::comm("add", &[b, a]));
        let ab = eg.add(Term::comm("add", &[a, b]));
        assert_eq!(ab, ba);
        assert_eq!(eg.lookup(&Term::comm("add", &[a, b])), Some(ba));
        // The node is the first insert's, as written.
        let nodes: Vec<&Term> = eg.nodes(ba.class).collect();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].children, vec![b, a]);
        assert_ne!(
            eg.add(Term::op("sub", &[a, b])),
            eg.add(Term::op("sub", &[b, a]))
        );
    }

    // An offset the key keeps stays beside the child it was written on, and
    // orders two operands of one class.
    #[test]
    fn a_commutative_node_keeps_its_offsets_beside_its_children() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let y = leaf(&mut eg, "y", 32);
        let (x1, x3) = (shifted(x, 1, 32), shifted(x, 3, 32));
        let mul = |eg: &mut Engine<Term>, operands: [Ref; 2]| {
            eg.insert(Term::comm("mul", &[ClassId(0); 2]), &operands)
        };
        let yx = mul(&mut eg, [y, x3]);
        assert_eq!(mul(&mut eg, [x3, y]), yx);
        assert_eq!(
            eg.children(eg.rows(yx.class).next().unwrap()).as_slice(),
            &[y, x3]
        );
        let xx = mul(&mut eg, [x3, x1]);
        assert_eq!(mul(&mut eg, [x1, x3]), xx);
        assert_ne!(mul(&mut eg, [x1, x1]), xx);
        assert_eq!(
            eg.children(eg.rows(xx.class).next().unwrap()).as_slice(),
            &[x3, x1]
        );
    }

    // A union that puts a commutative row's operands out of order re-keys
    // it, so it meets the row written the other way round.
    #[test]
    fn a_rebuild_keeps_commutative_rows_in_order() {
        for layered in [false, true] {
            let mut eg = Engine::new();
            let [a, b, c] = ["a", "b", "c"].map(|name| eg.add(Term::leaf(name)).class);
            let bc = eg.add(Term::comm("add", &[b, c]));
            let ba = eg.add(Term::comm("add", &[b, a]));
            eg.rebuild();
            assert!(!eg.connected(bc, ba));
            if layered {
                eg.push_context();
            }
            eg.union(c, a).unwrap();
            eg.rebuild();
            assert!(eg.connected(bc, ba));
            assert_eq!(eg.lookup(&Term::comm("add", &[c, b])), Some(eg.find(bc)));
        }
    }

    // A program to seed: per op its node and the earlier ops it reads.
    type Program = Vec<(Term, Vec<usize>)>;

    // A label's batch: an operand column per position, the references
    // reserved, the nodes.
    type Waiting = (LabelId, Vec<Vec<Ref>>, Vec<Ref>, Vec<Term>);

    fn mul(width: u8) -> Term {
        Term {
            commutative: true,
            ..Term::typed("mul", width, &[ClassId(0); 2])
        }
    }

    fn add(width: u8) -> Term {
        Term::typed("add", width, &[ClassId(0); 2])
    }

    fn seed_one_at_a_time(program: &Program) -> (Engine<Term>, Vec<Ref>) {
        let mut eg = Engine::new();
        let mut values: Vec<Ref> = Vec::new();
        for (node, reads) in program {
            let operands: Vec<Ref> = reads.iter().map(|&op| values[op]).collect();
            values.push(eg.insert(node.clone(), &operands));
        }
        (eg, values)
    }

    // Classes in walk order, then the labels' batches in the order `rank`
    // gives them.
    fn seed_in_batches(program: &Program, rank: &[u32]) -> (Engine<Term>, Vec<Ref>) {
        let mut eg = Engine::new();
        let mut values: Vec<Ref> = Vec::new();
        let mut batches: Vec<Waiting> = Vec::new();
        for (node, reads) in program {
            let operands: Vec<Ref> = reads.iter().map(|&op| values[op]).collect();
            let (value, label) = eg.reserve_class(node, &operands);
            values.push(value);
            let Some(label) = label else {
                continue;
            };
            let at = batches.iter().position(|batch| batch.0 == label);
            let at = at.unwrap_or_else(|| {
                batches.push((
                    label,
                    vec![Vec::new(); operands.len()],
                    Vec::new(),
                    Vec::new(),
                ));
                batches.len() - 1
            });
            for (column, &operand) in batches[at].1.iter_mut().zip(&operands) {
                column.push(operand);
            }
            batches[at].2.push(value);
            batches[at].3.push(node.clone());
        }
        let mut order: Vec<usize> = (0..batches.len()).collect();
        order.sort_by_key(|&at| (rank[at % rank.len()], at));
        let mut batches: Vec<_> = batches.into_iter().map(Some).collect();
        for at in order {
            let (label, operands, values, nodes) = batches[at].take().unwrap();
            let operands: Vec<&[Ref]> = operands.iter().map(Vec::as_slice).collect();
            eg.insert_rows(label, &operands, &values, nodes);
        }
        eg.rebuild_seeded();
        (eg, values)
    }

    // What a seeded graph is, in terms of the program alone: a reference as
    // the first op of its class and the offset from that op's value; per op
    // its value; per class its e-nodes in order; and the rows.
    fn seeded(eg: &Engine<Term>, values: &[Ref]) -> (Vec<(usize, u64)>, Vec<String>, Vec<String>) {
        let named = |reference: Ref| {
            let found = eg.find(reference);
            let first = values
                .iter()
                .position(|&value| eg.find(value).class == found.class)
                .expect("an op of the class");
            (
                first,
                found.offset.wrapping_sub(eg.find(values[first]).offset),
            )
        };
        let quotient = values.iter().map(|&value| named(value)).collect();
        let mut lists = Vec::new();
        for class in eg.class_ids() {
            let nodes: Vec<_> = eg
                .rows(class)
                .map(|row| {
                    let children: Vec<_> = eg.children(row).into_iter().map(named).collect();
                    (
                        &eg.node(row).op,
                        eg.node(row).width,
                        children,
                        named(eg.value(row)),
                    )
                })
                .collect();
            lists.push(format!("{:?} {nodes:?}", named(class.into())));
        }
        lists.sort();
        let mut rows = Vec::new();
        for table in eg.db.tables() {
            for row in 0..table.len() as u32 {
                let cell = |column: usize| {
                    let class = ClassId(table.column(column)[row as usize]);
                    named(Ref::new(class, table.weight(column, row)))
                };
                let label = eg
                    .vocabulary
                    .labels
                    .node(LabelId(table.column(0)[row as usize]));
                let key: Vec<_> = (1..table.arity()).map(cell).collect();
                let value = cell(table.arity());
                let tag = RowId(table.tags()[row as usize]);
                assert!(eg.rows(eg.value(tag).class).any(|listed| listed == tag));
                rows.push(format!("{} {} {key:?} {value:?}", label.op, label.width));
            }
        }
        rows.sort();
        (quotient, lists, rows)
    }

    // A row stored over an operand that a later batch unites: in one label
    // order the adds are stored before the muls they read are found to be
    // one, in the other the muls before their adds.
    #[test]
    fn batches_in_either_label_order_seed_what_one_at_a_time_does() {
        let leaf = |name: &str| (Term::typed(name, 8, &[]), vec![]);
        let shared: Program = vec![
            leaf("x"),
            (mul(8), vec![0, 0]),
            leaf("y"),
            (mul(8), vec![0, 0]),
            (add(8), vec![1, 2]),
            (add(8), vec![2, 3]),
        ];
        let mirrored: Program = vec![
            leaf("x"),
            (add(8), vec![0, 0]),
            leaf("y"),
            (add(8), vec![0, 0]),
            (mul(8), vec![1, 2]),
            (mul(8), vec![2, 3]),
        ];
        for program in [shared, mirrored] {
            let (eg, values) = seed_one_at_a_time(&program);
            let expected = seeded(&eg, &values);
            assert_eq!(expected.0[5], expected.0[4]);
            for rank in [[0, 1, 2, 3], [3, 2, 1, 0]] {
                let (eg, values) = seed_in_batches(&program, &rank);
                assert_eq!(seeded(&eg, &values), expected);
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn batches_seed_what_one_at_a_time_does(
            recipe in proptest::collection::vec((0usize..9, 0usize..64, 0usize..64), 1..40),
            rank in proptest::collection::vec(0u32..8, 1..12),
        ) {
            let mut program: Program = Vec::new();
            for (kind, a, b) in recipe {
                let reads = |count: usize| [a, b][..count].iter().map(|at| at % program.len()).collect();
                let op = match (kind, program.len()) {
                    (0, _) | (_, 0) => (Term::typed("x", 8, &[]), vec![]),
                    (1, _) => (Term::typed("y", 8, &[]), vec![]),
                    (2, _) => (Term::num(8, (a % 3) as u64), vec![]),
                    (3 | 4, _) => (add(8), reads(2)),
                    (5, _) => (mul(8), reads(2)),
                    (6, _) => (Term::typed("sub", 8, &[ClassId(0); 2]), reads(2)),
                    (7, _) => (Term::typed("g", 8, &[ClassId(0); 2]), reads(2)),
                    _ => (Term::typed("neg", 8, &[ClassId(0)]), reads(1)),
                };
                program.push(op);
            }
            let (eg, values) = seed_one_at_a_time(&program);
            let expected = seeded(&eg, &values);
            let (eg, values) = seed_in_batches(&program, &rank);
            proptest::prop_assert_eq!(seeded(&eg, &values), expected);
        }
    }

    #[test]
    fn classes_with_op_reports_each_class_once_in_minting_order() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a")).class;
        let b = eg.add(Term::leaf("b")).class;
        let key = Term::leaf("a").op_key();
        assert_eq!(eg.classes_with_op(key), vec![a]);
        eg.union(a, b).unwrap();
        assert_eq!(eg.classes_with_op(key), vec![eg.root(a)]);
        assert!(eg.classes_with_op(Term::leaf("zzz").op_key()).is_empty());
    }

    /// A scope is the same graph with more merges, so a lookup under it sees
    /// what congruence over those merges proves, and the pop takes it back.
    #[test]
    fn a_scoped_lookup_sees_the_scopes_congruence() {
        let mut eg = Engine::new();
        let b = eg.add(Term::leaf("b")).class;
        let a = eg.add(Term::leaf("a")).class;
        let fa = eg.add(Term::op("f", &[a]));
        eg.rebuild();
        assert_eq!(eg.lookup(&Term::op("f", &[b])), None);
        eg.push_context();
        eg.union(a, b).unwrap();
        eg.rebuild();
        assert_eq!(eg.lookup(&Term::op("f", &[b])), Some(eg.find(fa)));
        eg.pop_context();
        assert_eq!(eg.lookup(&Term::op("f", &[b])), None);
        assert_eq!(eg.lookup(&Term::op("f", &[a])), Some(eg.find(fa)));
    }

    #[test]
    fn offsets_on_an_add_fold_into_one_row() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let y = leaf(&mut eg, "y", 32);
        let (three, five, eight) = (
            num(&mut eg, 32, 3),
            num(&mut eg, 32, 5),
            num(&mut eg, 32, 8),
        );
        let x3 = op(&mut eg, "add", 32, &[x, three]);
        let y5 = op(&mut eg, "add", 32, &[y, five]);
        let first = op(&mut eg, "add", 32, &[x3, y5]);
        let xy = op(&mut eg, "add", 32, &[x, y]);
        let second = op(&mut eg, "add", 32, &[xy, eight]);
        assert_eq!(first, second);
        assert_eq!(first, Ref::new(xy.class, 8));
        // x, y, the carrier's zero and the one add.
        assert_eq!(eg.total_size(), 4);
    }

    #[test]
    fn an_identity_element_returns_the_other_operand() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let zero = num(&mut eg, 32, 0);
        assert_eq!(op(&mut eg, "add", 32, &[x, zero]), x);
        assert_eq!(op(&mut eg, "add", 32, &[zero, x]), x);
        assert_eq!(op(&mut eg, "sub", 32, &[x, zero]), x);
        // Zero is no identity on the left of a sub.
        assert_ne!(op(&mut eg, "sub", 32, &[zero, x]).class, x.class);
    }

    #[test]
    fn constants_are_offsets_of_one_zero_and_store_no_rows() {
        let mut eg = Engine::new();
        let five = num(&mut eg, 8, 5);
        let seven = num(&mut eg, 8, 7);
        let wide = num(&mut eg, 16, 7);
        assert_eq!(five.class, seven.class);
        assert_ne!(seven.class, wide.class);
        // One zero row per carrier.
        assert_eq!(eg.total_size(), 2);
        assert_eq!(eg.int_const(seven), Some(7));
        assert_eq!(eg.const_of(seven), Some(Term::num(8, 7)));
        // Two constants sum without a row.
        assert_eq!(
            op(&mut eg, "add", 8, &[five, seven]),
            Ref::new(five.class, 12)
        );
        assert_eq!(op(&mut eg, "neg", 8, &[five]), Ref::new(five.class, 251));
        assert_eq!(eg.total_size(), 2);
    }

    #[test]
    fn a_typed_constant_types_its_zero() {
        let mut eg = Engine::new();
        let five = eg.add(Term {
            ty: Some(3),
            ..Term::num(8, 5)
        });
        assert_eq!(eg.fact(ColumnId::Type, five.class), Some(3));
    }

    #[test]
    fn a_biased_linear_node_folds_constants_with_its_bias() {
        let mut eg = Engine::new();
        let five = num(&mut eg, 8, 5);
        // not(5) == -1 - 5 at eight bits.
        assert_eq!(op(&mut eg, "not", 8, &[five]), Ref::new(five.class, 250));
        // not(x + 3) == not(x) - 3, one row.
        let x = leaf(&mut eg, "x", 8);
        let three = num(&mut eg, 8, 3);
        let x3 = op(&mut eg, "add", 8, &[x, three]);
        let not_x = op(&mut eg, "not", 8, &[x]);
        assert_eq!(
            op(&mut eg, "not", 8, &[x3]),
            Ref::new(not_x.class, not_x.offset.wrapping_sub(3) & 0xff)
        );
    }

    #[test]
    fn a_shifted_union_makes_parents_congruent_at_the_shift() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let y = leaf(&mut eg, "y", 32);
        let three = num(&mut eg, 32, 3);
        let x3 = op(&mut eg, "add", 32, &[x, three]);
        let g_y = op(&mut eg, "g", 32, &[y]);
        let g_x3 = op(&mut eg, "g", 32, &[x3]);
        let neg_y = op(&mut eg, "neg", 32, &[y]);
        let neg_x = op(&mut eg, "neg", 32, &[x]);
        eg.rebuild();
        assert!(!eg.connected(g_y, g_x3));
        eg.union(y, x3).unwrap();
        eg.rebuild();
        assert!(eg.connected(y, shifted(x, 3, 32)));
        // A parent without a coefficient re-keys at the shift...
        assert!(eg.connected(g_y, g_x3));
        // ...and one with moves the shift to its value: -(x + 3) == -x - 3.
        assert!(eg.connected(neg_y, shifted(neg_x, 3u64.wrapping_neg(), 32)));
    }

    #[test]
    fn an_offset_without_a_coefficient_is_part_of_the_key() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let one = num(&mut eg, 32, 1);
        let x1 = op(&mut eg, "add", 32, &[x, one]);
        let g_x = op(&mut eg, "g", 32, &[x]);
        let g_x1 = op(&mut eg, "g", 32, &[x1]);
        assert_ne!(g_x.class, g_x1.class);
        assert_eq!(op(&mut eg, "g", 32, &[x1]), g_x1);
        assert_eq!(
            eg.children(eg.rows(g_x1.class).next().unwrap()).as_slice(),
            &[x1]
        );
    }

    #[test]
    fn offsets_wrap_at_the_carriers_width() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 8);
        let y = leaf(&mut eg, "y", 8);
        let (big, more) = (num(&mut eg, 8, 200), num(&mut eg, 8, 100));
        let partial = op(&mut eg, "add", 8, &[x, big]);
        assert_eq!(
            op(&mut eg, "add", 8, &[partial, more]),
            Ref::new(x.class, 44)
        );
        eg.union(y, Ref::new(x.class, 255)).unwrap();
        eg.rebuild();
        assert_eq!(eg.find(Ref::new(y.class, 1)), x);
    }

    #[test]
    fn a_class_united_with_itself_at_an_offset_is_reported_and_kept() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 8);
        let y = leaf(&mut eg, "y", 8);
        eg.union(y, Ref::new(x.class, 2)).unwrap();
        assert!(!eg.contradicted());
        eg.push_context();
        assert_eq!(eg.union(y, x), Err(Contradiction));
        eg.rebuild();
        assert!(eg.contradicted());
        assert!(eg.conflicted(y.class));
        // The first placement stands.
        assert_eq!(eg.find(y), Ref::new(x.class, 2));
        eg.pop_context();
        assert!(!eg.contradicted());
        assert!(!eg.conflicted(x.class));
    }

    #[test]
    fn a_pop_restores_offsets_and_the_constants_a_scope_assumed() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let y = leaf(&mut eg, "y", 32);
        let g_y = op(&mut eg, "g", 32, &[y]);
        eg.rebuild();
        eg.push_context();
        eg.union(y, Ref::new(x.class, 3)).unwrap();
        eg.assume_const(x, Term::num(32, 4));
        eg.rebuild();
        assert_eq!(eg.int_const(y), Some(7));
        assert_eq!(eg.assumed_const(y.class), Some(Term::num(32, 7)));
        assert_eq!(eg.classes_with_const(&Term::num(32, 7)), vec![y.class]);
        eg.pop_context();
        assert_eq!(eg.find(y), y);
        assert_eq!(eg.int_const(y), None);
        assert_eq!(eg.find(g_y), g_y);
        assert_eq!(op(&mut eg, "g", 32, &[y]), g_y);
    }

    // Put `st(c)` in `c`: the cyclic equation `c = st(c)`.
    fn seed(eg: &mut Engine<Term>, width: u8, class: Ref) {
        let node = eg.insert(Term::typed("st", width, &[ClassId(0)]), &[class]);
        eg.union(node, class).unwrap();
    }

    fn seed_of(eg: &Engine<Term>, width: u8, class: Ref) -> Option<Ref> {
        eg.lookup_at(&Term::typed("st", width, &[ClassId(0)]), &[class])
    }

    fn seeds(eg: &Engine<Term>, width: u8) -> usize {
        let label = eg
            .vocabulary
            .labels
            .get(&Term::typed("st", width, &[ClassId(0)]))
            .unwrap();
        let mut count = 0;
        eg.db
            .each_row(eg.label_slot(label), 0, label.0, |_, _| count += 1);
        count
    }

    #[test]
    fn a_node_over_its_own_class_is_a_row_of_that_class_alone() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let y = leaf(&mut eg, "y", 32);
        seed(&mut eg, 32, x);
        seed(&mut eg, 32, y);
        eg.rebuild();
        assert!(!eg.connected(x, y));
        assert_eq!(seed_of(&eg, 32, x), Some(x));
        assert_eq!(seed_of(&eg, 32, y), Some(y));
        let row = eg.rows(x.class).nth(1).expect("the leaf, then its seed");
        assert_eq!(eg.children(row).as_slice(), &[x]);
        assert_eq!(eg.value(row), x);
        // Seeding again finds the row.
        seed(&mut eg, 32, x);
        assert_eq!((eg.total_size(), seeds(&eg, 32)), (4, 2));
    }

    #[test]
    fn a_union_makes_two_self_loops_one_row() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let y = leaf(&mut eg, "y", 32);
        seed(&mut eg, 32, x);
        seed(&mut eg, 32, y);
        eg.rebuild();
        eg.union(x, y).unwrap();
        eg.rebuild();
        assert!(!eg.contradicted());
        assert_eq!(seeds(&eg, 32), 1);
        assert_eq!(seed_of(&eg, 32, y), Some(x));
        for row in eg.rows(x.class) {
            assert_eq!(eg.value(row), x);
        }
    }

    // `y == x + 5` leaves `y`'s loop over `x + 5` at `x + 5`, beside `x`'s
    // over `x` at `x`: one class, each reference with its own. A label
    // without a carrier keeps the offsets in its key all the same.
    #[test]
    fn an_offset_union_keeps_each_self_loop_at_its_own_offset() {
        for width in [0, 32] {
            let mut eg = Engine::new();
            let x = leaf(&mut eg, "x", 32);
            let y = leaf(&mut eg, "y", 32);
            let zero = num(&mut eg, 32, 0);
            let c = leaf(&mut eg, "c", 32);
            for class in [x, y, zero, c] {
                seed(&mut eg, width, class);
            }
            eg.rebuild();
            eg.union(y, Ref::new(x.class, 5)).unwrap();
            eg.union(c, Ref::new(zero.class, 7)).unwrap();
            eg.rebuild();
            assert!(!eg.contradicted());
            assert_eq!(seeds(&eg, width), 4);
            assert_eq!(seed_of(&eg, width, x), Some(x));
            assert_eq!(seed_of(&eg, width, y), Some(Ref::new(x.class, 5)));
            assert_eq!(seed_of(&eg, width, zero), Some(zero));
            assert_eq!(seed_of(&eg, width, c), Some(Ref::new(zero.class, 7)));
            for class in [x, zero] {
                for row in eg.rows(class.class).filter(|&row| eg.node(row).op == "st") {
                    assert_eq!(eg.children(row).as_slice(), &[eg.value(row)]);
                }
            }
        }
    }

    #[test]
    fn a_layer_merges_self_loops_and_its_pop_parts_them() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let y = leaf(&mut eg, "y", 32);
        let z = leaf(&mut eg, "z", 32);
        seed(&mut eg, 32, x);
        seed(&mut eg, 32, y);
        eg.push_context();
        seed(&mut eg, 32, z);
        eg.union(y, Ref::new(x.class, 5)).unwrap();
        eg.rebuild();
        assert!(!eg.contradicted());
        assert_eq!(seed_of(&eg, 32, y), Some(Ref::new(x.class, 5)));
        assert_eq!(seed_of(&eg, 32, z), Some(z));
        eg.pop_context();
        assert_eq!(seed_of(&eg, 32, x), Some(x));
        assert_eq!(seed_of(&eg, 32, y), Some(y));
        assert_eq!(seed_of(&eg, 32, z), None);
        assert_eq!((eg.total_size(), seeds(&eg, 32)), (5, 2));
    }

    #[test]
    fn a_label_keeps_its_id_past_the_layer_that_met_it() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        eg.push_context();
        let inside = op(&mut eg, "g", 32, &[x]);
        let label = eg.label(eg.rows(inside.class).next().unwrap());
        eg.pop_context();
        leaf(&mut eg, "y", 32);
        let after = op(&mut eg, "g", 32, &[x]);
        assert_eq!(eg.label(eg.rows(after.class).next().unwrap()), label);
    }

    /// A row a constant made an identity, or made all constants, leaves its
    /// table and its value joins what it now is.
    #[test]
    fn a_rebuild_dissolves_rows_their_laws_now_settle() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 8);
        let y = leaf(&mut eg, "y", 8);
        let sum = op(&mut eg, "add", 8, &[x, y]);
        let negated = op(&mut eg, "neg", 8, &[y]);
        eg.rebuild();
        let add = Term::typed("add", 8, &[]).op_key();
        assert_eq!(eg.classes_with_op(add).len(), 1);
        eg.push_context();
        eg.assume_const(y, Term::num(8, 5));
        eg.rebuild();
        assert!(eg.connected(sum, Ref::new(x.class, 5)));
        assert_eq!(eg.int_const(negated), Some(251));
        assert!(eg.classes_with_op(add).is_empty());
    }

    /// Assuming a constant re-keys the rows that named the class, not every
    /// row naming a constant of its carrier.
    #[test]
    fn assuming_a_constant_dirties_the_users_of_the_class_alone() {
        let mut eg = Engine::new();
        let x = leaf(&mut eg, "x", 32);
        let g_x = op(&mut eg, "g", 32, &[x]);
        let h_g = op(&mut eg, "h", 32, &[g_x]);
        let mut users = Vec::new();
        for value in 10..60 {
            let constant = num(&mut eg, 32, value);
            let user = op(&mut eg, "g", 32, &[constant]);
            users.push(op(&mut eg, "h", 32, &[user]).class);
        }
        eg.rebuild();
        eg.take_changed();
        eg.push_context();
        eg.assume_const(x, Term::num(32, 5));
        eg.rebuild();
        let dirty = eg.scope_dirty();
        assert!(dirty.contains(&eg.root(g_x.class)) && dirty.contains(&eg.root(h_g.class)));
        assert!(dirty.len() <= 4, "{dirty:?}");
        let changed = eg.take_changed().expect("a narrowed log");
        let frontier = eg.delta(&changed, 2);
        assert!(users.iter().all(|user| !frontier.contains(user)));
        assert!(frontier.len() <= 4, "{frontier:?}");
    }
}
