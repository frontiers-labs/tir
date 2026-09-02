use std::{
    any::Any,
    collections::HashMap,
    hash::{DefaultHasher, Hasher},
    sync::{Arc, Weak, atomic::AtomicU32},
};

use parking_lot::RwLock;

use tir_adt::{Interner, Sym};

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
    value::{Value, ValueId},
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

const NO_SLOT: u32 = u32::MAX;

/// Dense storage for one entity kind: the entities themselves, packed back to
/// back, plus the table saying where each dense id sits. Nothing may iterate
/// `items` in place of the ids — an erase closes the hole with the last entity,
/// so their order is not the ids' order. Walk `slots` instead.
struct Slab<T> {
    items: Vec<T>,
    /// Where each id sits in `items`, or [`NO_SLOT`] for an id never created or
    /// since erased.
    slots: Vec<u32>,
}

impl<T> Slab<T> {
    fn new() -> Self {
        Slab {
            items: Vec::new(),
            slots: Vec::new(),
        }
    }

    fn slot(&self, index: usize) -> Option<usize> {
        match self.slots.get(index).copied() {
            Some(slot) if slot != NO_SLOT => Some(slot as usize),
            _ => None,
        }
    }

    fn get(&self, index: usize) -> Option<&T> {
        self.slot(index).map(|slot| &self.items[slot])
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.slot(index).map(|slot| &mut self.items[slot])
    }

    /// Store `item` under `index`, replacing whatever that id held.
    fn put(&mut self, index: usize, item: T) {
        if let Some(slot) = self.slot(index) {
            self.items[slot] = item;
            return;
        }
        if index >= self.slots.len() {
            self.slots.resize(index + 1, NO_SLOT);
        }
        self.slots[index] = self.items.len() as u32;
        self.items.push(item);
    }

    /// Give an entity's storage back, closing the hole with the last live entity
    /// and repairing that entity's slot, which `id_of` reads off it.
    fn erase(&mut self, index: usize, id_of: impl Fn(&T) -> usize) {
        let Some(slot) = self.slot(index) else {
            return;
        };
        self.items.swap_remove(slot);
        if let Some(moved) = self.items.get(slot) {
            self.slots[id_of(moved)] = slot as u32;
        }
        self.slots[index] = NO_SLOT;
    }

