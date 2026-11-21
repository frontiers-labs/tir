mod attribute_store;
mod keys;
mod op_spec;
mod queries;
mod records;

#[allow(dead_code)]
mod ir_program {
    include!(concat!(env!("OUT_DIR"), "/ir_program.rs"));
}
use crate::{
    Dialect, Error, OpId, Operation, OperationParser, TypeId,
    attributes::{AttributeValue, NamedAttribute},
    block::{BlockHandle, BlockId},
    builtin::BuiltinDialect,
    dialects::{
        cfg::CfgDialect, fp::FpDialect, func::FuncDialect, scf::ScfDialect, state::StateDialect,
    },
    ir_formatter::IRFormatter,
    operation::{ImplementsOpInterface, OpHandle, downcast_op_interface, op_interface_converter},
    parse::text::Parser as IRParser,
    ptr::PtrDialect,
    region::{RegionHandle, RegionId},
    ty::{Spellings, Terms, TypeArg, TypeParser, TypeSpelling},
    value::{Use, Value, ValueId},
    vector::VectorDialect,
};
use records::{BlockRow, Defines, OperationRow, Placement, Record, RegionRow, ValueRow};
use std::rc::Rc;
use std::{
    any::Any,
    cell::{Cell, Ref, RefCell, RefMut},
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};
use tir_adt::Sym;
use tir_egglog::{EGraph, RawValues, Read, Value as EggValue, Write};

/// A context owns the structural IR relations in its egglog engine.
#[derive(Clone)]
pub struct Context(std::rc::Rc<Inner>);
impl AsRef<Terms> for Context {
    fn as_ref(&self) -> &Terms {
        self.terms()
    }
}

pub struct ContextIterator<I: GetFromContext> {
    context: Context,
    elements: Vec<I>,
    current_front: usize,
}
pub trait GetFromContext {
    type Item;
    fn get_from_context(&self, context: &Context) -> Self::Item;
}
struct Registry {
    dialects: HashMap<&'static str, Arc<dyn Dialect>>,
    reg_classes: HashMap<(crate::ty::Name, crate::ty::Name), crate::backend::regalloc::RegClassId>,
    spellings: Spellings,
    terms: Rc<Terms>,
    segment_sizes: Sym,
    core_types: bool,
}
#[derive(Clone)]
pub struct Names(Rc<RefCell<Registry>>);
impl Names {
    /// The name `sym` was interned from.
    pub fn name(&self, sym: Sym) -> String {
        self.0.borrow().terms.symbol_text(sym)
    }

    pub fn name_at(&self, index: usize) -> Option<String> {
        self.0.borrow().terms.symbol_at(index)
    }

    pub fn intern(&self, name: &str) -> Sym {
        self.0.borrow().terms.intern_symbol(name)
    }

    /// `ty` as the IR spells it.
    pub fn type_spelling(&self, ty: TypeId) -> String {
        let registry = self.0.borrow();
        let mut out = String::new();
        registry
            .spellings
            .print(ty, &mut IRFormatter::new(&mut out))
            .expect("type print must succeed");
        out
    }
}

struct Inner {
    registry: Rc<RefCell<Registry>>,
    terms: Rc<Terms>,
    engine: Rc<RefCell<EGraph>>,
    next_id: Arc<AtomicU32>,
    origin: Cell<Option<crate::parse::Span>>,
    queries: queries::Queries,
    analyses: crate::AnalysisManager,
    constructing: Cell<Option<OpId>>,
}

impl Context {
    pub fn new() -> Self {
        let mut engine = EGraph::default();
        ir_program::bind_std_ir_integer_width(
            engine.database_mut(),
            tir_symbolic::term::integer_width,
        )
        .unwrap_or_else(|error| panic!("IR integer width: {error}"));
        ir_program::bind_std_ir_type_argument(
            engine.database_mut(),
            tir_symbolic::term::type_argument,
        )
        .unwrap_or_else(|error| panic!("IR type argument: {error}"));
        ir_program::bind_std_ir_fp_format(engine.database_mut(), tir_symbolic::term::fp_format)
            .unwrap_or_else(|error| panic!("IR floating-point format: {error}"));
        ir_program::attach(&mut engine).unwrap_or_else(|error| panic!("IR rules: {error}"));
        keys::install(&mut engine).expect("IR entity keys");
        let queries = queries::Queries::new(engine.database());
        op_spec::install(&mut engine);
        let engine = Rc::new(RefCell::new(engine));
        let next_id = Arc::new(AtomicU32::new(0));
        let terms = Rc::new(Terms::with_engine(engine.clone(), next_id.clone()));
        for schema in crate::schema::OP_SCHEMAS {
            for attribute in schema.attributes {
                terms.intern_symbol(attribute.name);
            }
        }
        let segment_sizes = terms.intern_symbol("operand_segment_sizes");
        Self(std::rc::Rc::new(Inner {
            registry: Rc::new(RefCell::new(Registry {
                dialects: HashMap::new(),
                reg_classes: HashMap::new(),
                spellings: Spellings::new(terms.clone()),
                terms: terms.clone(),
                segment_sizes,
                core_types: false,
            })),
            engine,
            next_id,
            terms,
            origin: Cell::new(None),
            queries,
            analyses: crate::AnalysisManager::new(),
            constructing: Cell::new(None),
        }))
    }

