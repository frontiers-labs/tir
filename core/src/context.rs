use std::{
    any::Any,
    collections::HashMap,
    hash::{DefaultHasher, Hasher},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use std::cell::{Ref, RefCell, RefMut};
use tir_adt::{Interner, Sym};

use crate::overlay::{Delta, EditBatch, Frozen, commit_epoch};
use crate::run::AttrRunId;
use crate::store::{ERASED, Store};

pub use crate::store::Parent;

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

/// The overlay every reader and editor of the IR holds.
///
/// A context is a frozen base plus the edits made since it was frozen. Every
/// read resolves those edits first, so an edit is visible to the next read;
/// every edit lands in the overlay, so nothing touches the base until
/// [`Context::commit`] takes it over with no reader left. Ids are minted once
/// and never move, so an id read before a commit names the same entity after.
///
/// The context also owns what is not IR: the dialects, interned names and
/// types, and interface registrations.
///
/// A context belongs to one thread: it is neither `Send` nor `Sync`, and its
/// reads and edits take no lock. The committed base crosses threads as a
/// [`Frozen`], which is an immutable, shared read of the IR.
///
/// # Example
///
/// ```rust
/// let context = tir::Context::with_default_dialects();
/// ```
#[derive(Clone)]
pub struct Context(std::rc::Rc<Inner>);

pub struct ContextIterator<I: GetFromContext> {
    context: Context,
    elements: Vec<I>,
    current_front: usize,
}

pub trait GetFromContext {
    type Item;

    fn get_from_context(&self, context: &Context) -> Self::Item;
}

/// Entities an erase reclaims, gathered before the overlay is borrowed for
/// writing.
#[derive(Default)]
struct Owned {
    ops: Vec<OpId>,
    values: Vec<ValueId>,
    blocks: Vec<BlockId>,
    regions: Vec<RegionId>,
}

/// What a context holds beside the IR.
struct Registry {
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
    /// attribute name instead of a heap `String` per instance.
    names: Interner,
    /// The `(dialect, name)` pairs ops are identified by. Ids are dense and
    /// handed out in construction order.
    op_names: Vec<(&'static str, &'static str)>,
    op_name_ids: HashMap<(&'static str, &'static str), OpNameId>,
    segment_sizes: Sym,
}

/// The base and the edits over it. One thread owns a context, so the
/// edits need no lock; the base is shared read-only and needs none.
struct Overlay {
    base: Frozen,
    delta: Delta,
}

impl Overlay {
    fn base(&self) -> &Store {
        &self.base.0
    }

    fn parts(&mut self) -> (&Store, &mut Delta) {
        (&self.base.0, &mut self.delta)
    }

    fn op(&self, id: OpId) -> Option<&OpInstance> {
        self.delta.op(&self.base.0, id)
    }

    fn value(&self, id: ValueId) -> Option<&Value> {
        self.delta.value(&self.base.0, id)
    }

    fn block(&self, id: BlockId) -> Option<&Block> {
        self.delta.block(&self.base.0, id)
    }

    fn region(&self, id: RegionId) -> Option<&Region> {
        self.delta.region(&self.base.0, id)
    }

    fn op_parent(&self, id: OpId) -> Option<Parent> {
        self.delta.op_parent(&self.base.0, id)
    }

    fn block_parent(&self, id: BlockId) -> Option<RegionId> {
        self.delta.block_parent(&self.base.0, id)
    }

    fn value_block(&self, id: ValueId) -> Option<BlockId> {
        self.delta.value_block(&self.base.0, id)
    }

    fn value_region(&self, id: ValueId) -> Option<RegionId> {
        self.delta.value_region(&self.base.0, id)
    }

    fn op_operands(&self, id: OpId) -> crate::operation::ValueIds {
        self.delta.op_operands(&self.base.0, id)
    }

    fn op_results(&self, id: OpId) -> crate::operation::ValueIds {
        self.delta.op_results(&self.base.0, id)
    }

    fn op_regions(&self, id: OpId) -> crate::operation::RegionIds {
        self.delta.op_regions(&self.base.0, id)
    }

    fn op_attrs(&self, id: OpId) -> &[NamedAttribute] {
        self.delta.op_attrs(&self.base.0, id)
    }

    fn uses(&self, value: ValueId) -> Vec<Use> {
        self.delta.uses(&self.base.0, value)
    }

    fn is_used(&self, value: ValueId) -> bool {
        self.delta.is_used(&self.base.0, value)
    }

    fn enclosing_op_of(&self, op: OpId) -> Option<OpId> {
        self.delta.enclosing_op_of(&self.base.0, op)
    }
}

struct Inner {
    registry: RefCell<Registry>,
    overlay: RefCell<Overlay>,
    /// Structural version per op as of the last commit; the overlay's
    /// revisions add to it. See [`Context::op_version`].
    versions: RefCell<Vec<u32>>,
    /// Which overlay this is: bumped by every commit and discard, so a handle
    /// to an entity of a dropped overlay reads as stale.
    epoch: AtomicU32,
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
        let mut names = schema_vocabulary();
        let segment_sizes = names.intern("operand_segment_sizes");
        let base = Frozen::empty();
        let delta = Delta::new(base.0.frontier());
        let context = Context(std::rc::Rc::new(Inner {
            registry: RefCell::new(Registry {
                dialects: HashMap::new(),
                reg_classes: HashMap::new(),
                op_interface_converters: HashMap::new(),
                type_cache: vec![],
                type_lookup: HashMap::new(),
                names,
                op_names: Vec::new(),
                op_name_ids: HashMap::new(),
                segment_sizes,
            }),
            overlay: RefCell::new(Overlay { base, delta }),
            versions: RefCell::new(Vec::new()),
            epoch: AtomicU32::new(0),
        }));
        crate::builtin::StateType::new(&context);
        context
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

    fn registry(&self) -> Ref<'_, Registry> {
        self.0.registry.borrow()
    }

    fn registry_mut(&self) -> RefMut<'_, Registry> {
        self.0.registry.borrow_mut()
    }

    fn view(&self) -> Ref<'_, Overlay> {
        self.0.overlay.borrow()
    }

    fn view_mut(&self) -> RefMut<'_, Overlay> {
        self.0.overlay.borrow_mut()
    }

    /// Intern an attribute name.
    pub fn intern(&self, name: &str) -> Sym {
        self.registry_mut().names.intern(name)
    }

    /// The symbol `name` was interned as, if it ever was.
    pub fn sym(&self, name: &str) -> Option<Sym> {
        self.registry().names.lookup(name)
    }

    /// The name `sym` was interned from.
    pub fn resolve(&self, sym: Sym) -> String {
        self.registry().names.resolve(sym).to_string()
    }

    /// The dense id of an op identity, minted on first sight.
    pub(crate) fn intern_op_name(&self, dialect: &'static str, name: &'static str) -> OpNameId {
        let mut registry = self.registry_mut();
        if let Some(id) = registry.op_name_ids.get(&(dialect, name)) {
            return *id;
        }
        let id = OpNameId::new(registry.op_names.len() as u32);
        registry.op_names.push((dialect, name));
        registry.op_name_ids.insert((dialect, name), id);
        id
    }

    pub fn named_attribute(&self, name: &str, value: AttributeValue) -> NamedAttribute {
        NamedAttribute::new(self.intern(name), value)
    }

    /// Storage counts of the base and the overlay, for the memory census.
    pub fn slab_census(&self) -> crate::memstats::SlabCensus {
        let view = self.view();
        let base = view.base();
        let (blocks_heap, regions_heap) = base.owned_heap_bytes();
        let runs = base.runs.census();
        let attrs = base.attr_runs.census();
        crate::memstats::SlabCensus {
            ops_slab: base.ops.capacity(),
            ops_live: base.ops.len(),
            values_slab: base.values.capacity(),
            values_live: base.values.len(),
            blocks_slab: base.blocks.capacity(),
            blocks_live: base.blocks.len(),
            regions_slab: base.regions.capacity(),
            regions_live: base.regions.len(),
            runs_live: runs.0,
            runs_bytes: runs.1,
            attrs_live: attrs.0,
            attrs_bytes: attrs.1,
            ops_chunks: base.ops.chunk_count(),
            values_chunks: base.values.chunk_count(),
            blocks_chunks: base.blocks.chunk_count(),
            regions_chunks: base.regions.chunk_count(),
            ops_bytes: base.ops.bytes(),
            values_bytes: base.values.bytes(),
            blocks_bytes: base.blocks.bytes() + blocks_heap,
            regions_bytes: base.regions.bytes() + regions_heap,
            slab_bytes: base.bytes(),
            overlay: view.delta.census(),
        }
    }

    /// The overlay's own counts: what it created, copied and replaced.
    pub fn overlay_census(&self) -> crate::overlay::OverlayCensus {
        self.view().delta.census()
    }

    // Epochs.

    /// Which overlay this is; advances at every commit.
    pub fn epoch(&self) -> u32 {
        self.0.epoch.load(Ordering::Relaxed)
    }

    /// One more reader of the base as it stands. A commit waits for none: it
    /// panics while a reader is alive, so scope readers to what they read.
    pub fn frozen(&self) -> Frozen {
        self.view().base.clone()
    }

    /// Whether `id` names an entity created in the current overlay, and not
    /// yet committed.
    pub fn is_pending_op(&self, id: OpId) -> bool {
        self.view().delta.store.owns_op(id)
    }

    pub fn is_pending_value(&self, id: ValueId) -> bool {
        self.view().delta.store.owns_value(id)
    }

    /// Whether the overlay holds any edit.
    pub fn has_pending_edits(&self) -> bool {
        !self.view().delta.is_empty()
    }

    /// Take the overlay's edits as an owned batch, leaving an empty overlay
    /// over the same base. The batch names no handle of this context. Ids
    /// created afterwards would repeat the batch's, so a commit follows.
    fn finish(&self) -> EditBatch {
        let mut view = self.view_mut();
        let frontier = view.base().frontier();
        let delta = std::mem::replace(&mut view.delta, Delta::new(frontier));
        EditBatch::new(self.epoch(), view.base.identity(), delta)
    }

    /// Apply every edit made since the last commit to the base. No reader of
    /// the base ([`Context::frozen`]) may be alive. Ids do not move.
    pub fn commit(&self) {
        let mut view = self.view_mut();
        assert_eq!(
            Arc::strong_count(&view.base.0),
            1,
            "commit while a reader still holds the base"
        );
        let frontier = view.base().frontier();
        let delta = std::mem::replace(&mut view.delta, Delta::new(frontier));
        let batch = EditBatch::new(self.epoch(), view.base.identity(), delta);
        let base = std::mem::replace(&mut view.base, Frozen::empty());
        let (base, revisions) = commit_epoch(base, vec![batch]);
        view.delta = Delta::new(base.0.frontier());
        view.base = base;
        drop(view);
        self.fold_revisions(&revisions);
        self.0.epoch.store(self.epoch() + 1, Ordering::Relaxed);
    }

    /// Drop every edit made since the last commit. Every version the overlay
    /// advanced moves past what it reached, so nothing cached against the
    /// dropped state is reused.
    pub fn discard(&self) {
        let batch = self.finish();
        let past: Vec<u32> = batch
            .revision()
            .iter()
            .map(|bump| if *bump == 0 { 0 } else { bump + 1 })
            .collect();
        self.fold_revisions(&[past]);
        self.0.epoch.store(self.epoch() + 1, Ordering::Relaxed);
    }

    fn fold_revisions(&self, revisions: &[Vec<u32>]) {
        let mut versions = self.0.versions.borrow_mut();
        for revision in revisions {
            if revision.len() > versions.len() {
                versions.resize(revision.len(), 0);
            }
            for (version, bump) in versions.iter_mut().zip(revision) {
                *version = version
                    .checked_add(*bump)
                    .expect("a version counter wrapped");
            }
        }
    }

    /// Record that `old` was replaced by `new` in place, so a pipeline can
    /// follow its root across the swap.
    pub(crate) fn record_replaced_op(&self, old: OpId, new: OpId) {
        self.view_mut().delta.replaced_ops.insert(old, new);
    }

    /// The op that took `id`'s place in the current overlay, if a rewrite
    /// replaced it.
    pub fn replaced_op(&self, id: OpId) -> Option<OpId> {
        self.view().delta.replaced_ops.get(&id).copied()
    }

    // Dialects, interfaces and types.

    /// Register a dialect with context.
    pub fn register_dialect<D: Dialect>(&self) {
        let mut dialect = D::new();
        Arc::<dyn Dialect>::get_mut(&mut dialect)
            .unwrap()
            .register_operations(self);
        Arc::<dyn Dialect>::get_mut(&mut dialect)
            .unwrap()
            .register_types(self);
        self.registry_mut().dialects.insert(D::name(), dialect);
    }

    /// Register a target's register classes so the attribute parser can resolve a
    /// `CLASS[n]` register's class name back to its [`RegClassId`]. Backends call
    /// this from `register_dialects` with their generated `register_info().classes`.
    pub fn register_reg_classes(&self, classes: &'static [crate::backend::regalloc::RegClassInfo]) {
        let mut registry = self.registry_mut();
        for class in classes {
            registry
                .reg_classes
                .insert(class.name, crate::backend::regalloc::RegClassId::new(class));
        }
    }

    /// Resolve a register-class name to its [`RegClassId`], if a target that defines
    /// it has been registered (see [`Context::register_reg_classes`]).
    pub fn resolve_reg_class(&self, name: &str) -> Option<crate::backend::regalloc::RegClassId> {
        self.registry().reg_classes.get(name).copied()
    }

    pub fn find_dialect<D: Dialect>(&self) -> Option<Arc<D>> {
        self.registry()
            .dialects
            .get(D::name())
            .cloned()
            .and_then(|d| {
                let d: Arc<dyn Any + Send + Sync> = d;
                d.downcast::<D>().ok()
            })
    }

    pub fn register_op_interface<I: ?Sized + 'static>(
        &self,
        dialect: &'static str,
        op_name: &'static str,
        converter: OpInterfaceConverter,
    ) {
        self.registry_mut()
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
        let dialect_name = self.op_identity(op.id).0;
        let dialect = self.registry().dialects.get(dialect_name).unwrap().clone();
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
        self.registry()
            .op_interface_converters
            .get(&(identity.0, identity.1, std::any::TypeId::of::<I>()))
            .copied()
    }

    pub fn get_parser(&self, dialect: &str, name: &str) -> Result<OperationParser, Error> {
        let registry = self.registry();
        let dialect = registry
            .dialects
            .get(dialect)
            .ok_or(Error::UnknownDialect(dialect.to_string()))?;
        dialect.get_parser(name)
    }

    pub fn get_type_parser(&self, dialect: &str, name: &str) -> Result<TypeParser, Error> {
        let registry = self.registry();
        let dialect_impl = registry
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
        let mut registry = self.registry_mut();
        if let Some(candidates) = registry.type_lookup.get(&hash) {
            for &id in candidates {
                if registry.type_cache[id.as_index()].eq(&*ty) {
                    return id;
                }
            }
        }
        let id = TypeId::from_number(registry.type_cache.len() as u32);
        registry.type_cache.push(ty);
        registry.type_lookup.entry(hash).or_default().push(id);
        id
    }

    pub fn get_type_data(&self, ty: TypeId) -> Arc<dyn Type> {
        self.registry()
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

    // Handles.

    /// The epoch `id` was created in, or [`ERASED`] for an id no live op has.
    /// A handle records this when it is minted and compares on every read.
    pub(crate) fn op_generation(&self, id: OpId) -> u32 {
        let view = self.view();
        if view.delta.store.owns_op(id) {
            return if view.delta.store.op(id).is_some() {
                self.epoch()
            } else {
                ERASED
            };
        }
        if view.delta.erased_op(id) {
            return ERASED;
        }
        view.base().op_epoch(id)
    }

    pub(crate) fn block_generation(&self, id: BlockId) -> u32 {
        let view = self.view();
        if view.delta.store.owns_block(id) {
            return if view.delta.store.block(id).is_some() {
                self.epoch()
            } else {
                ERASED
            };
        }
        if view.delta.erased_block(id) {
            return ERASED;
        }
        view.base().block_epoch(id)
    }

    pub(crate) fn region_generation(&self, id: RegionId) -> u32 {
        let view = self.view();
        if view.delta.store.owns_region(id) {
            return if view.delta.store.region(id).is_some() {
                self.epoch()
            } else {
                ERASED
            };
        }
        if view.delta.erased_region(id) {
            return ERASED;
        }
        view.base().region_epoch(id)
    }

    /// The handle naming `id`. Panics for an id no live operation has: a handle
    /// reads the operation as it stands, and an erased one does not stand.
    pub fn get_op(&self, id: OpId) -> OpHandle {
        let generation = self.op_generation(id);
        assert!(generation != ERASED, "live operation {id:?}");
        OpHandle {
            context: self.clone(),
            id,
            generation,
        }
    }

    /// The handle naming `id`. Panics for an id no live block has.
    pub fn get_block(&self, id: BlockId) -> BlockHandle {
        let generation = self.block_generation(id);
        assert!(generation != ERASED, "live block {id:?}");
        BlockHandle {
            context: self.clone(),
            generation,
            id,
        }
    }

    /// The handle naming `id`; see [`Context::get_block`].
    pub fn get_region(&self, id: RegionId) -> RegionHandle {
        let generation = self.region_generation(id);
        assert!(generation != ERASED, "live region {id:?}");
        RegionHandle {
            context: self.clone(),
            generation,
            id,
        }
    }

    fn find_op(&self, id: OpId) -> Option<OpHandle> {
        (self.op_generation(id) != ERASED).then(|| self.get_op(id))
    }

    fn find_block(&self, id: BlockId) -> Option<BlockHandle> {
        (self.block_generation(id) != ERASED).then(|| self.get_block(id))
    }

    fn find_region(&self, id: RegionId) -> Option<RegionHandle> {
        (self.region_generation(id) != ERASED).then(|| self.get_region(id))
    }

    // Reads.

    pub fn has_operation(&self, id: OpId) -> bool {
        self.view().op(id).is_some()
    }

    pub fn has_value(&self, id: ValueId) -> bool {
        self.view().value(id).is_some()
    }

    pub fn has_region(&self, id: RegionId) -> bool {
        self.view().region(id).is_some()
    }

    pub fn has_block(&self, id: BlockId) -> bool {
        self.view().block(id).is_some()
    }

    /// The structural version of `op`: a counter bumped by every edit to `op` or
    /// to anything under it, continuous across commits. Analyses cached against
    /// a version are stale as soon as it moves; see
    /// [`crate::analysis::AnalysisManager`].
    pub fn op_version(&self, op: OpId) -> u32 {
        let committed = self
            .0
            .versions
            .borrow()
            .get(op.index())
            .copied()
            .unwrap_or(0);
        committed
            .checked_add(self.view().delta.revision(op))
            .expect("a version counter wrapped")
    }

    /// The subtrees edited since the last call, innermost-dirtied op per edit and
    /// deduplicated. The pass manager drains this to scope post-pass verification.
    pub(crate) fn take_dirty_ops(&self) -> Vec<OpId> {
        let dirty = self.view_mut().delta.take_dirty();
        dirty
            .into_iter()
            .filter(|op| self.has_operation(*op))
            .collect()
    }

    pub fn get_value(&self, id: ValueId) -> Value {
        self.view().value(id).expect("live value").clone()
    }

    /// The values of `ids` that are not memory states, in order.
    pub fn values_among(&self, ids: &[ValueId]) -> crate::operation::ValueIds {
        self.filter_states(ids, false)
    }

    /// The values of `ids` that are memory states, in order.
    pub fn states_among(&self, ids: &[ValueId]) -> crate::operation::ValueIds {
        self.filter_states(ids, true)
    }

    fn filter_states(&self, ids: &[ValueId], states: bool) -> crate::operation::ValueIds {
        let view = self.view();
        let (base, delta) = (view.base(), &view.delta);
        ids.iter()
            .copied()
            .filter(|&id| delta.value(base, id).is_some_and(Value::is_state) == states)
            .collect()
    }

    /// Every operand slot holding `value`, in the order the uses were recorded.
    ///
    /// This is the def-use chain: it lists what live operation storage holds,
    /// whether or not the reading op sits in the tree. Attributes naming a
    /// value are not uses: they record where the ABI places it, not a read.
    pub fn uses_of(&self, value: ValueId) -> Vec<Use> {
        self.view().uses(value)
    }

    /// The operations reading `value`, one entry per operand slot.
    pub fn users_of(&self, value: ValueId) -> Vec<OpId> {
        self.uses_of(value)
            .into_iter()
            .map(|r#use| r#use.op)
            .collect()
    }

    pub fn is_used(&self, value: ValueId) -> bool {
        self.view().is_used(value)
    }

    pub fn use_count(&self, value: ValueId) -> usize {
        self.uses_of(value).len()
    }

    /// Rebuild the use lists from live operation storage and compare. A
    /// mismatch means an operand mutator skipped its bookkeeping, which every
    /// def-use query would then answer wrongly; the pass manager runs this
    /// after each mutating pass when IR verification is on.
    pub fn verify_use_lists(&self) -> Result<(), Error> {
        let view = self.view();
        let (base, delta) = (view.base(), &view.delta);
        let mut expected: HashMap<ValueId, Vec<Use>> = HashMap::new();
        let base_ops = base
            .ops
            .handles()
            .map(OpId::new)
            .filter(|op| !delta.store.shadows_op(*op) && !delta.erased_op(*op));
        let delta_ops = delta
            .store
            .ops
            .handles()
            .filter_map(|handle| delta.store.ops.get(handle))
            .map(|instance| instance.id);
        for op in base_ops.chain(delta_ops) {
            for (slot, value) in delta.op_operands(base, op).iter().enumerate() {
                expected.entry(*value).or_default().push(Use::new(op, slot));
            }
        }
        let key = |r#use: &Use| (r#use.op.index(), r#use.index);
        for (value, mut expected) in expected {
            let mut held = delta.uses(base, value);
            expected.sort_unstable_by_key(key);
            held.sort_unstable_by_key(key);
            if expected != held {
                return Err(Error::VerificationError(format!(
                    "use list of value {} holds {held:?}, but operands say {expected:?}",
                    value.number()
                )));
            }
        }
        Ok(())
    }

    pub fn is_block_argument(&self, id: ValueId) -> bool {
        self.block_of_argument(id).is_some()
    }

    /// Whether `id` is an argument a region owns itself, rather than one its
    /// entry block owns or a value some operation defines.
    pub fn is_region_port(&self, id: ValueId) -> bool {
        self.region_of_port(id).is_some()
    }

    /// The region `id` is a port of, or `None` when it is a block argument or
    /// an operation defines it.
    pub fn region_of_port(&self, id: ValueId) -> Option<RegionId> {
        self.view().value_region(id)
    }

    /// The block `id` is an argument of, or `None` when an operation defines it.
    pub fn block_of_argument(&self, id: ValueId) -> Option<BlockId> {
        self.view().value_block(id)
    }

    /// The block currently holding `op`, or `None` for an op not in any block
    /// (the root op, or one detached by a rewrite).
    pub fn parent_block(&self, op: OpId) -> Option<BlockId> {
        match self.view().op_parent(op) {
            Some(Parent::Block(block)) => Some(block),
            _ => None,
        }
    }

    /// The unordered region holding `op` directly, or `None` for an op that
    /// sits in a block or in no region at all.
    pub fn parent_nodes_region(&self, op: OpId) -> Option<RegionId> {
        match self.view().op_parent(op) {
            Some(Parent::Region(region)) => Some(region),
            _ => None,
        }
    }

    /// The region holding `op`, through its block where it has one.
    pub fn region_of_op(&self, op: OpId) -> Option<RegionId> {
        match self.view().op_parent(op)? {
            Parent::Region(region) => Some(region),
            Parent::Block(block) => self.parent_region(block),
        }
    }

    /// The operation enclosing `op`: the owner of the region holding `op`'s
    /// block. `None` for a root op or one detached by a rewrite.
    pub fn parent_op(&self, op: OpId) -> Option<OpId> {
        self.view().enclosing_op_of(op)
    }

    /// The region currently holding `block`, or `None` for a detached block.
    pub fn parent_region(&self, block: BlockId) -> Option<RegionId> {
        self.view().block_parent(block)
    }

    /// Read an attribute of `op` in place. For an attribute large enough that
    /// cloning it per lookup would matter.
    ///
    /// `read` must not edit the context: the overlay is borrowed for the read.
    pub fn with_attr<R>(
        &self,
        id: OpId,
        name: &str,
        read: impl FnOnce(&AttributeValue) -> R,
    ) -> Option<R> {
        let name = self.sym(name)?;
        let view = self.view();
        let (base, delta) = (view.base(), &view.delta);
        delta
            .op_attrs(base, id)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| read(&attribute.value))
    }

    pub(crate) fn op_operands(&self, id: OpId) -> crate::operation::ValueIds {
        self.view().op_operands(id)
    }

    pub(crate) fn op_results(&self, id: OpId) -> crate::operation::ValueIds {
        self.view().op_results(id)
    }

    pub(crate) fn op_regions(&self, id: OpId) -> crate::operation::RegionIds {
        self.view().op_regions(id)
    }

    pub(crate) fn op_attributes(&self, id: OpId) -> Vec<NamedAttribute> {
        self.view().op_attrs(id).to_vec()
    }

    /// [`OpHandle::attr_sym`]: the lookup is a `u32` compare per attribute.
    pub(crate) fn op_attr_sym(&self, id: OpId, name: Sym) -> Option<AttributeValue> {
        self.view()
            .op_attrs(id)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.clone())
    }

    /// The `(dialect, name)` pair `id` is spelled by.
    pub(crate) fn op_identity(&self, id: OpId) -> (&'static str, &'static str) {
        let name = self.view().op(id).expect("live operation").name_id();
        self.registry().op_names[name.index()]
    }

    /// Read a block's storage record.
    ///
    /// `read` must not edit the context: the overlay is borrowed for the read.
    pub(crate) fn with_block<R>(&self, id: BlockId, read: impl FnOnce(&Block) -> R) -> R {
        let view = self.view();
        let (base, delta) = (view.base(), &view.delta);
        read(delta.block(base, id).expect("live block"))
    }

    /// [`Context::with_block`] for a region.
    pub(crate) fn with_region<R>(&self, id: RegionId, read: impl FnOnce(&Region) -> R) -> R {
        let view = self.view();
        let (base, delta) = (view.base(), &view.delta);
        read(delta.region(base, id).expect("live region"))
    }

    /// [`BlockHandle::attr`].
    pub(crate) fn block_attr(&self, block: BlockId, name: &str) -> Option<AttributeValue> {
        let name = self.sym(name)?;
        self.with_block(block, |block| {
            block
                .attributes()
                .iter()
                .find(|attribute| attribute.name == name)
                .map(|attribute| attribute.value.clone())
        })
    }

    // Edits. Each borrows the base for reading and the overlay for writing,
    // copies whatever base record it touches into the overlay first, and
    // reports the edit on the spine.

    pub fn add_operation(&self, op: crate::operation::NewOp) -> OpHandle {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        let op_id = delta.store.insert_op(|id| OpInstance {
            id,
            name: op.name,
            run: crate::run::RunId::NONE,
            operand_count: 0,
            result_count: 0,
            region_count: 0,
            _pad: 0,
            attrs: AttrRunId::NONE,
            attr_count: 0,
        });
        let operands: Vec<u32> = op.operands.iter().map(|value| value.number()).collect();
        let results: Vec<u32> = op.results.iter().map(|value| value.number()).collect();
        let regions: Vec<u32> = op.regions.iter().map(|region| region.number()).collect();
        delta.store.set_ports(op_id, &operands, &results, &regions);
        delta.store.set_op_attrs(op_id, op.attributes);
        // Results are created before op id assignment in builders; patch their
        // def-site now.
        for result in op.results {
            if delta.shadow_value(base, result) {
                delta
                    .store
                    .value_mut(result)
                    .expect("live value")
                    .set_defining_op(op_id);
            }
        }
        for region in op.regions {
            assert!(delta.shadow_region(base, region), "live region");
            delta
                .store
                .region_mut(region)
                .expect("live region")
                .set_parent_op(op_id);
        }
        delta.bump(op_id);
        drop(view);
        self.get_op(op_id)
    }

    /// Replace an operation's attributes in place, keeping its id, position, and
    /// regions.
    pub fn set_op_attributes(&self, id: OpId, attributes: Vec<NamedAttribute>) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if delta.shadow_op(base, id) {
            delta.store.set_op_attrs(id, attributes);
            delta.edit_op(base, id);
        }
    }

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

    /// Replace a single operation's SSA operand at `index`.
    pub fn set_op_operand(&self, id: OpId, index: usize, new: ValueId) {
        if self
            .op_operands(id)
            .get(index)
            .is_none_or(|old| *old == new)
        {
            return;
        }
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if delta.shadow_op(base, id) {
            delta.store.replace_operand_at(id, index, new);
            delta.edit_op(base, id);
        }
    }

    /// Replace all of an operation's SSA operands.
    pub fn set_op_operands(&self, id: OpId, operands: Vec<ValueId>) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, id) {
            return;
        }
        let (_, results, regions) = delta.store.ports(id);
        let operands: Vec<u32> = operands.iter().map(|value| value.number()).collect();
        delta.store.set_ports(id, &operands, &results, &regions);
        delta.edit_op(base, id);
    }

    /// Replace a single operation's SSA result at `index`, moving the
    /// definition of `new` onto this op.
    pub fn set_op_result(&self, id: OpId, index: usize, new: ValueId) {
        if self.op_results(id).get(index).is_none_or(|old| *old == new) {
            return;
        }
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, id) {
            return;
        }
        delta.store.replace_result_at(id, index, new);
        if delta.shadow_value(base, new) {
            delta
                .store
                .value_mut(new)
                .expect("live value")
                .set_defining_op(id);
        }
        delta.edit_op(base, id);
    }

    /// Give a value a new type, keeping its id and every use of it.
    pub fn retype_value(&self, value: ValueId, ty: TypeId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_value(base, value) {
            return;
        }
        let record = delta.store.value_mut(value).expect("live value");
        record.set_ty(ty);
        match (record.defining_op(), delta.value_block(base, value)) {
            (Some(op), _) => delta.edit_op(base, op),
            (None, Some(block)) => delta.edit_block(base, block),
            (None, None) => {
                if let Some(region) = delta.value_region(base, value) {
                    delta.edit_region(base, region);
                }
            }
        }
    }

    /// [`Context::retype_value`] for a block argument, whose type the block
    /// stores alongside the value arena's copy.
    pub fn retype_block_argument(&self, block: BlockId, index: usize, ty: TypeId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_block(base, block) {
            return;
        }
        let Some(argument) = delta
            .store
            .block_mut(block)
            .and_then(|block| block.arguments_mut().get_mut(index))
        else {
            return;
        };
        argument.set_ty(ty);
        let value_id = argument.id();
        if delta.shadow_value(base, value_id) {
            delta
                .store
                .value_mut(value_id)
                .expect("live value")
                .set_ty(ty);
        }
        delta.edit_block(base, block);
    }

    pub fn create_value(&self, ty: TypeId, defining_op: Option<OpId>) -> Value {
        let mut view = self.view_mut();
        let delta = &mut view.delta;
        let id = delta
            .store
            .insert_value(|id| Value::new(id, ty, defining_op));
        delta.store.value(id).expect("just inserted").clone()
    }

    /// Mint a memory state: a `!state` value whose only meaning is the
    /// ordering edges that name it.
    pub fn create_state(&self) -> ValueId {
        self.create_value(TypeId::STATE, None).id()
    }

    /// Replace every SSA operand use of `old` with `new`.
    ///
    /// Every reading slot answers `new` from here on, including slots of
    /// operations a rewrite has taken out of the tree or has not put in it
    /// yet. Attributes naming a value are left untouched: they record where
    /// the ABI places a value, not a read of it.
    pub fn replace_value_uses(&self, old: ValueId, new: ValueId) {
        if old == new {
            return;
        }
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        let mut edited = delta.replace_value_uses(base, old, new);
        edited.sort_unstable();
        edited.dedup();
        for op in edited {
            delta.edit_op(base, op);
        }
    }

    pub fn create_region(&self) -> RegionHandle {
        let id = self.view_mut().delta.store.insert_region(Region::new());
        self.get_region(id)
    }

    /// Create an unordered region holding `ops`, taking `ports` as its own
    /// arguments and producing `results`.
    pub fn create_nodes_region(
        &self,
        ports: Vec<Value>,
        ops: Vec<OpId>,
        results: Vec<ValueId>,
    ) -> RegionHandle {
        let region = self.create_region();
        self.set_region_nodes(region.id(), ports, ops, results);
        region
    }

    /// Put `op` into the unordered `region`. Nothing about the position means
    /// anything: the region's dependencies say what runs before what.
    pub fn add(&self, region: RegionId, op: OpId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        match delta.store.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { ops, .. }) => ops.push(op),
            _ => panic!("only an unordered region takes an operation without a position"),
        }
        debug_assert!(
            delta.op_parent(base, op).is_none(),
            "an operation joins an unordered region from nowhere else",
        );
        assert!(delta.shadow_op(base, op), "live op");
        delta.store.set_op_parent(op, Some(Parent::Region(region)));
        delta.edit_region(base, region);
    }

    /// Choose another insertion order for the operations the unordered
    /// `region` already holds; `ops` must be a permutation of them.
    pub fn set_region_ops(&self, region: RegionId, ops: Vec<OpId>) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        match delta.store.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { ops: held, .. }) => {
                debug_assert_eq!(held.len(), ops.len());
                *held = ops;
            }
            _ => panic!("only an unordered region holds an insertion order"),
        }
        delta.edit_region(base, region);
    }

    /// Name the values the unordered `region` produces.
    pub fn set_region_results(&self, region: RegionId, results: Vec<ValueId>) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        match delta.store.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { results: held, .. }) => *held = results,
            _ => panic!("only an unordered region names its results"),
        }
        delta.edit_region(base, region);
    }

    /// Make an empty region unordered; see [`Context::create_nodes_region`].
    pub(crate) fn set_region_nodes(
        &self,
        region: RegionId,
        ports: Vec<Value>,
        ops: Vec<OpId>,
        results: Vec<ValueId>,
    ) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        let port_ids: Vec<ValueId> = ports.iter().map(Value::id).collect();
        let held = ops.clone();
        assert!(
            matches!(
                delta.store.region(region).expect("live region").body(),
                crate::region::RegionBody::Blocks(blocks) if blocks.is_empty(),
            ),
            "only an empty ordered region becomes unordered",
        );
        let entry = delta.store.region_mut(region).expect("live region");
        let parent = entry.parent_op();
        *entry = Region::new_nodes(ports, ops, results);
        if let Some(parent) = parent {
            entry.set_parent_op(parent);
        }
        for port in port_ids {
            assert!(delta.shadow_value(base, port), "live value");
            delta.store.set_value_region(port, Some(region));
        }
        for op in held {
            debug_assert!(
                delta.op_parent(base, op).is_none(),
                "an operation joins an unordered region from nowhere else",
            );
            assert!(delta.shadow_op(base, op), "live op");
            delta.store.set_op_parent(op, Some(Parent::Region(region)));
        }
    }

    pub fn create_block(&self, arguments: Vec<Value>) -> BlockHandle {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        let argument_ids: Vec<ValueId> = arguments.iter().map(Value::id).collect();
        let block_id = delta.store.insert_block(Block::new(arguments));
        for argument in argument_ids {
            assert!(delta.shadow_value(base, argument), "live value");
            delta.store.set_value_block(argument, Some(block_id));
        }
        drop(view);
        self.get_block(block_id)
    }

    /// Append an argument of type `ty` to `block` and return it.
    pub fn append_block_argument(&self, block: BlockId, ty: TypeId) -> Value {
        let value = self.create_value(ty, None);
        self.place_block_argument(block, value.clone());
        value
    }

    /// Make `value` an entry argument of `block`, in place of the definition it
    /// had. Nothing is renamed: the value keeps its identity, so every reader
    /// goes on naming it.
    pub fn adopt_block_argument(&self, block: BlockId, value: ValueId) {
        let Some(adopted) = self.view().value(value).cloned() else {
            return;
        };
        let adopted = Value::new(value, adopted.ty(), None);
        if self.place_block_argument(block, adopted) {
            let mut view = self.view_mut();
            let (base, delta) = view.parts();
            if delta.shadow_value(base, value) {
                delta
                    .store
                    .value_mut(value)
                    .expect("live value")
                    .clear_defining_op();
            }
        }
    }

    fn place_block_argument(&self, block: BlockId, argument: Value) -> bool {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_block(base, block) {
            return false;
        }
        delta
            .store
            .block_mut(block)
            .expect("live block")
            .arguments_mut()
            .push(argument.clone());
        assert!(delta.shadow_value(base, argument.id()), "live value");
        delta.store.set_value_block(argument.id(), Some(block));
        delta.edit_block(base, block);
        true
    }

    /// Append `value` to `op`'s operands, keeping the segment sizes that
    /// describe the trailing variadic group in step.
    pub fn append_operand(&self, op: OpId, value: ValueId) {
        let segment_sizes = self.registry().segment_sizes;
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, op) {
            return;
        }
        let index = delta.store.op(op).expect("live op").operand_count as usize;
        delta.store.insert_operand(op, index, value);
        delta.store.adjust_last_segment(op, segment_sizes, 1);
        delta.edit_op(base, op);
    }

    /// Append `value` to `op`'s results, moving its definition onto `op`.
    pub fn adopt_result(&self, op: OpId, value: ValueId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, op) {
            return;
        }
        delta.store.append_result_port(op, value);
        if delta.shadow_value(base, value) {
            delta
                .store
                .value_mut(value)
                .expect("live value")
                .set_defining_op(op);
        }
        delta.edit_op(base, op);
    }

    /// Drop the operand at `index`, and with it the use it made.
    pub(crate) fn remove_operand(&self, op: OpId, index: usize) {
        let segment_sizes = self.registry().segment_sizes;
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, op) {
            return;
        }
        delta.store.shrink_segment_holding(op, segment_sizes, index);
        delta.store.remove_operand(op, index);
        delta.edit_op(base, op);
    }

    /// Drop the result at `index`. The value it named is left with no
    /// definition, so a caller drops one nothing reads.
    pub(crate) fn remove_result(&self, op: OpId, index: usize) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, op) {
            return;
        }
        let (operands, mut results, regions) = delta.store.ports(op);
        results.remove(index);
        delta.store.set_ports(op, &operands, &results, &regions);
        delta.edit_op(base, op);
    }

    /// Drop the port at `index` of an unordered region.
    pub(crate) fn remove_region_port(&self, region: RegionId, index: usize) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        match delta.store.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { ports, .. }) => {
                ports.remove(index);
            }
            _ => panic!("only an unordered region drops a port by position"),
        }
        delta.edit_region(base, region);
    }

    /// Grow `op` by one carried port of type `ty`.
    ///
    /// A port that carries a value in takes `init` as one more operand and
    /// gives the op's regions one more port, which `latch` receives; a gate
    /// that carries nothing in passes `None`. `latch` says what each region
    /// names for the port. The op gains one result, which is returned.
    pub fn grow_port(
        &self,
        op: OpId,
        ty: TypeId,
        init: Option<ValueId>,
        latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
    ) -> ValueId {
        self.grow_declared_port(op, ty, init, latch)
    }

    /// Put `value` at position `index` of `op`'s operands, inside the declared
    /// operand group that ends at `group_end`, so the segment sizes stay in
    /// step.
    pub(crate) fn insert_operand_at(
        &self,
        op: OpId,
        index: usize,
        value: ValueId,
        group_end: usize,
    ) {
        let segment_sizes = self.registry().segment_sizes;
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, op) {
            return;
        }
        delta.store.insert_operand(op, index, value);
        delta
            .store
            .grow_segment_ending_at(op, segment_sizes, group_end);
        delta.edit_op(base, op);
    }

    /// Put `port` at position `index` of the unordered `region`'s ports.
    pub(crate) fn insert_region_port(&self, region: RegionId, index: usize, port: Value) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        let id = port.id();
        match delta.store.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { ports, .. }) => ports.insert(index, port),
            _ => panic!("only an unordered region takes a port by position"),
        }
        assert!(delta.shadow_value(base, id), "live value");
        delta.store.set_value_region(id, Some(region));
        delta.edit_region(base, region);
    }

    /// Name `value` at position `index` of the unordered `region`'s results.
    pub(crate) fn insert_region_result(&self, region: RegionId, index: usize, value: ValueId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        match delta.store.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { results, .. }) => results.insert(index, value),
            _ => panic!("only an unordered region names its results by position"),
        }
        delta.edit_region(base, region);
    }

    /// Take `op` out of the unordered `region` without erasing it; the inverse
    /// of [`Context::add`].
    pub fn remove_from_region(&self, region: RegionId, op: OpId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        match delta.store.region_mut(region).map(Region::body_mut) {
            Some(crate::region::RegionBody::Nodes { ops, .. }) => ops.retain(|held| *held != op),
            _ => panic!("only an unordered region holds an operation without a position"),
        }
        if delta.shadow_op(base, op) {
            delta.store.set_op_parent(op, None);
        }
        delta.edit_region(base, region);
    }

    /// Put `value` at position `index` of `op`'s results, moving its
    /// definition onto `op`.
    pub(crate) fn insert_result_at(&self, op: OpId, index: usize, value: ValueId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_op(base, op) {
            return;
        }
        let at = delta.store.op(op).expect("live op").operand_count as usize + index;
        delta.store.insert_port(op, at, value.number());
        delta.store.op_mut(op).expect("live op").result_count += 1;
        if delta.shadow_value(base, value) {
            delta
                .store
                .value_mut(value)
                .expect("live value")
                .set_defining_op(op);
        }
        delta.edit_op(base, op);
    }

    /// Give `op` one more result of type `ty`.
    pub fn append_result(&self, op: OpId, ty: TypeId) -> ValueId {
        let result = self.create_value(ty, Some(op)).id();
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if delta.shadow_op(base, op) {
            delta.store.append_result_port(op, result);
            delta.edit_op(base, op);
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
    /// The old contents leave the tree, the uses they held on surviving values
    /// are dropped, and their parent links are cleared, so no walk reaches into
    /// them. Uses of old values recorded with [`StagedRegion::replace_value`]
    /// are then retargeted to their staged replacements. The swap itself bumps
    /// the spine exactly once, at `region`'s owner, and dirties that one
    /// subtree.
    pub fn replace_region_contents(&self, region: RegionId, mut staged: StagedRegion) {
        staged.discard = false;
        let handle = self.get_region(region);
        let owner = handle.parent_op();

        self.detach_subtree(&handle.block_ids());
        self.set_region_blocks(region, staged.blocks.clone());

        {
            let mut view = self.view_mut();
            let (base, delta) = view.parts();
            for &block in &staged.blocks {
                assert!(delta.shadow_block(base, block), "live block");
                delta.store.set_block_parent(block, Some(region));
            }
            if let Some(owner) = owner {
                delta.edit_subtree(base, owner);
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

        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, staged), "live region");
        assert!(delta.shadow_region(base, region), "live region");
        let body = std::mem::replace(
            delta
                .store
                .region_mut(staged)
                .expect("live region")
                .body_mut(),
            crate::region::RegionBody::Blocks(vec![]),
        );
        let crate::region::RegionBody::Nodes { ports, ops, .. } = &body else {
            panic!("only an unordered region replaces an ordered one's body");
        };
        for port in ports {
            assert!(delta.shadow_value(base, port.id()), "live value");
            delta.store.set_value_region(port.id(), Some(region));
        }
        for &op in ops {
            assert!(delta.shadow_op(base, op), "live op");
            delta.store.set_op_parent(op, Some(Parent::Region(region)));
        }
        *delta
            .store
            .region_mut(region)
            .expect("live region")
            .body_mut() = body;
        delta.erase_region(staged);
        if let Some(owner) = owner {
            delta.edit_subtree(base, owner);
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

        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_region(base, region), "live region");
        let body = std::mem::replace(
            delta
                .store
                .region_mut(region)
                .expect("live region")
                .body_mut(),
            crate::region::RegionBody::Blocks(blocks.clone()),
        );
        let crate::region::RegionBody::Nodes { ports, .. } = &body else {
            panic!("only an ordered region replaces an unordered one's body");
        };
        for port in ports {
            if delta.shadow_value(base, port.id()) {
                delta.store.set_value_region(port.id(), None);
            }
        }
        for &block in &blocks {
            assert!(delta.shadow_block(base, block), "live block");
            delta.store.set_block_parent(block, Some(region));
        }
        if let Some(owner) = owner {
            delta.edit_subtree(base, owner);
        }
    }

    /// Take the subtree under `blocks` out of the live IR and give its storage
    /// back. Bumps no version: the caller reports the edit.
    fn detach_subtree(&self, blocks: &[BlockId]) {
        self.free(self.collect_owned(Vec::new(), blocks.to_vec()));
    }

    /// Everything `ops` own: the ops themselves, their result values, their
    /// regions, and those regions' blocks, block arguments and nested ops.
    fn owned_entities(&self, ops: Vec<OpId>) -> Owned {
        self.collect_owned(ops, Vec::new())
    }

    /// Walks ops and blocks alternately, reading through handles.
    ///
    /// A nested entity is reclaimed only while its parent link still points at
    /// the entity being erased: a rewrite that lifts a block out of a region it
    /// is destroying leaves the block listed in the dying region, and that
    /// stale listing must not free live IR.
    fn collect_owned(&self, mut ops: Vec<OpId>, mut blocks: Vec<BlockId>) -> Owned {
        let mut owned = Owned::default();
        loop {
            while let Some(op) = ops.pop() {
                let Some(instance) = self.find_op(op) else {
                    continue;
                };
                owned.ops.push(op);
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

    /// Take entities that have left the IR out of the visible graph.
    fn free(&self, owned: Owned) {
        let mut view = self.view_mut();
        let delta = &mut view.delta;
        for op in owned.ops {
            delta.erase_op(op);
        }
        for value in owned.values {
            delta.erase_value(value);
        }
        for block in owned.blocks {
            delta.erase_block(block);
        }
        for region in owned.regions {
            delta.erase_region(region);
        }
    }

    /// Insert `op` into `block` at `index`, recording the new parent.
    pub(crate) fn insert_op(&self, block: BlockId, index: usize, op: OpId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if delta.shadow_block(base, block) {
            delta
                .store
                .block_mut(block)
                .expect("live block")
                .operations_mut()
                .insert(index, op);
        }
        assert!(delta.shadow_op(base, op), "live op");
        delta.store.set_op_parent(op, Some(Parent::Block(block)));
        delta.edit_block(base, block);
    }

    /// Insert `op` after everything `block` currently holds.
    pub(crate) fn append_op(&self, block: BlockId, op: OpId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if delta.shadow_block(base, block) {
            delta
                .store
                .block_mut(block)
                .expect("live block")
                .operations_mut()
                .push(op);
        }
        assert!(delta.shadow_op(base, op), "live op");
        delta.store.set_op_parent(op, Some(Parent::Block(block)));
        delta.edit_block(base, block);
    }

    pub(crate) fn replace_op_in_block(&self, block: BlockId, old: OpId, new: OpId) -> bool {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_block(base, block) {
            return false;
        }
        let operations = delta
            .store
            .block_mut(block)
            .expect("live block")
            .operations_mut();
        let Some(position) = operations.iter().position(|id| *id == old) else {
            return false;
        };
        operations[position] = new;
        if delta.shadow_op(base, old) {
            delta.store.set_op_parent(old, None);
        }
        assert!(delta.shadow_op(base, new), "live op");
        delta.store.set_op_parent(new, Some(Parent::Block(block)));
        delta.edit_block(base, block);
        true
    }

    pub(crate) fn remove_op_from_block(&self, block: BlockId, op: OpId) -> bool {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_block(base, block) {
            return false;
        }
        let operations = delta
            .store
            .block_mut(block)
            .expect("live block")
            .operations_mut();
        let Some(position) = operations.iter().position(|id| *id == op) else {
            return false;
        };
        operations.remove(position);
        if delta.shadow_op(base, op) {
            delta.store.set_op_parent(op, None);
        }
        delta.edit_block(base, block);
        true
    }

    pub(crate) fn set_block_attr(&self, block: BlockId, name: &str, value: AttributeValue) {
        let name = self.intern(name);
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_block(base, block) {
            return;
        }
        let attributes = delta
            .store
            .block_mut(block)
            .expect("live block")
            .attributes_mut();
        match attributes.iter_mut().find(|a| a.name == name) {
            Some(attribute) => attribute.value = value,
            None => attributes.push(NamedAttribute::new(name, value)),
        }
        delta.edit_block(base, block);
    }

    /// Edit a block's storage record, dirtying the subtree it sits in.
    ///
    /// `edit` must not touch the context: the overlay is borrowed for writing.
    pub(crate) fn with_block_mut<R>(&self, id: BlockId, edit: impl FnOnce(&mut Block) -> R) -> R {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        assert!(delta.shadow_block(base, id), "live block");
        let edited = edit(delta.store.block_mut(id).expect("live block"));
        delta.edit_block(base, id);
        edited
    }

    pub(crate) fn add_block_to_region(&self, region: RegionId, block: BlockId) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if delta.shadow_region(base, region) {
            delta
                .store
                .region_mut(region)
                .expect("live region")
                .blocks_mut()
                .push(block);
        }
        assert!(delta.shadow_block(base, block), "live block");
        delta.store.set_block_parent(block, Some(region));
        delta.edit_region(base, region);
    }

    pub(crate) fn remove_block_from_region(&self, region: RegionId, block: BlockId) -> bool {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if !delta.shadow_region(base, region) {
            return false;
        }
        let blocks = delta
            .store
            .region_mut(region)
            .expect("live region")
            .blocks_mut();
        let Some(position) = blocks.iter().position(|id| *id == block) else {
            return false;
        };
        blocks.remove(position);
        if delta.shadow_block(base, block) {
            delta.store.set_block_parent(block, None);
        }
        delta.edit_region(base, region);
        true
    }

    /// Replace `region`'s whole block list at once. Only
    /// [`Context::replace_region_contents`] uses this: it owns the parent
    /// bookkeeping and the single version bump the swap is allowed to make.
    pub(crate) fn set_region_blocks(&self, region: RegionId, blocks: Vec<BlockId>) {
        let mut view = self.view_mut();
        let (base, delta) = view.parts();
        if delta.shadow_region(base, region) {
            *delta
                .store
                .region_mut(region)
                .expect("live region")
                .blocks_mut() = blocks;
        }
    }
}

/// A region body under construction, detached from the live IR.
///
/// Its blocks, values and ops exist from the moment they are built, but they
/// belong to no region until the staging is committed: they print nowhere,
/// bump no version, and dirty no subtree. Staged ops may take values defined
/// outside the region as operands; those uses become live with the commit and
/// are dropped again if the staging is discarded.
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

impl<I: GetFromContext> ContextIterator<I> {
    pub fn new(context: Context, elements: Vec<I>) -> Self {
        Self {
            context,
            elements,
            current_front: 0,
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
