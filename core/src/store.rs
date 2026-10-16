//! Where the IR lives: four id spaces cut into chunks a partition owns, the
//! columns of each, and the groupings that answer what the columns do not.
//!
//! An id is a row position, minted once and never reused. Per-entity facts are
//! positional columns; the operands, results and regions of an op, the ports
//! and results of a region and the arguments of a block are clustered tables
//! in the owner's chunk. Use lists, the operations a block or region holds and
//! the blocks of a region are groupings of those same rows, built per
//! partition and brought up to date from its write log when one is read.
//!
//! A partition fills its newest chunk and takes a fresh one, at a higher
//! index, when that is full or when the module partition has minted since. Ids
//! therefore ascend with creation inside a partition and against the module's
//! ids. A task of a parallel epoch takes fresh chunks from indices reserved for
//! its place in the epoch, so two tasks never mint one id and a commit has
//! nothing to rename.
//!
//! A [`Db`] clones by pointer and copies a chunk on its first write to it, so
//! a fork, a snapshot and a task's batch are all the same thing: a `Db`.

use std::collections::HashMap;
use std::sync::Arc;

use tir_adt::Sym;
use tir_relational::chunk::{CHUNK_ROWS, Chunk, Paged, Payload, Space, chunk_of, id_at, row_of};
use tir_relational::grouping::{Grouping, Lists, Rows};

use crate::operation::{NewOp, OpNameId, RegionIds, ValueIds};
use crate::{
    BlockId, OpId, RegionId, TypeId, Use, Value, ValueId,
    attributes::{AttributeValue, NamedAttribute},
};

/// What holds an operation: a block of an ordered region, or an unordered
/// region directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    Block(BlockId),
    Region(RegionId),
}

/// No id: an empty cell of a column that names an entity.
const NONE: u32 = u32::MAX;
/// Set in a holder cell that names a block; clear for a region.
const BLOCK_BIT: u32 = 1 << 31;
/// Chunk indices leave [`BLOCK_BIT`] free in every id.
const MAX_CHUNKS: u32 = BLOCK_BIT >> tir_relational::chunk::CHUNK_BITS;
/// The gap between the positions of neighbours in an ordered holder, so an
/// insert between two of them finds a free position.
const STRIDE: u32 = 1 << 8;

const OPS: usize = 0;
const VALUES: usize = 1;
const REGIONS: usize = 2;
const BLOCKS: usize = 3;

mod op {
    pub const NAME: usize = 0;
    /// The block or region holding the op, a block marked by the high bit.
    pub const HOLDER: usize = 1;
    pub const POSITION: usize = 2;
    /// The byte offset of the text the op was parsed from, or of the op it
    /// was built for.
    pub const LOCATION: usize = 3;
    pub const OPERAND: usize = 0;
    pub const RESULT: usize = 1;
    pub const REGION: usize = 2;
}

mod value {
    pub const TYPE: usize = 0;
    /// The defining op, or [`crate::OpId::ARGUMENT`].
    pub const DEF: usize = 1;
    /// The block or region an argument belongs to, encoded as a holder.
    pub const SITE: usize = 2;
}

mod region {
    pub const PARENT: usize = 0;
    pub const NODES: usize = 1;
    /// The position the next op or block appended takes.
    pub const NEXT: usize = 2;
    pub const PORT: usize = 0;
    pub const RESULT: usize = 1;
    /// The type each port was declared with; see [`super::block::ARG_TYPE`].
    pub const PORT_TYPE: usize = 2;
}

mod block {
    pub const REGION: usize = 0;
    pub const POSITION: usize = 1;
    pub const NEXT: usize = 2;
    pub const ARG: usize = 0;
    /// The type each argument was declared with. Retyping the value alone
    /// leaves it, and the block answers with it: selection gives a value the
    /// class its first instruction reads it through, and lowering still sizes
    /// the parameter by the type it was declared with.
    pub const ARG_TYPE: usize = 1;
}

/// The attribute list of each row of a chunk. A list is immutable and shared,
/// so copying a chunk copies pointers.
#[derive(Clone, Default)]
pub(crate) struct Attrs(Vec<Option<Arc<[NamedAttribute]>>>);

impl Payload for Attrs {
    fn push_row(&mut self) {
        self.0.push(None);
    }

    fn copy_row(&mut self, from: &Self, row: usize) {
        self.0[row] = from.0[row].clone();
    }

    fn bytes(&self) -> usize {
        self.0.capacity() * size_of::<Option<Arc<[NamedAttribute]>>>()
            + self
                .0
                .iter()
                .flatten()
                .map(|list| list.len() * size_of::<NamedAttribute>())
                .sum::<usize>()
    }
}

impl Attrs {
    fn get(&self, row: usize) -> &[NamedAttribute] {
        self.0[row].as_deref().unwrap_or(&[])
    }

    fn set(&mut self, row: usize, attributes: Vec<NamedAttribute>) {
        self.0[row] = (!attributes.is_empty()).then(|| attributes.into());
    }
}

type Ops = Space<4, 3, Attrs>;
type Values = Space<3, 0>;
type Regions = Space<3, 3>;
type Blocks = Space<3, 2, Attrs>;

fn holder_key(parent: Parent) -> u32 {
    match parent {
        Parent::Block(block) => block.raw() | BLOCK_BIT,
        Parent::Region(region) => region.raw(),
    }
}

fn holder_of(key: u32) -> Option<Parent> {
    match key {
        NONE => None,
        key if key & BLOCK_BIT != 0 => Some(Parent::Block(BlockId::new(key & !BLOCK_BIT))),
        key => Some(Parent::Region(RegionId::new(key))),
    }
}

/// One partition: the chunks it owns that a grouping enumerates, the rows
/// written since its groupings last looked, and the groupings.
#[derive(Clone, Default)]
struct Part {
    op_chunks: Vec<u32>,
    block_chunks: Vec<u32>,
    region_chunks: Vec<u32>,
    /// The regions written since the result lists last looked.
    region_log: Vec<u32>,
    /// Region result rows by value: where each value is published.
    results: Grouping,
    /// The ops written since the use lists last looked.
    op_log: Vec<u32>,
    /// The ops that joined, left or moved inside a holder since the op lists
    /// last looked.
    held_log: Vec<u32>,
    block_log: Vec<u32>,
    /// Operand rows by value: the use lists.
    uses: Grouping,
    /// Ops by holder, in position order.
    held: Lists,
    /// Blocks by region, in position order.
    blocks: Lists,
    /// Bumped by every edit of a row of the partition that sits in the IR.
    edits: u32,
    /// The partition's newest chunk of each space, and how many rows had
    /// been minted when it last took one: by the module partition for a
    /// function's, by any partition for the module's.
    open: [Option<u32>; 4],
    seen: [u32; 4],
    /// `(pass, root, version)`: a pass that ran on `root` at that version
    /// and edited nothing.
    settled: Vec<(&'static str, u32, u32)>,
}

impl Part {
    fn forget(&mut self) {
        self.uses.reset();
        self.held.reset();
        self.blocks.reset();
        self.results.reset();
        self.uses.rewind();
        self.held.rewind();
        self.blocks.rewind();
        self.results.rewind();
        self.region_log.clear();
        self.op_log.clear();
        self.held_log.clear();
        self.block_log.clear();
    }
}

struct UseRows<'a> {
    ops: &'a Ops,
    chunks: &'a [u32],
}

fn live_rows<const P: usize, const S: usize, X: Payload>(
    space: &Space<P, S, X>,
    chunks: &[u32],
    each: &mut dyn FnMut(u32),
) {
    for &index in chunks {
        let chunk = space.chunk(index);
        for row in 0..chunk.len() {
            if chunk.is_live(row) {
                each(id_at(index, row));
            }
        }
    }
}

impl Rows for UseRows<'_> {
    fn owners(&self, each: &mut dyn FnMut(u32)) {
        live_rows(self.ops, self.chunks, each);
    }

    fn stamp(&self, owner: u32) -> u32 {
        self.ops.chunk(chunk_of(owner)).stamp(row_of(owner))
    }

    fn entries(&self, owner: u32, each: &mut dyn FnMut(u32, u32)) {
        let chunk = self.ops.chunk(chunk_of(owner));
        if chunk.is_live(row_of(owner)) {
            for (index, value) in chunk.seg(op::OPERAND).of(row_of(owner)).iter().enumerate() {
                each(*value, index as u32);
            }
        }
    }
}

struct HeldRows<'a> {
    ops: &'a Ops,
    chunks: &'a [u32],
}

impl Rows for HeldRows<'_> {
    fn owners(&self, each: &mut dyn FnMut(u32)) {
        live_rows(self.ops, self.chunks, each);
    }

    fn stamp(&self, owner: u32) -> u32 {
        self.ops.chunk(chunk_of(owner)).stamp(row_of(owner))
    }

    fn entries(&self, owner: u32, each: &mut dyn FnMut(u32, u32)) {
        let (chunk, row) = (self.ops.chunk(chunk_of(owner)), row_of(owner));
        if chunk.is_live(row) && chunk.cell(op::HOLDER, row) != NONE {
            each(chunk.cell(op::HOLDER, row), chunk.cell(op::POSITION, row));
        }
    }
}

struct BlockRows<'a> {
    blocks: &'a Blocks,
    chunks: &'a [u32],
}

struct ResultRows<'a> {
    regions: &'a Regions,
    chunks: &'a [u32],
}

impl Rows for ResultRows<'_> {
    fn owners(&self, each: &mut dyn FnMut(u32)) {
        live_rows(self.regions, self.chunks, each);
    }

    fn stamp(&self, owner: u32) -> u32 {
        self.regions.chunk(chunk_of(owner)).stamp(row_of(owner))
    }

    fn entries(&self, owner: u32, each: &mut dyn FnMut(u32, u32)) {
        let (chunk, row) = (self.regions.chunk(chunk_of(owner)), row_of(owner));
        if chunk.is_live(row) {
            for (index, value) in chunk.seg(region::RESULT).of(row).iter().enumerate() {
                each(*value, index as u32);
            }
        }
    }
}

impl Rows for BlockRows<'_> {
    fn owners(&self, each: &mut dyn FnMut(u32)) {
        live_rows(self.blocks, self.chunks, each);
    }

    fn stamp(&self, owner: u32) -> u32 {
        self.blocks.chunk(chunk_of(owner)).stamp(row_of(owner))
    }

    fn entries(&self, owner: u32, each: &mut dyn FnMut(u32, u32)) {
        let (chunk, row) = (self.blocks.chunk(chunk_of(owner)), row_of(owner));
        if chunk.is_live(row) && chunk.cell(block::REGION, row) != NONE {
            each(
                chunk.cell(block::REGION, row),
                chunk.cell(block::POSITION, row),
            );
        }
    }
}

