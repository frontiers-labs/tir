use std::{
    any::Any,
    collections::HashMap,
    sync::{Arc, Weak, atomic::AtomicU32},
};

use parking_lot::RwLock;

use crate::{
    Block, Dialect, Error, OpId, OpInstance, Operation, OperationParser, Region, TypeId,
    block::BlockId,
    builtin::BuiltinDialect,
    dialects::scf::ScfDialect,
    ir_formatter::IRFormatter,
    operation::{
        ImplementsOpInterface, OpInterfaceConverter, downcast_op_interface, op_interface_converter,
    },
    parse::text::Parser as IRParser,
    ptr::PtrDialect,
    region::RegionId,
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

/// Read an entry from a slab arena indexed by a dense id, or `None` if the id was
/// never inserted or has been removed.
fn slab_get<T>(slab: &[Option<T>], idx: usize) -> Option<&T> {
    slab.get(idx).and_then(Option::as_ref)
}

/// Insert into a slab arena at a dense id, growing the backing vector as needed.
/// Ids come from per-context monotonic counters, so the vector stays dense.
fn slab_put<T>(slab: &mut Vec<Option<T>>, idx: usize, val: T) {
    if idx >= slab.len() {
        slab.resize_with(idx + 1, || None);
    }
    slab[idx] = Some(val);
}

struct ContextInstance {
    // None for root context itself, reference to a root context if this is a forked Region.
    root_context: Option<Context>,
    // Arenas are slabs indexed by the dense, monotonic id counters below; see `slab_get`.
    operations: Vec<Option<Arc<OpInstance>>>,
    last_op_id: AtomicU32,
    values: Vec<Option<Arc<Value>>>,
    last_value_id: AtomicU32,
    regions: Vec<Option<Arc<Region>>>,
    last_region_id: AtomicU32,
    blocks: Vec<Option<Arc<Block>>>,
    last_block_id: AtomicU32,
    /// Reverse index from an operation to the block that holds it, maintained by
    /// `Block`'s membership mutators. Lets `parent_block` answer in O(1) instead of
    /// scanning every block's operation list.
    op_parent: Vec<Option<BlockId>>,
    dialects: HashMap<&'static str, Arc<dyn Dialect>>,
    op_interface_converters:
        HashMap<(&'static str, &'static str, std::any::TypeId), OpInterfaceConverter>,
    type_cache: Vec<Arc<dyn Type>>,
}

impl Context {
    /// Create a new empty context with no registered dialects.
    pub fn new() -> Self {
        Context(Arc::new(RwLock::new(ContextInstance {
            root_context: None,
            operations: Vec::new(),
            last_op_id: AtomicU32::new(0),
            values: Vec::new(),
            last_value_id: AtomicU32::new(0),
            regions: Vec::new(),
            last_region_id: AtomicU32::new(0),
            blocks: Vec::new(),
            last_block_id: AtomicU32::new(0),
            op_parent: Vec::new(),
            dialects: HashMap::new(),
            op_interface_converters: HashMap::new(),
            type_cache: vec![],
        })))
    }

    /// Create a new context with default dialects.
    pub fn with_default_dialects() -> Self {
        let context = Context::new();

        context.register_dialect::<BuiltinDialect>();
        context.register_dialect::<PtrDialect>();
        context.register_dialect::<ScfDialect>();
        context.register_dialect::<VectorDialect>();

        context
    }

    pub fn root_context(&self) -> Option<Context> {
        self.0.read().root_context.clone()
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

    pub fn add_operation(&self, mut instance: OpInstance) -> Arc<OpInstance> {
        let mut inner = self.0.write();

        let op_id = OpId::new(
            inner
                .last_op_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );

        instance.id = op_id;

        // Results are created before op id assignment in builders; patch their def-site now.
        for result_id in &instance.results {
            if let Some(value) = slab_get(&inner.values, result_id.index()).cloned() {
                slab_put(
                    &mut inner.values,
                    result_id.index(),
                    Arc::new((*value).clone().with_defining_op(op_id)),
                );
            }
        }

        // Register this op as a use of each operand value, so `Value::uses` is a live
        // def-use chain. Detached again when the op is erased or replaced.
        for (index, operand) in instance.operands.iter().enumerate() {
            if let Some(value) = slab_get(&inner.values, operand.index()).cloned() {
                let mut value = (*value).clone();
                value.add_use(op_id, crate::UseSite::Operand(index));
                slab_put(&mut inner.values, operand.index(), Arc::new(value));
            }
        }

        // Machine ops carry their register operands in role-tagged attributes rather
        // than `operands`/`results`, so mirror the above over those: a `Use` register
        // is a use of its virtual value; a `Def` register is that value's def-site.
        // Virtual register ids are value numbers; physical registers have none and are
        // skipped — they are not SSA. ReadWrite counts as both.
        for (attr_name, role) in instance.attribute_roles {
            use crate::attributes::{AttributeRole, AttributeValue, RegisterAttr};
            let Some(attr) = instance.attributes.iter().find(|a| a.name == *attr_name) else {
                continue;
            };
            let AttributeValue::Register(RegisterAttr::Virtual { id, .. }) = &attr.value else {
                continue;
            };
            let value_id = ValueId::from_number(*id);
            let Some(value) = slab_get(&inner.values, value_id.index()).cloned() else {
                continue;
            };
            let mut value = (*value).clone();
            if matches!(role, AttributeRole::Use | AttributeRole::ReadWrite) {
                value.add_use(op_id, crate::UseSite::Attribute(attr_name));
            }
            if matches!(role, AttributeRole::Def | AttributeRole::ReadWrite) {
                value = value.with_defining_op(op_id);
            }
            slab_put(&mut inner.values, value_id.index(), Arc::new(value));
        }

        for r in &instance.regions {
            slab_get(&inner.regions, r.index())
                .unwrap()
                .set_parent_op(op_id);
        }

        let instance = Arc::new(instance);

        slab_put(&mut inner.operations, op_id.index(), instance.clone());

        instance
    }

    pub fn has_operation(&self, id: OpId) -> bool {
        slab_get(&self.0.read().operations, id.index()).is_some()
    }

    /// Replace an operation's attributes in place, keeping its id, position, and
    /// regions. Register allocation uses this to rewrite virtual register operands
    /// to physical ones once the def-use chain is no longer needed; it deliberately
    /// does not update `Value::uses`, since physical registers are not SSA values.
    pub fn set_op_attributes(&self, id: OpId, attributes: Vec<crate::attributes::NamedAttribute>) {
        let mut inner = self.0.write();
        if let Some(existing) = slab_get(&inner.operations, id.index()).cloned() {
            let mut updated = (*existing).clone();
            updated.attributes = attributes;
            slab_put(&mut inner.operations, id.index(), Arc::new(updated));
        }
    }

    /// Remove an op from the operation arena. Called by `Rewriter::erase_op`/
    /// `replace_op` once the op has left its block, so the arena tracks the *live*
    /// IR rather than accumulating detached ops (which otherwise show up as phantom
    /// references to any whole-program scan). Existing `Arc<OpInstance>` handles
    /// (e.g. inside an `OperationRef`) keep the instance alive after removal.
    pub(crate) fn remove_operation(&self, id: OpId) {
        let mut inner = self.0.write();
        if let Some(slot) = inner.operations.get_mut(id.index()) {
            *slot = None;
        }
    }

    pub fn create_value(&self, ty: TypeId, defining_op: Option<OpId>) -> Value {
        let mut inner = self.0.write();

        let value_id = ValueId::new(
            inner
                .last_value_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );

        let value = Value::new(value_id, ty, defining_op);
        slab_put(&mut inner.values, value_id.index(), Arc::new(value.clone()));

        value
    }

    pub fn get_value(&self, id: ValueId) -> Arc<Value> {
        let inner = self.0.read();
        slab_get(&inner.values, id.index()).unwrap().clone()
    }

    /// The operands that reference `id`, as `(op, operand-index)` pairs. See
    /// [`Value::uses`] for what is and isn't tracked.
    pub fn value_uses(&self, id: ValueId) -> Vec<crate::Use> {
        let inner = self.0.read();
        slab_get(&inner.values, id.index())
            .map(|v| v.uses().to_vec())
            .unwrap_or_default()
    }

    /// Whether any operand references `id`.
    pub fn is_value_used(&self, id: ValueId) -> bool {
        let inner = self.0.read();
        slab_get(&inner.values, id.index()).is_some_and(|v| v.is_used())
    }

    /// Drop every use contributed by `op` from the values it referenced. Called when
    /// an op leaves the live IR (erase/replace) to keep `Value::uses` consistent.
    /// Visits both SSA `operands` and virtual register operands carried in attributes
    /// (the same set `add_operation` registered); `remove_uses_of` filters by op id,
    /// so visiting a value the op only *defined* is harmless. `defining_op` is left
    /// untouched — on replace, the new op has already claimed the def-site.
    pub(crate) fn detach_op_uses(&self, op: &OpInstance) {
        use crate::attributes::{AttributeValue, RegisterAttr};

        let mut touched: Vec<ValueId> = op.operands.clone();
        for attr in &op.attributes {
            if let AttributeValue::Register(RegisterAttr::Virtual { id, .. }) = &attr.value {
                touched.push(ValueId::from_number(*id));
            }
        }

        let mut inner = self.0.write();
        for value_id in touched {
            if let Some(value) = slab_get(&inner.values, value_id.index()).cloned() {
                let mut value = (*value).clone();
                value.remove_uses_of(op.id);
                slab_put(&mut inner.values, value_id.index(), Arc::new(value));
            }
        }
    }

    /// Replace every SSA operand use of `old` with `new` across the live IR.
    ///
    /// This keeps the context-owned def-use lists in sync with the rewritten
    /// operations. Register-attribute uses are intentionally left untouched: they
    /// are not SSA operands and currently belong to machine IR, not high-level
    /// scalar promotion.
    pub fn replace_value_uses(&self, old: ValueId, new: ValueId) {
        if old == new {
            return;
        }

        let mut inner = self.0.write();
        let uses = slab_get(&inner.values, old.index())
            .map(|v| v.uses().to_vec())
            .unwrap_or_default();

        for use_site in uses {
            let crate::UseSite::Operand(index) = use_site.site() else {
                continue;
            };

            let Some(op) = slab_get(&inner.operations, use_site.op().index()).cloned() else {
                continue;
            };
            if op.operands.get(index).copied() != Some(old) {
                continue;
            }

            let mut new_instance = (*op).clone();
            new_instance.operands[index] = new;
            slab_put(
                &mut inner.operations,
                use_site.op().index(),
                Arc::new(new_instance),
            );

            if let Some(old_value) = slab_get(&inner.values, old.index()).cloned() {
                let mut old_value = (*old_value).clone();
                old_value.remove_use(use_site.op(), use_site.site());
                slab_put(&mut inner.values, old.index(), Arc::new(old_value));
            }
            if let Some(new_value) = slab_get(&inner.values, new.index()).cloned() {
                let mut new_value = (*new_value).clone();
                new_value.add_use(use_site.op(), use_site.site());
                slab_put(&mut inner.values, new.index(), Arc::new(new_value));
            }
        }
    }

    pub fn has_value(&self, id: ValueId) -> bool {
        slab_get(&self.0.read().values, id.index()).is_some()
    }

    pub fn is_block_argument(&self, id: ValueId) -> bool {
        let inner = self.0.read();
        inner
            .blocks
            .iter()
            .flatten()
            .any(|block| block.arguments().iter().any(|arg| arg.id() == id))
    }

    pub fn create_region(&self) -> Arc<Region> {
        let mut inner = self.0.write();

        let region_id = RegionId::new(
            inner
                .last_region_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );

        let region = Arc::new(Region::new(region_id));
        slab_put(&mut inner.regions, region_id.index(), region.clone());

        region
    }

    pub fn create_block(&self, arguments: Vec<Value>) -> Arc<Block> {
        let context = self.as_context_ref();
        let mut inner = self.0.write();

        let block_id = BlockId::new(
            inner
                .last_block_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );

        let block = Arc::new(Block::new(block_id, arguments, context));
        slab_put(&mut inner.blocks, block_id.index(), block.clone());

        block
    }

    /// The block currently holding `op`, or `None` for an op not in any block (the
    /// root op, or one detached by a rewrite). Maintained by `Block`'s membership
    /// mutators; see [`ContextInstance::op_parent`].
    pub fn parent_block(&self, op: OpId) -> Option<BlockId> {
        slab_get(&self.0.read().op_parent, op.index()).copied()
    }

    pub(crate) fn set_op_parent(&self, op: OpId, block: BlockId) {
        slab_put(&mut self.0.write().op_parent, op.index(), block);
    }

    pub(crate) fn clear_op_parent(&self, op: OpId) {
        let mut inner = self.0.write();
        if let Some(slot) = inner.op_parent.get_mut(op.index()) {
            *slot = None;
        }
    }

    pub fn get_block(&self, id: BlockId) -> Arc<Block> {
        let inner = self.0.read();

        slab_get(&inner.blocks, id.index()).unwrap().clone()
    }

    pub fn get_region(&self, id: RegionId) -> Arc<Region> {
        let inner = self.0.read();

        slab_get(&inner.regions, id.index()).unwrap().clone()
    }

    pub fn get_op(&self, id: OpId) -> Arc<OpInstance> {
        let inner = self.0.read();

        slab_get(&inner.operations, id.index()).unwrap().clone()
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

    pub(crate) fn get_dyn_op(&self, op: Arc<OpInstance>) -> Box<dyn Operation> {
        let inner = self.0.read();

        let dialect = inner.dialects.get(op.dialect()).unwrap();

        dialect.get_dyn_op(op)
    }

    pub(crate) fn get_op_interface<I: ?Sized + 'static>(
        &self,
        op: Arc<OpInstance>,
    ) -> Option<Box<I>> {
        let converter = {
            let inner = self.0.read();
            inner
                .op_interface_converters
                .get(&(op.dialect(), op.name(), std::any::TypeId::of::<I>()))
                .copied()
        }?;

        let erased = converter(op);
        downcast_op_interface::<I>(erased)
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
        let mut inner = self.0.upgradable_read();
        let id = inner
            .type_cache
            .iter()
            .enumerate()
            .find_map(|(id, item)| if item.eq(&*ty) { Some(id) } else { None });

        if let Some(id) = id {
            (id as u32).into()
        } else {
            let id = inner.with_upgraded(|inner| {
                let id = inner.type_cache.len() as u32;
                inner.type_cache.push(ty);
                id
            });
            id.into()
        }
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
mod tests {
    use super::Context;
    use crate::{Commutative, Operation, Terminator, builtin};

    #[test]
    fn default_context() {
        let _ = Context::with_default_dialects();
    }

    #[test]
    fn parent_block_tracks_membership() {
        use crate::IRBuilder;
        let context = Context::with_default_dialects();
        let i32 = builtin::IntegerType::new(&context, 32);
        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);

        let block = context.create_block(vec![]);
        let mut builder = IRBuilder::new(block.clone());
        let add = builder.insert(builtin::ops::addi(&context, a.id(), b.id(), i32).build());

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
    fn adding_an_op_registers_operand_uses() {
        let context = Context::with_default_dialects();
        let i32 = builtin::IntegerType::new(&context, 32);
        let lhs = context.create_value(i32, None);
        let rhs = context.create_value(i32, None);

        assert!(!context.is_value_used(lhs.id()));

        let add = builtin::ops::addi(&context, lhs.id(), rhs.id(), i32).build();

        // The local `lhs` handle is a pre-use snapshot; the live value carries the use.
        assert!(context.is_value_used(lhs.id()));
        let uses = context.value_uses(lhs.id());
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].op(), add.id());
        assert_eq!(uses[0].operand_index(), Some(0));
        assert_eq!(context.value_uses(rhs.id())[0].operand_index(), Some(1));
    }

    #[test]
    fn an_operand_used_twice_records_both_indices() {
        let context = Context::with_default_dialects();
        let i32 = builtin::IntegerType::new(&context, 32);
        let x = context.create_value(i32, None);

        let add = builtin::ops::addi(&context, x.id(), x.id(), i32).build();

        let mut indices: Vec<usize> = context
            .value_uses(x.id())
            .iter()
            .filter_map(|u| u.operand_index())
            .collect();
        indices.sort();
        assert_eq!(indices, vec![0, 1]);
        assert!(
            context
                .value_uses(x.id())
                .iter()
                .all(|u| u.op() == add.id())
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
