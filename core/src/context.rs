use std::{
    any::Any,
    collections::HashMap,
    hash::{DefaultHasher, Hasher},
    sync::{Arc, Weak},
};

use parking_lot::RwLock;

use tir_adt::{Hive, Interner, Sym};

use crate::run::{AttrRunId, AttrRuns, Entry, EntryId, NO_ENTRY, RunId, Runs};

use crate::{
    Block, Dialect, Error, OpId, OpInstance, Operation, OperationParser, Region, TypeId,
    attributes::{AttributeValue, NamedAttribute},
    block::{BlockHandle, BlockId},
    builtin::BuiltinDialect,
    dialects::cfg::CfgDialect,
    dialects::func::FuncDialect,
    dialects::scf::ScfDialect,
    dialects::state::StateDialect,
    ir_formatter::IRFormatter,
    operation::{
        ImplementsOpInterface, OpHandle, OpInterfaceConverter, OpNameId, downcast_op_interface,
        op_interface_converter,
    },
    parse::text::Parser as IRParser,
    ptr::PtrDialect,
    region::{RegionHandle, RegionId},
    ty::{Type, TypeParser},
    value::{Use, Value, ValueId},
    vector::VectorDialect,
};

/// Central hub for managing all IR entities and state.
///
/// The `Context` serves as the global owner and access point for all
/// intermediate representation (IR) objects such as operations, values,
/// regions, and blocks. It orchestrates allocation, registration, lookup,
/// and mutation of these entities, providing a reliable foundation for
/// all transformation passes and analyses.
///
/// All IR objects in TIR are uniquely identified and stored within the
/// context, which enables:
/// - **Uniqueness and lifetime management:** Ensures that all IR nodes are
///   consistently referenced by identifier and have stable lifetimes throughout
///   graph construction and rewriting.
/// - **Thread safety:** Allows safe concurrent access to the IR graph, supporting
///   lock-free reads and coordinated mutation via interior mutability primitives.
/// - **Dialect and operation extensibility:** Registers and manages dialects and
///   operation kinds, enabling the IR to be extended with new languages or
///   target-specific features.
/// - **Forking and analysis:** Supports speculative graph forking, cloning, or
///   cost-based variant analysis by encapsulating IR state in a single location.
///
/// The `Context` enforces the design principle that individual IR objects
/// (like operations or blocks) do not exist in isolation; instead, they
/// are always part of a coherent context-managed graph.
///
/// # Example
///
/// ```rust
/// let context = tir::Context::with_default_dialects();
/// ```
///
/// The context is typically shared (via reference or smart pointer) throughout
/// the compiler pipeline, ensuring consistent access to all ongoing IR state
/// and registered dialects.
#[derive(Clone)]
pub struct Context(Arc<RwLock<ContextInstance>>);

#[derive(Debug, Clone)]
pub struct ContextRef(Weak<RwLock<ContextInstance>>);

pub struct ContextIterator<I: GetFromContext> {
    context: Context,
    elements: Vec<I>,
    current_front: usize,
    current_back: usize,
}

pub trait GetFromContext {
    type Item;

    fn get_from_context(&self, context: &Context) -> Self::Item;
}

/// Read an entry from a side table indexed by a dense id, or `None` if the id was
/// never inserted or has been removed.
fn slab_get<T>(slab: &[Option<T>], idx: usize) -> Option<&T> {
    slab.get(idx).and_then(Option::as_ref)
}

/// Insert into a side table at a dense id, growing the backing vector as needed.
/// Ids come from per-context monotonic counters, so the vector stays dense.
fn slab_put<T>(slab: &mut Vec<Option<T>>, idx: usize, val: T) {
    if idx >= slab.len() {
        slab.resize_with(idx + 1, || None);
    }
    slab[idx] = Some(val);
}

/// Give a side-table slot's contents back. The slot itself is kept empty forever;
/// see [`Context::free`].
fn clear_slot<T>(slab: &mut [Option<T>], idx: usize) {
    if let Some(slot) = slab.get_mut(idx) {
        *slot = None;
    }
}

/// Erase counts per entity id, so a handle minted before a slot was reused can
/// be told from one naming the entity that now holds it.
#[derive(Default)]
struct Generations {
    ops: GenerationTable,
    blocks: GenerationTable,
    regions: GenerationTable,
}

#[derive(Default)]
struct GenerationTable(Vec<u32>);

impl GenerationTable {
    fn get(&self, index: usize) -> u32 {
        self.0.get(index).copied().unwrap_or(0)
    }

    fn bump(&mut self, index: usize) {
        if index >= self.0.len() {
            self.0.resize(index + 1, 0);
        }
        self.0[index] += 1;
    }
}

/// What holds an operation: a block of an ordered region, or an unordered
/// region directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    Block(BlockId),
    Region(RegionId),
}

/// Entities an erase reclaims, gathered before the context lock is taken.
#[derive(Default)]
struct Owned {
    ops: Vec<OpId>,
    values: Vec<ValueId>,
    blocks: Vec<BlockId>,
    regions: Vec<RegionId>,
}

struct ContextInstance {
    // Entities live in chunked [`Hive`]s; an id *is* its hive handle, so a read
    // costs no indirection. Slots and ids are reused once an entity is erased.
    ops: Hive<OpInstance>,
    values: Hive<Value>,
    regions: Hive<Region>,
    blocks: Hive<Block>,
    /// Erase counts per entity kind. A slot is reused as soon as its entity is
    /// erased, so an id alone no longer identifies an entity across an erase;
    /// the pair of id and generation does. See [`OpHandle`].
    generations: Generations,
    /// Reverse index from an operation to whatever holds it, maintained by
    /// `Block`'s membership mutators and by [`Context::set_region_nodes`]. Lets
    /// `parent_block` answer in O(1) instead of scanning every block's
    /// operation list.
    op_parent: Vec<Option<Parent>>,
    /// Reverse index from a block to the region that holds it, maintained by
    /// [`Region::add_block`]. Together with [`Region::parent_op`] it lets walks
    /// climb from an op to its enclosing ops.
    block_parent: Vec<Option<RegionId>>,
    /// Def-site index for block arguments: the block whose argument list a value
    /// entered. The counterpart of [`Value::defining_op`] for values no operation
    /// defines, and what bounds the scope a use of such a value can sit in.
    value_block: Vec<Option<BlockId>>,
    /// Def-site index for the ports of an unordered region, the counterpart of
    /// `value_block` for the arguments a region owns itself.
    value_region: Vec<Option<RegionId>>,
    /// Ports: every op's operands, results and region ids, in one cell per op
    /// drawn from a size-classed pool.
    runs: Runs,
    /// Attributes, pooled the same way.
    attr_runs: AttrRuns,
    /// Use lists: value index → the first operand entry naming it. The rest of
    /// the list is threaded through the entries themselves, so a use costs no
    /// storage beyond the port it already is, and "who reads this value" is
    /// O(uses) rather than a walk.
    first_use: Vec<u32>,
    /// Structural version per op, bumped along the spine root-ward by every
    /// tree edit; see [`Context::op_version`].
    op_version: Vec<u32>,
    /// Ops whose own subtree an edit touched since the last
    /// [`Context::take_dirty_ops`], for scoping post-pass verification. Stamped
    /// with the generation of the id, so an op erased before the drain is
    /// dropped rather than resurrected as whatever took its slot.
    dirty_ops: Vec<(OpId, u32)>,
    dialects: HashMap<&'static str, Arc<dyn Dialect>>,
    /// Register-class names of registered targets, for resolving a parsed
    /// `CLASS[n]` register attribute back to a [`RegClassId`].
    reg_classes: HashMap<&'static str, crate::backend::regalloc::RegClassId>,
    op_interface_converters:
        HashMap<(&'static str, &'static str, std::any::TypeId), OpInterfaceConverter>,
    type_cache: Vec<Arc<dyn Type>>,
    /// Interned type ids bucketed by [`Type::hash`], so [`Context::get_type_id`]
    /// only runs [`Type::eq`] against colliding candidates.
    type_lookup: HashMap<u64, Vec<TypeId>>,
    /// The names attributes are keyed by, so an op carries four bytes per
    /// attribute name instead of a heap `String` per instance. One table per
    /// context: a [`Sym`] means nothing outside the context that minted it.
    names: Interner,
    /// The `(dialect, name)` pairs ops are identified by, so an op carries four
    /// bytes of identity instead of two fat pointers. Ids are dense and handed
    /// out in construction order.
    op_names: Vec<(&'static str, &'static str)>,
    op_name_ids: HashMap<(&'static str, &'static str), OpNameId>,
}

impl ContextInstance {
    fn op(&self, id: OpId) -> Option<&OpInstance> {
        self.ops.get(id.raw())
    }

    fn op_mut(&mut self, id: OpId) -> Option<&mut OpInstance> {
        self.ops.get_mut(id.raw())
    }

    fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.get(id.raw())
    }

    fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.blocks.get_mut(id.raw())
    }

    fn region(&self, id: RegionId) -> Option<&Region> {
        self.regions.get(id.raw())
    }

    fn region_mut(&mut self, id: RegionId) -> Option<&mut Region> {
        self.regions.get_mut(id.raw())
    }

    fn value(&self, id: ValueId) -> Option<&Value> {
        self.values.get(id.index() as u32)
    }

    fn value_mut(&mut self, id: ValueId) -> Option<&mut Value> {
        self.values.get_mut(id.index() as u32)
    }

    fn erase_op(&mut self, id: OpId) {
        if let Some(instance) = self.ops.get(id.raw()) {
            let (run, attrs) = (instance.run, instance.attrs);
            self.runs.free(run);
            self.attr_runs.free(attrs);
            self.ops.remove(id.raw());
            self.generations.ops.bump(id.index());
        }
    }

    fn erase_block(&mut self, id: BlockId) {
        if self.blocks.get(id.raw()).is_some() {
            self.blocks.remove(id.raw());
            self.generations.blocks.bump(id.index());
        }
    }

    fn erase_region(&mut self, id: RegionId) {
        if self.regions.get(id.raw()).is_some() {
            self.regions.remove(id.raw());
            self.generations.regions.bump(id.index());
        }
    }

    fn erase_value(&mut self, id: ValueId) {
        if self.values.get(id.index() as u32).is_none() {
            return;
        }
        self.values.remove(id.index() as u32);
    }

    /// Unthread the use list a recycled value id heads.
    ///
    /// An erased value keeps its use list: erasing a definition its readers
    /// have not been rewritten off is how a rewrite stages an erase, and those
    /// readers still answer "who names this". The list only stops meaning
    /// anything when the id is handed to a new value, and the old entries have
    /// to leave it then — a later unlink would otherwise splice through
    /// whatever took their neighbours' cells.
    fn clear_uses(&mut self, id: ValueId) {
        let mut current = self.first_use.get(id.index()).copied().unwrap_or(NO_ENTRY);
        while current != NO_ENTRY {
            let entry = self.runs.entry_mut(EntryId::from_raw(current));
            current = entry.next;
            entry.next = NO_ENTRY;
            entry.prev = NO_ENTRY;
        }
        if let Some(head) = self.first_use.get_mut(id.index()) {
            *head = NO_ENTRY;
        }
    }

    /// `op`'s ports, split into the three groups the run holds back to back.
    fn ports(&self, op: OpId) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        let Some(instance) = self.op(op) else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let (operands, results, regions) = (
            instance.operand_count as usize,
            instance.result_count as usize,
            instance.region_count as usize,
        );
        let entries = self.runs.entries(instance.run);
        let ids = |range: std::ops::Range<usize>| entries[range].iter().map(|e| e.id).collect();
        (
            ids(0..operands),
            ids(operands..operands + results),
            ids(operands + results..operands + results + regions),
        )
    }

    /// Replace `op`'s ports wholesale, growing its run to the next size class
    /// when the three groups no longer fit. Every operand use is relinked, so
    /// the caller does no bookkeeping of its own.
    fn set_ports(&mut self, op: OpId, operands: &[u32], results: &[u32], regions: &[u32]) {
        self.unlink_operands(op);
        let needed = operands.len() + results.len() + regions.len();
        let Some(instance) = self.op(op) else {
            return;
        };
        let mut run = instance.run;
        if self.runs.capacity(run) < needed {
            let live = self.op(op).expect("live op").port_count();
            run = self.runs.grow(run, live, needed);
        }
        let entries = self.runs.entries_mut(run);
        for (entry, id) in entries
            .iter_mut()
            .zip(operands.iter().chain(results).chain(regions))
        {
            entry.id = *id;
            entry.next = NO_ENTRY;
            entry.prev = NO_ENTRY;
        }
        let instance = self.op_mut(op).expect("live op");
        instance.run = run;
        instance.operand_count = operands.len() as u16;
        instance.result_count = results.len() as u16;
        instance.region_count = regions.len() as u16;
        self.link_operands(op);
    }

    fn op_operands(&self, op: OpId) -> crate::operation::ValueIds {
        let Some(instance) = self.op(op) else {
            return Default::default();
        };
        self.runs.entries(instance.run)[..instance.operand_count as usize]
            .iter()
            .map(|entry| ValueId::from_number(entry.id))
            .collect()
    }

    fn op_results(&self, op: OpId) -> crate::operation::ValueIds {
        let Some(instance) = self.op(op) else {
            return Default::default();
        };
        let start = instance.operand_count as usize;
        let end = start + instance.result_count as usize;
        self.runs.entries(instance.run)[start..end]
            .iter()
            .map(|entry| ValueId::from_number(entry.id))
            .collect()
    }

    fn op_regions(&self, op: OpId) -> crate::operation::RegionIds {
        let Some(instance) = self.op(op) else {
            return Default::default();
        };
        let start = (instance.operand_count + instance.result_count) as usize;
        let end = start + instance.region_count as usize;
        self.runs.entries(instance.run)[start..end]
            .iter()
            .map(|entry| RegionId::new(entry.id))
            .collect()
    }

    fn op_attrs(&self, op: OpId) -> &[NamedAttribute] {
        match self.op(op) {
            Some(instance) => self
                .attr_runs
                .get(instance.attrs, instance.attr_count as usize),
            None => &[],
        }
    }

    fn op_attrs_mut(&mut self, op: OpId) -> &mut [NamedAttribute] {
        let Some(instance) = self.op(op) else {
            return &mut [];
        };
        let (attrs, count) = (instance.attrs, instance.attr_count as usize);
        self.attr_runs.get_mut(attrs, count)
    }

    fn set_op_attrs(&mut self, op: OpId, attributes: Vec<NamedAttribute>) {
        let Some(instance) = self.op(op) else {
            return;
        };
        let old = instance.attrs;
        let count = attributes.len() as u16;
        let attrs = self.attr_runs.alloc(attributes);
        self.attr_runs.free(old);
        let instance = self.op_mut(op).expect("live op");
        instance.attrs = attrs;
        instance.attr_count = count;
    }

    /// Point `op`'s `index`-th operand slot at `new`, moving the slot from one
    /// value's use list to the other's.
    fn replace_operand_at(&mut self, op: OpId, index: usize, new: ValueId) {
        let entry = self.entry_of(op, index);
        let old = ValueId::from_number(self.runs.entry(entry).id);
        self.unlink_use(old, entry);
        self.runs.entry_mut(entry).id = new.number();
        self.link_use(new, entry);
    }

    fn replace_result_at(&mut self, op: OpId, index: usize, new: ValueId) {
        let offset = self.op(op).expect("live op").operand_count as usize + index;
        let entry = self.entry_of(op, offset);
        self.runs.entry_mut(entry).id = new.number();
    }

    /// Move `op`'s run to a class holding `needed` entries, if its own is too
    /// small. Entry addresses change with the move, so the operand use lists
    /// are rebuilt across it.
    fn reserve_ports(&mut self, op: OpId, needed: usize) {
        let instance = self.op(op).expect("live op");
        let (run, live) = (instance.run, instance.port_count());
        if self.runs.capacity(run) >= needed {
            return;
        }
        self.unlink_operands(op);
        let grown = self.runs.grow(run, live, needed);
        self.op_mut(op).expect("live op").run = grown;
        self.link_operands(op);
    }

    /// Insert `id` at port position `at`, shifting the ports after it along.
    ///
    /// Shifting moves an entry's address, so every operand at or after `at`
    /// would need relinking; callers pass an `at` no earlier than the end of
    /// the operand group, and the shifted entries are then results and regions,
    /// which sit in no use list.
    /// Append `value` to `op`'s results — at the end of the dependency
    /// partition when it is one, ahead of it otherwise. Results sit in no use
    /// list, so only the ports after the slot move.
    fn append_result_port(&mut self, op: OpId, value: ValueId, dependency: bool) {
        let instance = self.op(op).expect("live op");
        let mut at = (instance.operand_count + instance.result_count) as usize;
        if !dependency {
            at -= instance.dep_result_count as usize;
        }
        self.insert_port(op, at, value.number());
        let instance = self.op_mut(op).expect("live op");
        instance.result_count += 1;
        if dependency {
            instance.dep_result_count += 1;
        }
    }

    /// Put `value` at operand position `index`, shifting the operands after it
    /// along. Appending at the end is one slot write; anything else moves
    /// linked entries, so the run is rewritten and every use relinked.
    fn insert_operand(&mut self, op: OpId, index: usize, value: ValueId) {
        let count = self.op(op).expect("live op").operand_count as usize;
        if index == count {
            self.insert_port(op, index, value.number());
            self.op_mut(op).expect("live op").operand_count += 1;
            let entry = self.entry_of(op, index);
            self.link_use(value, entry);
            return;
        }
        let (mut operands, results, regions) = self.ports(op);
        operands.insert(index, value.number());
        self.set_ports(op, &operands, &results, &regions);
    }

    /// Drop the operand at `index`; see [`ContextInstance::insert_operand`].
    fn remove_operand(&mut self, op: OpId, index: usize) {
        let (mut operands, results, regions) = self.ports(op);
        operands.remove(index);
        self.set_ports(op, &operands, &results, &regions);
    }

    /// Grow or shrink the last operand segment by `delta`, where the op tracks
    /// segments: an appended or dropped value operand belongs to the trailing
    /// variadic group.
    fn adjust_last_segment(&mut self, op: OpId, delta: i64) {
        if let Some(sizes) = self.segment_sizes_mut(op)
            && let Some(crate::attributes::AttributeValue::UInt(last)) = sizes.last_mut()
        {
            *last = (*last as i64 + delta) as u64;
        }
    }

    /// The `operand_segment_sizes` an op with a variadic group records, for
    /// editing; `None` for a fixed-arity op.
    fn segment_sizes_mut(&mut self, op: OpId) -> Option<&mut [crate::attributes::AttributeValue]> {
        let segment_sizes = self.names.intern("operand_segment_sizes");
        match &mut self
            .op_attrs_mut(op)
            .iter_mut()
            .find(|attribute| attribute.name == segment_sizes)?
            .value
        {
            crate::attributes::AttributeValue::Array(sizes) => Some(sizes),
            _ => None,
        }
    }

    /// Grow by one the declared operand group whose value operands end at
    /// `index`, the last such group when several end there (an empty variadic
    /// group after a fixed one). A fixed-arity op tracks no segments.
    fn grow_segment_ending_at(&mut self, op: OpId, index: usize) {
        let Some(sizes) = self.segment_sizes_mut(op) else {
            return;
        };
        let mut end = 0;
        let mut chosen = None;
        for (position, size) in sizes.iter().enumerate() {
            if let crate::attributes::AttributeValue::UInt(size) = size {
                end += *size as usize;
                if end == index {
                    chosen = Some(position);
                }
            }
        }
        if let Some(crate::attributes::AttributeValue::UInt(size)) =
            chosen.and_then(|position| sizes.get_mut(position))
        {
            *size += 1;
        }
    }

    fn insert_port(&mut self, op: OpId, at: usize, id: u32) {
        let count = self.op(op).expect("live op").port_count();
        debug_assert!(at >= self.op(op).expect("live op").operand_count as usize);
        self.reserve_ports(op, count + 1);
        let run = self.op(op).expect("live op").run;
        let entries = self.runs.entries_mut(run);
        entries[at..=count].rotate_right(1);
        entries[at] = Entry::new(id);
    }

    /// Drop the port at `at`, shifting the ports after it back.
    ///
    /// Everything after `at` moves, so the caller must have unlinked the port
    /// at `at` and must leave no linked operand behind it: the callers drop the
    /// *last* operand or a result, so only unlinked entries move.
    fn remove_port(&mut self, op: OpId, at: usize) -> u32 {
        debug_assert!(at + 1 >= self.op(op).expect("live op").operand_count as usize);
        let count = self.op(op).expect("live op").port_count();
        let run = self.op(op).expect("live op").run;
        let entries = self.runs.entries_mut(run);
        let id = entries[at].id;
        entries[at..count].rotate_left(1);
        id
    }

    /// Record every operand slot of `op` under the value it holds.
    fn link_operands(&mut self, op: OpId) {
        for (index, value) in self.operand_slots(op) {
            let entry = self.entry_of(op, index);
            self.link_use(value, entry);
        }
    }

    /// Forget every operand slot of `op`. Reads `op`'s storage, so it runs
    /// before the op is erased or its operands are rewritten wholesale.
    fn unlink_operands(&mut self, op: OpId) {
        for (index, value) in self.operand_slots(op) {
            let entry = self.entry_of(op, index);
            self.unlink_use(value, entry);
        }
    }

    /// `op`'s operands paired with their slot indices, copied out so the run
    /// they live in can be borrowed mutably.
    fn operand_slots(&self, op: OpId) -> Vec<(usize, ValueId)> {
        let Some(instance) = self.op(op) else {
            return Vec::new();
        };
        self.runs.entries(instance.run)[..instance.operand_count as usize]
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, ValueId::from_number(entry.id)))
            .collect()
    }

    /// The address of `op`'s `index`-th port.
    fn entry_of(&self, op: OpId, index: usize) -> EntryId {
        let run = self.op(op).expect("live op").run;
        self.runs.entry_id(run, index)
    }

    /// Splice `op`'s `index`-th operand slot onto the front of the use list of
    /// the value it holds.
    fn link_use(&mut self, value: ValueId, entry: EntryId) {
        if value.index() >= self.first_use.len() {
            self.first_use.resize(value.index() + 1, NO_ENTRY);
        }
        let head = self.first_use[value.index()];
        {
            let slot = self.runs.entry_mut(entry);
            slot.prev = NO_ENTRY;
            slot.next = head;
        }
        if head != NO_ENTRY {
            self.runs.entry_mut(EntryId::from_raw(head)).prev = entry.raw();
        }
        self.first_use[value.index()] = entry.raw();
    }

    /// Splice `entry` out of the use list of `value`.
    fn unlink_use(&mut self, value: ValueId, entry: EntryId) {
        if value.index() >= self.first_use.len() {
            return;
        }
        let (prev, next) = {
            let slot = self.runs.entry(entry);
            (slot.prev, slot.next)
        };
        if prev == NO_ENTRY {
            if self.first_use[value.index()] != entry.raw() {
                return;
            }
            self.first_use[value.index()] = next;
        } else {
            self.runs.entry_mut(EntryId::from_raw(prev)).next = next;
        }
        if next != NO_ENTRY {
            self.runs.entry_mut(EntryId::from_raw(next)).prev = prev;
        }
        let slot = self.runs.entry_mut(entry);
        slot.prev = NO_ENTRY;
        slot.next = NO_ENTRY;
    }

    /// The head of `value`'s use list, or [`NO_ENTRY`] if nothing names it.
    fn first_use(&self, value: ValueId) -> u32 {
        self.first_use
            .get(value.index())
            .copied()
            .unwrap_or(NO_ENTRY)
    }

    /// The entries naming `value`, newest first.
    fn use_entries(&self, value: ValueId) -> impl Iterator<Item = EntryId> + '_ {
        let mut current = self.first_use(value);
        std::iter::from_fn(move || {
            let entry = (current != NO_ENTRY).then(|| EntryId::from_raw(current))?;
            current = self.runs.entry(entry).next;
            Some(entry)
        })
    }

    /// Every operand slot naming `value`, oldest first: the list is threaded
    /// front-first, so the walk is reversed to restore the order the slots were
    /// recorded in.
    fn uses(&self, value: ValueId) -> Vec<Use> {
        let mut uses: Vec<Use> = self
            .use_entries(value)
            .map(|entry| {
                let (op, index) = self.runs.locate(entry);
                Use::new(op, index)
            })
            .collect();
        uses.reverse();
        uses
    }

    /// The op enclosing `block`, if the block sits in a region owned by one.
    fn enclosing_op(&self, block: BlockId) -> Option<OpId> {
        let region = *slab_get(&self.block_parent, block.index())?;
        self.region(region)?.parent_op()
    }

    /// The op enclosing `op`, walking out through whatever holds it.
    fn enclosing_op_of(&self, op: OpId) -> Option<OpId> {
        match *slab_get(&self.op_parent, op.index())? {
            Parent::Block(block) => self.enclosing_op(block),
            Parent::Region(region) => self.region(region)?.parent_op(),
        }
    }

    fn bump_version(&mut self, op: OpId) {
        if op.index() >= self.op_version.len() {
            self.op_version.resize(op.index() + 1, 0);
        }
        self.op_version[op.index()] += 1;
        let version = self.op_version[op.index()];
        if let Some(instance) = self.op_mut(op) {
            instance.version = version;
        }
    }

    /// Record that `op`'s subtree changed. Consecutive edits to the same subtree
    /// collapse; [`Context::take_dirty_ops`] removes the rest of the duplicates.
    fn mark_dirty(&mut self, op: OpId) {
        let entry = (op, self.generations.ops.get(op.index()));
        if self.dirty_ops.last() != Some(&entry) {
            self.dirty_ops.push(entry);
        }
    }

    /// `op`'s subtree changed: bump it and every enclosing op, so an analysis
    /// keyed on any ancestor (the function root, typically) sees the edit.
    fn edit_subtree(&mut self, op: OpId) {
        self.mark_dirty(op);
        let mut current = Some(op);
        while let Some(op) = current {
            self.bump_version(op);
            current = self.enclosing_op_of(op);
        }
    }

    /// `op` itself changed (operands, attributes). The dirtied subtree is its
    /// owner's, so verification also sees the siblings the edit may have broken.
    fn edit_op(&mut self, op: OpId) {
        self.bump_version(op);
        match self.enclosing_op_of(op) {
            Some(parent) => self.edit_subtree(parent),
            None => self.mark_dirty(op),
        }
    }

    /// `block`'s contents changed.
    fn edit_block(&mut self, block: BlockId) {
        if let Some(op) = self.enclosing_op(block) {
            self.edit_subtree(op);
        }
    }

    /// `region`'s contents changed: its block list, or an unordered region's
    /// operations or results.
    fn edit_region(&mut self, region: RegionId) {
        if let Some(op) = self.region(region).and_then(Region::parent_op) {
            self.edit_subtree(op);
        }
    }
}