/// Where a task of a parallel epoch takes its chunk indices: task `index` of
/// `count` takes every `count`th index from `base` on, so no two tasks take
/// one and the indices depend on nothing but the task's place in the epoch.
#[derive(Clone, Copy)]
pub(crate) struct Lane {
    index: u32,
    count: u32,
    base: [u32; 4],
    taken: [u32; 4],
    /// Whether no other task of the epoch mints into this one's partition,
    /// so it may go on filling the partition's committed chunk.
    exclusive: bool,
}

/// Where the next row of each space goes.
#[derive(Clone, Copy, Default)]
struct Mint {
    /// The partition new rows belong to.
    target: u32,
    lane: Option<Lane>,
    /// Rows minted per space, by any partition and by the module's.
    minted: [u32; 4],
    module_minted: [u32; 4],
}

/// Live rows, chunks and bytes of each space, for the memory census.
#[derive(Debug, Default, Clone, Copy)]
pub struct Census {
    pub ops_live: usize,
    pub values_live: usize,
    pub blocks_live: usize,
    pub regions_live: usize,
    pub ops_chunks: usize,
    pub values_chunks: usize,
    pub blocks_chunks: usize,
    pub regions_chunks: usize,
    pub ops_bytes: usize,
    pub values_bytes: usize,
    pub blocks_bytes: usize,
    pub regions_bytes: usize,
    /// Bytes the groupings hold.
    pub groupings_bytes: usize,
    /// Chunks written since the last commit, and the bytes they hold.
    pub touched_chunks: usize,
    pub touched_bytes: usize,
}

impl Census {
    pub fn bytes(&self) -> usize {
        self.ops_bytes + self.values_bytes + self.blocks_bytes + self.regions_bytes
    }
}

#[derive(Clone, Copy)]
pub(crate) struct OpRow<'a> {
    chunk: &'a Chunk<4, 3, Attrs>,
    row: usize,
}

impl<'a> OpRow<'a> {
    pub(crate) fn name(self) -> OpNameId {
        OpNameId::new(self.chunk.cell(op::NAME, self.row))
    }

    pub(crate) fn operands(self) -> impl ExactSizeIterator<Item = ValueId> + 'a {
        let cells = self.chunk.seg(op::OPERAND).of(self.row);
        cells.iter().map(|raw| ValueId::from_number(*raw))
    }

    pub(crate) fn results(self) -> impl ExactSizeIterator<Item = ValueId> + 'a {
        let cells = self.chunk.seg(op::RESULT).of(self.row);
        cells.iter().map(|raw| ValueId::from_number(*raw))
    }

    pub(crate) fn owns_regions(self) -> bool {
        !self.chunk.seg(op::REGION).of(self.row).is_empty()
    }

    pub(crate) fn attrs(self) -> &'a [NamedAttribute] {
        self.chunk.extra.get(self.row)
    }
}

#[derive(Clone)]
pub(crate) struct Db {
    ops: Ops,
    values: Values,
    regions: Regions,
    blocks: Blocks,
    parts: Paged<Part>,
    /// The stamp a row written now gets. It moves past every grouping
    /// refresh, so a row written after one is logged again.
    tick: u32,
    /// The stamp at the last commit: a chunk first written after it is in
    /// `touched`, and a row stamped after it was written since.
    committed: u32,
    /// The chunks written since the last commit, per space.
    touched: [Vec<u32>; 4],
    mint: Mint,
    /// Bumped by every edit, whichever partition it lands in.
    edits: u32,
    /// The partitions an edit was counted for since the last commit.
    bumped: Vec<u32>,
    /// Ops whose subtree an edit touched, innermost per edit.
    dirty: Vec<OpId>,
    /// Erased op → the op that took its place, so a pipeline can follow its
    /// root across a replacement.
    pub replaced_ops: HashMap<OpId, OpId>,
    /// `(old, new)` for every value whose uses were replaced since the last
    /// commit, in order. A commit of several batches replays one batch's
    /// replacements over the rows the others wrote.
    retargets: Vec<(u32, u32)>,
    /// The location an op added now gets.
    origin: u32,
    /// The stamp each `builtin.refine` had when its obligation was proved.
    pub proved_refines: HashMap<OpId, u32>,
}

/// Mark `chunk` as written since the last commit.
macro_rules! touch {
    ($db:expr, $space:ident, $kind:expr, $index:expr) => {{
        let chunk = $db.$space.chunk_mut($index);
        if chunk.touched <= $db.committed {
            chunk.touched = $db.tick;
            $db.touched[$kind].push($index);
        }
        chunk
    }};
}

impl Db {
    pub(crate) fn new() -> Self {
        let mut parts = Paged::default();
        parts.put(0, Arc::new(Part::default()));
        Db {
            ops: Space::default(),
            values: Space::default(),
            regions: Space::default(),
            blocks: Space::default(),
            parts,
            tick: 1,
            committed: 0,
            touched: Default::default(),
            mint: Mint::default(),
            edits: 0,
            bumped: Vec::new(),
            dirty: Vec::new(),
            replaced_ops: HashMap::new(),
            retargets: Vec::new(),
            origin: NONE,
            proved_refines: HashMap::new(),
        }
    }

    // Minting.

    /// The chunk of `space` the next row goes to, if the target partition's
    /// newest chunk may still take it.
    ///
    /// Ids ascend with creation inside a partition, and against the module
    /// partition: a function's chunk closes once the module mints a row,
    /// and the module's once anything does. A function's values are then
    /// ordered by age among themselves and against every module-level value
    /// they can be compared with. A task keeps to the chunks of its lane,
    /// unless its partition is its alone.
    fn open_chunk(&self, space: usize, full: impl Fn(u32) -> bool) -> Option<u32> {
        let target = self.mint.target;
        let part = self.parts.get(target as usize)?;
        let index = part.open[space].filter(|index| !full(*index))?;
        let current = match target {
            0 => self.mint.minted[space],
            _ => self.mint.module_minted[space],
        };
        let own = self
            .mint
            .lane
            .is_none_or(|lane| index >= lane.base[space] || (lane.exclusive && target != 0));
        (part.seen[space] == current && own).then_some(index)
    }

    /// Count a row minted into `index`, the target partition's newest chunk.
    fn minted(&mut self, space: usize, index: u32) {
        let target = self.mint.target;
        self.mint.minted[space] = self.mint.minted[space].wrapping_add(1);
        if target == 0 {
            self.mint.module_minted[space] = self.mint.module_minted[space].wrapping_add(1);
        }
        let seen = match target {
            0 => self.mint.minted[space],
            _ => self.mint.module_minted[space],
        };
        let part = self.part_mut(target);
        part.open[space] = Some(index);
        part.seen[space] = seen;
    }

    /// The index of a fresh chunk of `space`, past every chunk this context
    /// has minted into.
    fn fresh(&mut self, space: usize, chunks: u32) -> u32 {
        let index = match &mut self.mint.lane {
            None => chunks,
            Some(lane) => {
                let index = lane.base[space] + lane.taken[space] * lane.count + lane.index;
                lane.taken[space] += 1;
                index
            }
        };
        assert!(index < MAX_CHUNKS, "id space exhausted");
        self.touched[space].push(index);
        index
    }

    fn part_mut(&mut self, part: u32) -> &mut Part {
        self.parts.get_mut(part as usize).expect("a partition")
    }

    fn mint_op(&mut self, cells: [u32; 4]) -> OpId {
        let index = match self.open_chunk(OPS, |index| self.ops.chunk(index).is_full()) {
            Some(index) => index,
            None => {
                let index = self.fresh(OPS, self.ops.chunks());
                let mut chunk = Chunk::new(self.mint.target);
                chunk.touched = self.tick;
                self.ops.put(index, Arc::new(chunk));
                self.part_mut(self.mint.target).op_chunks.push(index);
                index
            }
        };
        let tick = self.tick;
        let row = touch!(self, ops, OPS, index).push(cells, tick);
        self.minted(OPS, index);
        let id = id_at(index, row);
        self.part_mut(self.mint.target).op_log.push(id);
        OpId::new(id)
    }

    fn mint_value(&mut self, cells: [u32; 3]) -> ValueId {
        let index = match self.open_chunk(VALUES, |index| self.values.chunk(index).is_full()) {
            Some(index) => index,
            None => {
                let index = self.fresh(VALUES, self.values.chunks());
                let mut chunk = Chunk::new(self.mint.target);
                chunk.touched = self.tick;
                self.values.put(index, Arc::new(chunk));
                index
            }
        };
        let tick = self.tick;
        let row = touch!(self, values, VALUES, index).push(cells, tick);
        self.minted(VALUES, index);
        ValueId::from_number(id_at(index, row))
    }

    fn mint_region(&mut self, cells: [u32; 3]) -> RegionId {
        let index = match self.open_chunk(REGIONS, |index| self.regions.chunk(index).is_full()) {
            Some(index) => index,
            None => {
                let index = self.fresh(REGIONS, self.regions.chunks());
                let mut chunk = Chunk::new(self.mint.target);
                chunk.touched = self.tick;
                self.regions.put(index, Arc::new(chunk));
                self.part_mut(self.mint.target).region_chunks.push(index);
                index
            }
        };
        let tick = self.tick;
        let row = touch!(self, regions, REGIONS, index).push(cells, tick);
        self.minted(REGIONS, index);
        let id = id_at(index, row);
        self.part_mut(self.mint.target).region_log.push(id);
        RegionId::new(id)
    }

    fn mint_block(&mut self, cells: [u32; 3]) -> BlockId {
        let index = match self.open_chunk(BLOCKS, |index| self.blocks.chunk(index).is_full()) {
            Some(index) => index,
            None => {
                let index = self.fresh(BLOCKS, self.blocks.chunks());
                let mut chunk = Chunk::new(self.mint.target);
                chunk.touched = self.tick;
                self.blocks.put(index, Arc::new(chunk));
                self.part_mut(self.mint.target).block_chunks.push(index);
                index
            }
        };
        let tick = self.tick;
        let row = touch!(self, blocks, BLOCKS, index).push(cells, tick);
        self.minted(BLOCKS, index);
        let id = id_at(index, row);
        self.part_mut(self.mint.target).block_log.push(id);
        BlockId::new(id)
    }

