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
    /// Register-class names of registered targets, for resolving parsed
    /// `%virtN:CLASS` operands back to a [`RegClassId`].
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

    /// Register a target's register classes so the generic op parser can resolve a
    /// `%virtN:CLASS` operand's class name back to its [`RegClassId`]. Backends call
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

        // Machine ops carry their register operands in role-tagged attributes;
        // resolving the opcode's roles goes back through the context, so it waits
        // until the op is in storage and the lock is free.
        let handle = OpHandle {
            context: self.as_context_ref(),
            id: op_id,
        };
        self.record_register_defs(&handle);
        handle
    }

    /// A `Def`-role register attribute is the def-site of its virtual value, the
    /// machine-IR spelling of an SSA result. Virtual register ids are value
    /// numbers; physical registers have none and are skipped — they are not SSA.
    /// ReadWrite defines too.
    fn record_register_defs(&self, handle: &OpHandle) {
        let Some(semantics) =
            self.get_op_interface::<dyn crate::attributes::RegisterSemantics>(handle.clone())
        else {
            return;
        };
        let attribute_roles = semantics.attribute_roles();
        if attribute_roles.is_empty() {
            return;
        }

        let mut inner = self.0.write();
        for (attr_name, role) in attribute_roles {
            use crate::attributes::{AttributeRole, AttributeValue, RegisterAttr};
            if !matches!(role, AttributeRole::Def | AttributeRole::ReadWrite) {
                continue;
            }
            // Resolved through the held instance: the context lock is not
            // reentrant, so nothing here may go back through `Context`.
            let Some(name) = inner.names.lookup(attr_name) else {
                continue;
            };
            let Some(AttributeValue::Register(register)) =
                inner.op(handle.id).and_then(|op| op.attr_sym(name))
            else {
                continue;
            };
            let id = match register {
                RegisterAttr::Virtual { id, .. }
                | RegisterAttr::FixedUse { id, .. }
                | RegisterAttr::FixedDef { id, .. } => *id,
                RegisterAttr::Physical { .. } => continue,
            };
            let value_id = ValueId::from_number(id);
            if let Some(value) = inner.value_mut(value_id) {
                value.set_defining_op(handle.id);
            }
        }
    }

    pub fn has_operation(&self, id: OpId) -> bool {
        self.0.read().op(id).is_some()
    }

    /// Replace an operation's attributes in place, keeping its id, position, and
    /// regions. Register allocation uses this to rewrite virtual register operands
    /// to physical ones once the def-use chain is no longer needed; it deliberately
    /// does not update `Value::uses`, since physical registers are not SSA values.
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
        self.free(self.owned_entities(vec![id]));
    }

    /// [`Context::remove_operation`] for a replacement that hands the erased op's
    /// result values to the new op: machine ops declare no SSA results and claim
    /// the original result's def-site through a register attribute, so those
    /// values outlive the op that defined them.
    pub(crate) fn remove_operation_keeping_results(&self, id: OpId) {
        let mut owned = self.owned_entities(vec![id]);
        let results = self.get_op(id).results().to_vec();
        owned.values.retain(|value| !results.contains(value));
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
    /// [`StagedRegion::replace_value`] exists for. Register-attribute uses are
    /// intentionally left untouched: they are not SSA operands and belong to
    /// machine IR.
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
    /// every reader goes on naming it — including the register attributes machine
    /// IR carries, which are not operands and which no rename reaches. The
    /// definition it leaves must be going away, so what an operation produced
    /// becomes the parameter of the block continuing it.
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
                owned.values.extend(instance.results().iter().copied());
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

    pub fn parse_type_spec(&self, spec: &str) -> Result<TypeId, Error> {
        let spec = spec.strip_prefix('!').unwrap_or(spec);
        if let Some((dialect, name)) = spec.split_once('.') {
            self.parse_type_mnemonic(dialect, name)
        } else {
            self.parse_type_mnemonic("builtin", spec)
        }
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

#[cfg(test)]
mod staging_tests {
    use super::Context;
    use crate::{
        BlockHandle, BlockId, IRFormatter, OpId, Operand, Operation, RegionId, ValueId, builtin,
        scf,
    };

    /// `module { func demo(%cond) { %c = 1; scf.if %cond { %old = 7; scf.yield }; return } }`
    /// — a region owned by an op nested inside a function, so a commit to it has a
    /// spine to bump and live values around it to reference.
    struct Fixture {
        module: OpId,
        func: OpId,
        if_op: OpId,
        then_region: RegionId,
        then_block: BlockId,
        /// `%old`, defined inside the region a commit replaces.
        old: ValueId,
        /// `%c`, defined outside it and still live after a commit.
        constant: ValueId,
        module_body: BlockHandle,
    }

    fn fixture(context: &Context) -> Fixture {
        let i1 = builtin::IntegerType::new(context, 1);
        let i32_ty = builtin::IntegerType::new(context, 32);
        let unit = builtin::UnitType::new(context);

        let then_region = context.create_region();
        let then_block = context.create_block(vec![]);
        then_region.add_block(then_block.id());
        let old = builtin::ops::constant(context, 7, i32_ty).build();
        then_block.append(old.id());
        then_block.append(scf::ops::r#yield(context, vec![]).build().id());

        let else_region = context.create_region();
        let else_block = context.create_block(vec![]);
        else_region.add_block(else_block.id());
        else_block.append(scf::ops::r#yield(context, vec![]).build().id());

        let body = context.create_region();
        let cond = context.create_value(i1, None);
        let entry = context.create_block(vec![cond.clone()]);
        body.add_block(entry.id());
        let constant = builtin::ops::constant(context, 1, i32_ty).build();
        entry.append(constant.id());
        let if_op = scf::ops::r#if(
            context,
            cond.id(),
            vec![],
            vec![],
            Some(then_region.id()),
            Some(else_region.id()),
        )
        .build();
        entry.append(if_op.id());
        entry.append(
            builtin::ops::r#return(context, Operand::none())
                .build()
                .id(),
        );

        let func = builtin::ops::func(context, "demo", unit, Some(body.id())).build();
        let module = builtin::ops::module(context, None).build();
        module.body().append(func.id());

        Fixture {
            module: module.id(),
            func: func.id(),
            if_op: if_op.id(),
            then_region: then_region.id(),
            then_block: then_block.id(),
            old: old.result(),
            constant: constant.result(),
            module_body: context.get_block(module.body().id()),
        }
    }

    fn printed(context: &Context, module: OpId) -> String {
        let mut out = String::new();
        let mut fmt = IRFormatter::new(&mut out);
        let op = context.get_dyn_op(context.get_op(module));
        crate::print_ir(op.as_ref(), context, &mut fmt).expect("print must succeed");
        out
    }

    /// A staged `^block: scf.yield` body, ready to swap into an `scf.if` region.
    fn staged_yield(context: &Context) -> super::StagedRegion {
        let mut staged = context.stage_region();
        let block = staged.append_block(&[]);
        staged.append_op(block, scf::ops::r#yield(context, vec![]).build().id());
        staged
    }

    #[test]
    fn a_discarded_staging_leaves_the_tree_untouched() {
        let context = Context::with_default_dialects();
        let f = fixture(&context);
        let i32_ty = builtin::IntegerType::new(&context, 32);
        let before = printed(&context, f.module);
        let module_version = context.op_version(f.module);
        let if_version = context.op_version(f.if_op);
        context.take_dirty_ops();

        let staged_op = {
            let mut staged = context.stage_region();
            let block = staged.append_block(&[]);
            let add = builtin::ops::addi(&context, f.constant, f.constant, i32_ty).build();
            staged.append_op(block, add.id());
            add.id()
        };

        assert_eq!(printed(&context, f.module), before, "the IR is unchanged");
        assert_eq!(context.op_version(f.module), module_version);
        assert_eq!(context.op_version(f.if_op), if_version);
        assert!(context.take_dirty_ops().is_empty());
        assert!(!context.has_operation(staged_op), "staged ops are dropped");
        assert!(
            !crate::analysis::DefUse::new(&context, f.func).is_used(f.constant.number()),
            "a discarded staging leaves no uses of live values behind"
        );
    }

    #[test]
    fn a_commit_bumps_the_spine_once() {
        let context = Context::with_default_dialects();
        let f = fixture(&context);
        let module_version = context.op_version(f.module);
        let func_version = context.op_version(f.func);
        let if_version = context.op_version(f.if_op);
        context.take_dirty_ops();

        context.replace_region_contents(f.then_region, staged_yield(&context));

        assert_eq!(context.op_version(f.if_op), if_version + 1);
        assert_eq!(context.op_version(f.func), func_version + 1);
        assert_eq!(context.op_version(f.module), module_version + 1);
        assert_eq!(
            context.take_dirty_ops(),
            vec![f.if_op],
            "the region's owner is the one dirtied subtree"
        );
    }

    #[test]
    fn a_commit_detaches_the_old_subtree() {
        let context = Context::with_default_dialects();
        let f = fixture(&context);
        let old_op = context.get_value(f.old).defining_op().unwrap();

        context.replace_region_contents(f.then_region, staged_yield(&context));

        assert!(!context.has_operation(old_op));
        assert_eq!(context.parent_block(old_op), None);
        assert_eq!(context.parent_op(old_op), None);
        assert_eq!(context.parent_region(f.then_block), None);
        assert!(!printed(&context, f.module).contains("7"));
        // Nothing dirtied walks into the detached subtree.
        crate::verify_op_tree(&context, f.if_op).expect("the committed tree verifies");
    }

    #[test]
    fn staged_ops_keep_their_live_operands() {
        let context = Context::with_default_dialects();
        let f = fixture(&context);
        let i32_ty = builtin::IntegerType::new(&context, 32);

        let mut staged = context.stage_region();
        let block = staged.append_block(&[]);
        let add = builtin::ops::addi(&context, f.constant, f.constant, i32_ty).build();
        staged.append_op(block, add.id());
        staged.append_op(block, scf::ops::r#yield(&context, vec![]).build().id());
        context.replace_region_contents(f.then_region, staged);

        assert_eq!(
            context.get_op(add.id()).operands().as_slice(),
            vec![f.constant; 2]
        );
        assert_eq!(context.parent_block(add.id()), Some(block));
        assert_eq!(context.parent_region(block), Some(f.then_region));
        assert_eq!(
            crate::analysis::DefUse::new(&context, f.func).users_of(f.constant.number()),
            [add.id(); 2]
        );
        crate::verify_op_tree(&context, f.func).expect("the committed tree verifies");
    }

    #[test]
    fn staged_blocks_carry_their_arguments() {
        let context = Context::with_default_dialects();
        let f = fixture(&context);
        let i32_ty = builtin::IntegerType::new(&context, 32);

        let mut staged = context.stage_region();
        let block = staged.append_block(&[i32_ty]);
        let argument = staged.block_argument(block, 0).id();
        let add = builtin::ops::addi(&context, argument, f.constant, i32_ty).build();
        staged.append_op(block, add.id());
        staged.append_op(block, scf::ops::r#yield(&context, vec![]).build().id());
        context.replace_region_contents(f.then_region, staged);

        let committed = context.get_block(block);
        assert_eq!(committed.arguments().len(), 1);
        assert_eq!(committed.arguments()[0].id(), argument);
        assert_eq!(context.get_op(add.id()).operands()[0], argument);
    }

    #[test]
    fn a_commit_remaps_uses_of_replaced_values() {
        let context = Context::with_default_dialects();
        let f = fixture(&context);
        let i32_ty = builtin::IntegerType::new(&context, 32);
        // A use of the old region's value that outlives the swap.
        let user = builtin::ops::addi(&context, f.old, f.old, i32_ty).build();
        f.module_body.append(user.id());

        let mut staged = context.stage_region();
        let block = staged.append_block(&[]);
        let fresh = builtin::ops::constant(&context, 9, i32_ty).build();
        staged.append_op(block, fresh.id());
        staged.append_op(block, scf::ops::r#yield(&context, vec![]).build().id());
        staged.replace_value(f.old, fresh.result());
        context.replace_region_contents(f.then_region, staged);

        assert_eq!(
            context.get_op(user.id()).operands().as_slice(),
            vec![fresh.result(); 2],
            "surviving uses read the staged replacement"
        );
        assert!(!crate::analysis::DefUse::new(&context, f.module).is_used(f.old.number()));
    }

    #[test]
    fn a_commit_keeps_analyses_of_untouched_functions() {
        use crate::{Analysis, AnalysisManager, OpId as Id};
        struct Probe;
        impl Analysis for Probe {
            fn build(_: &AnalysisManager, _: &Context, _: Id) -> Self {
                Probe
            }
        }

        let context = Context::with_default_dialects();
        let f = fixture(&context);
        let unit = builtin::UnitType::new(&context);
        let sibling_body = context.create_region();
        let sibling_entry = context.create_block(vec![]);
        sibling_body.add_block(sibling_entry.id());
        sibling_entry.append(
            builtin::ops::r#return(&context, Operand::none())
                .build()
                .id(),
        );
        let sibling = builtin::ops::func(&context, "sib", unit, Some(sibling_body.id())).build();
        f.module_body.append(sibling.id());

        let analyses = AnalysisManager::new();
        analyses.get::<Probe>(&context, sibling.id());
        analyses.get::<Probe>(&context, f.func);

        context.replace_region_contents(f.then_region, staged_yield(&context));

        assert!(
            analyses
                .get_cached::<Probe>(&context, sibling.id())
                .is_some(),
            "a sibling function's analyses survive a commit elsewhere"
        );
        assert!(analyses.get_cached::<Probe>(&context, f.func).is_none());
    }
}

#[cfg(test)]
mod port_tests {
    use super::Context;
    use crate::{OpId, Operation, RegionId, ValueId, builtin, scf};

    /// A loop with no carried port yet, and a constant outside it to carry in.
    const LOOP: &str = r#"module {
  func @f(%0: !index, %1: !index, %2: !index) -> !i32 {
    %3 = constant {value = 7} : !i32
    scf.for %0, %1, %2 {
      scf.yield
    }
    return %3
  }
  module_end
}"#;

    fn loop_fixture(context: &Context) -> (OpId, OpId, ValueId) {
        let module: builtin::ModuleOp =
            crate::parse::ir::parse_ir(context, LOOP).expect("the fixture parses");
        let func = context
            .get_region(context.get_op(module.id()).regions()[0])
            .iter(context.clone())
            .next()
            .expect("module body")
            .op_ids()[0];
        let body = context
            .get_region(context.get_op(func).regions()[0])
            .iter(context.clone())
            .next()
            .expect("function body");
        let constant = context.get_op(body.op_ids()[0]).results()[0];
        (module.id(), body.op_ids()[1], constant)
    }

    #[test]
    fn growing_a_loop_port_carries_one_more_value() {
        let context = Context::with_default_dialects();
        let (module, loop_op, constant) = loop_fixture(&context);
        let i32_ty = builtin::IntegerType::new(&context, 32);

        let result = context.grow_port(loop_op, i32_ty, Some(constant), |_, carried| carried);

        let grown = context.get_op(loop_op);
        assert_eq!(
            grown.results().as_slice(),
            vec![result],
            "the port's value leaves the op"
        );
        assert_eq!(
            grown.operands().last(),
            Some(&constant),
            "the port's initial value enters as one more operand"
        );
        let body = single_block(&context, grown.regions()[0]);
        let carried = body.arguments()[0].id();
        assert_eq!(body.arguments().len(), 1);
        assert_eq!(
            context
                .get_op(*body.op_ids().last().unwrap())
                .operands()
                .as_slice(),
            vec![carried],
            "the region yields what the port carries"
        );
        crate::verify_op_tree(&context, module).expect("the grown loop verifies");
    }

    #[test]
    fn growing_a_conditional_port_yields_from_every_arm() {
        let context = Context::with_default_dialects();
        let (module, _, constant) = loop_fixture(&context);
        let i1 = builtin::IntegerType::new(&context, 1);
        let i32_ty = builtin::IntegerType::new(&context, 32);
        let condition = builtin::ops::constant(&context, 1, i1).build();
        let arms: Vec<RegionId> = (0..2)
            .map(|_| {
                let region = context.create_region();
                let block = context.create_block(vec![]);
                region.add_block(block.id());
                block.append(scf::ops::r#yield(&context, vec![]).build().id());
                region.id()
            })
            .collect();
        let conditional = scf::ops::r#if(
            &context,
            condition.result(),
            vec![],
            vec![],
            Some(arms[0]),
            Some(arms[1]),
        )
        .build();
        let function_body = context.get_block(
            context
                .get_region(context.get_op(loop_owner(&context, module)).regions()[0])
                .block_ids()[0],
        );
        function_body.insert(0, condition.id());
        function_body.insert(1, conditional.id());

        let result = context.grow_port(conditional.id(), i32_ty, None, |_, carried| {
            assert!(carried.is_none(), "a conditional carries nothing in");
            Some(constant)
        });

        let grown = context.get_op(conditional.id());
        assert_eq!(grown.results().as_slice(), vec![result]);
        assert_eq!(
            grown.operands().as_slice(),
            vec![condition.result()],
            "a conditional carries nothing in"
        );
        for arm in grown.regions() {
            let block = single_block(&context, arm);
            assert!(block.arguments().is_empty(), "an arm takes no argument");
            assert_eq!(
                context
                    .get_op(*block.op_ids().last().unwrap())
                    .operands()
                    .as_slice(),
                vec![constant],
                "every arm yields the port's value"
            );
        }
        crate::verify_op_tree(&context, module).expect("the grown conditional verifies");
    }

    fn loop_owner(context: &Context, module: OpId) -> OpId {
        context
            .get_region(context.get_op(module).regions()[0])
            .iter(context.clone())
            .next()
            .expect("module body")
            .op_ids()[0]
    }

    fn single_block(context: &Context, region: RegionId) -> crate::BlockHandle {
        context.get_block(context.get_region(region).block_ids()[0])
    }
}

#[cfg(test)]
mod tests {
    use super::Context;
    use crate::{BlockHandle, Commutative, OpId, Operand, Operation, Terminator, builtin};

    #[test]
    fn default_context() {
        let _ = Context::with_default_dialects();
    }

    #[test]
    fn an_attribute_name_resolves_back_to_its_spelling() {
        let context = Context::with_default_dialects();

        let attribute = context.named_attribute("size", crate::attributes::AttributeValue::UInt(4));

        assert_eq!(context.resolve(attribute.name), "size");
        assert_eq!(context.sym("size"), Some(attribute.name));
    }

    /// A name no one has used is not an id, so a lookup answers "absent" instead
    /// of minting one.
    #[test]
    fn an_unused_name_has_no_id() {
        let context = Context::with_default_dialects();

        assert_eq!(context.sym("no_op_declares_this"), None);
    }

    /// Registered ops' attribute names are interned before any IR exists, so they
    /// hold the low ids and a lookup never has to intern on a read path.
    #[test]
    fn schema_attribute_names_are_interned_up_front() {
        let context = Context::with_default_dialects();

        let value = context
            .sym("value")
            .expect("builtin.constant declares 'value'");

        assert!(context.sym("sym_name").is_some());
        assert!(value.index() < crate::schema::OP_SCHEMAS.len());
    }

    /// Ids are per-context: two contexts assign them independently, and the same
    /// spelling reaches the same attribute in each.
    #[test]
    fn ids_are_local_to_one_context() {
        let first = Context::with_default_dialects();
        let second = Context::with_default_dialects();

        let only_in_first = first.intern("a_name_only_the_first_context_sees");

        assert_eq!(
            first.resolve(only_in_first),
            "a_name_only_the_first_context_sees"
        );
        assert_eq!(second.sym("a_name_only_the_first_context_sees"), None);
        assert_eq!(first.sym("value"), second.sym("value"));
    }

    /// `module { func demo { ^entry: } }` — the func body sits two regions deep,
    /// so an edit there must reach the module to prove root-ward propagation.
    fn module_with_function(context: &Context) -> (OpId, OpId, BlockHandle) {
        let i32 = builtin::IntegerType::new(context, 32);
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        let func = builtin::ops::func(context, "demo", i32, Some(region.id())).build();
        let module = builtin::ops::module(context, None).build();
        module.body().append(func.id());
        (module.id(), func.id(), context.get_block(block.id()))
    }

    /// Runs `edit` and asserts it bumped the versions of both enclosing ops.
    fn assert_bumps_spine(context: &Context, module: OpId, func: OpId, edit: impl FnOnce()) {
        let module_before = context.op_version(module);
        let func_before = context.op_version(func);
        edit();
        assert!(
            context.op_version(func) > func_before,
            "the edited op's owner must be dirtied"
        );
        assert!(
            context.op_version(module) > module_before,
            "the bump must propagate root-ward"
        );
    }

    #[test]
    fn removing_a_block_argument_drops_it() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let i32 = builtin::IntegerType::new(&context, 32);
        let first = context.append_block_argument(body.id(), i32);
        let second = context.append_block_argument(body.id(), i32);

        assert_bumps_spine(&context, module, func, || {
            context.remove_block_argument(body.id(), 0);
        });

        let block = context.get_block(body.id());
        assert_eq!(block.arguments().len(), 1);
        assert_eq!(block.arguments()[0].id(), second.id());
        assert!(!context.is_block_argument(first.id()));
    }

    #[test]
    fn adopting_a_value_makes_the_block_define_it() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let i32 = builtin::IntegerType::new(&context, 32);
        let input = context.create_value(i32, None);
        let produced = builtin::ops::addi(&context, input.id(), input.id(), i32).build();
        let result = context.get_op(produced.id()).results()[0];
        let reader = builtin::ops::addi(&context, result, result, i32).build();
        body.append(reader.id());

        assert_bumps_spine(&context, module, func, || {
            context.adopt_block_argument(body.id(), result);
        });

        let block = context.get_block(body.id());
        assert_eq!(block.arguments().len(), 1);
        assert_eq!(block.arguments()[0].id(), result);
        assert!(context.is_block_argument(result));
        assert_eq!(context.get_value(result).defining_op(), None);
        assert_eq!(
            context.get_op(reader.id()).operands().as_slice(),
            vec![result; 2]
        );
    }

    #[test]
    fn appending_an_op_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        assert_bumps_spine(&context, module, func, || {
            body.append(
                builtin::ops::r#return(&context, Operand::none())
                    .build()
                    .id(),
            );
        });
    }

    #[test]
    fn inserting_an_op_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        assert_bumps_spine(&context, module, func, || {
            body.insert(
                0,
                builtin::ops::r#return(&context, Operand::none())
                    .build()
                    .id(),
            );
        });
    }

    #[test]
    fn removing_an_op_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let ret = builtin::ops::r#return(&context, Operand::none()).build();
        body.append(ret.id());
        assert_bumps_spine(&context, module, func, || {
            assert!(body.remove_op(ret.id()));
        });
    }

    #[test]
    fn replacing_an_op_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let old = builtin::ops::r#return(&context, Operand::none()).build();
        body.append(old.id());
        let new = builtin::ops::r#return(&context, Operand::none()).build();
        assert_bumps_spine(&context, module, func, || {
            assert!(body.replace_op(old.id(), new.id()));
        });
    }

    #[test]
    fn appending_a_block_argument_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let i32 = builtin::IntegerType::new(&context, 32);
        assert_bumps_spine(&context, module, func, || {
            context.append_block_argument(body.id(), i32);
        });
    }

    #[test]
    fn setting_a_block_attribute_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        assert_bumps_spine(&context, module, func, || {
            body.set_attr("fpmath", crate::attributes::AttributeValue::Bool(true));
        });
    }

    #[test]
    fn adding_a_block_to_a_region_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, _) = module_with_function(&context);
        let region = context.get_region(context.get_op(func).regions()[0]);
        let extra = context.create_block(vec![]);
        assert_bumps_spine(&context, module, func, || {
            region.add_block(extra.id());
        });
        assert_bumps_spine(&context, module, func, || {
            assert!(region.remove_block(extra.id()));
        });
    }

    #[test]
    fn setting_op_attributes_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let ret = builtin::ops::r#return(&context, Operand::none()).build();
        body.append(ret.id());
        assert_bumps_spine(&context, module, func, || {
            context.set_op_attributes(ret.id(), vec![]);
        });
    }

    #[test]
    fn setting_an_operand_bumps_the_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let i32 = builtin::IntegerType::new(&context, 32);
        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);
        let add = builtin::ops::addi(&context, a.id(), a.id(), i32).build();
        body.append(add.id());
        assert_bumps_spine(&context, module, func, || {
            context.set_op_operand(add.id(), 1, b.id());
        });
        assert_bumps_spine(&context, module, func, || {
            context.set_op_operands(add.id(), vec![b.id(), b.id()]);
        });
    }

    #[test]
    fn replacing_value_uses_bumps_the_users_spine() {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let i32 = builtin::IntegerType::new(&context, 32);
        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);
        let add = builtin::ops::addi(&context, a.id(), a.id(), i32).build();
        body.append(add.id());
        assert_bumps_spine(&context, module, func, || {
            context.replace_value_uses(a.id(), b.id());
        });
    }

    #[test]
    fn an_edit_dirties_only_its_own_subtree() {
        let context = Context::with_default_dialects();
        let (_, func, body) = module_with_function(&context);
        let (_, untouched, _) = module_with_function(&context);
        context.take_dirty_ops();

        body.append(
            builtin::ops::r#return(&context, Operand::none())
                .build()
                .id(),
        );

        let dirty = context.take_dirty_ops();
        assert_eq!(dirty, vec![func], "only the edited subtree is verified");
        assert!(!dirty.contains(&untouched));
        assert!(
            context.take_dirty_ops().is_empty(),
            "draining leaves nothing behind"
        );
    }

    #[test]
    fn an_untouched_function_keeps_its_version() {
        let context = Context::with_default_dialects();
        let (_, edited, body) = module_with_function(&context);
        let (_, untouched, _) = module_with_function(&context);
        let before = context.op_version(untouched);

        body.append(
            builtin::ops::r#return(&context, Operand::none())
                .build()
                .id(),
        );

        assert!(context.op_version(edited) > 0);
        assert_eq!(context.op_version(untouched), before);
    }

    #[test]
    fn parent_block_tracks_membership() {
        let context = Context::with_default_dialects();
        let i32 = builtin::IntegerType::new(&context, 32);
        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);

        let block = context.create_block(vec![]);
        let add = block.append_op(builtin::ops::addi(&context, a.id(), b.id(), i32).build());

        // Inserting into a block records the parent, reachable from just the op.
        assert_eq!(context.parent_block(add.id()), Some(block.id()));
        assert_eq!(context.get_op(add.id()).parent_block(), Some(block.id()));

        // Replacing swaps the parent over to the new op; the old op is detached.
        let sub = builtin::ops::subi(&context, a.id(), b.id(), i32).build();
        assert!(block.replace_op(add.id(), sub.id()));
        assert_eq!(context.parent_block(add.id()), None);
        assert_eq!(context.parent_block(sub.id()), Some(block.id()));

        // Removing clears it.
        assert!(block.remove_op(sub.id()));
        assert_eq!(context.parent_block(sub.id()), None);
    }

    #[test]
    fn parent_region_tracks_membership() {
        let context = Context::with_default_dialects();
        let region = context.create_region();
        let block = context.create_block(vec![]);

        region.add_block(block.id());
        assert_eq!(context.parent_region(block.id()), Some(region.id()));

        assert!(region.remove_block(block.id()));
        assert_eq!(context.parent_region(block.id()), None);
    }

    #[test]
    fn replacing_value_uses_reaches_a_nested_region() {
        let context = Context::with_default_dialects();
        let (_, _, body) = module_with_function(&context);
        let i32 = builtin::IntegerType::new(&context, 32);
        let i1 = builtin::IntegerType::new(&context, 1);
        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);
        let cond = context.create_value(i1, None);

        let then_region = context.create_region();
        let then_block = context.create_block(vec![]);
        then_region.add_block(then_block.id());
        let nested = builtin::ops::addi(&context, a.id(), a.id(), i32).build();
        then_block.append(nested.id());
        then_block.append(crate::scf::ops::r#yield(&context, vec![]).build().id());
        let else_region = context.create_region();
        let else_block = context.create_block(vec![]);
        else_region.add_block(else_block.id());
        else_block.append(crate::scf::ops::r#yield(&context, vec![]).build().id());
        body.append(
            crate::scf::ops::r#if(
                &context,
                cond.id(),
                vec![],
                vec![],
                Some(then_region.id()),
                Some(else_region.id()),
            )
            .build()
            .id(),
        );

        context.replace_value_uses(a.id(), b.id());

        assert_eq!(
            context.get_op(nested.id()).operands().as_slice(),
            vec![b.id(); 2]
        );
    }

    #[test]
    fn replacing_uses_of_a_block_argument_rewrites_its_readers() {
        let context = Context::with_default_dialects();
        let context_ref = &context;
        let i32 = builtin::IntegerType::new(context_ref, 32);
        let region = context.create_region();
        let argument = context.create_value(i32, None);
        let block = context.create_block(vec![argument.clone()]);
        region.add_block(block.id());
        let func = builtin::ops::func(&context, "demo", i32, Some(region.id())).build();
        let module = builtin::ops::module(&context, None).build();
        module.body().append(func.id());
        let reader = builtin::ops::addi(&context, argument.id(), argument.id(), i32).build();
        block.append(reader.id());
        let replacement = context.create_value(i32, None);

        context.replace_value_uses(argument.id(), replacement.id());

        assert_eq!(
            context.get_op(reader.id()).operands().as_slice(),
            vec![replacement.id(); 2]
        );
    }

    #[test]
    fn custom_interface_for_existing_op() {
        let context = Context::with_default_dialects();

        let lhs = context.create_value(builtin::IntegerType::new(&context, 32), None);
        let rhs = context.create_value(builtin::IntegerType::new(&context, 32), None);
        let add = builtin::ops::addi(
            &context,
            lhs.id(),
            rhs.id(),
            builtin::IntegerType::new(&context, 32),
        )
        .build();

        assert!(context.get_op(add.id()).has_interface::<dyn Commutative>());
        let iface = context
            .get_op(add.id())
            .as_interface::<dyn Commutative>()
            .expect("interface should be available");
        assert!(iface.is_commutative());
    }

    #[test]
    fn builtin_terminator_interface() {
        let context = Context::with_default_dialects();
        let value = context.create_value(builtin::IntegerType::new(&context, 32), None);
        let ret = builtin::ops::r#return(&context, value.id()).build();

        let iface = context
            .get_op(ret.id())
            .as_interface::<dyn Terminator>()
            .expect("terminator interface should be available");
        assert!(iface.is_terminator());
    }
}