/// The attribute names every registered operation declares, interned ahead of
/// any IR so schema names get dense low ids in registration order and the hot
/// path never copies a string: the names are `'static`, contributed by the
/// `operation!` macro.
fn schema_vocabulary() -> Interner {
    let mut names = Interner::new();
    for schema in crate::schema::OP_SCHEMAS {
        for attribute in schema.attributes {
            names.intern_static(attribute.name);
        }
    }
    names
}

fn type_hash(ty: &dyn Type) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(ty.dialect().as_bytes());
    ty.hash(&mut hasher);
    hasher.finish()
}

impl Context {
    /// Create a new empty context with no registered dialects.
    pub fn new() -> Self {
        Context(Arc::new(RwLock::new(ContextInstance {
            ops: Hive::new(),
            values: Hive::new(),
            regions: Hive::new(),
            blocks: Hive::new(),
            generations: Generations::default(),
            op_parent: Vec::new(),
            block_parent: Vec::new(),
            value_block: Vec::new(),
            value_region: Vec::new(),
            runs: Runs::default(),
            attr_runs: AttrRuns::default(),
            first_use: Vec::new(),
            op_version: Vec::new(),
            dirty_ops: Vec::new(),
            dialects: HashMap::new(),
            reg_classes: HashMap::new(),
            op_interface_converters: HashMap::new(),
            type_cache: vec![],
            type_lookup: HashMap::new(),
            names: schema_vocabulary(),
            op_names: Vec::new(),
            op_name_ids: HashMap::new(),
        })))
    }

    /// Create a new context with default dialects.
    pub fn with_default_dialects() -> Self {
        let context = Context::new();

        context.register_dialect::<BuiltinDialect>();
        context.register_dialect::<CfgDialect>();
        context.register_dialect::<FuncDialect>();
        context.register_dialect::<PtrDialect>();
        context.register_dialect::<ScfDialect>();
        context.register_dialect::<StateDialect>();
        context.register_dialect::<VectorDialect>();

        context
    }

    /// The id `name` is keyed by in this context, interning it if it is new.
    pub fn intern(&self, name: &str) -> Sym {
        self.0.write().names.intern(name)
    }

    /// The id `name` already has in this context, or `None` if nothing has ever
    /// been named that here — which is the answer a lookup wants, and costs a
    /// read lock instead of a write one.
    pub fn sym(&self, name: &str) -> Option<Sym> {
        self.0.read().names.lookup(name)
    }

    /// The name behind `sym`. Only ids this context minted are meaningful.
    pub fn resolve(&self, sym: Sym) -> String {
        self.0.read().names.resolve(sym).to_string()
    }

    /// The id this context keys the op identity `(dialect, name)` by, minting
    /// one if the pair is new.
    pub(crate) fn intern_op_name(&self, dialect: &'static str, name: &'static str) -> OpNameId {
        if let Some(id) = self.0.read().op_name_ids.get(&(dialect, name)) {
            return *id;
        }
        let mut inner = self.0.write();
        if let Some(id) = inner.op_name_ids.get(&(dialect, name)) {
            return *id;
        }
        let id = OpNameId::new(inner.op_names.len() as u32);
        inner.op_names.push((dialect, name));
        inner.op_name_ids.insert((dialect, name), id);
        id
    }

    /// Pair an attribute name with its value, interning the name.
    pub fn named_attribute(&self, name: &str, value: AttributeValue) -> NamedAttribute {
        NamedAttribute::new(self.intern(name), value)
    }

    /// Hive capacities against live-entity counts, for the `TIR_MEM_STATS`
    /// census (see [`crate::memstats`]).
    pub fn slab_census(&self) -> crate::memstats::SlabCensus {
        let inner = self.0.read();
        let blocks_heap: usize = inner
            .blocks
            .handles()
            .filter_map(|handle| inner.blocks.get(handle))
            .map(Block::heap_bytes)
            .sum();
        let regions_heap: usize = inner
            .regions
            .handles()
            .filter_map(|handle| inner.regions.get(handle))
            .map(Region::heap_bytes)
            .sum();
        let runs = inner.runs.census();
        let attrs = inner.attr_runs.census();
        crate::memstats::SlabCensus {
            ops_slab: inner.ops.capacity(),
            ops_live: inner.ops.len(),
            values_slab: inner.values.capacity(),
            values_live: inner.values.len(),
            blocks_slab: inner.blocks.capacity(),
            blocks_live: inner.blocks.len(),
            regions_slab: inner.regions.capacity(),
            regions_live: inner.regions.len(),
            runs_live: runs.0,
            runs_chunks: runs.1,
            runs_bytes: runs.2,
            attrs_live: attrs.0,
            attrs_chunks: attrs.1,
            attrs_bytes: attrs.2,
            ops_chunks: inner.ops.chunk_count(),
            values_chunks: inner.values.chunk_count(),
            blocks_chunks: inner.blocks.chunk_count(),
            regions_chunks: inner.regions.chunk_count(),
            ops_bytes: inner.ops.bytes(),
            values_bytes: inner.values.bytes(),
            blocks_bytes: inner.blocks.bytes() + blocks_heap,
            regions_bytes: inner.regions.bytes() + regions_heap,
            slab_bytes: inner.ops.bytes()
                + inner.values.bytes()
                + inner.blocks.bytes()
                + inner.regions.bytes()
                + runs.2
                + attrs.2,
        }
    }

    /// Hand the storage of erased operations' ports and attributes back for
    /// reuse, and release the chunks that emptied.
    ///
    /// Entity ids are *not* recycled. An id is a value's or an operation's
    /// name, and names outlive their bearers here: passes carry worklists of
    /// ids across erases, and register allocation orders virtual registers by
    /// value id, which reads "created later" off "numbered higher". Handing a
    /// dead id to a new entity breaks both, and made a function's generated
    /// code depend on what had been compiled before it. Runs carry no such
    /// meaning — nothing outside this file ever names one — so they are the
    /// storage that can be, and is, reused.
    ///
    /// The backend calls this once per function, where the function's machine
    /// IR has been emitted and erased.
    pub fn recycle(&self) {
        let mut inner = self.0.write();
        inner.runs.recycle();
        inner.attr_runs.recycle();
    }

    pub fn as_context_ref(&self) -> ContextRef {
        ContextRef(Arc::downgrade(&self.0))
    }

    /// Register a dialect with context.
    pub fn register_dialect<D: Dialect>(&self) {
        let mut dialect = D::new();
        Arc::<dyn Dialect>::get_mut(&mut dialect)
            .unwrap()
            .register_operations(self);
        Arc::<dyn Dialect>::get_mut(&mut dialect)
            .unwrap()
            .register_types(self);
        self.0.write().dialects.insert(D::name(), dialect);
    }

    /// Register a target's register classes so the attribute parser can resolve a
    /// `CLASS[n]` register's class name back to its [`RegClassId`]. Backends call
    /// this from `register_dialects` with their generated `register_info().classes`.
    pub fn register_reg_classes(&self, classes: &'static [crate::backend::regalloc::RegClassInfo]) {
        let mut inner = self.0.write();
        for class in classes {
            inner
                .reg_classes
                .insert(class.name, crate::backend::regalloc::RegClassId::new(class));
        }
    }

    /// Resolve a register-class name to its [`RegClassId`], if a target that defines
    /// it has been registered (see [`Context::register_reg_classes`]).
    pub fn resolve_reg_class(&self, name: &str) -> Option<crate::backend::regalloc::RegClassId> {
        self.0.read().reg_classes.get(name).copied()
    }

    pub fn find_dialect<D: Dialect>(&self) -> Option<Arc<D>> {
        self.0
            .read()
            .dialects
            .get(D::name())
            .cloned()
            .and_then(|d| {
                let d: Arc<dyn Any + Send + Sync> = d;
                d.downcast::<D>().ok()
            })
    }