    /// Open a partition and mint into it; answers the partition minted into
    /// before. A task of a parallel epoch opens none: its lane is its own.
    pub(crate) fn enter_partition(&mut self) -> u32 {
        let previous = self.mint.target;
        if self.mint.lane.is_none() {
            let part = self.parts.len() as u32;
            self.parts.put(part as usize, Arc::new(Part::default()));
            self.mint_into(part);
        }
        previous
    }

    /// Mint into `part` from here on.
    pub(crate) fn mint_into(&mut self, part: u32) {
        self.mint.target = part;
    }

    /// Mint into the partition owning `op` from here on, when it has one of
    /// its own; answers the partition minted into before. A pass walking a
    /// module puts what it builds for a function into that function's chunks.
    pub(crate) fn mint_under(&mut self, op: OpId) -> Option<u32> {
        let part = self.partition_of(op);
        (part != 0 && part != self.mint.target && self.mint.lane.is_none())
            .then(|| std::mem::replace(&mut self.mint.target, part))
    }

    /// The partition owning `op`'s row.
    pub(crate) fn partition_of(&self, op: OpId) -> u32 {
        self.ops.chunk(chunk_of(op.raw())).owner
    }

    /// The ops named one of `names` under `root`, by id: a scan of the name
    /// column of `root`'s partition and the module's, where whatever is under
    /// an op of a partition of its own sits.
    pub(crate) fn ops_named(&self, root: OpId, names: &[OpNameId]) -> Vec<OpId> {
        let part = self.partition_of(root);
        let parts = if part == 0 { &[0][..] } else { &[part, 0][..] };
        let mut found = Vec::new();
        for &part in parts {
            let Some(held) = self.parts.get(part as usize) else {
                continue;
            };
            for &index in &held.op_chunks {
                let chunk = self.ops.chunk(index);
                let named = chunk
                    .col(op::NAME)
                    .iter()
                    .enumerate()
                    .filter(|&(row, &name)| {
                        names.contains(&OpNameId::new(name))
                            && chunk.is_live(row)
                            && chunk.cell(op::HOLDER, row) != NONE
                    });
                found.extend(named.map(|(row, _)| OpId::new(id_at(index, row))));
            }
        }
        let under = |&op: &OpId| {
            std::iter::successors(self.enclosing_op_of(op), |&holder| {
                self.enclosing_op_of(holder)
            })
            .any(|holder| holder == root)
        };
        found.retain(under);
        found.sort_unstable_by_key(|op| op.raw());
        found
    }

    // Row writers: each marks the chunk, stamps the row, logs it for the
    // partition's groupings and bumps the partition's edit count.

    fn op_row(&mut self, id: OpId) -> (&mut Chunk<4, 3, Attrs>, usize) {
        let (index, row) = (chunk_of(id.raw()), row_of(id.raw()));
        let tick = self.tick;
        let chunk = touch!(self, ops, OPS, index);
        let (owner, logged) = (chunk.owner, chunk.set_stamp(row, tick));
        if logged {
            self.part_mut(owner).op_log.push(id.raw());
        }
        (self.ops.chunk_mut(index), row)
    }

    fn value_row(&mut self, id: ValueId) -> (&mut Chunk<3, 0>, usize) {
        let (index, row) = (chunk_of(id.number()), row_of(id.number()));
        let tick = self.tick;
        let chunk = touch!(self, values, VALUES, index);
        chunk.set_stamp(row, tick);
        (self.values.chunk_mut(index), row)
    }

    fn region_row(&mut self, id: RegionId) -> (&mut Chunk<3, 3>, usize) {
        let (index, row) = (chunk_of(id.raw()), row_of(id.raw()));
        let tick = self.tick;
        let chunk = touch!(self, regions, REGIONS, index);
        let (owner, logged) = (chunk.owner, chunk.set_stamp(row, tick));
        if logged {
            self.part_mut(owner).region_log.push(id.raw());
        }
        (self.regions.chunk_mut(index), row)
    }

    fn block_row(&mut self, id: BlockId) -> (&mut Chunk<3, 2, Attrs>, usize) {
        let (index, row) = (chunk_of(id.raw()), row_of(id.raw()));
        let tick = self.tick;
        let chunk = touch!(self, blocks, BLOCKS, index);
        let (owner, logged) = (chunk.owner, chunk.set_stamp(row, tick));
        if logged {
            self.part_mut(owner).block_log.push(id.raw());
        }
        (self.blocks.chunk_mut(index), row)
    }

    /// Record that a row of partition `part` names `value`, so a use-list
    /// read of it looks in that partition too.
    fn note_value(&mut self, value: u32, part: u32) {
        let index = chunk_of(value);
        if self
            .values
            .find(index)
            .is_some_and(|chunk| !chunk.names_from(part))
        {
            touch!(self, values, VALUES, index).foreign.push(part);
        }
    }

    fn note_holder(&mut self, key: u32, part: u32) {
        if key == NONE {
            return;
        }
        if key & BLOCK_BIT != 0 {
            let index = chunk_of(key & !BLOCK_BIT);
            if self
                .blocks
                .find(index)
                .is_some_and(|chunk| !chunk.names_from(part))
            {
                touch!(self, blocks, BLOCKS, index).foreign.push(part);
            }
        } else {
            self.note_region(key, part);
        }
    }

    fn note_region(&mut self, region: u32, part: u32) {
        let index = chunk_of(region);
        if self
            .regions
            .find(index)
            .is_some_and(|chunk| !chunk.names_from(part))
        {
            touch!(self, regions, REGIONS, index).foreign.push(part);
        }
    }

    // Liveness.

    #[inline]
    pub(crate) fn op_live(&self, id: OpId) -> bool {
        self.ops.is_live(id.raw())
    }

    #[inline]
    pub(crate) fn value_live(&self, id: ValueId) -> bool {
        self.values.is_live(id.number())
    }

    #[inline]
    pub(crate) fn block_live(&self, id: BlockId) -> bool {
        self.blocks.is_live(id.raw())
    }

    #[inline]
    pub(crate) fn region_live(&self, id: RegionId) -> bool {
        self.regions.is_live(id.raw())
    }

    /// The chunk and row of a live op.
    #[inline]
    fn op(&self, id: OpId) -> Option<(&Chunk<4, 3, Attrs>, usize)> {
        let chunk = self.ops.find(chunk_of(id.raw()))?;
        let row = row_of(id.raw());
        chunk.is_live(row).then_some((chunk, row))
    }

    #[inline]
    fn region(&self, id: RegionId) -> (&Chunk<3, 3>, usize) {
        let chunk = self.regions.find(chunk_of(id.raw()));
        let row = row_of(id.raw());
        match chunk {
            Some(chunk) if chunk.is_live(row) => (chunk, row),
            _ => panic!("live region {id:?}"),
        }
    }

    #[inline]
    fn block(&self, id: BlockId) -> (&Chunk<3, 2, Attrs>, usize) {
        let chunk = self.blocks.find(chunk_of(id.raw()));
        let row = row_of(id.raw());
        match chunk {
            Some(chunk) if chunk.is_live(row) => (chunk, row),
            _ => panic!("live block {id:?}"),
        }
    }

    // Reads.

    pub(crate) fn op_name(&self, id: OpId) -> OpNameId {
        self.op_cells(id).name()
    }

    pub(crate) fn op_location(&self, id: OpId) -> Option<u32> {
        let (chunk, row) = self.op(id)?;
        Some(chunk.cell(op::LOCATION, row)).filter(|&location| location != NONE)
    }

    /// Locate the ops added from now on at `location`; returns the location
    /// they had.
    pub(crate) fn set_origin(&mut self, location: Option<u32>) -> Option<u32> {
        let previous = std::mem::replace(&mut self.origin, location.unwrap_or(NONE));
        Some(previous).filter(|&previous| previous != NONE)
    }

    /// The stamp of `id`'s row: it moves whenever the op is written.
    pub(crate) fn op_stamp(&self, id: OpId) -> u32 {
        let (chunk, row) = self.op(id).expect("live operation");
        chunk.stamp(row)
    }