    /// Ids in increasing order, live ones only.
    fn live_ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| **slot != NO_SLOT)
            .map(|(index, _)| index)
    }
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
    // Entities live densely in [`Slab`]s keyed by the monotonic id counters below;
    // the reverse indices after them are plain side tables.
    ops: Slab<OpInstance>,
    last_op_id: AtomicU32,
    values: Slab<Value>,
    last_value_id: AtomicU32,
    regions: Slab<Region>,
    last_region_id: AtomicU32,
    blocks: Slab<Block>,
    last_block_id: AtomicU32,
    /// Reverse index from an operation to the block that holds it, maintained by
    /// `Block`'s membership mutators. Lets `parent_block` answer in O(1) instead of
    /// scanning every block's operation list.
    op_parent: Vec<Option<BlockId>>,
    /// Reverse index from a block to the region that holds it, maintained by
    /// [`Region::add_block`]. Together with [`Region::parent_op`] it lets walks
    /// climb from an op to its enclosing ops.
    block_parent: Vec<Option<RegionId>>,
    /// Def-site index for block arguments: the block whose argument list a value
    /// entered. The counterpart of [`Value::defining_op`] for values no operation
    /// defines, and what bounds the scope a use of such a value can sit in.
    value_block: Vec<Option<BlockId>>,
    /// Structural version per op, bumped along the spine root-ward by every
    /// tree edit; see [`Context::op_version`].
    op_version: Vec<u32>,
    /// Ops whose own subtree an edit touched since the last
    /// [`Context::take_dirty_ops`], for scoping post-pass verification.
    dirty_ops: Vec<OpId>,
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
        self.ops.get(id.index())
    }

    fn op_mut(&mut self, id: OpId) -> Option<&mut OpInstance> {
        self.ops.get_mut(id.index())
    }

    fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.get(id.index())
    }

    fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.blocks.get_mut(id.index())
    }

    fn region(&self, id: RegionId) -> Option<&Region> {
        self.regions.get(id.index())
    }

    fn region_mut(&mut self, id: RegionId) -> Option<&mut Region> {
        self.regions.get_mut(id.index())
    }

    fn value(&self, id: ValueId) -> Option<&Value> {
        self.values.get(id.index())
    }

    fn value_mut(&mut self, id: ValueId) -> Option<&mut Value> {
        self.values.get_mut(id.index())
    }

    fn erase_op(&mut self, id: OpId) {
        self.ops.erase(id.index(), |op| op.id.index());
    }

    fn erase_block(&mut self, id: BlockId) {
        self.blocks.erase(id.index(), |block| block.id().index());
    }

    fn erase_region(&mut self, id: RegionId) {
        self.regions.erase(id.index(), |region| region.id().index());
    }

    fn erase_value(&mut self, id: ValueId) {
        self.values.erase(id.index(), |value| value.id().index());
    }

    /// The op enclosing `block`, if the block sits in a region owned by one.
    fn enclosing_op(&self, block: BlockId) -> Option<OpId> {
        let region = *slab_get(&self.block_parent, block.index())?;
        self.region(region)?.parent_op()
    }

    /// The op enclosing `op`, walking out through its block and region.
    fn enclosing_op_of(&self, op: OpId) -> Option<OpId> {
        let block = *slab_get(&self.op_parent, op.index())?;
        self.enclosing_op(block)
    }

    fn bump_version(&mut self, op: OpId) {
        if op.index() >= self.op_version.len() {
            self.op_version.resize(op.index() + 1, 0);
        }
        self.op_version[op.index()] += 1;
    }

    /// Record that `op`'s subtree changed. Consecutive edits to the same subtree
    /// collapse; [`Context::take_dirty_ops`] removes the rest of the duplicates.
    fn mark_dirty(&mut self, op: OpId) {
        if self.dirty_ops.last() != Some(&op) {
            self.dirty_ops.push(op);
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

    /// `region`'s block list changed.
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
            ops: Slab::new(),
            last_op_id: AtomicU32::new(0),
            values: Slab::new(),
            last_value_id: AtomicU32::new(0),
            regions: Slab::new(),
            last_region_id: AtomicU32::new(0),
            blocks: Slab::new(),
            last_block_id: AtomicU32::new(0),
            op_parent: Vec::new(),
            block_parent: Vec::new(),
            value_block: Vec::new(),
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

    /// Slab capacities against live-entity counts, for the `TIR_MEM_STATS`
    /// census (see [`crate::memstats`]).
    pub fn slab_census(&self) -> crate::memstats::SlabCensus {
        let inner = self.0.read();
        const SLOT_BYTES: usize = 4;
        let ops_live = inner.ops.items.len();
        let values_live = inner.values.items.len();
        let blocks_live = inner.blocks.items.len();
        let regions_live = inner.regions.items.len();
        let ops_heap: usize = inner.ops.items.iter().map(OpInstance::heap_bytes).sum();
        let blocks_heap: usize = inner.blocks.items.iter().map(Block::heap_bytes).sum();
        let regions_heap: usize = inner.regions.items.iter().map(Region::heap_bytes).sum();
        crate::memstats::SlabCensus {
            ops_slab: inner.ops.slots.len(),
            ops_live,
            values_slab: inner.values.slots.len(),
            values_live,
            blocks_slab: inner.blocks.slots.len(),
            blocks_live,
            regions_slab: inner.regions.slots.len(),
            regions_live,
            ops_bytes: ops_live * std::mem::size_of::<OpInstance>() + ops_heap,
            values_bytes: values_live * std::mem::size_of::<Value>(),
            blocks_bytes: blocks_live * std::mem::size_of::<Block>() + blocks_heap,
            regions_bytes: regions_live * std::mem::size_of::<Region>() + regions_heap,
            slab_bytes: (inner.ops.slots.capacity()
                + inner.values.slots.capacity()
                + inner.blocks.slots.capacity()
                + inner.regions.slots.capacity())
                * SLOT_BYTES,
        }
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

    pub fn add_operation(&self, mut instance: OpInstance) -> OpHandle {
        let op_id = {
            let mut inner = self.0.write();

            let op_id = OpId::new(
                inner
                    .last_op_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            );

            instance.id = op_id;

            // Results are created before op id assignment in builders; patch their def-site now.
            for result_id in instance.results() {
                if let Some(value) = inner.value_mut(result_id) {
                    value.set_defining_op(op_id);
                }
            }

            for r in instance.regions() {
                inner.region_mut(r).unwrap().set_parent_op(op_id);
            }

            inner.ops.put(op_id.index(), instance);
            op_id
        };

        OpHandle {
            context: self.as_context_ref(),
            id: op_id,
        }
    }

    pub fn has_operation(&self, id: OpId) -> bool {
        self.0.read().op(id).is_some()
    }

    /// Replace an operation's attributes in place, keeping its id, position, and
    /// regions.
    pub fn set_op_attributes(&self, id: OpId, attributes: Vec<crate::attributes::NamedAttribute>) {
        let mut inner = self.0.write();
        if let Some(existing) = inner.op_mut(id) {
            existing.set_attributes(attributes);
            inner.edit_op(id);
        }
    }

    /// The structural version of `op`: a counter bumped by every edit to `op` or
    /// to anything under it. Analyses cached against a version are stale as soon
    /// as it moves; see [`crate::analysis::AnalysisManager`].
    pub fn op_version(&self, op: OpId) -> u32 {
        self.0
            .read()
            .op_version
            .get(op.index())
            .copied()
            .unwrap_or(0)
    }

    /// The subtrees edited since the last call, innermost-dirtied op per edit and
    /// deduplicated. The pass manager drains this to scope post-pass verification.
    pub(crate) fn take_dirty_ops(&self) -> Vec<OpId> {
        let mut dirty = std::mem::take(&mut self.0.write().dirty_ops);
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
        match inner
            .op(id)
            .and_then(|op| op.operands().get(index).copied())
        {
            Some(old) if old != new => {}
            _ => return,
        }
        if let Some(op) = inner.op_mut(id) {
            op.replace_operand_at(index, new);
        }
        inner.edit_op(id);
    }

    /// Replace all of an operation's SSA operands. Register allocation uses this
    /// to clear a branch's forwarded block arguments once they have been lowered
    /// to explicit copies.
    pub fn set_op_operands(&self, id: OpId, operands: Vec<ValueId>) {
        let mut inner = self.0.write();
        if let Some(op) = inner.op_mut(id) {
            op.set_operands(operands);
            inner.edit_op(id);
        }
    }

    /// Replace a single operation's SSA result at `index`, moving the
    /// definition of `new` onto this op. Register allocation uses it to rename a
    /// spilled definition onto the fresh value the spill store writes back.
    pub fn set_op_result(&self, id: OpId, index: usize, new: ValueId) {
        let mut inner = self.0.write();
        match inner.op(id).and_then(|op| op.results().get(index).copied()) {
            Some(old) if old != new => {}
            _ => return,
        }
        if let Some(op) = inner.op_mut(id) {
            op.replace_result_at(index, new);
        }
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

        let value_id = ValueId::from_number(
            inner
                .last_value_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );

        let value = Value::new(value_id, ty, defining_op);
        inner.values.put(value_id.index(), value.clone());

        value
    }

    pub fn get_value(&self, id: ValueId) -> Value {
        self.0.read().value(id).expect("live value").clone()
    }

    /// Replace every SSA operand use of `old` with `new`.
    ///
    /// The green core keeps no use lists, so the uses are found by walking the
    /// scope a use of `old` can sit in (see [`Context::use_scope`]); the
    /// [`DefUse`](crate::analysis::DefUse) analysis is the cached index passes
    /// query. The edit applies to the tree: a use in a block an edit has taken
    /// out of it is not rewritten, which is what
    /// [`StagedRegion::replace_value`] exists for. Attributes naming a value are
    /// left untouched: they record where the ABI places a value, not a read of
    /// it.
    pub fn replace_value_uses(&self, old: ValueId, new: ValueId) {
        if old == new {
            return;
        }

        let ops = match self.use_scope(old) {
            Some(root) => self.ops_under(root),
            None => self.live_ops(),
        };

        let mut inner = self.0.write();
        for op in ops {
            let uses_old = inner
                .op(op)
                .is_some_and(|instance| instance.operands().contains(&old));
            if !uses_old {
                continue;
            }
            if let Some(instance) = inner.op_mut(op) {
                instance.replace_operand_uses(old, new);
            }
            inner.edit_op(op);
        }
    }

    /// Point every operand use under `root` at the value `bindings` maps it to.
    ///
    /// Frontends and the text parser name a module-level λ or δ before its
    /// definition exists, then bind the placeholders they used once the whole
    /// module is in.
    pub fn rebind_operands(&self, root: OpId, bindings: &HashMap<ValueId, ValueId>) {
        let instance = self.get_op(root);
        let operands = instance.operands();
        if operands.iter().any(|value| bindings.contains_key(value)) {
            let rebound = operands
                .iter()
                .map(|value| bindings.get(value).copied().unwrap_or(*value))
                .collect();
            self.set_op_operands(root, rebound);
        }
        for region in instance.regions() {
            for block in self.get_region(region).block_ids() {
                for child in self.get_block(block).op_ids() {
                    self.rebind_operands(child, bindings);
                }
            }
        }
    }

    /// The operation whose subtree holds every use of `value`: the one enclosing
    /// its definition, since SSA confines a use to the region tree the definition
    /// sits in. `None` for a value whose def-site left the tree, which forces a
    /// scan of everything live.
    fn use_scope(&self, value: ValueId) -> Option<OpId> {
        let inner = self.0.read();
        match inner.value(value)?.defining_op() {
            Some(op) => inner.enclosing_op_of(op),
            None => inner.enclosing_op(*slab_get(&inner.value_block, value.index())?),
        }
    }

    /// Every operation nested under `root`, at any depth.
    fn ops_under(&self, root: OpId) -> Vec<OpId> {
        let blocks: Vec<BlockId> = self
            .get_op(root)
            .regions()
            .iter()
            .flat_map(|region| self.get_region(*region).block_ids())
            .collect();
        self.subtree(&blocks).0
    }

    /// Every live op, in id order. Walks the slot table, not the dense storage:
    /// an erase reorders the latter.
    fn live_ops(&self) -> Vec<OpId> {
        let inner = self.0.read();
        inner
            .ops
            .live_ids()
            .map(|index| OpId::new(index as u32))
            .collect()
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

    /// The block `id` is an argument of, or `None` when an operation defines it.
    pub fn block_of_argument(&self, id: ValueId) -> Option<BlockId> {
        let inner = self.0.read();
        slab_get(&inner.value_block, id.index()).copied()
    }

    pub fn create_region(&self) -> RegionHandle {
        let mut inner = self.0.write();

        let region_id = RegionId::new(
            inner
                .last_region_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );

        inner.regions.put(region_id.index(), Region::new(region_id));

        RegionHandle {
            context: self.as_context_ref(),
            id: region_id,
        }
    }

    pub fn create_block(&self, arguments: Vec<Value>) -> BlockHandle {
        let mut inner = self.0.write();

        let block_id = BlockId::new(
            inner
                .last_block_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );

        for argument in &arguments {
            slab_put(&mut inner.value_block, argument.id().index(), block_id);
        }
        inner
            .blocks
            .put(block_id.index(), Block::new(block_id, arguments));

        BlockHandle {
            context: self.as_context_ref(),
            id: block_id,
        }
    }

    /// Append `value`'s type as a new entry argument of `block` and return the
    /// argument. Block ids are stable across the edit, so branches naming this
    /// block keep pointing at it.
    pub fn append_block_argument(&self, block: BlockId, ty: TypeId) -> Value {
        let value = self.create_value(ty, None);
        let mut inner = self.0.write();
        if let Some(entry) = inner.block_mut(block) {
            entry.arguments_mut().push(value.clone());
            slab_put(&mut inner.value_block, value.id().index(), block);
            inner.edit_block(block);
        }
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
        let mut inner = self.0.write();
        let Some(ty) = inner.value(value).map(Value::ty) else {
            return;
        };
        let adopted = Value::new(value, ty, None);
        let Some(entry) = inner.block_mut(block) else {
            return;
        };
        entry.arguments_mut().push(adopted.clone());
        inner.values.put(value.index(), adopted);
        slab_put(&mut inner.value_block, value.index(), block);
        inner.edit_block(block);
    }

    /// Drop `block`'s `index`-th argument and return it. Nothing may read the
    /// argument: it stops being a definition with the edit.
    pub fn remove_block_argument(&self, block: BlockId, index: usize) -> Value {
        let mut inner = self.0.write();
        let entry = inner.block_mut(block).expect("live block");
        let argument = entry.arguments_mut().remove(index);
        clear_slot(&mut inner.value_block, argument.id().index());
        inner.erase_value(argument.id());
        inner.edit_block(block);
        argument
    }

    /// Append `value` to `op`'s trailing variadic operand group, keeping the
    /// segment sizes that describe the grouping in step.
    pub fn append_operand(&self, op: OpId, value: ValueId) {
        let mut inner = self.0.write();
        let segment_sizes = inner.names.intern("operand_segment_sizes");
        let Some(instance) = inner.op_mut(op) else {
            return;
        };
        instance.push_operand(value);
        if let Some(attribute) = instance
            .attributes_mut()
            .iter_mut()
            .find(|attribute| attribute.name == segment_sizes)
            && let crate::attributes::AttributeValue::Array(sizes) = &mut attribute.value
            && let Some(crate::attributes::AttributeValue::UInt(last)) = sizes.last_mut()
        {
            *last += 1;
        }
        inner.edit_op(op);
    }

    /// Append `value` to `op`'s results, moving its definition onto `op`. A
    /// lowering that replaces an instruction hands the replacement the state the
    /// original published this way, so the chain crosses the rewrite intact.
    pub fn adopt_result(&self, op: OpId, value: ValueId) {
        let mut inner = self.0.write();
        let Some(instance) = inner.op_mut(op) else {
            return;
        };
        instance.push_result(value);
        if let Some(value) = inner.value_mut(value) {
            value.set_defining_op(op);
        }
        inner.edit_op(op);
    }

    /// Drop `op`'s last operand, keeping the segment sizes that describe the
    /// grouping in step. The inverse of [`Context::append_operand`].
    pub fn pop_operand(&self, op: OpId) {
        let mut inner = self.0.write();
        let segment_sizes = inner.names.intern("operand_segment_sizes");
        let Some(instance) = inner.op_mut(op) else {
            return;
        };
        instance.pop_operand();
        if let Some(attribute) = instance
            .attributes_mut()
            .iter_mut()
            .find(|attribute| attribute.name == segment_sizes)
            && let crate::attributes::AttributeValue::Array(sizes) = &mut attribute.value
            && let Some(crate::attributes::AttributeValue::UInt(last)) = sizes.last_mut()
        {
            *last -= 1;
        }
        inner.edit_op(op);
    }

    /// Drop `op`'s last result. Nothing may read it: it stops being a definition
    /// with the edit. The inverse of the result [`Context::grow_port`] adds.
    pub fn pop_result(&self, op: OpId) {
        let mut inner = self.0.write();
        let Some(instance) = inner.op_mut(op) else {
            return;
        };
        if let Some(result) = instance.pop_result() {
            inner.erase_value(result);
        }
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
    /// consistent; the ports it grows are what scalar promotion, state threading
    /// and a view commit all materialize.
    pub fn grow_port(
        &self,
        op: OpId,
        ty: TypeId,
        init: Option<ValueId>,
        mut latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
    ) -> ValueId {
        let instance = self.get_op(op);

        for region in instance.regions() {
            let entry = self.get_region(region).block_ids()[0];
            let incoming = init.map(|_| {
                let argument = self.append_block_argument(entry, ty).id();
                self.place_argument(entry, ty);
                argument
            });
            if let Some(latched) = latch(region, incoming) {
                let terminator = *self
                    .get_block(entry)
                    .op_ids()
                    .last()
                    .expect("a region is terminated");
                self.append_operand(terminator, latched);
                self.place_operand(terminator, ty);
            }
        }
        if let Some(init) = init {
            self.append_operand(op, init);
            self.place_operand(op, ty);
        }
        let result = self.append_result(op, ty);
        self.place_result(op, ty);
        result
    }

    /// Carry one more port on `op`, an edge [`Context::grow_port`] does not reach:
    /// an `scf.break`/`scf.continue` feeds the port it leaves through, so it takes
    /// the value where a port belongs among its operands. Answers the index the
    /// value took, which is the port's own: what belongs to a port stays where the
    /// port was placed, however much later the value it carries is known.
    pub fn append_port_operand(&self, op: OpId, value: ValueId) -> usize {
        let ty = self.get_value(value).ty();
        self.append_operand(op, value);
        self.place_operand(op, ty)
    }

    /// Where the port of type `ty` just appended to `values` belongs: ahead of
    /// the trailing `!state` ports, which are read off the end. `None` when it is
    /// already there — a state port joins them, and an op no chain crosses has
    /// none for it to precede.
    fn port_index(&self, values: &[ValueId], ty: TypeId) -> Option<usize> {
        let state = crate::builtin::StateType::new(self);
        if ty == state {
            return None;
        }
        values
            .iter()
            .position(|&value| self.has_value(value) && self.get_value(value).ty() == state)
    }

    /// Move the port just appended to `op`'s results into the place it belongs.
    fn place_result(&self, op: OpId, ty: TypeId) {
        let Some(index) = self.port_index(&self.get_op(op).results(), ty) else {
            return;
        };
        let mut inner = self.0.write();
        if let Some(instance) = inner.op_mut(op) {
            instance.rotate_results_from(index);
            inner.edit_op(op);
        }
    }

    /// The same for the operand just appended, answering where it ended up.
    /// Rotating within the trailing variadic group leaves the segment sizes
    /// describing it unchanged.
    fn place_operand(&self, op: OpId, ty: TypeId) -> usize {
        let instance = self.get_op(op);
        let operands = instance.operands();
        let last = operands.len() - 1;
        let Some(index) = self.port_index(&operands, ty) else {
            return last;
        };
        let mut inner = self.0.write();
        if let Some(instance) = inner.op_mut(op) {
            instance.rotate_operands_from(index);
            inner.edit_op(op);
        }
        index
    }

    /// The same for the entry argument just appended to `block`.
    fn place_argument(&self, block: BlockId, ty: TypeId) {
        let arguments: Vec<ValueId> = self
            .get_block(block)
            .arguments()
            .iter()
            .map(|argument| argument.id())
            .collect();
        let Some(index) = self.port_index(&arguments, ty) else {
            return;
        };
        let mut inner = self.0.write();
        if let Some(entry) = inner.block_mut(block) {
            entry.arguments_mut()[index..].rotate_right(1);
            inner.edit_block(block);
        }
    }

    /// Give `op` one more result of type `ty`.
    fn append_result(&self, op: OpId, ty: TypeId) -> ValueId {
        let result = self.create_value(ty, Some(op)).id();
        let mut inner = self.0.write();
        if let Some(instance) = inner.op_mut(op) {
            instance.push_result(result);
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

    /// Every op and block under `blocks`, transitively through nested regions.
    /// Collected without the context lock held, so no region lock is ever taken
    /// under it.
    fn subtree(&self, blocks: &[BlockId]) -> (Vec<OpId>, Vec<BlockId>) {
        let mut pending = blocks.to_vec();
        let mut visited_blocks = Vec::new();
        let mut ops = Vec::new();
        while let Some(block) = pending.pop() {
            visited_blocks.push(block);
            for op in self.get_block(block).op_ids() {
                ops.push(op);
                for region in self.get_op(op).regions() {
                    pending.extend(self.get_region(region).block_ids());
                }
            }
        }
        (ops, visited_blocks)
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
    /// Slots are emptied, never handed to another entity: ids come from monotonic
    /// per-context counters and are never reused, so a stale id can only read as
    /// "gone", never as some later entity. The empty slot costs one entry until
    /// the context dies; the entity behind it is freed here.
    fn free(&self, owned: Owned) {
        let mut inner = self.0.write();
        for op in owned.ops {
            inner.erase_op(op);
            clear_slot(&mut inner.op_parent, op.index());
        }
        for value in owned.values {
            inner.erase_value(value);
            clear_slot(&mut inner.value_block, value.index());
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
        self.0.read().op(id).is_some().then(|| OpHandle {
            context: self.as_context_ref(),
            id,
        })
    }

    fn find_block(&self, id: BlockId) -> Option<BlockHandle> {
        self.0.read().block(id).is_some().then(|| BlockHandle {
            context: self.as_context_ref(),
            id,
        })
    }

    fn find_region(&self, id: RegionId) -> Option<RegionHandle> {
        self.0.read().region(id).is_some().then(|| RegionHandle {
            context: self.as_context_ref(),
            id,
        })
    }

    /// Insert `op` into `block` at `index`, recording the new parent.
    pub(crate) fn insert_op(&self, block: BlockId, index: usize, op: OpId) {
        let mut inner = self.0.write();
        if let Some(entry) = inner.block_mut(block) {
            entry.operations_mut().insert(index, op);
        }
        slab_put(&mut inner.op_parent, op.index(), block);
        inner.edit_block(block);
    }

    /// Insert `op` after everything `block` currently holds.
    pub(crate) fn append_op(&self, block: BlockId, op: OpId) {
        let mut inner = self.0.write();
        if let Some(entry) = inner.block_mut(block) {
            entry.operations_mut().push(op);
        }
        slab_put(&mut inner.op_parent, op.index(), block);
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
        slab_put(&mut inner.op_parent, new.index(), block);
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
        slab_get(&self.0.read().op_parent, op.index()).copied()
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
        self.0.read().block(id).expect("live block");
        BlockHandle {
            context: self.as_context_ref(),
            id,
        }
    }

    /// The handle naming `id`; see [`Context::get_block`].
    pub fn get_region(&self, id: RegionId) -> RegionHandle {
        self.0.read().region(id).expect("live region");
        RegionHandle {
            context: self.as_context_ref(),
            id,
        }
    }

    /// The handle naming `id`. Panics for an id no live operation has: a handle
    /// reads the operation as it stands, and an erased one does not stand.
    pub fn get_op(&self, id: OpId) -> OpHandle {
        let inner = self.0.read();
        inner.op(id).expect("live operation");
        OpHandle {
            context: self.as_context_ref(),
            id,
        }
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
        inner.op(id)?.attr_sym(name).map(read)
    }

    /// Read an operation's storage record under the context lock.
    ///
    /// `read` must not touch the context: the lock is not reentrant.
    pub(crate) fn with_op<R>(&self, id: OpId, read: impl FnOnce(&OpInstance) -> R) -> R {
        let inner = self.0.read();
        read(inner.op(id).expect("live operation"))
    }

    /// [`OpHandle::attr`]: the name is resolved in the same lock as the lookup.
    pub(crate) fn op_attr(&self, id: OpId, name: &str) -> Option<AttributeValue> {
        let inner = self.0.read();
        let name = inner.names.lookup(name)?;
        inner
            .op(id)
            .expect("live operation")
            .attr_sym(name)
            .cloned()
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