    pub fn add_operation(&self, op: crate::operation::NewOp) -> OpHandle {
        let op_id = {
            let mut inner = self.0.write();
            let op_id = OpId::new(inner.ops.insert_with(|handle| OpInstance {
                id: OpId::new(handle),
                name: op.name,
                run: RunId::NONE,
                operand_count: op.operands.len() as u16,
                result_count: op.results.len() as u16,
                dep_operand_count: op.dep_operands,
                dep_result_count: op.dep_results,
                region_count: op.regions.len() as u16,
                attrs: AttrRunId::NONE,
                attr_count: op.attributes.len() as u16,
                version: 0,
            }));
            // An id reused after an erase must not answer a cached analysis of
            // the op that held it; the version carries across the reuse.
            inner.bump_version(op_id);

            let ids: Vec<u32> = op
                .operands
                .iter()
                .map(|value| value.number())
                .chain(op.results.iter().map(|value| value.number()))
                .chain(op.regions.iter().map(|region| region.number()))
                .collect();
            let run = inner.runs.alloc(op_id, &ids);
            let attrs = inner.attr_runs.alloc(op.attributes);
            let instance = inner.op_mut(op_id).expect("just inserted");
            instance.run = run;
            instance.attrs = attrs;

            // Results are created before op id assignment in builders; patch their def-site now.
            for result_id in op.results {
                if let Some(value) = inner.value_mut(result_id) {
                    value.set_defining_op(op_id);
                }
            }

            for region in op.regions {
                inner.region_mut(region).unwrap().set_parent_op(op_id);
            }

            inner.link_operands(op_id);
            op_id
        };

        self.op_handle(op_id)
    }

    /// Mint a handle for a live op, recording the generation its id carries so
    /// the handle can tell itself from one naming the op that took the slot.
    /// Takes the guard the caller already holds: minting is hot enough that a
    /// second lock acquisition per handle shows up in a profile.
    fn op_handle_in(&self, inner: &ContextInstance, id: OpId) -> OpHandle {
        OpHandle {
            context: self.as_context_ref(),
            id,
            generation: inner.generations.ops.get(id.index()),
        }
    }

    fn op_handle(&self, id: OpId) -> OpHandle {
        self.op_handle_in(&self.0.read(), id)
    }

    pub fn has_operation(&self, id: OpId) -> bool {
        self.0.read().op(id).is_some()
    }

    /// Replace an operation's attributes in place, keeping its id, position, and
    /// regions.
    pub fn set_op_attributes(&self, id: OpId, attributes: Vec<crate::attributes::NamedAttribute>) {
        let mut inner = self.0.write();
        if inner.op(id).is_some() {
            inner.set_op_attrs(id, attributes);
            inner.edit_op(id);
        }
    }

    /// The structural version of `op`: a counter bumped by every edit to `op` or
    /// to anything under it. Analyses cached against a version are stale as soon
    /// as it moves; see [`crate::analysis::AnalysisManager`].
    pub fn op_version(&self, op: OpId) -> u32 {
        let inner = self.0.read();
        match inner.op(op) {
            Some(instance) => instance.version,
            // The retained counter outlives the op, so a cache keyed on an
            // erased op's version can never match the op that took its id.
            None => inner.op_version.get(op.index()).copied().unwrap_or(0),
        }
    }

    /// The subtrees edited since the last call, innermost-dirtied op per edit and
    /// deduplicated. The pass manager drains this to scope post-pass verification.
    pub(crate) fn take_dirty_ops(&self) -> Vec<OpId> {
        let mut inner = self.0.write();
        let mut dirty = std::mem::take(&mut inner.dirty_ops);
        dirty.retain(|(op, generation)| inner.generations.ops.get(op.index()) == *generation);
        let mut dirty: Vec<OpId> = dirty.into_iter().map(|(op, _)| op).collect();
        dirty.sort_unstable();
        dirty.dedup();
        dirty
    }

    /// Erase an op and everything it owns: its result values, its regions, their
    /// blocks and block arguments, and every op nested in them. Called by
    /// `Rewriter::erase_op`/`replace_op` once the op has left its block, so the
    /// arenas track the *live* IR rather than accumulating erased entities.
    /// An [`OpHandle`] naming an erased op (e.g. inside an `OperationRef`) reads
    /// as a panic from here on: read what an erase needs before performing it.
    pub(crate) fn remove_operation(&self, id: OpId) {
        self.remove_operation_except(id, &[]);
    }

    /// [`Context::remove_operation`] for a replacement that adopted some of the
    /// erased op's result values: a post-allocation re-encoding keeps the values
    /// the register assignment already placed, so they outlive the op that used
    /// to define them.
    pub(crate) fn remove_operation_except(&self, id: OpId, keep: &[ValueId]) {
        let mut owned = self.owned_entities(vec![id]);
        owned.values.retain(|value| !keep.contains(value));
        self.free(owned);
    }

    /// Replace a single operation's SSA operand at `index`. Used by register
    /// allocation to retarget a terminator's return value onto a freshly copied
    /// register.
    pub fn set_op_operand(&self, id: OpId, index: usize, new: ValueId) {
        let mut inner = self.0.write();
        match inner.op_operands(id).get(index).copied() {
            Some(old) if old != new => {}
            _ => return,
        }
        inner.replace_operand_at(id, index, new);
        inner.edit_op(id);
    }

    /// Replace all of an operation's SSA operands, the trailing `dep_operands`
    /// of `operands` being its dependencies. Register allocation uses this to
    /// clear a branch's forwarded block arguments once they have been lowered
    /// to explicit copies.
    pub fn set_op_operands(&self, id: OpId, operands: Vec<ValueId>, dep_operands: usize) {
        let mut inner = self.0.write();
        if inner.op(id).is_none() {
            return;
        }
        let (_, results, regions) = inner.ports(id);
        let operands: Vec<u32> = operands.iter().map(|value| value.number()).collect();
        inner.set_ports(id, &operands, &results, &regions);
        inner.op_mut(id).expect("live op").dep_operand_count = dep_operands as u16;
        inner.edit_op(id);
    }

    /// Replace a single operation's SSA result at `index`, moving the
    /// definition of `new` onto this op. Register allocation uses it to rename a
    /// spilled definition onto the fresh value the spill store writes back.
    pub fn set_op_result(&self, id: OpId, index: usize, new: ValueId) {
        let mut inner = self.0.write();
        match inner.op_results(id).get(index).copied() {
            Some(old) if old != new => {}
            _ => return,
        }
        inner.replace_result_at(id, index, new);
        if let Some(value) = inner.value_mut(new) {
            value.set_defining_op(id);
        }
        inner.edit_op(id);
    }

    /// Give a value a new type, keeping its id and every use of it.
    /// Instruction selection retypes the values a machine instruction names to
    /// the register classes they live in: a register class is what a machine
    /// instruction can say about a value, and no copy is paid for the change.
    pub fn retype_value(&self, value: ValueId, ty: TypeId) {
        let mut inner = self.0.write();
        if let Some(value) = inner.value_mut(value) {
            value.set_ty(ty);
        }
    }

    /// [`Context::retype_value`] for a block argument, whose type the block
    /// stores alongside the value arena's copy.
    pub fn retype_block_argument(&self, block: BlockId, index: usize, ty: TypeId) {
        let mut inner = self.0.write();
        let Some(argument) = inner
            .block_mut(block)
            .and_then(|block| block.arguments_mut().get_mut(index))
        else {
            return;
        };
        argument.set_ty(ty);
        let value_id = argument.id();
        if let Some(value) = inner.value_mut(value_id) {
            value.set_ty(ty);
        }
    }

    pub fn create_value(&self, ty: TypeId, defining_op: Option<OpId>) -> Value {
        let mut inner = self.0.write();

        let handle = inner
            .values
            .insert_with(|handle| Value::new(ValueId::from_number(handle), ty, defining_op));
        inner.clear_uses(ValueId::from_number(handle));
        inner.values.get(handle).expect("just inserted").clone()
    }

    /// Mint a dependency: a value carrying no bits, whose only meaning is the
    /// ordering edges that name it.
    pub fn create_dependency(&self) -> ValueId {
        self.create_value(TypeId::DEPENDENCY, None).id()
    }

    pub fn get_value(&self, id: ValueId) -> Value {
        self.0.read().value(id).expect("live value").clone()
    }

    /// Replace every SSA operand use of `old` with `new`.
    ///
    /// The use list names the reading slots directly, so the edit costs one
    /// write per use and reaches every live operation — including ones a
    /// rewrite has taken out of the tree or has not put in it yet. Attributes
    /// naming a value are left untouched: they record where the ABI places a
    /// value, not a read of it.
    pub fn replace_value_uses(&self, old: ValueId, new: ValueId) {
        if old == new {
            return;
        }

        let mut inner = self.0.write();
        let uses = inner.uses(old);
        let mut edited: Option<OpId> = None;
        for r#use in uses {
            inner.replace_operand_at(r#use.op, r#use.index, new);
            if edited != Some(r#use.op) {
                inner.edit_op(r#use.op);
                edited = Some(r#use.op);
            }
        }
    }

    /// Rebuild the use lists from live operation storage and compare. A
    /// mismatch means an operand mutator skipped its bookkeeping, which every
    /// def-use query would then answer wrongly; the pass manager runs this
    /// after each mutating pass when IR verification is on.
    pub fn verify_use_lists(&self) -> Result<(), Error> {
        let inner = self.0.read();
        let mut expected: Vec<Vec<Use>> = vec![Vec::new(); inner.first_use.len()];
        for handle in inner.ops.handles() {
            let op = OpId::new(handle);
            for (slot, value) in inner.op_operands(op).iter().enumerate() {
                if value.index() >= expected.len() {
                    expected.resize_with(value.index() + 1, Vec::new);
                }
                expected[value.index()].push(Use::new(op, slot));
            }
        }
        for (index, mut expected) in expected.into_iter().enumerate() {
            let mut held = inner.uses(ValueId::from_number(index as u32));
            let key = |r#use: &Use| (r#use.op.index(), r#use.index);
            expected.sort_unstable_by_key(key);
            held.sort_unstable_by_key(key);
            if expected != held {
                return Err(Error::VerificationError(format!(
                    "use list of value {index} holds {held:?}, but operands say {expected:?}"
                )));
            }
        }
        Ok(())
    }

    /// Every operand slot holding `value`, in the order the uses were recorded.
    ///
    /// This is the def-use chain: it lists what live operation storage holds,
    /// whether or not the reading op sits in the tree. Attributes naming a
    /// value are not uses — they record where the ABI places it, not a read.
    pub fn uses_of(&self, value: ValueId) -> Vec<Use> {
        self.0.read().uses(value).to_vec()
    }

    /// The operations reading `value`, one entry per operand slot.
    pub fn users_of(&self, value: ValueId) -> Vec<OpId> {
        let inner = self.0.read();
        let mut users: Vec<OpId> = inner
            .use_entries(value)
            .map(|entry| inner.runs.locate(entry).0)
            .collect();
        users.reverse();
        users
    }

    pub fn is_used(&self, value: ValueId) -> bool {
        self.0.read().first_use(value) != NO_ENTRY
    }

    pub fn use_count(&self, value: ValueId) -> usize {
        self.0.read().use_entries(value).count()
    }

    pub fn has_value(&self, id: ValueId) -> bool {
        self.0.read().value(id).is_some()
    }

    pub fn has_region(&self, id: RegionId) -> bool {
        self.0.read().region(id).is_some()
    }

    pub fn has_block(&self, id: BlockId) -> bool {
        self.0.read().block(id).is_some()
    }

    pub fn is_block_argument(&self, id: ValueId) -> bool {
        let inner = self.0.read();
        slab_get(&inner.value_block, id.index()).is_some()
    }

    /// Whether `id` is an argument a region owns itself, rather than one its
    /// entry block owns or a value some operation defines.
    pub fn is_region_port(&self, id: ValueId) -> bool {
        self.region_of_port(id).is_some()
    }

    /// The region `id` is a port of, or `None` when it is a block argument or
    /// an operation defines it.
    pub fn region_of_port(&self, id: ValueId) -> Option<RegionId> {
        let inner = self.0.read();
        slab_get(&inner.value_region, id.index()).copied()
    }

    /// The block `id` is an argument of, or `None` when an operation defines it.
    pub fn block_of_argument(&self, id: ValueId) -> Option<BlockId> {
        let inner = self.0.read();
        slab_get(&inner.value_block, id.index()).copied()
    }

    pub fn create_region(&self) -> RegionHandle {
        let mut inner = self.0.write();

        let region_id = RegionId::new(inner.regions.insert(Region::new()));
        drop(inner);
        self.region_handle(region_id)
    }

    /// Create an unordered region holding `ops`, taking `ports` as its own
    /// arguments and producing `results`.
    ///
    /// The operations pass into the region's ownership here: nothing but this
    /// region holds them, and their parent link says so.
    pub fn create_nodes_region(
        &self,
        ports: Vec<Value>,
        dep_ports: usize,
        ops: Vec<OpId>,
        results: Vec<ValueId>,
        dep_results: usize,
    ) -> RegionHandle {
        let region = self.create_region();
        self.set_region_nodes(region.id(), ports, dep_ports, ops, results, dep_results);
        region
    }

    /// Put `op` into the unordered `region`. Nothing about the position means
    /// anything: the region's dependencies say what runs before what.
    pub fn add(&self, region: RegionId, op: OpId) {
        let mut inner = self.0.write();
        match inner.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { ops, .. }) => ops.push(op),
            _ => panic!("only an unordered region takes an operation without a position"),
        }
        debug_assert!(
            slab_get(&inner.op_parent, op.index()).is_none(),
            "an operation joins an unordered region from nowhere else",
        );
        slab_put(&mut inner.op_parent, op.index(), Parent::Region(region));
        inner.edit_region(region);
    }