    pub(crate) fn op_cells(&self, id: OpId) -> OpRow<'_> {
        let (chunk, row) = self.op(id).expect("live operation");
        OpRow { chunk, row }
    }

    pub(crate) fn value_type(&self, id: ValueId) -> TypeId {
        let chunk = self.values.chunk(chunk_of(id.number()));
        TypeId::from_number(chunk.cell(value::TYPE, row_of(id.number())))
    }

    pub(crate) fn value_def(&self, id: ValueId) -> Option<OpId> {
        let chunk = self.values.chunk(chunk_of(id.number()));
        let def = OpId::new(chunk.cell(value::DEF, row_of(id.number())));
        (def != OpId::ARGUMENT).then_some(def)
    }

    pub(crate) fn operands(&self, id: OpId) -> ValueIds {
        self.op(id).map_or_else(ValueIds::new, |(chunk, row)| {
            chunk
                .seg(op::OPERAND)
                .of(row)
                .iter()
                .map(|raw| ValueId::from_number(*raw))
                .collect()
        })
    }

    pub(crate) fn results(&self, id: OpId) -> ValueIds {
        self.op(id).map_or_else(ValueIds::new, |(chunk, row)| {
            chunk
                .seg(op::RESULT)
                .of(row)
                .iter()
                .map(|raw| ValueId::from_number(*raw))
                .collect()
        })
    }

    pub(crate) fn regions(&self, id: OpId) -> RegionIds {
        self.op(id).map_or_else(RegionIds::new, |(chunk, row)| {
            chunk
                .seg(op::REGION)
                .of(row)
                .iter()
                .map(|raw| RegionId::new(*raw))
                .collect()
        })
    }

    pub(crate) fn op_attrs(&self, id: OpId) -> &[NamedAttribute] {
        self.op(id).map_or(&[], |(chunk, row)| chunk.extra.get(row))
    }

    pub(crate) fn op_parent(&self, id: OpId) -> Option<Parent> {
        let (chunk, row) = self.op(id)?;
        holder_of(chunk.cell(op::HOLDER, row))
    }

    pub(crate) fn value(&self, id: ValueId) -> Option<Value> {
        let chunk = self.values.find(chunk_of(id.number()))?;
        let row = row_of(id.number());
        chunk.is_live(row).then(|| {
            let def = OpId::new(chunk.cell(value::DEF, row));
            Value::new(
                id,
                TypeId::from_number(chunk.cell(value::TYPE, row)),
                (def != OpId::ARGUMENT).then_some(def),
            )
        })
    }

    fn value_site(&self, id: ValueId) -> Option<Parent> {
        let chunk = self.values.find(chunk_of(id.number()))?;
        let row = row_of(id.number());
        chunk
            .is_live(row)
            .then(|| holder_of(chunk.cell(value::SITE, row)))?
    }

    pub(crate) fn value_block(&self, id: ValueId) -> Option<BlockId> {
        match self.value_site(id)? {
            Parent::Block(block) => Some(block),
            Parent::Region(_) => None,
        }
    }

    pub(crate) fn value_region(&self, id: ValueId) -> Option<RegionId> {
        match self.value_site(id)? {
            Parent::Region(region) => Some(region),
            Parent::Block(_) => None,
        }
    }

    /// The arguments `ids` as their holder declared them, typed by `types`.
    /// No operation defines an argument.
    fn values_of(ids: &[u32], types: &[u32]) -> Vec<Value> {
        ids.iter()
            .zip(types)
            .map(|(raw, ty)| Value::new(ValueId::from_number(*raw), TypeId::from_number(*ty), None))
            .collect()
    }

    pub(crate) fn block_args(&self, id: BlockId) -> Vec<Value> {
        let (chunk, row) = self.block(id);
        Self::values_of(
            chunk.seg(block::ARG).of(row),
            chunk.seg(block::ARG_TYPE).of(row),
        )
    }

    pub(crate) fn block_attrs(&self, id: BlockId) -> &[NamedAttribute] {
        let (chunk, row) = self.block(id);
        chunk.extra.get(row)
    }

    pub(crate) fn block_parent(&self, id: BlockId) -> Option<RegionId> {
        let chunk = self.blocks.find(chunk_of(id.raw()))?;
        let row = row_of(id.raw());
        let region = chunk.is_live(row).then(|| chunk.cell(block::REGION, row))?;
        (region != NONE).then(|| RegionId::new(region))
    }

    pub(crate) fn region_parent(&self, id: RegionId) -> Option<OpId> {
        let (chunk, row) = self.region(id);
        let parent = chunk.cell(region::PARENT, row);
        (parent != NONE).then(|| OpId::new(parent))
    }

    pub(crate) fn region_is_nodes(&self, id: RegionId) -> bool {
        let (chunk, row) = self.region(id);
        chunk.cell(region::NODES, row) != 0
    }

    /// The ports an unordered region owns itself; none for an ordered one.
    pub(crate) fn region_ports(&self, id: RegionId) -> Vec<Value> {
        let (chunk, row) = self.region(id);
        Self::values_of(
            chunk.seg(region::PORT).of(row),
            chunk.seg(region::PORT_TYPE).of(row),
        )
    }

    pub(crate) fn region_results(&self, id: RegionId) -> Vec<ValueId> {
        let (chunk, row) = self.region(id);
        chunk
            .seg(region::RESULT)
            .of(row)
            .iter()
            .map(|raw| ValueId::from_number(*raw))
            .collect()
    }

    /// The op enclosing `op`, walking out through whatever holds it.
    pub(crate) fn enclosing_op_of(&self, op: OpId) -> Option<OpId> {
        match self.op_parent(op)? {
            Parent::Block(block) => self.enclosing_op(block),
            Parent::Region(region) => self.region_parent_if_live(region),
        }
    }

    pub(crate) fn enclosing_op(&self, block: BlockId) -> Option<OpId> {
        self.region_parent_if_live(self.block_parent(block)?)
    }

    fn region_parent_if_live(&self, region: RegionId) -> Option<OpId> {
        self.region_live(region)
            .then(|| self.region_parent(region))?
    }

    /// A counter that moves with every edit under `op`: the edits to its
    /// partition and the module's, or every edit for an op of the module's
    /// own partition. Everything under an op of a partition of its own sits
    /// in that partition or in the module's.
    pub(crate) fn version(&self, op: OpId) -> u32 {
        let Some(chunk) = self.ops.find(chunk_of(op.raw())) else {
            return 0;
        };
        let edits = |part: u32| self.parts.get(part as usize).map_or(0, |part| part.edits);
        match chunk.owner {
            0 => self.edits,
            owner => edits(owner).wrapping_add(edits(0)),
        }
    }

    // Moves with every edit to the module's own partition: its symbols and
    // type definitions.
    pub(crate) fn module_version(&self) -> u32 {
        self.parts.get(0).map_or(0, |part| part.edits)
    }

    fn bump(&mut self, part: u32) {
        self.edits = self.edits.wrapping_add(1);
        if !self.bumped.contains(&part) {
            self.bumped.push(part);
        }
        let part = self.part_mut(part);
        part.edits = part.edits.wrapping_add(1);
    }

    /// Whether `pass` already ran on `op` as it stands and edited nothing.
    /// Only an op with a partition of its own is tracked: its version counts
    /// the edits under it and no other function's.
    pub(crate) fn is_settled(&self, pass: &'static str, op: OpId) -> bool {
        let (part, version) = (self.partition_of(op), self.version(op));
        part != 0
            && self
                .parts
                .get(part as usize)
                .is_some_and(|part| part.settled.contains(&(pass, op.raw(), version)))
    }

    /// Record that `pass` ran on `op` as it stands and edited nothing.
    pub(crate) fn settle(&mut self, pass: &'static str, op: OpId) {
        let (part, version) = (self.partition_of(op), self.version(op));
        if part == 0 {
            return;
        }
        let settled = &mut self.part_mut(part).settled;
        match settled
            .iter_mut()
            .find(|held| held.0 == pass && held.1 == op.raw())
        {
            Some(held) => held.2 = version,
            None => settled.push((pass, op.raw(), version)),
        }
    }

    /// Count an edit of `op`. An op the module's partition minted counts
    /// for the partition holding it: a module pass builds an op and puts it
    /// into a function, which is the one function that changed.
    fn bump_op(&mut self, id: OpId) {
        let Some((chunk, row)) = self.op(id) else {
            return;
        };
        let part = match (chunk.owner, holder_of(chunk.cell(op::HOLDER, row))) {
            (0, Some(Parent::Block(block))) => self
                .blocks
                .find(chunk_of(block.raw()))
                .map_or(0, |chunk| chunk.owner),
            (0, Some(Parent::Region(region))) => self
                .regions
                .find(chunk_of(region.raw()))
                .map_or(0, |chunk| chunk.owner),
            (owner, _) => owner,
        };
        self.bump(part);
    }

    fn bump_value(&mut self, id: ValueId) {
        if let Some(chunk) = self.values.find(chunk_of(id.number())) {
            self.bump(chunk.owner);
        }
    }

    fn bump_region(&mut self, id: RegionId) {
        if let Some(chunk) = self.regions.find(chunk_of(id.raw())) {
            self.bump(chunk.owner);
        }
    }

    fn bump_block(&mut self, id: BlockId) {
        if let Some(chunk) = self.blocks.find(chunk_of(id.raw())) {
            self.bump(chunk.owner);
        }
    }

    // Groupings.

    /// The partitions whose rows may name an id of a chunk owned by `owner`
    /// with `foreign` readers.
    fn readers(owner: u32, foreign: &[u32]) -> smallvec::SmallVec<[u32; 4]> {
        std::iter::once(owner)
            .chain(foreign.iter().copied())
            .collect()
    }

    fn refresh_uses(&mut self, part: u32) {
        let current = self
            .parts
            .get(part as usize)
            .is_some_and(|part| part.uses.cursor() == Some(part.op_log.len()));
        if current {
            return;
        }
        let tick = self.tick;
        let Some(held) = self.parts.get_mut(part as usize) else {
            return;
        };
        let rows = UseRows {
            ops: &self.ops,
            chunks: &held.op_chunks,
        };
        held.uses.refresh(&rows, &held.op_log, tick);
        held.op_log.clear();
        held.uses.rewind();
        self.tick += 1;
    }

    fn refresh_held(&mut self, part: u32) {
        let current = self
            .parts
            .get(part as usize)
            .is_some_and(|part| part.held.cursor() == Some(part.held_log.len()));
        if current {
            return;
        }
        let Some(held) = self.parts.get_mut(part as usize) else {
            return;
        };
        let rows = HeldRows {
            ops: &self.ops,
            chunks: &held.op_chunks,
        };
        held.held.refresh(&rows, &held.held_log);
        held.held_log.clear();
        held.held.rewind();
    }

    fn refresh_blocks(&mut self, part: u32) {
        let current = self
            .parts
            .get(part as usize)
            .is_some_and(|part| part.blocks.cursor() == Some(part.block_log.len()));
        if current {
            return;
        }
        let Some(held) = self.parts.get_mut(part as usize) else {
            return;
        };
        let rows = BlockRows {
            blocks: &self.blocks,
            chunks: &held.block_chunks,
        };
        held.blocks.refresh(&rows, &held.block_log);
        held.block_log.clear();
        held.blocks.rewind();
        self.tick += 1;
    }

    /// Every operand slot naming `value`: ascending by op, with the ops
    /// written since their partition's base was built last.
    pub(crate) fn uses(&mut self, value: ValueId) -> Vec<Use> {
        let Some(chunk) = self.values.find(chunk_of(value.number())) else {
            return Vec::new();
        };
        let readers = Self::readers(chunk.owner, &chunk.foreign);
        let mut found = Vec::new();
        for part in readers {
            self.refresh_uses(part);
            let Some(held) = self.parts.get(part as usize) else {
                continue;
            };
            let rows = UseRows {
                ops: &self.ops,
                chunks: &held.op_chunks,
            };
            held.uses.read(&rows, value.number(), &mut found);
        }
        found
            .into_iter()
            .map(|(op, index)| Use::new(OpId::new(op), index as usize))
            .collect()
    }

    pub(crate) fn is_used(&mut self, value: ValueId) -> bool {
        let Some(chunk) = self.values.find(chunk_of(value.number())) else {
            return false;
        };
        let readers = Self::readers(chunk.owner, &chunk.foreign);
        readers.into_iter().any(|part| {
            self.refresh_uses(part);
            self.parts.get(part as usize).is_some_and(|held| {
                let rows = UseRows {
                    ops: &self.ops,
                    chunks: &held.op_chunks,
                };
                held.uses.any(&rows, value.number())
            })
        })
    }

    fn position(&self, op: u32) -> u32 {
        self.ops.chunk(chunk_of(op)).cell(op::POSITION, row_of(op))
    }

    /// The ops `holder` holds, in position order.
    fn held(&mut self, holder: Parent) -> Vec<u32> {
        let key = holder_key(holder);
        let (owner, foreign) = match holder {
            Parent::Block(block) => {
                let (chunk, _) = self.block(block);
                (chunk.owner, &chunk.foreign)
            }
            Parent::Region(region) => {
                let (chunk, _) = self.region(region);
                (chunk.owner, &chunk.foreign)
            }
        };
        if foreign.is_empty() {
            self.refresh_held(owner);
            return self
                .parts
                .get(owner as usize)
                .map_or_else(Vec::new, |part| part.held.read(key).to_vec());
        }
        // The ops other partitions put into the holder interleave with its
        // own by position; most holders have none.
        let mut found = Vec::new();
        let mut mixed = false;
        for part in Self::readers(owner, foreign) {
            self.refresh_held(part);
            if let Some(part) = self.parts.get(part as usize) {
                let held = part.held.read(key);
                mixed |= !found.is_empty() && !held.is_empty();
                found.extend_from_slice(held);
            }
        }
        if mixed {
            found.sort_unstable_by_key(|&op| (self.position(op), op));
        }
        found
    }

    pub(crate) fn block_ops(&mut self, block: BlockId) -> Vec<OpId> {
        let held = self.held(Parent::Block(block));
        held.into_iter().map(OpId::new).collect()
    }

    /// The ops an unordered region holds, in the order they joined it.
    pub(crate) fn region_ops(&mut self, region: RegionId) -> Vec<OpId> {
        let held = self.held(Parent::Region(region));
        held.into_iter().map(OpId::new).collect()
    }

    pub(crate) fn region_blocks(&mut self, region: RegionId) -> Vec<BlockId> {
        let (chunk, _) = self.region(region);
        let readers = Self::readers(chunk.owner, &chunk.foreign);
        let mut found = Vec::new();
        let mut mixed = false;
        for part in &readers {
            self.refresh_blocks(*part);
            if let Some(part) = self.parts.get(*part as usize) {
                let held = part.blocks.read(region.raw());
                mixed |= !found.is_empty() && !held.is_empty();
                found.extend_from_slice(held);
            }
        }
        if mixed {
            found.sort_unstable_by_key(|&block| {
                let position = self
                    .blocks
                    .chunk(chunk_of(block))
                    .cell(block::POSITION, row_of(block));
                (position, block)
            });
        }
        found.into_iter().map(BlockId::new).collect()
    }

    /// The regions `op` owns, or nothing for an erased op.
    fn regions_raw(&self, op: u32) -> &[u32] {
        self.op(OpId::new(op))
            .map_or(&[], |(chunk, row)| chunk.seg(op::REGION).of(row))
    }

    /// Every op `region` holds: its own for an unordered region, its blocks'
    /// in block order for an ordered one.
    fn region_op_ids(&mut self, region: RegionId, out: &mut Vec<u32>) {
        if self.region_is_nodes(region) {
            let held = self.held(Parent::Region(region));
            out.extend(held);
            return;
        }
        for block in self.region_blocks(region) {
            let held = self.held(Parent::Block(block));
            out.extend(held);
        }
    }

    /// Every op under `roots`, the roots included, each ahead of the ops
    /// its regions hold.
    fn subtree(&mut self, mut found: Vec<u32>) -> Vec<u32> {
        let mut index = 0;
        while index < found.len() {
            // Most ops own no region, and those cost two offset loads here.
            let regions: smallvec::SmallVec<[u32; 2]> =
                self.regions_raw(found[index]).iter().copied().collect();
            for region in regions {
                self.region_op_ids(RegionId::new(region), &mut found);
            }
            index += 1;
        }
        found
    }

    pub(crate) fn subtree_ops(&mut self, roots: &[OpId]) -> Vec<OpId> {
        let found = self.subtree(roots.iter().map(|op| op.raw()).collect());
        found.into_iter().map(OpId::new).collect()
    }

    /// `region` and every region nested in it, each ahead of the regions
    /// inside it.
    pub(crate) fn nested_regions(&mut self, region: RegionId) -> Vec<RegionId> {
        let mut ops = Vec::new();
        self.region_op_ids(region, &mut ops);
        let ops = self.subtree(ops);
        let mut regions = vec![region];
        for op in ops {
            regions.extend(self.regions_raw(op).iter().map(|raw| RegionId::new(*raw)));
        }
        regions
    }

    fn region_of_op(&self, op: OpId) -> Option<RegionId> {
        match self.op_parent(op)? {
            Parent::Region(region) => Some(region),
            Parent::Block(block) => self.block_parent(block),
        }
    }

    pub(crate) fn sibling_read(
        &self,
        reader: OpId,
        value: ValueId,
        within: Option<RegionId>,
    ) -> Option<(OpId, OpId)> {
        let producer = self.value(value)?.defining_op()?;
        let home = self.region_of_op(producer)?;
        if within.is_some_and(|region| region != home) {
            return None;
        }
        let mut reader = reader;
        loop {
            let region = self.region_of_op(reader)?;
            if region == home {
                return Some((reader, producer));
            }
            reader = self.region_parent_if_live(region)?;
        }
    }

    pub(crate) fn sibling_reads(&mut self, root: RegionId, nested: bool) -> Vec<(OpId, OpId)> {
        let within = (!nested).then_some(root);
        let mut ops = Vec::new();
        self.region_op_ids(root, &mut ops);
        let ops = self.subtree(ops);
        let mut pairs = Vec::with_capacity(2 * ops.len());
        for op in ops {
            let reader = OpId::new(op);
            let Some((chunk, row)) = self.op(reader) else {
                continue;
            };
            let operands = chunk.seg(op::OPERAND).of(row).iter();
            let results = chunk.seg(op::REGION).of(row).iter().flat_map(|region| {
                let (chunk, row) = self.region(RegionId::new(*region));
                chunk.seg(region::RESULT).of(row).iter()
            });
            pairs.extend(operands.chain(results).filter_map(|value| {
                self.sibling_read(reader, ValueId::from_number(*value), within)
            }));
        }
        pairs
    }

    // Edits.

    pub(crate) fn add_operation(&mut self, new: NewOp) -> OpId {
        let id = self.mint_op([new.name.index() as u32, NONE, 0, self.origin]);
        let part = self.mint.target;
        let operands: Vec<u32> = new.operands.iter().map(|value| value.number()).collect();
        let results: Vec<u32> = new.results.iter().map(|value| value.number()).collect();
        let regions: Vec<u32> = new.regions.iter().map(|region| region.raw()).collect();
        let (chunk, row) = (self.ops.chunk_mut(chunk_of(id.raw())), row_of(id.raw()));
        chunk.seg_mut(op::OPERAND).replace(row, &operands);
        chunk.seg_mut(op::RESULT).replace(row, &results);
        chunk.seg_mut(op::REGION).replace(row, &regions);
        chunk.extra.set(row, new.attributes);
        for value in operands {
            self.note_value(value, part);
        }
        // Results are created before the op that defines them.
        for result in new.results {
            if self.value_live(result) {
                let (chunk, row) = self.value_row(result);
                chunk.set(value::DEF, row, id.raw());
            }
        }
        for region in new.regions {
            assert!(self.region_live(region), "live region");
            let (chunk, row) = self.region_row(region);
            chunk.set(region::PARENT, row, id.raw());
        }
        id
    }

    pub(crate) fn set_op_attrs(&mut self, id: OpId, attributes: Vec<NamedAttribute>) -> bool {
        if !self.op_live(id) {
            return false;
        }
        self.bump_op(id);
        let (chunk, row) = self.op_row(id);
        chunk.extra.set(row, attributes);
        true
    }

    pub(crate) fn set_operand(&mut self, id: OpId, index: usize, new: ValueId) -> bool {
        if !self.op_live(id) {
            return false;
        }
        self.bump_op(id);
        let (chunk, row) = self.op_row(id);
        chunk.seg_mut(op::OPERAND).set(row, index, new.number());
        let part = chunk.owner;
        self.note_value(new.number(), part);
        true
    }

    /// Replace `remove` operands of `id` from `at` with `insert`.
    pub(crate) fn splice_operands(
        &mut self,
        id: OpId,
        at: usize,
        remove: usize,
        insert: &[ValueId],
    ) -> bool {
        if !self.op_live(id) {
            return false;
        }
        self.bump_op(id);
        let cells: Vec<u32> = insert.iter().map(|value| value.number()).collect();
        let (chunk, row) = self.op_row(id);
        chunk.seg_mut(op::OPERAND).splice(row, at, remove, &cells);
        let part = chunk.owner;
        for value in cells {
            self.note_value(value, part);
        }
        true
    }

    /// Replace `remove` results of `id` from `at` with `insert`, which `id`
    /// defines from here on.
    pub(crate) fn splice_results(
        &mut self,
        id: OpId,
        at: usize,
        remove: usize,
        insert: &[ValueId],
    ) -> bool {
        if !self.op_live(id) {
            return false;
        }
        self.bump_op(id);
        let cells: Vec<u32> = insert.iter().map(|value| value.number()).collect();
        let (chunk, row) = self.op_row(id);
        chunk.seg_mut(op::RESULT).splice(row, at, remove, &cells);
        for value in insert {
            self.set_value_def(*value, Some(id));
        }
        true
    }

    pub(crate) fn set_value_def(&mut self, value: ValueId, def: Option<OpId>) {
        if self.value_live(value) {
            self.bump_value(value);
            let (chunk, row) = self.value_row(value);
            chunk.set(value::DEF, row, def.unwrap_or(OpId::ARGUMENT).raw());
        }
    }

    pub(crate) fn set_value_type(&mut self, value: ValueId, ty: TypeId) -> bool {
        if !self.value_live(value) {
            return false;
        }
        self.bump_value(value);
        let (chunk, row) = self.value_row(value);
        chunk.set(value::TYPE, row, ty.number());
        true
    }

    fn set_value_site(&mut self, value: ValueId, site: Option<Parent>) {
        if self.value_live(value) {
            let (chunk, row) = self.value_row(value);
            chunk.set(value::SITE, row, site.map_or(NONE, holder_key));
        }
    }

    pub(crate) fn create_value(&mut self, ty: TypeId, def: Option<OpId>) -> Value {
        let id = self.mint_value([ty.number(), def.unwrap_or(OpId::ARGUMENT).raw(), NONE]);
        Value::new(id, ty, def)
    }

    /// Point `op`'s segment sizes at the operand list an edit left: `edit`
    /// rewrites the sizes of an op that tracks them.
    pub(crate) fn edit_segment_sizes(
        &mut self,
        id: OpId,
        segment_sizes: Sym,
        edit: impl FnOnce(&mut [AttributeValue]),
    ) {
        let attributes = self.op_attrs(id);
        let Some(at) = attributes
            .iter()
            .position(|attribute| attribute.name == segment_sizes)
        else {
            return;
        };
        let mut attributes = attributes.to_vec();
        if let AttributeValue::Array(sizes) = &mut attributes[at].value {
            edit(sizes);
            let (chunk, row) = self.op_row(id);
            chunk.extra.set(row, attributes);
        }
    }

    /// Every slot naming `old` names `new` from here on. Answers the ops
    /// whose operands changed.
    pub(crate) fn replace_value_uses(&mut self, old: ValueId, new: ValueId) -> Vec<OpId> {
        self.retargets.push((old.number(), new.number()));
        let uses = self.uses(old);
        let mut edited = Vec::with_capacity(uses.len());
        for r#use in uses {
            self.set_operand(r#use.op, r#use.index, new);
            edited.push(r#use.op);
        }
        edited
    }

    pub(crate) fn create_region(&mut self) -> RegionId {
        self.mint_region([NONE, 0, STRIDE])
    }

    pub(crate) fn create_block(&mut self, arguments: &[Value]) -> BlockId {
        let id = self.mint_block([NONE, 0, STRIDE]);
        let cells: Vec<u32> = arguments.iter().map(|value| value.id().number()).collect();
        let types: Vec<u32> = arguments.iter().map(|value| value.ty().number()).collect();
        let (chunk, row) = (self.blocks.chunk_mut(chunk_of(id.raw())), row_of(id.raw()));
        chunk.seg_mut(block::ARG).replace(row, &cells);
        chunk.seg_mut(block::ARG_TYPE).replace(row, &types);
        for argument in arguments {
            assert!(self.value_live(argument.id()), "live value");
            self.set_value_site(argument.id(), Some(Parent::Block(id)));
        }
        id
    }

    /// Append `value` to `block`'s arguments, which it is one of from here on.
    pub(crate) fn push_block_argument(&mut self, block: BlockId, value: ValueId) -> bool {
        if !self.block_live(block) {
            return false;
        }
        self.bump_block(block);
        let ty = self.value(value).expect("live value").ty();
        let (chunk, row) = self.block_row(block);
        let count = chunk.seg(block::ARG).of(row).len();
        chunk
            .seg_mut(block::ARG)
            .splice(row, count, 0, &[value.number()]);
        chunk
            .seg_mut(block::ARG_TYPE)
            .splice(row, count, 0, &[ty.number()]);
        self.set_value_site(value, Some(Parent::Block(block)));
        true
    }

    /// Drop the `index`th argument of `block`, returning the value it named,
    /// which is left with no site.
    pub(crate) fn remove_block_argument(
        &mut self,
        block: BlockId,
        index: usize,
    ) -> Option<ValueId> {
        let argument = self.block_argument(block, index)?;
        self.bump_block(block);
        let (chunk, row) = self.block_row(block);
        chunk.seg_mut(block::ARG).splice(row, index, 1, &[]);
        chunk.seg_mut(block::ARG_TYPE).splice(row, index, 1, &[]);
        self.set_value_site(argument, None);
        Some(argument)
    }

    /// Give the `index`th argument of `block` the type `ty`, in the block's
    /// declaration and on the value.
    pub(crate) fn retype_block_argument(&mut self, block: BlockId, index: usize, ty: TypeId) {
        let Some(argument) = self.block_argument(block, index) else {
            return;
        };
        self.bump_block(block);
        let (chunk, row) = self.block_row(block);
        chunk.seg_mut(block::ARG_TYPE).set(row, index, ty.number());
        self.set_value_type(argument, ty);
    }

    pub(crate) fn block_argument(&self, block: BlockId, index: usize) -> Option<ValueId> {
        let (chunk, row) = self.block(block);
        chunk
            .seg(block::ARG)
            .of(row)
            .get(index)
            .map(|raw| ValueId::from_number(*raw))
    }

    pub(crate) fn set_block_attrs(&mut self, block: BlockId, attributes: Vec<NamedAttribute>) {
        self.bump_block(block);
        let (chunk, row) = self.block_row(block);
        chunk.extra.set(row, attributes);
    }

    /// Put `op` into `holder` at `position`.
    fn hold(&mut self, id: OpId, holder: Parent, position: u32) {
        assert!(self.op_live(id), "live op");
        // An op joining from nowhere changed nothing where it was.
        if self.op_parent(id).is_some() {
            self.bump_op(id);
        }
        let key = holder_key(holder);
        let (chunk, row) = self.op_row(id);
        chunk.set(op::HOLDER, row, key);
        chunk.set(op::POSITION, row, position);
        let part = chunk.owner;
        self.part_mut(part).held_log.push(id.raw());
        self.note_holder(key, part);
        self.bump_op(id);
    }

    /// Take `op` out of whatever holds it.
    fn release(&mut self, id: OpId) {
        if self.op_live(id) {
            self.bump_op(id);
            let (chunk, row) = self.op_row(id);
            chunk.set(op::HOLDER, row, NONE);
            let part = chunk.owner;
            self.part_mut(part).held_log.push(id.raw());
        }
    }

    /// The position after everything `holder` holds, which is taken.
    fn next_position(&mut self, holder: Parent) -> u32 {
        let next = match holder {
            Parent::Block(block) => {
                let (chunk, row) = self.block_row(block);
                let next = chunk.cell(block::NEXT, row);
                chunk.set(block::NEXT, row, next.wrapping_add(STRIDE));
                next
            }
            Parent::Region(region) => {
                let (chunk, row) = self.region_row(region);
                let next = chunk.cell(region::NEXT, row);
                chunk.set(region::NEXT, row, next.wrapping_add(STRIDE));
                next
            }
        };
        if next < u32::MAX - 2 * STRIDE {
            return next;
        }
        // The counter ran out of room: renumber what the holder holds.
        let held: Vec<OpId> = self.held(holder).into_iter().map(OpId::new).collect();
        self.set_held(holder, &held);
        self.next_position(holder)
    }

    /// Give `holder` exactly `ops`, in that order.
    pub(crate) fn set_held(&mut self, holder: Parent, ops: &[OpId]) {
        for (index, op) in ops.iter().enumerate() {
            self.hold(*op, holder, (index as u32 + 1) * STRIDE);
        }
        let next = (ops.len() as u32 + 1) * STRIDE;
        match holder {
            Parent::Block(block) => {
                let (chunk, row) = self.block_row(block);
                chunk.set(block::NEXT, row, next);
            }
            Parent::Region(region) => {
                let (chunk, row) = self.region_row(region);
                chunk.set(region::NEXT, row, next);
            }
        }
    }

    /// Put `op` after everything `holder` holds.
    pub(crate) fn append_held(&mut self, holder: Parent, id: OpId) {
        let position = self.next_position(holder);
        self.hold(id, holder, position);
    }

    /// Put `op` before the `index`th op `block` holds.
    pub(crate) fn insert_held(&mut self, block: BlockId, index: usize, id: OpId) {
        let holder = Parent::Block(block);
        let held = self.held(holder);
        assert!(index <= held.len(), "an insert lands inside the block");
        if index == held.len() {
            return self.append_held(holder, id);
        }
        let below = |db: &Db| index.checked_sub(1).map_or(0, |at| db.position(held[at]));
        if self.position(held[index]) - below(self) < 2 {
            let ops: Vec<OpId> = held.iter().map(|op| OpId::new(*op)).collect();
            self.set_held(holder, &ops);
        }
        let low = below(self);
        self.hold(id, holder, low + (self.position(held[index]) - low) / 2);
    }

    /// Put `new` where `block` holds `old`, which leaves it.
    pub(crate) fn replace_held(&mut self, block: BlockId, old: OpId, new: OpId) -> bool {
        if self.op_parent(old) != Some(Parent::Block(block)) {
            return false;
        }
        let (chunk, row) = self.op(old).expect("a held op is live");
        let position = chunk.cell(op::POSITION, row);
        self.release(old);
        self.hold(new, Parent::Block(block), position);
        true
    }

    /// Take `op` out of `holder`, if that is what holds it.
    pub(crate) fn remove_held(&mut self, holder: Parent, id: OpId) -> bool {
        if self.op_parent(id) != Some(holder) {
            return false;
        }
        self.release(id);
        true
    }

    /// Make an empty ordered region unordered, with `ports` its own.
    pub(crate) fn make_nodes(&mut self, id: RegionId, ports: &[Value], results: &[ValueId]) {
        assert!(
            !self.region_is_nodes(id) && self.region_blocks(id).is_empty(),
            "only an empty ordered region becomes unordered",
        );
        let (chunk, row) = self.region_row(id);
        chunk.set(region::NODES, row, 1);
        self.splice_region_results(id, 0, 0, results);
        self.splice_region_ports(id, 0, 0, ports);
    }

    pub(crate) fn splice_region_results(
        &mut self,
        id: RegionId,
        at: usize,
        remove: usize,
        insert: &[ValueId],
    ) {
        self.bump_region(id);
        assert!(
            self.region_is_nodes(id),
            "only an unordered region names its results"
        );
        let cells: Vec<u32> = insert.iter().map(|value| value.number()).collect();
        let (chunk, row) = self.region_row(id);
        chunk
            .seg_mut(region::RESULT)
            .splice(row, at, remove, &cells);
        let part = chunk.owner;
        for value in cells {
            self.note_value(value, part);
        }
    }

    fn refresh_results(&mut self, part: u32) {
        let current = self
            .parts
            .get(part as usize)
            .is_some_and(|part| part.results.cursor() == Some(part.region_log.len()));
        if current {
            return;
        }
        let tick = self.tick;
        let Some(held) = self.parts.get_mut(part as usize) else {
            return;
        };
        let rows = ResultRows {
            regions: &self.regions,
            chunks: &held.region_chunks,
        };
        held.results.refresh(&rows, &held.region_log, tick);
        held.region_log.clear();
        held.results.rewind();
        self.tick += 1;
    }

    /// Every region result slot naming `value`, as `(region, index)`.
    pub(crate) fn result_uses(&mut self, value: ValueId) -> Vec<(RegionId, usize)> {
        let Some(chunk) = self.values.find(chunk_of(value.number())) else {
            return Vec::new();
        };
        let readers = Self::readers(chunk.owner, &chunk.foreign);
        let mut found = Vec::new();
        for part in readers {
            self.refresh_results(part);
            let Some(held) = self.parts.get(part as usize) else {
                continue;
            };
            let rows = ResultRows {
                regions: &self.regions,
                chunks: &held.region_chunks,
            };
            held.results.read(&rows, value.number(), &mut found);
        }
        found
            .into_iter()
            .map(|(region, index)| (RegionId::new(region), index as usize))
            .collect()
    }

    /// Whether `region` is `root` or nested in it, at any depth.
    pub(crate) fn region_under(&self, region: RegionId, root: RegionId) -> bool {
        let mut current = Some(region);
        while let Some(here) = current {
            if here == root {
                return true;
            }
            current = self
                .region_parent_if_live(here)
                .and_then(|op| match self.op_parent(op)? {
                    Parent::Region(holder) => Some(holder),
                    Parent::Block(block) => self.block_parent(block),
                });
        }
        false
    }

    pub(crate) fn set_region_results(&mut self, id: RegionId, results: &[ValueId]) {
        let count = self.region_results(id).len();
        self.splice_region_results(id, 0, count, results);
    }

    pub(crate) fn splice_region_ports(
        &mut self,
        id: RegionId,
        at: usize,
        remove: usize,
        insert: &[Value],
    ) {
        self.bump_region(id);
        assert!(
            self.region_is_nodes(id),
            "only an unordered region owns its ports"
        );
        let cells: Vec<u32> = insert.iter().map(|port| port.id().number()).collect();
        let types: Vec<u32> = insert.iter().map(|port| port.ty().number()).collect();
        let (chunk, row) = self.region_row(id);
        chunk.seg_mut(region::PORT).splice(row, at, remove, &cells);
        chunk
            .seg_mut(region::PORT_TYPE)
            .splice(row, at, remove, &types);
        for port in insert {
            assert!(self.value_live(port.id()), "live value");
            self.set_value_site(port.id(), Some(Parent::Region(id)));
        }
    }

    /// Give `region` the ports, operations and results of the unordered
    /// `staged`, which is gone afterwards.
    pub(crate) fn take_nodes(&mut self, id: RegionId, staged: RegionId) {
        assert!(
            self.region_is_nodes(staged),
            "only an unordered region replaces an ordered one's body"
        );
        let ports = self.region_ports(staged);
        let results = self.region_results(staged);
        let ops = self.region_ops(staged);
        let (chunk, row) = self.region_row(id);
        chunk.set(region::NODES, row, 0);
        self.make_nodes(id, &ports, &results);
        self.set_held(Parent::Region(id), &ops);
        self.kill_region(staged);
    }

    /// Make the unordered `region` ordered, holding `blocks`.
    pub(crate) fn make_blocks(&mut self, id: RegionId, blocks: &[BlockId]) {
        self.bump_region(id);
        assert!(
            self.region_is_nodes(id),
            "only an ordered region replaces an unordered one's body"
        );
        for port in self.region_ports(id) {
            if self.value_region(port.id()) == Some(id) {
                self.set_value_site(port.id(), None);
            }
        }
        let (chunk, row) = self.region_row(id);
        chunk.set(region::NODES, row, 0);
        chunk.seg_mut(region::PORT).replace(row, &[]);
        chunk.seg_mut(region::PORT_TYPE).replace(row, &[]);
        chunk.seg_mut(region::RESULT).replace(row, &[]);
        self.set_region_blocks(id, blocks);
    }

    fn place_block(&mut self, id: BlockId, region: Option<RegionId>, position: u32) {
        assert!(self.block_live(id), "live block");
        self.bump_block(id);
        if let Some(region) = region {
            self.bump_region(region);
        }
        let (chunk, row) = self.block_row(id);
        chunk.set(block::REGION, row, region.map_or(NONE, RegionId::raw));
        chunk.set(block::POSITION, row, position);
        let part = chunk.owner;
        if let Some(region) = region {
            self.note_region(region.raw(), part);
        }
    }

    /// Give `region` exactly `blocks`, in that order.
    pub(crate) fn set_region_blocks(&mut self, id: RegionId, blocks: &[BlockId]) {
        for (index, block) in blocks.iter().enumerate() {
            self.place_block(*block, Some(id), (index as u32 + 1) * STRIDE);
        }
        let (chunk, row) = self.region_row(id);
        chunk.set(region::NEXT, row, (blocks.len() as u32 + 1) * STRIDE);
    }

    pub(crate) fn append_block(&mut self, id: RegionId, block: BlockId) {
        let (chunk, row) = self.region_row(id);
        let next = chunk.cell(region::NEXT, row);
        chunk.set(region::NEXT, row, next + STRIDE);
        self.place_block(block, Some(id), next);
    }

    pub(crate) fn remove_block(&mut self, id: RegionId, block: BlockId) -> bool {
        if self.block_parent(block) != Some(id) {
            return false;
        }
        self.place_block(block, None, 0);
        true
    }

    // Erasure: the row stops being live and is stamped, so every grouping
    // drops what it contributed.

    pub(crate) fn kill_op(&mut self, id: OpId) {
        if self.op_live(id) {
            self.bump_op(id);
            let (chunk, row) = self.op_row(id);
            chunk.kill(row);
            let part = chunk.owner;
            self.part_mut(part).held_log.push(id.raw());
        }
    }

    pub(crate) fn kill_value(&mut self, id: ValueId) {
        if self.value_live(id) {
            self.bump_value(id);
            let (chunk, row) = self.value_row(id);
            chunk.kill(row);
        }
    }

    pub(crate) fn kill_block(&mut self, id: BlockId) {
        if self.block_live(id) {
            self.bump_block(id);
            let (chunk, row) = self.block_row(id);
            chunk.kill(row);
        }
    }

    pub(crate) fn kill_region(&mut self, id: RegionId) {
        if self.region_live(id) {
            self.bump_region(id);
            let (chunk, row) = self.region_row(id);
            chunk.kill(row);
        }
    }

    // Dirty subtrees, for scoping verification after a pass.

    pub(crate) fn mark_dirty(&mut self, op: OpId) {
        if self.dirty.last() != Some(&op) {
            self.dirty.push(op);
        }
    }

    /// `op` itself changed. The dirtied subtree is its owner's, so
    /// verification also sees the siblings the edit may have broken.
    pub(crate) fn edit_op(&mut self, op: OpId) {
        let dirtied = self.enclosing_op_of(op).unwrap_or(op);
        self.mark_dirty(dirtied);
    }

    pub(crate) fn edit_block(&mut self, block: BlockId) {
        if let Some(op) = self.enclosing_op(block) {
            self.mark_dirty(op);
        }
    }

    pub(crate) fn edit_region(&mut self, region: RegionId) {
        if let Some(op) = self.region_parent_if_live(region) {
            self.mark_dirty(op);
        }
    }

    pub(crate) fn take_dirty(&mut self) -> Vec<OpId> {
        let mut dirty = std::mem::take(&mut self.dirty);
        dirty.sort_unstable();
        dirty.dedup();
        dirty.retain(|op| self.op_live(*op));
        dirty
    }

    // Epochs.

    /// Close the epoch: every row written so far is committed, and the
    /// chunks it wrote give back the cells of rows it erased.
    pub(crate) fn commit(&mut self) {
        for index in std::mem::take(&mut self.touched[OPS]) {
            if self.ops.chunk(index).wants_compaction() {
                self.ops.chunk_mut(index).compact();
            }
        }
        for touched in &mut self.touched {
            touched.clear();
        }
        self.retargets.clear();
        self.bumped.clear();
        self.committed = self.tick;
        self.tick += 1;
    }

    /// This database as a task of a parallel epoch sees it: it mints into
    /// partition `target` through lane `index` of `count`. `exclusive` says
    /// no other task of the epoch has that target.
    pub(crate) fn lane(&self, index: u32, count: u32, target: u32, exclusive: bool) -> Db {
        let mut db = self.clone();
        db.retargets.clear();
        db.bumped.clear();
        db.mint.target = target;
        db.mint.lane = Some(Lane {
            index,
            count,
            base: [
                self.ops.chunks(),
                self.values.chunks(),
                self.regions.chunks(),
                self.blocks.chunks(),
            ],
            taken: [0; 4],
            exclusive,
        });
        db
    }

    /// Whether anything was written since the last commit.
    pub(crate) fn is_clean(&self) -> bool {
        self.touched.iter().all(Vec::is_empty)
    }

    /// Apply `batches`, each a task's database over this one as committed, in
    /// the order given. A chunk one batch wrote is taken whole; a chunk
    /// several wrote is merged row by row, and a row two of them wrote is
    /// refused, naming it, before anything changes.
    pub(crate) fn merge(&mut self, batches: Vec<Db>) -> Result<(), String> {
        assert!(self.is_clean(), "batches commit over a committed base");
        let committed = self.committed;
        let maps: Vec<HashMap<u32, u32>> = batches.iter().map(Db::retarget_map).collect();
        let retargeted = maps.iter().any(|map| !map.is_empty());
        let unedited =
            |batch: usize, base: &Chunk<4, 3, Attrs>, theirs: &Chunk<4, 3, Attrs>, row: usize| {
                retargeted && only_retargeted(&maps[batch], base, theirs, row)
            };
        let mut ops = merge_space(
            &self.ops,
            &batches,
            OPS,
            |db| &db.ops,
            committed,
            "op",
            &unedited,
        )?;
        if retargeted {
            for (index, chunk, writers) in &mut ops {
                retarget_rows(Arc::make_mut(chunk), writers, &maps, committed);
                debug_assert!(*index < MAX_CHUNKS);
            }
        }
        let never = |_: usize, _: &_, _: &_, _: usize| false;
        let values = merge_space(
            &self.values,
            &batches,
            VALUES,
            |db| &db.values,
            committed,
            "value",
            &never,
        )?;
        let never = |_: usize, _: &_, _: &_, _: usize| false;
        let regions = merge_space(
            &self.regions,
            &batches,
            REGIONS,
            |db| &db.regions,
            committed,
            "region",
            &never,
        )?;
        let never = |_: usize, _: &_, _: &_, _: usize| false;
        let blocks = merge_space(
            &self.blocks,
            &batches,
            BLOCKS,
            |db| &db.blocks,
            committed,
            "block",
            &never,
        )?;
        let mut written = Vec::new();
        for (index, chunk, _) in ops {
            let fresh = self.ops.find(index).is_none();
            let owner = chunk.owner;
            self.ops.put(index, chunk);
            if fresh {
                self.part_mut(owner).op_chunks.push(index);
            }
            written.push(owner);
        }
        for (index, chunk, _) in blocks {
            let fresh = self.blocks.find(index).is_none();
            let owner = chunk.owner;
            self.blocks.put(index, chunk);
            if fresh {
                self.part_mut(owner).block_chunks.push(index);
            }
            written.push(owner);
        }
        for (index, chunk, _) in values {
            written.push(chunk.owner);
            self.values.put(index, chunk);
        }
        for (index, chunk, _) in regions {
            let fresh = self.regions.find(index).is_none();
            let owner = chunk.owner;
            self.regions.put(index, chunk);
            if fresh {
                self.part_mut(owner).region_chunks.push(index);
            }
            written.push(owner);
        }
        written.sort_unstable();
        written.dedup();
        for owner in written {
            let part = self.part_mut(owner);
            part.op_chunks.sort_unstable();
            part.block_chunks.sort_unstable();
            part.region_chunks.sort_unstable();
            part.forget();
        }
        // Every partition's edit count moves by what each batch counted for
        // it, so a version read inside a task reads the same after the commit
        // unless another task edited under it.
        let (before, edits) = (self.mint, self.edits);
        let mut counted: HashMap<u32, u32> = HashMap::new();
        for batch in &batches {
            self.edits = self.edits.wrapping_add(batch.edits.wrapping_sub(edits));
            for &part in &batch.bumped {
                let Some(theirs) = batch.parts.get(part as usize) else {
                    continue;
                };
                let was = *counted
                    .entry(part)
                    .or_insert_with(|| self.parts.get(part as usize).map_or(0, |part| part.edits));
                let ours = self.part_mut(part);
                ours.edits = ours.edits.wrapping_add(theirs.edits.wrapping_sub(was));
            }
        }
        for batch in batches {
            self.tick = self.tick.max(batch.tick);
            self.replaced_ops.extend(batch.replaced_ops);
            for space in 0..4 {
                let by = |minted: [u32; 4], was: [u32; 4]| minted[space].wrapping_sub(was[space]);
                self.mint.minted[space] =
                    self.mint.minted[space].wrapping_add(by(batch.mint.minted, before.minted));
                self.mint.module_minted[space] = self.mint.module_minted[space]
                    .wrapping_add(by(batch.mint.module_minted, before.module_minted));
            }
            // A partition one task had to itself goes on from the chunk that
            // task left open.
            let target = batch.mint.target;
            if batch.mint.lane.is_some_and(|lane| lane.exclusive)
                && let Some(theirs) = batch.parts.get(target as usize)
            {
                let (open, seen, settled) = (theirs.open, theirs.seen, theirs.settled.clone());
                let part = self.part_mut(target);
                part.open = open;
                part.seen = seen;
                part.settled = settled;
            }
        }
        self.retargets.clear();
        self.committed = self.tick;
        self.tick += 1;
        Ok(())
    }

    /// What each value replaced since the last commit stands for now.
    fn retarget_map(&self) -> HashMap<u32, u32> {
        let mut map: HashMap<u32, u32> = HashMap::new();
        for &(old, new) in &self.retargets {
            for target in map.values_mut() {
                if *target == old {
                    *target = new;
                }
            }
            map.insert(old, new);
        }
        map
    }

    pub(crate) fn census(&self) -> Census {
        fn count<const P: usize, const S: usize, X: Payload>(
            space: &Space<P, S, X>,
            touched: &[u32],
        ) -> (usize, usize, usize, usize) {
            let (mut live, mut chunks, mut bytes) = (0, 0, 0);
            for (_, chunk) in space.iter() {
                live += chunk.live_rows();
                chunks += 1;
                bytes += chunk.bytes();
            }
            let touched = touched
                .iter()
                .filter_map(|index| space.find(*index))
                .map(Chunk::bytes)
                .sum();
            (live, chunks, bytes, touched)
        }
        let ops = count(&self.ops, &self.touched[OPS]);
        let values = count(&self.values, &self.touched[VALUES]);
        let regions = count(&self.regions, &self.touched[REGIONS]);
        let blocks = count(&self.blocks, &self.touched[BLOCKS]);
        Census {
            ops_live: ops.0,
            values_live: values.0,
            blocks_live: blocks.0,
            regions_live: regions.0,
            ops_chunks: ops.1,
            values_chunks: values.1,
            blocks_chunks: blocks.1,
            regions_chunks: regions.1,
            ops_bytes: ops.2,
            values_bytes: values.2,
            blocks_bytes: blocks.2,
            regions_bytes: regions.2,
            groupings_bytes: self
                .parts
                .iter()
                .map(|(_, part)| part.uses.bytes() + part.held.bytes() + part.blocks.bytes())
                .sum(),
            touched_chunks: self.touched.iter().map(Vec::len).sum(),
            touched_bytes: ops.3 + values.3 + regions.3 + blocks.3,
        }
    }
}