    /// Borrow the live IR with up-to-date derived relations. After raw edits,
    /// release the guard and reacquire it to refresh the derived rows.
    pub fn egraph(&self) -> RefMut<'_, EGraph> {
        self.0.queries.verification(&mut self.engine());
        self.engine()
    }

    /// The producer catalog and cache for this context.
    pub fn analyses(&self) -> &crate::AnalysisManager {
        &self.0.analyses
    }

    pub(crate) fn eqsat_database(&self) -> Result<tir_egglog::Database, tir_egglog::Error> {
        let mut db = self.egraph().database().clone();
        attribute_store::bind_integer_attributes(&mut db)?;
        Ok(db)
    }

    fn engine(&self) -> RefMut<'_, EGraph> {
        self.0.engine.borrow_mut()
    }

    pub(crate) fn typing_engine(&self) -> Result<RefMut<'_, EGraph>, Error> {
        drop(self.0.engine.try_borrow_mut().map_err(|_| {
            Error::VerificationError("IR engine already borrowed during typechecking".into())
        })?);
        self.0.queries.verification(&mut self.engine());
        Ok(self.engine())
    }

    /// Request a fresh evaluation of the structural queries after an external edit.
    pub fn invalidate_indexes(&self) {
        self.0.queries.invalidate();
    }

    fn mint(&self) -> u32 {
        self.0.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn intern(&self, name: &str) -> Sym {
        self.terms().intern_symbol(name)
    }
    pub fn with_default_dialects() -> Self {
        let context = Context::new();

        context.register_dialect::<BuiltinDialect>();
        context.register_dialect::<crate::builtin::TypeDialect>();
        context.register_dialect::<crate::layout::LayoutDialect>();
        context.register_dialect::<CfgDialect>();
        context.register_dialect::<FpDialect>();
        context.register_dialect::<FuncDialect>();
        context.register_dialect::<PtrDialect>();
        context.register_dialect::<ScfDialect>();
        context.register_dialect::<StateDialect>();
        context.register_dialect::<crate::son::SonDialect>();
        context.register_dialect::<VectorDialect>();
        context.declare_core_types();

        context
    }

    fn declare_core_types(&self) {
        let terms = self.terms();
        for (name, default) in [
            ("struct", TypeArg::Nat(0)),
            ("variant", TypeArg::Type(terms.int(32))),
        ] {
            let Some(ctor) = terms.ctor_named("type", name) else {
                continue;
            };
            terms
                .set_default(ctor, default)
                .expect("each core constructor has one default");
        }
        self.registry_mut().core_types = true;
    }

    pub(crate) fn core_type_names(&self) -> Option<&'static [&'static str]> {
        self.registry()
            .core_types
            .then_some(&["struct", "variant", "union", "array"])
    }

    fn registry(&self) -> Ref<'_, Registry> {
        self.0.registry.borrow()
    }

    fn registry_mut(&self) -> RefMut<'_, Registry> {
        self.0.registry.borrow_mut()
    }

    pub fn terms(&self) -> &Terms {
        &self.0.terms
    }

    pub fn names(&self) -> Names {
        Names(self.0.registry.clone())
    }

    pub fn sym(&self, name: &str) -> Option<Sym> {
        self.terms().symbol_lookup(name)
    }

    pub fn resolve(&self, sym: Sym) -> String {
        self.terms().symbol_text(sym)
    }

    pub fn named_attribute(&self, name: &str, value: AttributeValue) -> NamedAttribute {
        NamedAttribute::new(self.intern(name), value)
    }

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

    pub fn register_reg_classes(&self, classes: &'static [crate::backend::regalloc::RegClassInfo]) {
        for class in classes {
            self.note_reg_class(crate::backend::regalloc::RegClassId::new(class));
        }
    }

    pub(crate) fn note_reg_class(&self, class: crate::backend::regalloc::RegClassId) {
        let terms = self.terms();
        let key = (terms.name(class.dialect()), terms.name(class.name()));
        let known = self.registry().reg_classes.contains_key(&key);
        if !known {
            self.registry_mut().reg_classes.insert(key, class);
        }
    }

    pub fn resolve_reg_class(&self, name: &str) -> Option<crate::backend::regalloc::RegClassId> {
        let registry = self.registry();
        let named = registry
            .reg_classes
            .values()
            .filter(|class| class.name() == name);
        named.min_by_key(|class| class.dialect()).copied()
    }

    pub(crate) fn reg_class(
        &self,
        target: crate::ty::Name,
        class: crate::ty::Name,
    ) -> Option<crate::backend::regalloc::RegClassId> {
        self.registry().reg_classes.get(&(target, class)).copied()
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
        let converter = self.find_op_interface::<I>(&self.operation_kind(op.id)?)?;
        let erased = converter(op);
        downcast_op_interface::<I>(erased)
    }

    pub fn get_parser(&self, dialect: &str, name: &str) -> Result<OperationParser, Error> {
        let registry = self.registry();
        let dialect = registry
            .dialects
            .get(dialect)
            .ok_or(Error::UnknownDialect(dialect.to_string()))?;
        dialect.get_parser(name)
    }

    pub fn register_type(&self, dialect: &'static str, spelling: &TypeSpelling) {
        self.registry_mut().spellings.register(dialect, spelling);
    }

    pub fn register_type_parser(
        &self,
        dialect: &'static str,
        key: &'static str,
        parser: TypeParser,
    ) {
        self.registry_mut()
            .spellings
            .register_parser(dialect, key, parser);
    }

    pub fn get_type_parser(&self, dialect: &str, name: &str) -> Result<TypeParser, Error> {
        let registry = self.registry();
        if let Some(parser) = registry.spellings.parser(dialect, name) {
            return Ok(parser);
        }
        if registry.dialects.contains_key(dialect) {
            Err(Error::UnknownType(dialect.to_string(), name.to_string()))
        } else {
            Err(Error::UnknownDialect(dialect.to_string()))
        }
    }

    pub fn parse_type_mnemonic(&self, dialect: &str, name: &str) -> Result<TypeId, Error> {
        let parser = self.get_type_parser(dialect, name)?;
        let mut p = IRParser::new("");
        parser(name, &mut p, self).map_err(|(_, err)| err)
    }

    pub fn types_where(&self, keep: impl Fn(TypeId) -> bool) -> Vec<TypeId> {
        (0..self.terms().len())
            .map(TypeId::from_number)
            .filter(|&ty| keep(ty))
            .collect()
    }

    pub fn state_resource(&self, ty: TypeId) -> Option<crate::builtin::StateResource> {
        crate::builtin::StateType::view(ty).map(|state| state.resource())
    }

    pub fn is_state_type(&self, ty: TypeId) -> bool {
        self.state_resource(ty).is_some()
    }

    pub fn is_variadic_tail(&self, ty: TypeId) -> bool {
        self.registry().spellings.is_variadic_tail(ty)
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

    pub fn type_arg_to_string(&self, arg: &crate::TypeArg, names: &[String]) -> String {
        let mut out = String::new();
        {
            let mut fmt = IRFormatter::new(&mut out);
            fmt.set_type_params(names.to_vec());
            self.registry()
                .spellings
                .print_arg(arg, &mut fmt)
                .expect("type print must succeed");
        }
        out
    }

    pub fn print_type(&self, ty: TypeId, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        self.registry().spellings.print(ty, fmt)
    }

    pub fn get_op(&self, id: OpId) -> OpHandle {
        assert!(self.has_operation(id), "live operation {id:?}");
        OpHandle {
            context: self.clone(),
            id,
        }
    }

    pub fn get_block(&self, id: BlockId) -> BlockHandle {
        assert!(self.has_block(id), "live block {id:?}");
        BlockHandle {
            context: self.clone(),
            id,
        }
    }

    pub fn get_region(&self, id: RegionId) -> RegionHandle {
        assert!(self.has_region(id), "live region {id:?}");
        RegionHandle {
            context: self.clone(),
            id,
        }
    }

    pub fn values_among(&self, ids: &[ValueId]) -> crate::operation::ValueIds {
        self.filter_states(ids, false)
    }

    pub fn states_among(&self, ids: &[ValueId]) -> crate::operation::ValueIds {
        self.filter_states(ids, true)
    }

    pub fn users_of(&self, value: ValueId) -> Vec<OpId> {
        self.uses_of(value)
            .into_iter()
            .map(|r#use| r#use.op)
            .collect()
    }

    pub fn use_count(&self, value: ValueId) -> usize {
        self.uses_of(value).len()
    }

    pub fn is_block_argument(&self, id: ValueId) -> bool {
        self.block_of_argument(id).is_some()
    }

    pub fn is_region_port(&self, id: ValueId) -> bool {
        self.region_of_port(id).is_some()
    }

    pub fn grow_port(
        &self,
        op: OpId,
        ty: TypeId,
        init: Option<ValueId>,
        latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
    ) -> ValueId {
        self.grow_declared_port(op, ty, init, latch)
    }
    fn filter_states(&self, ids: &[ValueId], states: bool) -> crate::operation::ValueIds {
        ids.iter()
            .copied()
            .filter(|id| {
                self.has_value(*id) && self.is_state_type(self.get_value(*id).ty()) == states
            })
            .collect()
    }
    fn lookup(&self, table: &str, keys: impl tir_egglog::IntoValues) -> Option<EggValue> {
        self.engine()
            .read(|state| state.lookup(table, keys))
            .unwrap_or_else(|error| panic!("{table}: {error}"))
    }

    fn put(&self, table: &str, keys: Vec<EggValue>, value: EggValue) {
        let mut engine = self.engine();
        engine
            .update(|mut state| state.remove(table, RawValues(keys.clone())))
            .unwrap_or_else(|error| panic!("{table}: {error}"));
        engine
            .update(|mut state| state.set(table, RawValues(keys), value))
            .unwrap_or_else(|error| panic!("{table}: {error}"));
    }

    fn delete(&self, table: &str, keys: Vec<EggValue>) {
        self.engine()
            .update(|mut state| state.remove(table, RawValues(keys)))
            .unwrap_or_else(|error| panic!("{table}: {error}"));
    }

    fn boolean(&self, flag: bool) -> EggValue {
        self.engine().base_to_value(flag)
    }
    fn int(&self, n: impl Into<i64>) -> EggValue {
        self.engine().base_to_value(n.into())
    }
    fn text(&self, text: impl Into<String>) -> EggValue {
        self.engine().base_to_value(text.into())
    }
    fn integer(&self, value: EggValue) -> i64 {
        self.engine().value_to_base(value)
    }
    fn string(&self, value: EggValue) -> String {
        self.engine().value_to_base(value)
    }

    fn insert_row(&self, table: &str, fields: impl tir_egglog::IntoValues) {
        let mut engine = self.engine();
        let db = engine.database_mut();
        let row = fields.into_values(db.base_values()).collect::<Vec<_>>();
        db.insert(db.table(table).expect("IR table"), &row)
            .expect("IR row");
    }

    fn erase_row(&self, name: &str, row: &[EggValue]) {
        let mut engine = self.engine();
        let db = engine.database_mut();
        let table = db.table(name).expect("IR table");
        let arity = db.schema().table(table).key_arity();
        db.remove(table, &row[..arity]);
    }

    fn erase_by(&self, name: &str, columns: &[usize], key: &[EggValue]) {
        for row in self.indexed(name, columns, key, &[]) {
            self.erase_row(name, &row);
        }
    }

    fn indexed(
        &self,
        name: &str,
        key_columns: &[usize],
        keys: &[EggValue],
        order: &[usize],
    ) -> Vec<Vec<EggValue>> {
        let mut engine = self.engine();
        let db = engine.database_mut();
        let table = db
            .table(name)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let index = db.add_kept_index(table, key_columns, order);
        db.listed(index, keys)
            .map(|row| row.values().collect())
            .collect()
    }

    pub fn has_operation(&self, id: OpId) -> bool {
        self.record::<OperationRow>(id.raw()).is_some()
    }
    pub fn has_value(&self, id: ValueId) -> bool {
        self.record::<ValueRow>(id.number()).is_some()
    }
    pub fn has_region(&self, id: RegionId) -> bool {
        self.record::<RegionRow>(id.raw()).is_some()
    }
    pub fn has_block(&self, id: BlockId) -> bool {
        self.record::<BlockRow>(id.raw()).is_some()
    }

    pub fn get_value(&self, id: ValueId) -> Value {
        let row = self.record::<ValueRow>(id.number()).expect("live value");
        Value::new(
            id,
            TypeId::from_number(row.ty as u32),
            (row.definition == "result").then(|| OpId::new(row.owner as u32)),
        )
    }
    pub fn region_of_port(&self, id: ValueId) -> Option<RegionId> {
        let row = self.record::<ValueRow>(id.number())?;
        (row.definition == "region").then(|| RegionId::new(row.owner as u32))
    }
    pub fn block_of_argument(&self, id: ValueId) -> Option<BlockId> {
        let row = self.record::<ValueRow>(id.number())?;
        (row.definition == "block").then(|| BlockId::new(row.owner as u32))
    }
    pub fn parent_block(&self, op: OpId) -> Option<BlockId> {
        let row = self.record::<OperationRow>(op.raw())?;
        (row.placement == "block").then(|| BlockId::new(row.container as u32))
    }
    pub fn parent_nodes_region(&self, op: OpId) -> Option<RegionId> {
        let row = self.record::<OperationRow>(op.raw())?;
        (row.placement == "graph").then(|| RegionId::new(row.container as u32))
    }
    pub fn region_of_op(&self, op: OpId) -> Option<RegionId> {
        let row = self.record::<OperationRow>(op.raw())?;
        match row.placement.as_str() {
            "graph" => Some(RegionId::new(row.container as u32)),
            "block" => self.parent_region(BlockId::new(row.container as u32)),
            _ => None,
        }
    }
    pub fn parent_op(&self, op: OpId) -> Option<OpId> {
        self.region_parent_op(self.region_of_op(op)?)
    }
    pub fn parent_region(&self, b: BlockId) -> Option<RegionId> {
        u32::try_from(self.record::<BlockRow>(b.raw())?.region)
            .ok()
            .map(RegionId::new)
    }
    pub(crate) fn region_parent_op(&self, r: RegionId) -> Option<OpId> {
        u32::try_from(self.record::<RegionRow>(r.raw())?.owner)
            .ok()
            .map(OpId::new)
    }
    pub(crate) fn region_entry(&self, region: RegionId) -> Option<BlockId> {
        u32::try_from(self.record::<RegionRow>(region.raw())?.entry)
            .ok()
            .map(BlockId::new)
    }
    pub(crate) fn region_is_nodes(&self, r: RegionId) -> bool {
        self.record::<RegionRow>(r.raw())
            .is_some_and(|row| row.mode == "graph")
    }
    pub(crate) fn op_operands(&self, op: OpId) -> crate::operation::ValueIds {
        self.indexed("Operand", &[0], &[self.int(op.raw())], &[1])
            .into_iter()
            .map(|row| ValueId::from_number(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn op_results(&self, op: OpId) -> crate::operation::ValueIds {
        self.definitions(Defines::Result(op)).collect()
    }
    fn definitions(&self, definition: Defines) -> impl Iterator<Item = ValueId> {
        let (kind, owner) = definition.key();
        self.indexed("Value", &[2, 3], &[self.text(kind), self.int(owner)], &[4])
            .into_iter()
            .map(|r| ValueId::from_number(self.integer(r[0]) as u32))
    }
    pub(crate) fn op_regions(&self, op: OpId) -> crate::operation::RegionIds {
        self.indexed("Region", &[2], &[self.int(op.raw())], &[3])
            .into_iter()
            .map(|r| RegionId::new(self.integer(r[0]) as u32))
            .collect()
    }
    pub(crate) fn block_arguments(&self, block: BlockId) -> Vec<Value> {
        self.definitions(Defines::BlockArgument(block))
            .map(|v| self.declared_value(v))
            .collect()
    }
    fn declared_value(&self, id: ValueId) -> Value {
        let ty = self
            .record::<ValueRow>(id.number())
            .expect("parameter")
            .declared;
        Value::new(id, TypeId::from_number(ty as u32), None)
    }
    pub(crate) fn region_ports(&self, region: RegionId) -> Vec<Value> {
        self.definitions(Defines::RegionPort(region))
            .map(|v| self.declared_value(v))
            .collect()
    }
    pub(crate) fn region_results(&self, region: RegionId) -> Vec<ValueId> {
        self.indexed("Yield", &[0], &[self.int(region.raw())], &[1])
            .into_iter()
            .map(|row| ValueId::from_number(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn block_ops(&self, block: BlockId) -> Vec<OpId> {
        self.members("block", block.raw(), 4)
    }
    pub(crate) fn block_tail_position(&self, block: BlockId) -> Option<i64> {
        self.member_tail("block", block.raw()).1
    }
    // A native container ID belongs to one block or graph. Raw IR may use
    // the same number in both spaces, in which case select the placement too.
    fn member_tail(&self, placement: &str, owner: u32) -> (usize, Option<i64>) {
        let mut engine = self.engine();
        let db = engine.database_mut();
        let table = db.table("Operation").expect("operation table");
        let index = db.add_kept_index(table, &[3], &[2, 4, 5]);
        let owner = db.base(i64::from(owner));
        let kind = db.base(placement.to_owned());
        let key = [owner];
        let count = db.listed_len(index, &key);
        if count == 0 {
            return (0, None);
        }
        let first = db.listed_nth(index, &key, 0).expect("first member");
        let last = db.listed_nth(index, &key, count - 1).expect("last member");
        if first[2] == kind && last[2] == kind {
            return (count, Some(db.base_value(last[4])));
        }
        let mut count = 0;
        let last = db
            .listed(index, &key)
            .filter(|row| row[2] == kind)
            .map(|row| {
                count += 1;
                db.base_value(row[4])
            })
            .last();
        (count, last)
    }
    fn members(&self, placement: &str, owner: u32, order: usize) -> Vec<OpId> {
        self.indexed(
            "Operation",
            &[2, 3],
            &[self.text(placement), self.int(owner)],
            &[order],
        )
        .into_iter()
        .map(|r| OpId::new(self.integer(r[0]) as u32))
        .collect()
    }
    pub(crate) fn region_ops(&self, region: RegionId) -> Vec<OpId> {
        self.members("graph", region.raw(), 5)
    }
    pub(crate) fn region_blocks(&self, region: RegionId) -> Vec<BlockId> {
        self.indexed("Block", &[1], &[self.int(region.raw())], &[2])
            .into_iter()
            .map(|r| BlockId::new(self.integer(r[0]) as u32))
            .collect()
    }
    pub(crate) fn subtree_ops(&self, roots: &[OpId]) -> Vec<OpId> {
        self.0.queries.structure(&mut self.engine());
        roots
            .iter()
            .filter(|&&root| self.has_operation(root))
            .flat_map(|&root| {
                let mut ops = vec![root];
                for region in self.op_regions(root) {
                    ops.extend(
                        self.indexed("ReaderAt", &[1], &[self.int(region.raw())], &[])
                            .into_iter()
                            .map(|r| OpId::new(self.integer(r[0]) as u32)),
                    );
                }
                ops
            })
            .collect()
    }
    pub(crate) fn region_under(&self, region: RegionId, root: RegionId) -> bool {
        self.0.queries.structure(&mut self.engine());
        (region == root && self.has_region(root))
            || self.region_parent_op(region).is_some_and(|op| {
                !self
                    .indexed(
                        "ReaderAt",
                        &[0, 1],
                        &[self.int(op.raw()), self.int(root.raw())],
                        &[],
                    )
                    .is_empty()
            })
    }
    pub fn uses_of(&self, value: ValueId) -> Vec<Use> {
        self.indexed("Operand", &[2], &[self.int(value.number())], &[])
            .into_iter()
            .map(|r| {
                Use::new(
                    OpId::new(self.integer(r[0]) as u32),
                    self.integer(r[1]) as usize,
                )
            })
            .collect()
    }
    pub fn is_used(&self, value: ValueId) -> bool {
        !self.uses_of(value).is_empty()
    }
    pub fn result_uses(&self, value: ValueId) -> Vec<(RegionId, usize)> {
        self.indexed("Yield", &[2], &[self.int(value.number())], &[])
            .into_iter()
            .map(|row| {
                (
                    RegionId::new(self.integer(row[0]) as u32),
                    self.integer(row[1]) as usize,
                )
            })
            .collect()
    }

    pub(crate) fn sibling_reads(&self, root: RegionId) -> Vec<(OpId, OpId)> {
        self.0.queries.structure(&mut self.engine());
        self.indexed("DependsOn", &[0], &[self.int(root.raw())], &[])
            .into_iter()
            .map(|r| {
                (
                    OpId::new(self.integer(r[1]) as u32),
                    OpId::new(self.integer(r[2]) as u32),
                )
            })
            .collect()
    }
    fn normalize_binding(&self, region: RegionId, binding: &crate::Binding) {
        for (role, pairs) in [
            ("entry", binding.ports.clone().zip(binding.operands.clone())),
            ("exit", binding.results.clone().zip(binding.exit.clone())),
            (
                "feedback",
                binding.ports.clone().zip(binding.continue_.clone()),
            ),
        ] {
            for (from, to) in pairs {
                self.put(
                    "Binding",
                    vec![
                        self.int(region.raw()),
                        self.text(role),
                        self.int(from as i64),
                    ],
                    self.int(to as i64),
                );
            }
        }
    }
    fn normalize_op(&self, id: OpId) {
        if self.0.constructing.get() == Some(id) {
            return;
        }
        let key = self.int(id.raw());
        let op = self.get_op(id);
        for name in ["Successor", "SuccessorArgument", "Arm", "Control"] {
            self.erase_by(name, &[0], &[key]);
        }
        for reg in op.regions() {
            self.erase_by("Binding", &[0], &[self.int(reg.raw())]);
        }
        if let Some(branch) = op.clone().as_interface::<dyn crate::BranchTerminator>() {
            for (edge, (block, slots)) in branch.successor_operand_slots().into_iter().enumerate() {
                self.put(
                    "Successor",
                    vec![key, self.int(edge as i64)],
                    self.int(block.raw()),
                );
                for (param, operand) in slots.into_iter().enumerate() {
                    self.put(
                        "SuccessorArgument",
                        vec![key, self.int(edge as i64), self.int(param as i64)],
                        self.int(operand as i64),
                    );
                }
            }
        }
        let gamma = op.clone().as_interface::<dyn crate::Gamma>();
        let theta = op.clone().as_interface::<dyn crate::Theta>();
        if let Some(g) = &gamma {
            let arms = g.arms();
            let binding = g.binding();
            for r in &arms {
                self.normalize_binding(*r, &binding);
            }
            for (case, r) in g.cases().into_iter().zip(&arms) {
                self.put(
                    "Arm",
                    vec![key, self.boolean(false), self.text(case.to_string())],
                    self.int(r.raw()),
                );
            }
            if let Some(r) = arms.last() {
                self.put(
                    "Arm",
                    vec![key, self.boolean(true), self.text("")],
                    self.int(r.raw()),
                );
            }
        }
        if let Some(t) = &theta {
            self.normalize_binding(t.body(), &t.binding());
        }
        if gamma.is_some() || theta.is_some() {
            self.insert_row(
                "Control",
                (
                    i64::from(id.raw()),
                    gamma.is_some(),
                    gamma.as_ref().map_or(-1, |g| g.predicate_slot() as i64),
                    theta.is_some(),
                    theta.as_ref().map_or(-1, |t| i64::from(t.body().raw())),
                    theta.as_ref().map_or(-1, |t| t.predicate_slot() as i64),
                    if theta.is_some() { "true" } else { "none" },
                ),
            );
        }
        if let Some(b) = op.as_interface::<dyn crate::RegionBinding>() {
            self.normalize_binding(b.region(), &b.binding());
        }
    }
    fn stored_attribute(&self, id: EggValue) -> AttributeValue {
        (*attribute_store::get(self, id)).clone()
    }
    fn set_attribute(&self, kind: &str, id: u32, name: &str, value: AttributeValue) {
        let handle = attribute_store::encode(self, value);
        self.put(
            "Attribute",
            vec![self.text(kind), self.int(id), self.text(name)],
            handle,
        );
    }
    fn attributes(&self, kind: &str, id: u32) -> Vec<NamedAttribute> {
        self.indexed("Attribute", &[0, 1], &[self.text(kind), self.int(id)], &[])
            .into_iter()
            .map(|r| {
                NamedAttribute::new(self.intern(&self.string(r[2])), self.stored_attribute(r[3]))
            })
            .collect()
    }
    pub(crate) fn op_attributes(&self, op: OpId) -> Vec<NamedAttribute> {
        self.attributes("operation", op.raw())
    }
    pub(crate) fn block_attributes(&self, b: BlockId) -> Vec<NamedAttribute> {
        self.attributes("block", b.raw())
    }
    pub(crate) fn op_attr_sym(&self, op: OpId, name: Sym) -> Option<AttributeValue> {
        self.lookup(
            "Attribute",
            ("operation", i64::from(op.raw()), self.resolve(name)),
        )
        .map(|v| self.stored_attribute(v))
    }
    pub(crate) fn block_attr(&self, b: BlockId, name: &str) -> Option<AttributeValue> {
        self.lookup("Attribute", ("block", i64::from(b.raw()), name))
            .map(|v| self.stored_attribute(v))
    }
    pub fn with_attr<R>(
        &self,
        op: OpId,
        name: &str,
        read: impl FnOnce(&AttributeValue) -> R,
    ) -> Option<R> {
        let id = self.lookup("Attribute", ("operation", i64::from(op.raw()), name))?;
        let value = attribute_store::get(self, id);
        Some(read(&value))
    }
    pub(crate) fn set_block_attr(&self, b: BlockId, name: &str, value: AttributeValue) {
        self.set_attribute("block", b.raw(), name, value);
    }
    pub fn set_op_attributes(&self, op: OpId, attributes: Vec<NamedAttribute>) {
        self.erase_by(
            "Attribute",
            &[0, 1],
            &[self.text("operation"), self.int(op.raw())],
        );
        for attr in attributes {
            self.set_attribute("operation", op.raw(), &self.resolve(attr.name), attr.value);
        }
        self.normalize_op(op);
    }
    pub fn create_value(&self, ty: TypeId, defining_op: Option<OpId>) -> Value {
        let id = ValueId::from_number(self.mint());
        let mut row = ValueRow {
            id: id.number(),
            ty: i64::from(ty.number()),
            definition: "none".into(),
            owner: -1,
            slot: -1,
            declared: -1,
        };
        if let Some(op) = defining_op {
            row.definition = "result".into();
            row.owner = i64::from(op.raw());
            row.slot = self.op_results(op).len() as i64;
        }
        row.write(self);
        Value::new(id, ty, defining_op)
    }
    fn define_value(&self, value: ValueId, definition: Defines, slot: usize) {
        let (kind, owner) = definition.key();
        self.edit_record::<ValueRow>(value.number(), |r| {
            r.definition = kind.into();
            r.owner = i64::from(owner);
            r.slot = slot as i64;
            if kind != "result" {
                r.declared = r.ty;
            }
        });
    }
    fn detach_value(&self, id: ValueId) {
        self.edit_record::<ValueRow>(id.number(), |r| {
            r.definition = "none".into();
            r.owner = -1;
            r.slot = -1;
        });
    }
    pub fn add_operation(
        &self,
        insertion: crate::InsertionPoint,
        op: crate::operation::NewOp,
    ) -> OpHandle {
        let id = OpId::new(self.mint());
        self.0.constructing.set(Some(id));
        let (dialect, name) = op.name;
        OperationRow {
            id: id.raw(),
            kind: format!("{dialect}.{name}"),
            placement: "root".into(),
            container: -1,
            position: -1,
            display: -1,
            location: self.0.origin.get().map_or(-1, |s| i64::from(s.0)),
        }
        .write(self);
        self.set_op_operands(id, op.operands);
        for (slot, v) in op.results.into_iter().enumerate() {
            self.define_value(v, Defines::Result(id), slot);
        }
        for (slot, r) in op.regions.into_iter().enumerate() {
            self.edit_record::<RegionRow>(r.raw(), |r| {
                r.owner = i64::from(id.raw());
                r.slot = slot as i64;
            });
        }
        self.set_op_attributes(id, op.attributes);
        self.0.constructing.set(None);
        self.insert(insertion, id);
        self.normalize_op(id);
        self.get_op(id)
    }
    pub fn op_location(&self, op: OpId) -> Option<crate::parse::Span> {
        u32::try_from(self.record::<OperationRow>(op.raw())?.location)
            .ok()
            .map(crate::parse::Span)
    }
    pub fn set_origin(&self, location: Option<crate::parse::Span>) -> Option<crate::parse::Span> {
        self.0.origin.replace(location)
    }
    pub fn retype_value(&self, value: ValueId, ty: TypeId) {
        self.edit_record::<ValueRow>(value.number(), |r| r.ty = i64::from(ty.number()));
    }
    pub fn retype_block_argument(&self, block: BlockId, index: usize, ty: TypeId) {
        if let Some(value) = self.block_arguments(block).get(index) {
            self.edit_record::<ValueRow>(value.id().number(), |r| {
                r.ty = i64::from(ty.number());
                r.declared = r.ty;
            });
        }
    }
    pub fn set_op_operand(&self, op: OpId, index: usize, value: ValueId) {
        self.put(
            "Operand",
            vec![self.int(op.raw()), self.int(index as i64)],
            self.int(value.number()),
        );
    }
    pub fn set_op_operands(&self, op: OpId, operands: Vec<ValueId>) {
        self.write_op_operands(op, operands);
        self.normalize_op(op);
    }
    fn write_op_operands(&self, op: OpId, operands: Vec<ValueId>) {
        for row in self.indexed("Operand", &[0], &[self.int(op.raw())], &[]) {
            self.delete("Operand", row[..2].to_vec());
        }
        for (slot, value) in operands.into_iter().enumerate() {
            self.set_op_operand(op, slot, value);
        }
    }
    pub fn set_op_result(&self, op: OpId, index: usize, value: ValueId) {
        if let Some(old) = self.op_results(op).get(index) {
            self.detach_value(*old);
        }
        self.define_value(value, Defines::Result(op), index);
    }
    pub fn replace_value_uses(&self, old: ValueId, new: ValueId) {
        if old == new {
            return;
        }
        for use_site in self.uses_of(old) {
            self.set_op_operand(use_site.op, use_site.index, new);
        }
    }
    pub fn create_region(&self) -> RegionHandle {
        let id = RegionId::new(self.mint());
        RegionRow {
            id: id.raw(),
            mode: "cfg".into(),
            owner: -1,
            slot: -1,
            entry: -1,
        }
        .write(self);
        self.get_region(id)
    }
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
    pub(crate) fn set_region_nodes(
        &self,
        region: RegionId,
        ports: Vec<Value>,
        ops: Vec<OpId>,
        results: Vec<ValueId>,
    ) {
        self.edit_record::<RegionRow>(region.raw(), |r| r.mode = "graph".into());
        for (slot, value) in ports.into_iter().enumerate() {
            self.define_value(value.id(), Defines::RegionPort(region), slot);
        }
        self.set_region_results(region, results);
        for op in ops {
            self.insert(crate::InsertionPoint::Region(region), op);
        }
    }
    pub fn insertion_point(&self, op: OpId) -> crate::InsertionPoint {
        use crate::InsertionPoint;
        if let Some(region) = self.parent_nodes_region(op) {
            return InsertionPoint::Region(region);
        }
        match self.parent_block(op) {
            Some(block) => InsertionPoint::Block {
                block,
                position: self
                    .block_ops(block)
                    .iter()
                    .take_while(|&&held| held != op)
                    .count(),
            },
            None => InsertionPoint::Root,
        }
    }

    pub fn insert(&self, insertion: crate::InsertionPoint, op: OpId) {
        use crate::InsertionPoint;
        match insertion {
            InsertionPoint::Root => self.place(op, Placement::Root),
            InsertionPoint::Region(region) => {
                let (position, _) = self.member_tail("graph", region.raw());
                self.place(op, Placement::Graph(region, position));
            }
            InsertionPoint::Block {
                block,
                mut position,
            } => {
                let mut ops = self.block_ops(block);
                if let Some(previous) = ops.iter().position(|&held| held == op) {
                    ops.remove(previous);
                    position -= usize::from(previous < position);
                }
                ops.insert(position, op);
                self.set_block_ops(block, ops);
            }
        }
    }
    fn place(&self, op: OpId, placement: Placement) {
        self.edit_record::<OperationRow>(op.raw(), |row| {
            let (kind, container, position) = match placement {
                Placement::Root => ("root", -1, -1),
                Placement::Block(block, slot) => ("block", i64::from(block.raw()), slot as i64),
                Placement::Graph(region, display) => {
                    row.display = display as i64;
                    ("graph", i64::from(region.raw()), -1)
                }
            };
            row.placement = kind.into();
            row.container = container;
            row.position = position;
        });
    }
    pub fn set_region_ops(&self, region: RegionId, ops: Vec<OpId>) {
        for (position, op) in ops.into_iter().enumerate() {
            self.place(op, Placement::Graph(region, position));
        }
    }
    pub fn set_region_results(&self, region: RegionId, results: Vec<ValueId>) {
        for row in self.indexed("Yield", &[0], &[self.int(region.raw())], &[]) {
            self.delete("Yield", row[..2].to_vec());
        }
        for (slot, value) in results.into_iter().enumerate() {
            self.put(
                "Yield",
                vec![self.int(region.raw()), self.int(slot as i64)],
                self.int(value.number()),
            );
        }
        if let Some(owner) = self.region_parent_op(region) {
            self.normalize_op(owner);
        }
    }
    pub fn create_block(&self, arguments: Vec<Value>) -> BlockHandle {
        let id = BlockId::new(self.mint());
        BlockRow {
            id: id.raw(),
            region: -1,
            layout: 0,
        }
        .write(self);
        for (slot, value) in arguments.into_iter().enumerate() {
            self.define_value(value.id(), Defines::BlockArgument(id), slot);
        }
        self.get_block(id)
    }
    pub(crate) fn add_block_to_region(&self, region: RegionId, block: BlockId) {
        let slot = self.region_blocks(region).len();
        self.edit_record::<BlockRow>(block.raw(), |r| {
            r.region = i64::from(region.raw());
            r.layout = slot as i64;
        });
        if slot == 0 {
            self.edit_record::<RegionRow>(region.raw(), |r| r.entry = i64::from(block.raw()));
        }
    }
    pub(crate) fn remove_block_from_region(&self, region: RegionId, block: BlockId) -> bool {
        if self.parent_region(block) != Some(region) {
            return false;
        }
        self.edit_record::<BlockRow>(block.raw(), |r| r.region = -1);
        if self
            .record::<RegionRow>(region.raw())
            .expect("live region")
            .entry
            == i64::from(block.raw())
        {
            let next = self
                .region_blocks(region)
                .first()
                .map_or(-1, |b| i64::from(b.raw()));
            self.edit_record::<RegionRow>(region.raw(), |r| r.entry = next);
        }
        true
    }
    pub fn reorder_region_blocks(
        &self,
        region: RegionId,
        blocks: Vec<BlockId>,
    ) -> Result<(), String> {
        let original = self.region_blocks(region);
        let mut remaining: HashSet<_> = original.iter().copied().collect();
        if self.region_is_nodes(region)
            || blocks.len() != original.len()
            || blocks.first() != original.first()
            || blocks.iter().any(|block| !remaining.remove(block))
        {
            return Err("block order must be a permutation preserving the entry".into());
        }
        for (slot, block) in blocks.into_iter().enumerate() {
            self.edit_record::<BlockRow>(block.raw(), |r| r.layout = slot as i64);
        }
        Ok(())
    }
    pub(crate) fn set_block_ops(&self, block: BlockId, ops: Vec<OpId>) {
        for (slot, op) in ops.into_iter().enumerate() {
            self.place(op, Placement::Block(block, slot));
        }
    }
    pub(crate) fn remove_op_from_block(&self, block: BlockId, op: OpId) -> bool {
        if self.parent_block(op) != Some(block) {
            return false;
        }
        self.place(op, Placement::Root);
        true
    }
    pub(crate) fn replace_op_in_block(&self, block: BlockId, old: OpId, new: OpId) -> bool {
        let Some(row) = self.record::<OperationRow>(old.raw()) else {
            return false;
        };
        if row.placement != "block" || row.container != i64::from(block.raw()) {
            return false;
        }
        self.place(new, Placement::Block(block, row.position as usize));
        self.place(old, Placement::Root);
        true
    }
    pub fn remove_from_region(&self, region: RegionId, op: OpId) {
        if self.parent_nodes_region(op) == Some(region) {
            self.place(op, Placement::Root);
        }
    }
    pub fn append_block_argument(&self, block: BlockId, ty: TypeId) -> Value {
        let value = self.create_value(ty, None);
        self.adopt_block_argument(block, value.id());
        value
    }
    pub fn adopt_block_argument(&self, block: BlockId, value: ValueId) {
        self.define_value(
            value,
            Defines::BlockArgument(block),
            self.block_arguments(block).len(),
        );
    }
    pub fn remove_block_argument(&self, block: BlockId, index: usize) {
        let mut values = self.block_arguments(block);
        let removed = values.remove(index);
        self.detach_value(removed.id());
        for (slot, value) in values.into_iter().enumerate() {
            self.define_value(value.id(), Defines::BlockArgument(block), slot);
        }
    }
    fn edit_segments(&self, op: OpId, edit: impl FnOnce(&mut [AttributeValue])) {
        let sym = self.registry().segment_sizes;
        if let Some(AttributeValue::Array(mut values)) = self.op_attr_sym(op, sym) {
            edit(&mut values);
            self.set_attribute(
                "operation",
                op.raw(),
                &self.resolve(sym),
                AttributeValue::Array(values),
            );
        }
    }
    pub fn append_operand(&self, op: OpId, value: ValueId) {
        self.set_op_operand(op, self.op_operands(op).len(), value);
        self.edit_segments(op, |sizes| {
            if let Some(AttributeValue::UInt(last)) = sizes.last_mut() {
                *last += 1;
            }
        });
        self.normalize_op(op);
    }
    pub fn adopt_result(&self, op: OpId, value: ValueId) {
        self.insert_result_at(op, self.op_results(op).len(), value);
    }
    pub fn append_result(&self, op: OpId, ty: TypeId) -> ValueId {
        let value = self.create_value(ty, None).id();
        self.adopt_result(op, value);
        value
    }
    pub(crate) fn remove_operand(&self, op: OpId, index: usize) {
        let mut values = self.op_operands(op).to_vec();
        values.remove(index);
        self.write_op_operands(op, values);
        self.edit_segments(op, |sizes| {
            let mut start = 0;
            for size in sizes {
                if let AttributeValue::UInt(size) = size {
                    if index < start + *size as usize {
                        *size -= 1;
                        break;
                    }
                    start += *size as usize;
                }
            }
        });
        self.normalize_op(op);
    }
    pub(crate) fn insert_operand_at(
        &self,
        op: OpId,
        index: usize,
        value: ValueId,
        group_end: usize,
    ) {
        let mut values = self.op_operands(op).to_vec();
        values.insert(index, value);
        self.write_op_operands(op, values);
        self.edit_segments(op, |sizes| {
            let mut end = 0;
            let mut chosen = None;
            for (i, size) in sizes.iter().enumerate() {
                if let AttributeValue::UInt(size) = size {
                    end += *size as usize;
                    if end == group_end {
                        chosen = Some(i);
                    }
                }
            }
            if let Some(AttributeValue::UInt(size)) = chosen.and_then(|i| sizes.get_mut(i)) {
                *size += 1;
            }
        });
        self.normalize_op(op);
    }
    fn set_results(&self, op: OpId, values: &[ValueId]) {
        for old in self.op_results(op) {
            self.detach_value(old);
        }
        for (slot, value) in values.iter().enumerate() {
            self.define_value(*value, Defines::Result(op), slot);
        }
    }
    pub(crate) fn insert_result_at(&self, op: OpId, index: usize, value: ValueId) {
        let mut values = self.op_results(op).to_vec();
        values.insert(index, value);
        self.set_results(op, &values);
    }
    pub(crate) fn remove_result(&self, op: OpId, index: usize) {
        let mut values = self.op_results(op).to_vec();
        values.remove(index);
        self.set_results(op, &values);
    }
    pub(crate) fn remove_region_port(&self, region: RegionId, index: usize) {
        let mut values = self.region_ports(region);
        let removed = values.remove(index);
        self.detach_value(removed.id());
        for (slot, value) in values.into_iter().enumerate() {
            self.define_value(value.id(), Defines::RegionPort(region), slot);
        }
    }
    pub(crate) fn insert_region_port(&self, region: RegionId, index: usize, value: Value) {
        let owner = Defines::RegionPort(region);
        let ports: Vec<_> = self.definitions(owner).collect();
        for (slot, &port) in ports[index..].iter().enumerate().rev() {
            self.define_value(port, owner, index + slot + 1);
        }
        self.define_value(value.id(), owner, index);
    }
    pub(crate) fn insert_region_result(&self, region: RegionId, index: usize, value: ValueId) {
        let mut values = self.region_results(region);
        values.insert(index, value);
        self.set_region_results(region, values);
    }
    pub(crate) fn remove_operation(&self, op: OpId) {
        self.remove_operation_except(op, &[]);
    }
    pub(crate) fn remove_operation_except(&self, root: OpId, keep: &[ValueId]) {
        let ops = self.subtree_ops(&[root]);
        for op in ops {
            for r in self.op_regions(op) {
                for b in self.region_blocks(r) {
                    for v in self.definitions(Defines::BlockArgument(b)) {
                        if !keep.contains(&v) {
                            self.erase_by("Value", &[0], &[self.int(v.number())]);
                        }
                    }
                    self.erase_by(
                        "Attribute",
                        &[0, 1],
                        &[self.text("block"), self.int(b.raw())],
                    );
                    self.erase_by("Block", &[0], &[self.int(b.raw())]);
                }
                for v in self.definitions(Defines::RegionPort(r)) {
                    if !keep.contains(&v) {
                        self.erase_by("Value", &[0], &[self.int(v.number())]);
                    }
                }
                for table in ["Yield", "Binding", "Region"] {
                    self.erase_by(table, &[0], &[self.int(r.raw())]);
                }
            }
            for v in self.definitions(Defines::Result(op)) {
                if !keep.contains(&v) {
                    self.erase_by("Value", &[0], &[self.int(v.number())]);
                }
            }
            self.erase_by(
                "Attribute",
                &[0, 1],
                &[self.text("operation"), self.int(op.raw())],
            );
            for table in [
                "Operand",
                "Successor",
                "SuccessorArgument",
                "Arm",
                "Control",
                "Operation",
            ] {
                self.erase_by(table, &[0], &[self.int(op.raw())]);
            }
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