    /// Name the values the unordered `region` produces, the trailing
    /// `dep_results` of them dependencies.
    pub fn set_region_results(&self, region: RegionId, results: Vec<ValueId>, dep_results: usize) {
        let mut inner = self.0.write();
        match inner.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes {
                results: held,
                dep_results: held_deps,
                ..
            }) => {
                *held = results;
                *held_deps = dep_results as u32;
            }
            _ => panic!("only an unordered region names its results"),
        }
        inner.edit_region(region);
    }

    /// Make an empty region unordered; see [`Context::create_nodes_region`].
    /// The parser uses this: which kind a region is only becomes clear once its
    /// body has been read.
    pub(crate) fn set_region_nodes(
        &self,
        region: RegionId,
        ports: Vec<Value>,
        dep_ports: usize,
        ops: Vec<OpId>,
        results: Vec<ValueId>,
        dep_results: usize,
    ) {
        let mut inner = self.0.write();
        let port_ids: Vec<ValueId> = ports.iter().map(Value::id).collect();
        let held = ops.clone();
        assert!(
            matches!(
                inner.region(region).expect("live region").body(),
                crate::region::RegionBody::Blocks(blocks) if blocks.is_empty(),
            ),
            "only an empty ordered region becomes unordered",
        );
        let entry = inner.region_mut(region).expect("live region");
        let parent = entry.parent_op();
        *entry = Region::new_nodes(ports, dep_ports, ops, results, dep_results);
        if let Some(parent) = parent {
            entry.set_parent_op(parent);
        }
        for port in port_ids {
            slab_put(&mut inner.value_region, port.index(), region);
        }
        for op in held {
            debug_assert!(
                slab_get(&inner.op_parent, op.index()).is_none(),
                "an operation joins an unordered region from nowhere else",
            );
            slab_put(&mut inner.op_parent, op.index(), Parent::Region(region));
        }
    }

    /// Mint a handle for a live region; see [`Context::op_handle`].
    fn region_handle_in(&self, inner: &ContextInstance, id: RegionId) -> RegionHandle {
        RegionHandle {
            context: self.as_context_ref(),
            generation: inner.generations.regions.get(id.index()),
            id,
        }
    }

    fn region_handle(&self, id: RegionId) -> RegionHandle {
        self.region_handle_in(&self.0.read(), id)
    }

    pub fn create_block(&self, arguments: Vec<Value>) -> BlockHandle {
        self.create_block_with_dependencies(arguments, 0)
    }

    /// [`Context::create_block`] where the trailing `dep_arguments` of
    /// `arguments` are dependencies.
    pub fn create_block_with_dependencies(
        &self,
        arguments: Vec<Value>,
        dep_arguments: usize,
    ) -> BlockHandle {
        let mut inner = self.0.write();

        let argument_ids: Vec<ValueId> = arguments.iter().map(Value::id).collect();
        let mut block = Block::new(arguments);
        block.set_dep_argument_count(dep_arguments);
        let block_id = BlockId::new(inner.blocks.insert(block));
        for argument in argument_ids {
            slab_put(&mut inner.value_block, argument.index(), block_id);
        }
        drop(inner);
        self.block_handle(block_id)
    }

    /// Mint a handle for a live block; see [`Context::op_handle`].
    fn block_handle_in(&self, inner: &ContextInstance, id: BlockId) -> BlockHandle {
        BlockHandle {
            context: self.as_context_ref(),
            generation: inner.generations.blocks.get(id.index()),
            id,
        }
    }

    fn block_handle(&self, id: BlockId) -> BlockHandle {
        self.block_handle_in(&self.0.read(), id)
    }

    /// Append a value argument of type `ty` to `block`, ahead of its
    /// dependencies, and return it. Block ids are stable across the edit, so
    /// branches naming this block keep pointing at it.
    pub fn append_block_argument(&self, block: BlockId, ty: TypeId) -> Value {
        let value = self.create_value(ty, None);
        self.place_block_argument(block, value.clone(), false);
        value
    }

    /// Append a dependency argument to `block`: one more chain the block is
    /// entered on.
    pub fn append_dep_block_argument(&self, block: BlockId) -> Value {
        let value = self.create_value(TypeId::DEPENDENCY, None);
        self.place_block_argument(block, value.clone(), true);
        value
    }

    /// Make `value` an entry argument of `block`, in place of the definition it
    /// had.
    ///
    /// Nothing is renamed, which is the point: the value keeps its identity, so
    /// every reader goes on naming it. The definition it leaves must be going
    /// away, so what an operation produced becomes the parameter of the block
    /// continuing it.
    pub fn adopt_block_argument(&self, block: BlockId, value: ValueId) {
        self.adopt_argument(block, value, false);
    }

    /// [`Context::adopt_block_argument`] for a dependency.
    pub fn adopt_dep_block_argument(&self, block: BlockId, value: ValueId) {
        self.adopt_argument(block, value, true);
    }

    fn adopt_argument(&self, block: BlockId, value: ValueId, dependency: bool) {
        let Some(adopted) = self.0.read().value(value).cloned() else {
            return;
        };
        let adopted = Value::new(value, adopted.ty(), None);
        if self.place_block_argument(block, adopted, dependency) {
            self.0
                .write()
                .value_mut(value)
                .expect("live value")
                .clear_defining_op();
        }
    }

    /// Put `argument` where it belongs among `block`'s arguments: at the end of
    /// the dependencies when it is one, ahead of them otherwise.
    fn place_block_argument(&self, block: BlockId, argument: Value, dependency: bool) -> bool {
        let mut inner = self.0.write();
        let Some(entry) = inner.block_mut(block) else {
            return false;
        };
        let deps = entry.dep_argument_count();
        let at = entry.arguments().len() - if dependency { 0 } else { deps };
        entry.arguments_mut().insert(at, argument.clone());
        if dependency {
            entry.set_dep_argument_count(deps + 1);
        }
        slab_put(&mut inner.value_block, argument.id().index(), block);
        inner.edit_block(block);
        true
    }

    /// Drop `block`'s `index`-th argument and return it. Nothing may read the
    /// argument: it stops being a definition with the edit.
    pub fn remove_block_argument(&self, block: BlockId, index: usize) -> Value {
        let mut inner = self.0.write();
        let entry = inner.block_mut(block).expect("live block");
        let deps = entry.dep_argument_count();
        if index >= entry.arguments().len() - deps {
            entry.set_dep_argument_count(deps - 1);
        }
        let argument = entry.arguments_mut().remove(index);
        clear_slot(&mut inner.value_block, argument.id().index());
        inner.erase_value(argument.id());
        inner.edit_block(block);
        argument
    }

    /// Drop every dependency argument of `block`.
    pub fn clear_dep_arguments(&self, block: BlockId) {
        while let Some(last) = self.get_block(block).dep_arguments().len().checked_sub(1) {
            let values = self.get_block(block).arguments().len() - last - 1;
            self.remove_block_argument(block, values + last);
        }
    }

    /// Append `value` to `op`'s value operands, ahead of its dependencies,
    /// keeping the segment sizes that describe the trailing variadic group in
    /// step.
    pub fn append_operand(&self, op: OpId, value: ValueId) {
        let mut inner = self.0.write();
        let Some(instance) = inner.op(op) else {
            return;
        };
        let index = (instance.operand_count - instance.dep_operand_count) as usize;
        inner.insert_operand(op, index, value);
        inner.adjust_last_segment(op, 1);
        inner.edit_op(op);
    }

    /// Append `value` to `op`'s dependencies: one more chain it observes.
    pub fn append_dep_operand(&self, op: OpId, value: ValueId) {
        let mut inner = self.0.write();
        let Some(instance) = inner.op(op) else {
            return;
        };
        let index = instance.operand_count as usize;
        inner.insert_operand(op, index, value);
        inner.op_mut(op).expect("live op").dep_operand_count += 1;
        inner.edit_op(op);
    }

    /// Append `value` to `op`'s value results, moving its definition onto `op`.
    pub fn adopt_result(&self, op: OpId, value: ValueId) {
        self.adopt_result_port(op, value, false);
    }

    /// Append `value` to `op`'s dependency results, moving its definition onto
    /// `op`. A lowering that replaces an instruction hands the replacement the
    /// chain the original published this way, so the chain crosses the rewrite
    /// intact.
    pub fn adopt_dep_result(&self, op: OpId, value: ValueId) {
        self.adopt_result_port(op, value, true);
    }

    fn adopt_result_port(&self, op: OpId, value: ValueId, dependency: bool) {
        let mut inner = self.0.write();
        if inner.op(op).is_none() {
            return;
        }
        inner.append_result_port(op, value, dependency);
        if let Some(value) = inner.value_mut(value) {
            value.set_defining_op(op);
        }
        inner.edit_op(op);
    }

    /// Give `op` one more dependency result: a chain it leaves behind.
    pub fn append_dep_result(&self, op: OpId) -> ValueId {
        let value = self.create_dependency();
        self.adopt_dep_result(op, value);
        value
    }

    /// Drop `op`'s last value operand, keeping the segment sizes that describe
    /// the grouping in step. The inverse of [`Context::append_operand`].
    pub fn pop_operand(&self, op: OpId) {
        let mut inner = self.0.write();
        let values = match inner.op(op) {
            Some(instance) if instance.operand_count > instance.dep_operand_count => {
                (instance.operand_count - instance.dep_operand_count) as usize
            }
            _ => return,
        };
        inner.remove_operand(op, values - 1);
        inner.adjust_last_segment(op, -1);
        inner.edit_op(op);
    }

    /// Drop every dependency operand of `op`.
    pub fn clear_dep_operands(&self, op: OpId) {
        let mut inner = self.0.write();
        let deps = match inner.op(op) {
            Some(instance) if instance.dep_operand_count > 0 => instance.dep_operand_count as usize,
            _ => return,
        };
        let (mut operands, results, regions) = inner.ports(op);
        operands.truncate(operands.len() - deps);
        inner.set_ports(op, &operands, &results, &regions);
        inner.op_mut(op).expect("live op").dep_operand_count = 0;
        inner.edit_op(op);
    }

    /// Drop `op`'s last value result. Nothing may read it: it stops being a
    /// definition with the edit. The inverse of the result [`Context::grow_port`]
    /// adds.
    pub fn pop_result(&self, op: OpId) {
        let mut inner = self.0.write();
        let at = match inner.op(op) {
            Some(instance) if instance.result_count > instance.dep_result_count => {
                (instance.operand_count + instance.result_count - instance.dep_result_count)
                    as usize
                    - 1
            }
            _ => return,
        };
        let result = inner.remove_port(op, at);
        inner.op_mut(op).expect("live op").result_count -= 1;
        inner.erase_value(ValueId::from_number(result));
        inner.edit_op(op);
    }

    /// Drop every dependency result of `op`. Nothing may read them.
    pub fn clear_dep_results(&self, op: OpId) {
        let mut inner = self.0.write();
        let Some(instance) = inner.op(op) else {
            return;
        };
        let (deps, mut end) = (
            instance.dep_result_count as usize,
            (instance.operand_count + instance.result_count) as usize,
        );
        for _ in 0..deps {
            end -= 1;
            let result = inner.remove_port(op, end);
            inner.erase_value(ValueId::from_number(result));
        }
        let instance = inner.op_mut(op).expect("live op");
        instance.result_count -= deps as u16;
        instance.dep_result_count = 0;
        inner.edit_op(op);
    }

    /// Grow `op` by one carried port of type `ty`.
    ///
    /// A port that carries a value in — a loop's — takes `init` as one more
    /// operand and gives each of the op's regions one more entry argument, which
    /// `latch` receives; a gate that carries nothing in (a conditional) passes
    /// `None` and its regions keep the arguments they had. `latch` says what each
    /// region yields for the port — `None` where the region leaves through an
    /// exit edge that already carries the value. The op gains one result, which
    /// is returned.
    ///
    /// This is the one edit that keeps results, region arguments and yields
    /// consistent; the ports it grows are what scalar promotion and a view
    /// commit materialize.
    pub fn grow_port(
        &self,
        op: OpId,
        ty: TypeId,
        init: Option<ValueId>,
        latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
    ) -> ValueId {
        if self.has_declared_binding(op) {
            return self.grow_declared_port(op, ty, init, latch, false);
        }
        self.grow_port_with(
            op,
            init,
            latch,
            |entry| self.append_block_argument(entry, ty).id(),
            |op, value| self.append_operand(op, value),
        );
        self.append_result(op, ty)
    }

    /// [`Context::grow_port`] for a dependency: the port state threading grows
    /// to carry a chain across a loop or a gate.
    pub fn grow_dep_port(
        &self,
        op: OpId,
        init: Option<ValueId>,
        latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
    ) -> ValueId {
        if self.has_declared_binding(op) {
            return self.grow_declared_port(op, TypeId::DEPENDENCY, init, latch, true);
        }
        self.grow_port_with(
            op,
            init,
            latch,
            |entry| self.append_dep_block_argument(entry).id(),
            |op, value| self.append_dep_operand(op, value),
        );
        self.append_dep_result(op)
    }

    fn grow_port_with(
        &self,
        op: OpId,
        init: Option<ValueId>,
        mut latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
        argument: impl Fn(BlockId) -> ValueId,
        operand: impl Fn(OpId, ValueId),
    ) {
        let instance = self.get_op(op);
        for region in instance.regions() {
            let entry = self.get_region(region).entry_block();
            let incoming = init.map(|_| argument(entry));
            if let Some(latched) = latch(region, incoming) {
                let terminator = *self
                    .get_block(entry)
                    .op_ids()
                    .last()
                    .expect("a region is terminated");
                operand(terminator, latched);
            }
        }
        if let Some(init) = init {
            operand(op, init);
        }
    }

    /// Carry one more port on `op`, an edge [`Context::grow_port`] does not reach:
    /// an `scf.break`/`scf.continue` feeds the port it leaves through, so it takes
    /// the value where a port belongs among its operands. Answers the index the
    /// value took, which is the port's own.
    pub fn append_port_operand(&self, op: OpId, value: ValueId) -> usize {
        self.append_operand(op, value);
        self.get_op(op).value_operands().len() - 1
    }

    /// Whether `op` declares its ports through a `binds:` binding, which
    /// [`Context::grow_port`] reads instead of walking blocks and terminators.
    fn has_declared_binding(&self, op: OpId) -> bool {
        let handle = self.get_op(op);
        handle.has_interface::<dyn crate::Theta>() || handle.has_interface::<dyn crate::Gamma>()
    }

    /// Put `value` at position `index` of `op`'s value operands, or of its
    /// dependency operands when `dependency`. A value joins the declared
    /// operand group ending at `index`, so the segment sizes stay in step.
    pub(crate) fn insert_operand_at(
        &self,
        op: OpId,
        index: usize,
        value: ValueId,
        dependency: bool,
    ) {
        let mut inner = self.0.write();
        let Some(instance) = inner.op(op) else {
            return;
        };
        let values = (instance.operand_count - instance.dep_operand_count) as usize;
        inner.insert_operand(op, if dependency { values + index } else { index }, value);
        if dependency {
            inner.op_mut(op).expect("live op").dep_operand_count += 1;
        } else {
            inner.grow_segment_ending_at(op, index);
        }
        inner.edit_op(op);
    }

    /// Put `port` at position `index` of the unordered `region`'s value ports,
    /// or of its dependency ports when `dependency`.
    pub(crate) fn insert_region_port(
        &self,
        region: RegionId,
        index: usize,
        port: Value,
        dependency: bool,
    ) {
        let mut inner = self.0.write();
        let id = port.id();
        match inner.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes {
                ports, dep_ports, ..
            }) => {
                let values = ports.len() - *dep_ports as usize;
                ports.insert(if dependency { values + index } else { index }, port);
                if dependency {
                    *dep_ports += 1;
                }
            }
            _ => panic!("only an unordered region takes a port by position"),
        }
        slab_put(&mut inner.value_region, id.index(), region);
        inner.edit_region(region);
    }

    /// Name `value` at position `index` of the unordered `region`'s value
    /// results, or of its dependency results when `dependency`.
    pub(crate) fn insert_region_result(
        &self,
        region: RegionId,
        index: usize,
        value: ValueId,
        dependency: bool,
    ) {
        let mut inner = self.0.write();
        match inner.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes {
                results,
                dep_results,
                ..
            }) => {
                let values = results.len() - *dep_results as usize;
                results.insert(if dependency { values + index } else { index }, value);
                if dependency {
                    *dep_results += 1;
                }
            }
            _ => panic!("only an unordered region names its results by position"),
        }
        inner.edit_region(region);
    }

    /// Take `op` out of the unordered `region` without erasing it; the inverse
    /// of [`Context::add`].
    pub fn remove_from_region(&self, region: RegionId, op: OpId) {
        let mut inner = self.0.write();
        match inner.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { ops, .. }) => ops.retain(|held| *held != op),
            _ => panic!("only an unordered region holds an operation without a position"),
        }
        clear_slot(&mut inner.op_parent, op.index());
        inner.edit_region(region);
    }

    /// Give `op` one more value result of type `ty`.
    pub(crate) fn append_result(&self, op: OpId, ty: TypeId) -> ValueId {
        let result = self.create_value(ty, Some(op)).id();
        let mut inner = self.0.write();
        if inner.op(op).is_some() {
            inner.append_result_port(op, result, false);
            inner.edit_op(op);
        }
        result
    }

    /// Begin building a region body off to the side of the live IR.
    ///
    /// The staged blocks belong to no region, so building them bumps no version
    /// and dirties no subtree. Hand the result to
    /// [`Context::replace_region_contents`] to swap it in, or drop it to discard.
    pub fn stage_region(&self) -> StagedRegion {
        StagedRegion {
            context: self.clone(),
            blocks: Vec::new(),
            remap: Vec::new(),
            discard: true,
        }
    }

    /// Swap `staged` in as `region`'s contents, in one edit.
    ///
    /// The old contents leave the tree: their ops are removed from the arena, the
    /// uses they held on surviving values are dropped, and their parent links are
    /// cleared, so no walk reaches into them. Uses of old values recorded with
    /// [`StagedRegion::replace_value`] are then retargeted to their staged
    /// replacements. The swap itself bumps the spine exactly once, at `region`'s
    /// owner, and dirties that one subtree.
    pub fn replace_region_contents(&self, region: RegionId, mut staged: StagedRegion) {
        staged.discard = false;
        let handle = self.get_region(region);
        let owner = handle.parent_op();

        self.detach_subtree(&handle.block_ids());
        handle.set_blocks(staged.blocks.clone());

        {
            let mut inner = self.0.write();
            for &block in &staged.blocks {
                slab_put(&mut inner.block_parent, block.index(), region);
            }
            if let Some(owner) = owner {
                inner.edit_subtree(owner);
            }
        }

        for &(old, new) in &staged.remap {
            self.replace_value_uses(old, new);
        }
    }

    /// Make the ordered `region` the unordered region `staged` was built as:
    /// the old blocks leave the tree with whatever still sits in them, and
    /// `staged`'s ports, operations and results become `region`'s own. `staged`
    /// is gone afterwards.
    pub fn replace_region_with_nodes(&self, region: RegionId, staged: RegionId) {
        let handle = self.get_region(region);
        let owner = handle.parent_op();
        self.detach_subtree(&handle.block_ids());

        let mut inner = self.0.write();
        let body = std::mem::replace(
            inner.region_mut(staged).expect("live region").body_mut(),
            crate::region::RegionBody::Blocks(vec![]),
        );
        let crate::region::RegionBody::Nodes { ports, ops, .. } = &body else {
            panic!("only an unordered region replaces an ordered one's body");
        };
        for port in ports {
            slab_put(&mut inner.value_region, port.id().index(), region);
        }
        for &op in ops {
            slab_put(&mut inner.op_parent, op.index(), Parent::Region(region));
        }
        *inner.region_mut(region).expect("live region").body_mut() = body;
        inner.erase_region(staged);
        if let Some(owner) = owner {
            inner.edit_subtree(owner);
        }
    }

    /// Make the unordered `region` the ordered one `blocks` spell: what the
    /// blocks hold was moved out of the region already, its ports were adopted
    /// as block arguments, and whatever still sits in the region leaves the
    /// tree with it. The first block is the entry.
    pub fn replace_region_with_blocks(&self, region: RegionId, blocks: Vec<BlockId>) {
        let handle = self.get_region(region);
        let owner = handle.parent_op();
        let leftover = handle.op_ids();
        self.free(self.owned_entities(leftover));

        let mut inner = self.0.write();
        let body = std::mem::replace(
            inner.region_mut(region).expect("live region").body_mut(),
            crate::region::RegionBody::Blocks(blocks.clone()),
        );
        let crate::region::RegionBody::Nodes { ports, .. } = &body else {
            panic!("only an ordered region replaces an unordered one's body");
        };
        for port in ports {
            clear_slot(&mut inner.value_region, port.id().index());
        }
        for &block in &blocks {
            slab_put(&mut inner.block_parent, block.index(), region);
        }
        if let Some(owner) = owner {
            inner.edit_subtree(owner);
        }
    }

    /// Take the subtree under `blocks` out of the live IR and give its storage
    /// back. Bumps no version — the caller reports the edit.
    fn detach_subtree(&self, blocks: &[BlockId]) {
        self.free(self.collect_owned(Vec::new(), blocks.to_vec()));
    }

    /// Everything `ops` own: the ops themselves, their result values, their
    /// regions, and those regions' blocks, block arguments and nested ops.
    fn owned_entities(&self, ops: Vec<OpId>) -> Owned {
        self.collect_owned(ops, Vec::new())
    }

    /// Walks ops and blocks alternately. Collected without the context lock held,
    /// so no region lock is ever taken under it.
    ///
    /// A nested entity is reclaimed only while its parent link still points at the
    /// entity being erased: a rewrite that lifts a block out of a region it is
    /// destroying (destruction moves loop bodies into the function region) leaves
    /// the block listed in the dying region, and that stale listing must not free
    /// live IR.
    fn collect_owned(&self, mut ops: Vec<OpId>, mut blocks: Vec<BlockId>) -> Owned {
        let mut owned = Owned::default();
        loop {
            while let Some(op) = ops.pop() {
                let Some(instance) = self.find_op(op) else {
                    continue;
                };
                owned.ops.push(op);
                // A result a rewrite has adopted as a block argument (region
                // destruction hands a region's result to the join block) belongs
                // to that block now, and outlives the op that produced it.
                owned.values.extend(
                    instance
                        .results()
                        .iter()
                        .copied()
                        .filter(|value| !self.is_block_argument(*value)),
                );
                for region in instance.regions() {
                    let Some(handle) = self.find_region(region) else {
                        continue;
                    };
                    if handle.parent_op() != Some(op) {
                        continue;
                    }
                    owned.regions.push(region);
                    if handle.is_nodes() {
                        owned.values.extend(
                            handle
                                .ports()
                                .iter()
                                .map(Value::id)
                                .filter(|port| !self.is_block_argument(*port)),
                        );
                        ops.extend(
                            handle
                                .op_ids()
                                .into_iter()
                                .filter(|op| self.parent_nodes_region(*op) == Some(region)),
                        );
                        continue;
                    }
                    let held = handle
                        .block_ids()
                        .into_iter()
                        .filter(|block| self.parent_region(*block) == Some(region));
                    blocks.extend(held);
                }
            }
            let Some(block) = blocks.pop() else {
                return owned;
            };
            let Some(block) = self.find_block(block) else {
                continue;
            };
            owned.blocks.push(block.id());
            owned
                .values
                .extend(block.arguments().iter().map(|argument| argument.id()));
            ops.extend(
                block
                    .op_ids()
                    .into_iter()
                    .filter(|op| self.parent_block(*op) == Some(block.id())),
            );
        }
    }

    /// Drop the storage of entities that have left the IR, and the reverse-index
    /// entries that pointed into it.
    ///
    /// The hive slot goes back on its chunk's free list, so a later entity of the
    /// same kind can take the id. A handle minted before the reuse names the
    /// generation it was minted with and panics rather than reading its
    /// successor; see [`OpHandle`].
    fn free(&self, owned: Owned) {
        let mut inner = self.0.write();
        for op in owned.ops {
            inner.unlink_operands(op);
            inner.erase_op(op);
            clear_slot(&mut inner.op_parent, op.index());
        }
        for value in owned.values {
            inner.erase_value(value);
            clear_slot(&mut inner.value_block, value.index());
            clear_slot(&mut inner.value_region, value.index());
        }
        for block in owned.blocks {
            inner.erase_block(block);
            clear_slot(&mut inner.block_parent, block.index());
        }
        for region in owned.regions {
            inner.erase_region(region);
        }
    }

    fn find_op(&self, id: OpId) -> Option<OpHandle> {
        let inner = self.0.read();
        inner
            .op(id)
            .is_some()
            .then(|| self.op_handle_in(&inner, id))
    }

    fn find_block(&self, id: BlockId) -> Option<BlockHandle> {
        let inner = self.0.read();
        inner
            .block(id)
            .is_some()
            .then(|| self.block_handle_in(&inner, id))
    }

    fn find_region(&self, id: RegionId) -> Option<RegionHandle> {
        let inner = self.0.read();
        inner
            .region(id)
            .is_some()
            .then(|| self.region_handle_in(&inner, id))
    }

    /// Insert `op` into `block` at `index`, recording the new parent.
    pub(crate) fn insert_op(&self, block: BlockId, index: usize, op: OpId) {
        let mut inner = self.0.write();
        if let Some(entry) = inner.block_mut(block) {
            entry.operations_mut().insert(index, op);
        }
        slab_put(&mut inner.op_parent, op.index(), Parent::Block(block));
        inner.edit_block(block);
    }

    /// Insert `op` after everything `block` currently holds.
    pub(crate) fn append_op(&self, block: BlockId, op: OpId) {
        let mut inner = self.0.write();
        if let Some(entry) = inner.block_mut(block) {
            entry.operations_mut().push(op);
        }
        slab_put(&mut inner.op_parent, op.index(), Parent::Block(block));
        inner.edit_block(block);
    }

    pub(crate) fn replace_op_in_block(&self, block: BlockId, old: OpId, new: OpId) -> bool {
        let mut inner = self.0.write();
        let Some(entry) = inner.block_mut(block) else {
            return false;
        };
        let operations = entry.operations_mut();
        let Some(position) = operations.iter().position(|id| *id == old) else {
            return false;
        };
        operations[position] = new;
        if let Some(slot) = inner.op_parent.get_mut(old.index()) {
            *slot = None;
        }
        slab_put(&mut inner.op_parent, new.index(), Parent::Block(block));
        inner.edit_block(block);
        true
    }

    /// Reorder the operations `block` holds. `ops` must be a permutation of
    /// them: an order is chosen, nothing is added or removed, and no parent
    /// changes.
    pub(crate) fn set_block_ops(&self, block: BlockId, ops: Vec<OpId>) {
        let mut inner = self.0.write();
        if let Some(entry) = inner.block_mut(block) {
            debug_assert_eq!(
                entry.operations().len(),
                ops.len(),
                "a reordering holds the block's own operations",
            );
            *entry.operations_mut() = ops;
        }
        inner.edit_block(block);
    }

    pub(crate) fn remove_op_from_block(&self, block: BlockId, op: OpId) -> bool {
        let mut inner = self.0.write();
        let Some(entry) = inner.block_mut(block) else {
            return false;
        };
        let operations = entry.operations_mut();
        let Some(position) = operations.iter().position(|id| *id == op) else {
            return false;
        };
        operations.remove(position);
        if let Some(slot) = inner.op_parent.get_mut(op.index()) {
            *slot = None;
        }
        inner.edit_block(block);
        true
    }

    pub(crate) fn set_block_attr(
        &self,
        block: BlockId,
        name: &str,
        value: crate::attributes::AttributeValue,
    ) {
        let mut inner = self.0.write();
        let name = inner.names.intern(name);
        if let Some(entry) = inner.block_mut(block) {
            let attributes = entry.attributes_mut();
            match attributes.iter_mut().find(|a| a.name == name) {
                Some(attribute) => attribute.value = value,
                None => attributes.push(crate::attributes::NamedAttribute::new(name, value)),
            }
            inner.edit_block(block);
        }
    }

    /// [`BlockHandle::attr`]: the name is resolved in the same lock as the lookup.
    pub(crate) fn block_attr(
        &self,
        block: BlockId,
        name: &str,
    ) -> Option<crate::attributes::AttributeValue> {
        let inner = self.0.read();
        let name = inner.names.lookup(name)?;
        inner
            .block(block)
            .expect("live block")
            .attributes()
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.clone())
    }

    /// Read a block's storage record under the context lock.
    ///
    /// `read` must not touch the context: the lock is not reentrant.
    pub(crate) fn with_block<R>(&self, id: BlockId, read: impl FnOnce(&Block) -> R) -> R {
        let inner = self.0.read();
        read(inner.block(id).expect("live block"))
    }

    /// The operation owning `region`, if it has been attached to one.
    pub(crate) fn region_parent_op(&self, region: RegionId) -> Option<OpId> {
        self.0
            .read()
            .region(region)
            .expect("live region")
            .parent_op()
    }

    pub(crate) fn region_block_ids(&self, region: RegionId) -> Vec<BlockId> {
        self.0
            .read()
            .region(region)
            .expect("live region")
            .blocks()
            .to_vec()
    }

    pub(crate) fn region_is_nodes(&self, region: RegionId) -> bool {
        matches!(
            self.0.read().region(region).expect("live region").body(),
            crate::region::RegionBody::Nodes { .. }
        )
    }

    /// Every operation the region holds; see [`RegionHandle::op_ids`].
    pub(crate) fn region_op_ids(&self, region: RegionId) -> Vec<OpId> {
        let blocks = {
            let inner = self.0.read();
            match inner.region(region).expect("live region").body() {
                crate::region::RegionBody::Nodes { ops, .. } => return ops.clone(),
                crate::region::RegionBody::Blocks(blocks) => blocks.clone(),
            }
        };
        blocks
            .into_iter()
            .flat_map(|block| self.get_block(block).op_ids())
            .collect()
    }

    /// See [`RegionHandle::ports`].
    pub(crate) fn region_ports(&self, region: RegionId) -> Vec<Value> {
        let entry = {
            let inner = self.0.read();
            match inner.region(region).expect("live region").body() {
                crate::region::RegionBody::Nodes { ports, .. } => return ports.clone(),
                crate::region::RegionBody::Blocks(blocks) => match blocks.first() {
                    Some(entry) => *entry,
                    None => return Vec::new(),
                },
            }
        };
        self.get_block(entry).arguments()
    }

    /// See [`RegionHandle::results`].
    pub(crate) fn region_results(&self, region: RegionId) -> Vec<ValueId> {
        match self.0.read().region(region).expect("live region").body() {
            crate::region::RegionBody::Nodes { results, .. } => results.clone(),
            crate::region::RegionBody::Blocks(_) => Vec::new(),
        }
    }

    /// How many of the region's ports and results are dependencies.
    pub(crate) fn region_dep_counts(&self, region: RegionId) -> (usize, usize) {
        let entry = {
            let inner = self.0.read();
            match inner.region(region).expect("live region").body() {
                crate::region::RegionBody::Nodes {
                    dep_ports,
                    dep_results,
                    ..
                } => return (*dep_ports as usize, *dep_results as usize),
                crate::region::RegionBody::Blocks(blocks) => match blocks.first() {
                    Some(entry) => *entry,
                    None => return (0, 0),
                },
            }
        };
        (
            self.with_block(entry, |block| block.dep_argument_count()),
            0,
        )
    }

    pub(crate) fn add_block_to_region(&self, region: RegionId, block: BlockId) {
        let mut inner = self.0.write();
        if let Some(entry) = inner.region_mut(region) {
            entry.blocks_mut().push(block);
        }
        slab_put(&mut inner.block_parent, block.index(), region);
        inner.edit_region(region);
    }

    pub(crate) fn remove_block_from_region(&self, region: RegionId, block: BlockId) -> bool {
        let mut inner = self.0.write();
        let Some(entry) = inner.region_mut(region) else {
            return false;
        };
        let blocks = entry.blocks_mut();
        let Some(position) = blocks.iter().position(|id| *id == block) else {
            return false;
        };
        blocks.remove(position);
        clear_slot(&mut inner.block_parent, block.index());
        inner.edit_region(region);
        true
    }

    pub(crate) fn set_region_blocks(&self, region: RegionId, blocks: Vec<BlockId>) {
        let mut inner = self.0.write();
        if let Some(entry) = inner.region_mut(region) {
            *entry.blocks_mut() = blocks;
        }
    }

    /// The block currently holding `op`, or `None` for an op not in any block (the
    /// root op, or one detached by a rewrite). Maintained by `Block`'s membership
    /// mutators; see [`ContextInstance::op_parent`].
    pub fn parent_block(&self, op: OpId) -> Option<BlockId> {
        match slab_get(&self.0.read().op_parent, op.index()) {
            Some(Parent::Block(block)) => Some(*block),
            _ => None,
        }
    }

    /// The unordered region holding `op` directly, or `None` for an op that
    /// sits in a block or in no region at all.
    pub fn parent_nodes_region(&self, op: OpId) -> Option<RegionId> {
        match slab_get(&self.0.read().op_parent, op.index()) {
            Some(Parent::Region(region)) => Some(*region),
            _ => None,
        }
    }

    /// The region holding `op`, through its block where it has one.
    pub fn region_of_op(&self, op: OpId) -> Option<RegionId> {
        match slab_get(&self.0.read().op_parent, op.index()).copied()? {
            Parent::Region(region) => Some(region),
            Parent::Block(block) => self.parent_region(block),
        }
    }

    /// The operation enclosing `op`: the owner of the region holding `op`'s
    /// block. `None` for a root op or one detached by a rewrite.
    pub fn parent_op(&self, op: OpId) -> Option<OpId> {
        self.0.read().enclosing_op_of(op)
    }

    /// The region currently holding `block`, or `None` for a detached block.
    /// Maintained by [`Region::add_block`]; see [`ContextInstance::block_parent`].
    pub fn parent_region(&self, block: BlockId) -> Option<RegionId> {
        slab_get(&self.0.read().block_parent, block.index()).copied()
    }

    /// The handle naming `id`. Panics for an id no live block has: a handle reads
    /// the block as it stands, and an erased one does not stand.
    pub fn get_block(&self, id: BlockId) -> BlockHandle {
        let inner = self.0.read();
        inner.block(id).expect("live block");
        self.block_handle_in(&inner, id)
    }

    /// The handle naming `id`; see [`Context::get_block`].
    pub fn get_region(&self, id: RegionId) -> RegionHandle {
        let inner = self.0.read();
        inner.region(id).expect("live region");
        self.region_handle_in(&inner, id)
    }

    /// The handle naming `id`. Panics for an id no live operation has: a handle
    /// reads the operation as it stands, and an erased one does not stand.
    pub fn get_op(&self, id: OpId) -> OpHandle {
        let inner = self.0.read();
        inner.op(id).expect("live operation");
        self.op_handle_in(&inner, id)
    }

    /// The number of times `id`'s slot has been erased, so a caller holding an
    /// id across an erase can tell the entity it named from the one that took
    /// its place. See [`OpHandle`].
    pub(crate) fn op_generation(&self, id: OpId) -> u32 {
        self.0.read().generations.ops.get(id.index())
    }

    /// [`Context::op_generation`] for a block.
    pub(crate) fn block_generation(&self, id: BlockId) -> u32 {
        self.0.read().generations.blocks.get(id.index())
    }

    /// [`Context::op_generation`] for a region.
    pub(crate) fn region_generation(&self, id: RegionId) -> u32 {
        self.0.read().generations.regions.get(id.index())
    }

    /// Panic if `id`'s slot has been reused since a handle was minted with
    /// `generation`.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_op_generation(&self, id: OpId, generation: u32) {
        assert_eq!(
            self.0.read().generations.ops.get(id.index()),
            generation,
            "handle to erased operation {id:?}"
        );
    }

    /// [`Context::assert_op_generation`] for a block.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_block_generation(&self, id: BlockId, generation: u32) {
        assert_eq!(
            self.0.read().generations.blocks.get(id.index()),
            generation,
            "handle to erased block {id:?}"
        );
    }

    /// [`Context::assert_op_generation`] for a region.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_region_generation(&self, id: RegionId, generation: u32) {
        assert_eq!(
            self.0.read().generations.regions.get(id.index()),
            generation,
            "handle to erased region {id:?}"
        );
    }

    /// Read an attribute of `op` in place. For an attribute large enough that
    /// cloning it per lookup would matter — the register assignment of a whole
    /// function, read once per instruction slot.
    ///
    /// `read` must not touch the context: the lock is not reentrant.
    pub fn with_attr<R>(
        &self,
        id: OpId,
        name: &str,
        read: impl FnOnce(&AttributeValue) -> R,
    ) -> Option<R> {
        let inner = self.0.read();
        let name = inner.names.lookup(name)?;
        inner
            .op_attrs(id)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| read(&attribute.value))
    }

    pub(crate) fn op_operands(&self, id: OpId) -> crate::operation::ValueIds {
        self.0.read().op_operands(id)
    }

    pub(crate) fn op_results(&self, id: OpId) -> crate::operation::ValueIds {
        self.0.read().op_results(id)
    }

    /// How many of `id`'s operands and results are dependencies.
    pub(crate) fn op_dep_counts(&self, id: OpId) -> (usize, usize) {
        match self.0.read().op(id) {
            Some(instance) => (
                instance.dep_operand_count as usize,
                instance.dep_result_count as usize,
            ),
            None => (0, 0),
        }
    }

    pub(crate) fn op_regions(&self, id: OpId) -> crate::operation::RegionIds {
        self.0.read().op_regions(id)
    }

    pub(crate) fn op_attributes(&self, id: OpId) -> Vec<NamedAttribute> {
        self.0.read().op_attrs(id).to_vec()
    }

    /// [`OpHandle::attr_sym`]: the lookup is a `u32` compare per attribute.
    pub(crate) fn op_attr_sym(&self, id: OpId, name: Sym) -> Option<AttributeValue> {
        self.0
            .read()
            .op_attrs(id)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.clone())
    }

    /// [`OpHandle::attr`]: the name is resolved in the same lock as the lookup.
    pub(crate) fn op_attr(&self, id: OpId, name: &str) -> Option<AttributeValue> {
        let inner = self.0.read();
        let name = inner.names.lookup(name)?;
        inner
            .op_attrs(id)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.clone())
    }

    /// The `(dialect, name)` pair `id` is spelled by.
    pub(crate) fn op_identity(&self, id: OpId) -> (&'static str, &'static str) {
        let inner = self.0.read();
        let name = inner.op(id).expect("live operation").name_id();
        inner.op_names[name.index()]
    }

    pub fn register_op_interface<I: ?Sized + 'static>(
        &self,
        dialect: &'static str,
        op_name: &'static str,
        converter: OpInterfaceConverter,
    ) {
        self.0
            .write()
            .op_interface_converters
            .insert((dialect, op_name, std::any::TypeId::of::<I>()), converter);
    }

    pub fn register_operation_interface<Op, I>(&self)
    where
        Op: ImplementsOpInterface<I>,
        I: ?Sized + 'static,
    {
        self.register_op_interface::<I>(Op::dialect(), Op::name(), op_interface_converter::<Op, I>);
    }

    pub(crate) fn get_dyn_op(&self, op: OpHandle) -> Box<dyn Operation> {
        // The identity read takes the lock, so it happens before this one does.
        let dialect_name = self.op_identity(op.id).0;
        let dialect = self.0.read().dialects.get(dialect_name).unwrap().clone();
        dialect.get_dyn_op(op)
    }

    pub(crate) fn get_op_interface<I: ?Sized + 'static>(&self, op: OpHandle) -> Option<Box<I>> {
        let converter = self.find_op_interface::<I>(self.op_identity(op.id))?;
        let erased = converter(op);
        downcast_op_interface::<I>(erased)
    }

    pub(crate) fn find_op_interface<I: ?Sized + 'static>(
        &self,
        identity: (&'static str, &'static str),
    ) -> Option<OpInterfaceConverter> {
        self.0
            .read()
            .op_interface_converters
            .get(&(identity.0, identity.1, std::any::TypeId::of::<I>()))
            .copied()
    }

    pub fn get_parser(&self, dialect: &str, name: &str) -> Result<OperationParser, Error> {
        let inner = self.0.read();

        let dialect = inner
            .dialects
            .get(dialect)
            .ok_or(Error::UnknownDialect(dialect.to_string()))?;

        dialect.get_parser(name)
    }

    pub fn get_type_parser(&self, dialect: &str, name: &str) -> Result<TypeParser, Error> {
        let inner = self.0.read();

        let dialect_impl = inner
            .dialects
            .get(dialect)
            .ok_or(Error::UnknownDialect(dialect.to_string()))?;

        if let Ok(parser) = dialect_impl.get_type_parser(name) {
            return Ok(parser);
        }

        let prefix: String = name
            .chars()
            .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
            .collect();

        if prefix.is_empty() || prefix == name {
            return Err(Error::UnknownType(dialect.to_string(), name.to_string()));
        }

        dialect_impl.get_type_parser(&prefix)
    }

    pub fn parse_type_mnemonic(&self, dialect: &str, name: &str) -> Result<TypeId, Error> {
        let parser = self.get_type_parser(dialect, name)?;
        let mut p = IRParser::new("");
        parser(name, &mut p, self).map_err(|(_, err)| err)
    }

    pub fn get_type_id(&self, ty: Arc<dyn Type>) -> TypeId {
        let hash = type_hash(&*ty);
        let mut inner = self.0.upgradable_read();
        if let Some(candidates) = inner.type_lookup.get(&hash) {
            for &id in candidates {
                if inner.type_cache[id.as_index()].eq(&*ty) {
                    return id;
                }
            }
        }

        inner.with_upgraded(|inner| {
            let id = TypeId::from_number(inner.type_cache.len() as u32);
            inner.type_cache.push(ty);
            inner.type_lookup.entry(hash).or_default().push(id);
            id
        })
    }

    pub fn get_type_data(&self, ty: TypeId) -> Arc<dyn Type> {
        self.0
            .read()
            .type_cache
            .get(ty.as_index())
            .cloned()
            .expect("unknown type id")
    }

    pub fn type_to_string(&self, ty: TypeId) -> String {
        let mut out = String::new();
        {
            let mut fmt = IRFormatter::new(&mut out);
            self.print_type(ty, &mut fmt)
                .expect("type print must succeed");
        }
        out
    }

    pub fn print_type(&self, ty: TypeId, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        let ty_data = self.get_type_data(ty);
        fmt.write("!")?;
        if ty_data.dialect() != "builtin" {
            fmt.write(format!("{}.", ty_data.dialect()))?;
        }
        ty_data.print(fmt)
    }
}