/// Who wrote the rows of a chunk a commit took: one batch all of them, or a
/// batch per row, counted from one, with zero for a row none wrote and
/// [`RETARGETED`] for a row batches only replaced uses in.
enum Writers {
    One(usize),
    Each(Vec<u16>),
}

const RETARGETED: u16 = u16::MAX;

/// Whether a batch's copy of a row differs from the base's only by the
/// batch's own use replacements: `(batch, base, theirs, row)`.
type Unedited<'a, const P: usize, const S: usize, X> =
    dyn Fn(usize, &Chunk<P, S, X>, &Chunk<P, S, X>, usize) -> bool + 'a;

type Merged<const P: usize, const S: usize, X> = Vec<(u32, Arc<Chunk<P, S, X>>, Writers)>;

/// The chunks of one space `batches` wrote, as a commit leaves them.
/// `unedited` says a batch's copy of a row differs from the base only by that
/// batch's own use replacements, which leaves the row to whoever else wrote it.
fn merge_space<const P: usize, const S: usize, X: Payload>(
    base: &Space<P, S, X>,
    batches: &[Db],
    kind: usize,
    space: impl Fn(&Db) -> &Space<P, S, X>,
    committed: u32,
    name: &str,
    unedited: &Unedited<'_, P, S, X>,
) -> Result<Merged<P, S, X>, String> {
    let mut writers: Vec<(u32, usize)> = batches
        .iter()
        .enumerate()
        .flat_map(|(batch, db)| db.touched[kind].iter().map(move |index| (*index, batch)))
        .collect();
    writers.sort_unstable();
    writers.dedup();
    let mut merged = Vec::new();
    for group in writers.chunk_by(|a, b| a.0 == b.0) {
        let index = group[0].0;
        let shared = |batch: usize| {
            space(&batches[batch])
                .shared(index)
                .expect("a chunk a batch wrote")
        };
        if let [(_, batch)] = group {
            merged.push((index, shared(*batch).clone(), Writers::One(*batch)));
            continue;
        }
        let original = base
            .find(index)
            .expect("only a committed chunk has two writers");
        let mut chunk = original.clone();
        let mut written = vec![0u16; CHUNK_ROWS];
        for &(_, batch) in group {
            let theirs = shared(batch);
            for (row, writer) in written.iter_mut().enumerate().take(theirs.len()) {
                if theirs.stamp(row) <= committed {
                    continue;
                }
                if row < original.len() && unedited(batch, original, theirs, row) {
                    if *writer == 0 {
                        *writer = RETARGETED;
                    }
                    continue;
                }
                if *writer != 0 && *writer != RETARGETED {
                    return Err(format!(
                        "batch {batch} edits {name} {}, which an earlier batch of the same epoch edits too",
                        id_at(index, row)
                    ));
                }
                *writer = batch as u16 + 1;
                chunk.copy_row(theirs, row);
            }
            for part in &theirs.foreign {
                if !chunk.names_from(*part) {
                    chunk.foreign.push(*part);
                }
            }
        }
        merged.push((index, Arc::new(chunk), Writers::Each(written)));
    }
    Ok(merged)
}