/// A region body under construction, detached from the live IR.
///
/// Its blocks, values and ops live in the context's arenas from the moment they
/// are built, but they belong to no region until the staging is committed: they
/// print nowhere, bump no version, and dirty no subtree. Staged ops may take
/// values defined outside the region as operands; those uses become live with the
/// commit and are dropped again if the staging is discarded.
///
/// Created by [`Context::stage_region`]; committed by
/// [`Context::replace_region_contents`], discarded by dropping it.
pub struct StagedRegion {
    context: Context,
    blocks: Vec<BlockId>,
    remap: Vec<(ValueId, ValueId)>,
    discard: bool,
}

impl StagedRegion {
    /// Append a block carrying one argument per entry of `argument_types`. The
    /// first staged block becomes the region's entry.
    pub fn append_block(&mut self, argument_types: &[TypeId]) -> BlockId {
        let arguments = argument_types
            .iter()
            .map(|&ty| self.context.create_value(ty, None))
            .collect();
        let block = self.context.create_block(arguments);
        self.blocks.push(block.id());
        block.id()
    }

    /// The `index`-th argument of a staged block, for staged ops to consume.
    pub fn block_argument(&self, block: BlockId, index: usize) -> Value {
        self.context.get_block(block).arguments()[index].clone()
    }