/// Whether `theirs` holds `row` as `base` does but for the operands `map`
/// replaced.
fn only_retargeted(
    map: &HashMap<u32, u32>,
    base: &Chunk<4, 3, Attrs>,
    theirs: &Chunk<4, 3, Attrs>,
    row: usize,
) -> bool {
    let same = |column: usize| base.cell(column, row) == theirs.cell(column, row);
    let (ours, replaced) = (
        base.seg(op::OPERAND).of(row),
        theirs.seg(op::OPERAND).of(row),
    );
    base.is_live(row) == theirs.is_live(row)
        && same(op::NAME)
        && same(op::HOLDER)
        && same(op::POSITION)
        && base.seg(op::RESULT).of(row) == theirs.seg(op::RESULT).of(row)
        && base.seg(op::REGION).of(row) == theirs.seg(op::REGION).of(row)
        && base.extra.get(row) == theirs.extra.get(row)
        && ours.len() == replaced.len()
        && ours
            .iter()
            .zip(replaced)
            .all(|(old, new)| map.get(old).unwrap_or(old) == new)
}

/// Replay every batch's use replacements over the rows of `chunk` another
/// batch wrote: an op one task made or edited may name a value another task
/// replaced.
fn retarget_rows(
    chunk: &mut Chunk<4, 3, Attrs>,
    writers: &Writers,
    maps: &[HashMap<u32, u32>],
    committed: u32,
) {
    for row in 0..chunk.len() {
        let writer = match writers {
            Writers::One(batch) if chunk.stamp(row) > committed => *batch as u16 + 1,
            Writers::One(_) => continue,
            Writers::Each(written) if written[row] != 0 => written[row],
            Writers::Each(_) => continue,
        };
        for index in 0..chunk.seg(op::OPERAND).of(row).len() {
            let mut value = chunk.seg(op::OPERAND).of(row)[index];
            for (batch, map) in maps.iter().enumerate() {
                if batch as u16 + 1 != writer
                    && let Some(new) = map.get(&value)
                {
                    value = *new;
                }
            }
            chunk.seg_mut(op::OPERAND).set(row, index, value);
        }
    }
}