    /// Append `op` after everything the staged block holds.
    pub fn append_op(&self, block: BlockId, op: OpId) {
        self.context.append_op(block, op);
    }

    /// On commit, retarget every use of `old` that outlives the swap to `new`.
    pub fn replace_value(&mut self, old: ValueId, new: ValueId) {
        self.remap.push((old, new));
    }
}

impl Drop for StagedRegion {
    fn drop(&mut self) {
        if self.discard {
            self.context.detach_subtree(&self.blocks);
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Context::with_default_dialects()
    }
}

impl ContextRef {
    pub fn upgrade(&self) -> Context {
        Context(self.0.upgrade().unwrap())
    }
}

impl<I: GetFromContext> ContextIterator<I> {
    pub fn new(context: Context, elements: Vec<I>) -> Self {
        let current_back = elements.len();
        Self {
            context,
            elements,
            current_front: 0,
            current_back,
        }
    }
}

impl<I: GetFromContext> Iterator for ContextIterator<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_front == self.elements.len() {
            None
        } else {
            let element = self.elements[self.current_front].get_from_context(&self.context);
            self.current_front += 1;
            Some(element)
        }
    }
}

impl<I: GetFromContext> ExactSizeIterator for ContextIterator<I> {
    fn len(&self) -> usize {
        self.elements.len()
    }
}

impl<I: GetFromContext> DoubleEndedIterator for ContextIterator<I> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.current_back == 0 {
            None
        } else {
            self.current_back -= 1;
            let element = self.elements[self.current_back].get_from_context(&self.context);
            Some(element)
        }
    }
}
