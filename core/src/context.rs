use crate::{
    Dialect, Error, OpId, Operation, OperationParser, TypeId,
    attributes::{AttributeValue, NamedAttribute},
    block::{BlockHandle, BlockId},
    builtin::BuiltinDialect,
    dialects::{cfg::CfgDialect, func::FuncDialect, scf::ScfDialect, state::StateDialect},
    ir_formatter::IRFormatter,
    operation::{OpHandle, OpInterfaceConverter, OpNameId, downcast_op_interface},
    parse::text::Parser as IRParser,
    ptr::PtrDialect,
    region::{RegionHandle, RegionId},
    ty::{Spellings, Terms, TypeParser, TypeSpelling},
    value::{Use, Value, ValueId},
    vector::VectorDialect,
};
use std::rc::Rc;
use std::{
    any::Any,
    cell::{Cell, Ref, RefCell, RefMut},
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};
use tir_adt::{Interner, Sym};
use tir_egglog::{EGraph, RawValues, Read, Value as EggValue, Write};
use tir_symbolic::term::constructor_fields;

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
    interfaces: Vec<Vec<(std::any::TypeId, OpInterfaceConverter)>>,
    spellings: Spellings,
    names: Interner,
    op_names: Vec<OpName>,
    op_name_ids: HashMap<(&'static str, &'static str), OpNameId>,
    segment_sizes: Sym,
    core_types: bool,
    constructors: Option<Arc<HashMap<String, OpId>>>,
}
struct OpName {
    identity: (&'static str, &'static str),
    definition: Option<&'static crate::OpDefSpec>,
}
#[derive(Clone)]
pub struct Names(Rc<RefCell<Registry>>);
impl Names {
    /// The name `sym` was interned from.
    pub fn name(&self, sym: Sym) -> String {
        let registry = self.0.borrow();
        registry.names.resolve(sym).to_string()
    }

    /// The name whose [`Sym::index`] is `index`, if one is.
    pub fn name_at(&self, index: usize) -> Option<String> {
        let registry = self.0.borrow();
        registry.names.at(index).map(str::to_string)
    }

    /// Intern `name`, as [`Context::intern`] does.
    pub fn intern(&self, name: &str) -> Sym {
        let known = self.0.borrow();
        if let Some(sym) = known.names.lookup(name) {
            return sym;
        }
        drop(known);
        let mut registry = self.0.borrow_mut();
        registry.names.intern(name)
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
    defines_types: Cell<bool>,
    indexes_dirty: Arc<AtomicBool>,
    constructing: Cell<Option<OpId>>,
}

impl Context {
    pub fn new() -> Self {
        let mut engine = EGraph::default();
        engine
            .load_program(include_bytes!(concat!(env!("OUT_DIR"), "/ir-schema.bin")))
            .unwrap_or_else(|error| panic!("IR schema: {error}"));
        let engine = Rc::new(RefCell::new(engine));
        let next_id = Arc::new(AtomicU32::new(0));
        let indexes_dirty = Arc::new(AtomicBool::new(true));
        let terms = Rc::new(Terms::with_engine(
            engine.clone(),
            next_id.clone(),
            indexes_dirty.clone(),
        ));
        let mut names = Interner::new();
        for schema in crate::schema::OP_SCHEMAS {
            for attribute in schema.attributes {
                names.intern_static(attribute.name);
            }
        }
        let segment_sizes = names.intern("operand_segment_sizes");
        let mut op_names = Vec::new();
        let mut op_name_ids = HashMap::new();
        for definition in crate::schema::OP_DEFINITIONS {
            let schema = definition.schema;
            op_name_ids
                .entry((schema.dialect, schema.name))
                .or_insert_with(|| {
                    let id = OpNameId::new(op_names.len() as u32);
                    op_names.push(OpName {
                        identity: (schema.dialect, schema.name),
                        definition: Some(definition),
                    });
                    id
                });
        }
        Self(std::rc::Rc::new(Inner {
            registry: Rc::new(RefCell::new(Registry {
                dialects: HashMap::new(),
                reg_classes: HashMap::new(),
                interfaces: Vec::new(),
                spellings: Spellings::new(terms.clone()),
                names,
                op_names,
                op_name_ids,
                segment_sizes,
                core_types: false,
                constructors: None,
            })),
            engine,
            next_id,
            terms,
            origin: Cell::new(None),
            defines_types: Cell::new(false),
            indexes_dirty,
            constructing: Cell::new(None),
        }))
    }

    /// Direct access to the authoritative engine. Derived indexes are invalidated
    /// before the mutable guard is returned. Raw writes followed by queries
    /// within the same guard require running `ir-index` explicitly.
    pub fn egraph(&self) -> RefMut<'_, EGraph> {
        self.refresh_indexes();
        self.invalidate_indexes();
        self.engine()
    }

    /// Access the authoritative engine with support for adding egglog rules.
    /// The first call replaces the precompiled IR program with its source
    /// rules, preserving the database. Later calls retain user rules.
    #[cfg(feature = "egglog-rules")]
    pub fn egraph_with_rules(&self) -> Result<RefMut<'_, EGraph>, tir_egglog::Error> {
        use tir_egglog::{Rules, ast::Command};

        let mut engine = self.egraph();
        if engine.uses_loaded_program() {
            let mut rules = Rules::new();
            rules.add_ruleset("");
            for command in engine.parse_program(
                "IR schema",
                include_str!(concat!(env!("OUT_DIR"), "/ir-schema.egg")),
            )? {
                match command {
                    Command::AddRuleset(_, name) => rules.add_ruleset(&name),
                    Command::UnstableCombinedRuleset(_, name, members) => {
                        rules.add_combined_ruleset(&name, &members)?;
                    }
                    command @ Command::Rule { .. } => {
                        rules.add_command(engine.database(), command)?;
                    }
                    _ => {}
                }
            }
            engine.set_rules(rules)?;
        }
        Ok(engine)
    }

    fn engine(&self) -> RefMut<'_, EGraph> {
        self.0.engine.borrow_mut()
    }

    pub(crate) fn clone_engine(&self) -> EGraph {
        self.refresh_indexes();
        self.engine().clone()
    }

    // The caller rebuilds indexes before installation, including rollback.
    pub(crate) fn replace_engine(&self, engine: EGraph) -> EGraph {
        self.0.indexes_dirty.store(false, Ordering::Relaxed);
        std::mem::replace(&mut *self.engine(), engine)
    }

    /// Invalidate rule-derived facts after an external edit.
    pub fn invalidate_indexes(&self) {
        self.0.indexes_dirty.store(true, Ordering::Relaxed);
    }

    fn mint(&self) -> u32 {
        self.0.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn intern(&self, name: &str) -> Sym {
        self.registry_mut().names.intern(name)
    }
    pub fn intern_fp_semantics(
        &self,
        semantics: impl Into<crate::fp::Semantics>,
    ) -> Arc<crate::fp::Semantics> {
        Arc::new(semantics.into())
    }
    pub fn intern_evaluation_contract(
        &self,
        contract: impl Into<crate::fp::EvaluationContract>,
    ) -> Arc<crate::fp::EvaluationContract> {
        Arc::new(contract.into())
    }
    pub(crate) fn intern_op_name(&self, dialect: &'static str, name: &'static str) -> OpNameId {
        let mut registry = self.registry_mut();
        if let Some(id) = registry.op_name_ids.get(&(dialect, name)) {
            return *id;
        }
        let id = OpNameId::new(registry.op_names.len() as u32);
        let definition = crate::schema::OP_DEFINITIONS.iter().find(|definition| {
            definition.schema.dialect == dialect && definition.schema.name == name
        });
        registry.op_names.push(OpName {
            identity: (dialect, name),
            definition,
        });
        registry.op_name_ids.insert((dialect, name), id);
        id
    }
    pub(crate) fn defines_types(&self) -> bool {
        self.0.defines_types.get()
    }
    pub(crate) fn constructors(&self) -> Option<(Context, Arc<HashMap<String, OpId>>)> {
        if !self.registry().core_types {
            return None;
        }
        if let Some(names) = self.registry().constructors.clone() {
            return Some((self.clone(), names));
        }
        self.0.defines_types.set(true);
        let module = crate::dialects::layout::types::define(self);
        self.0.defines_types.set(false);
        let names = Arc::new(crate::layout::definitions_in(self, module.id()));
        self.registry_mut().constructors = Some(names.clone());
        Some((self.clone(), names))
    }
    pub(crate) fn scope_layouts(
        &self,
        _scope: OpId,
        build: impl FnOnce() -> crate::layout::ModuleLayouts,
    ) -> Arc<crate::layout::ModuleLayouts> {
        Arc::new(build())
    }
    pub(crate) fn module_type_definitions(&self, module: OpId) -> Arc<HashMap<String, OpId>> {
        Arc::new(crate::layout::definitions_in(self, module))
    }

    pub fn with_default_dialects() -> Self {
        let context = Context::new();

        context.register_dialect::<BuiltinDialect>();
        context.register_dialect::<crate::builtin::TypeDialect>();
        context.register_dialect::<crate::layout::LayoutDialect>();
        context.register_dialect::<CfgDialect>();
        context.register_dialect::<FuncDialect>();
        context.register_dialect::<crate::fp::FpDialect>();
        context.register_dialect::<PtrDialect>();
        context.register_dialect::<ScfDialect>();
        context.register_dialect::<StateDialect>();
        context.register_dialect::<crate::son::SonDialect>();
        context.register_dialect::<VectorDialect>();
        context.declare_core_types();

        context
    }

    fn declare_core_types(&self) {
        for (name, default) in crate::dialects::layout::types::defaults(self) {
            let Some(ctor) = self.terms().ctor_named("type", name) else {
                continue;
            };
            let default = crate::builtin::type_arg_of(&default)
                .expect("a constructor's default is a type or a flag");
            self.terms()
                .set_default(ctor, default)
                .expect("the core type definitions give each constructor one default");
        }
        self.registry_mut().core_types = true;
    }

    pub(crate) fn core_type_names(&self) -> Option<&'static [&'static str]> {
        self.registry()
            .core_types
            .then_some(&crate::dialects::layout::types::NAMES[..])
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
        self.registry().names.lookup(name)
    }

    pub fn resolve(&self, sym: Sym) -> String {
        self.registry().names.resolve(sym).to_string()
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

    pub fn register_operation(&self, definition: &'static crate::OpDefSpec) {
        let (dialect, name) = (definition.schema.dialect, definition.schema.name);
        let id = self.intern_op_name(dialect, name);
        self.registry_mut().op_names[id.index()].definition = Some(definition);
        for &(interface, interface_name, converter) in definition.interfaces {
            self.register_interface(dialect, name, interface(), interface_name(), converter);
        }
    }
    pub fn register_op_interface<I: ?Sized + 'static>(
        &self,
        dialect: &'static str,
        name: &'static str,
        converter: OpInterfaceConverter,
    ) {
        self.register_interface(
            dialect,
            name,
            std::any::TypeId::of::<I>(),
            std::any::type_name::<I>(),
            converter,
        );
    }
    fn register_interface(
        &self,
        dialect: &'static str,
        name: &'static str,
        interface: std::any::TypeId,
        interface_name: &'static str,
        converter: OpInterfaceConverter,
    ) {
        let id = self.intern_op_name(dialect, name);
        let mut registry = self.registry_mut();
        if registry.interfaces.len() <= id.index() {
            registry.interfaces.resize_with(id.index() + 1, Vec::new);
        }
        let held = &mut registry.interfaces[id.index()];
        if let Some(entry) = held.iter_mut().find(|(ty, _)| *ty == interface) {
            entry.1 = converter;
        } else {
            held.push((interface, converter));
        }
        drop(registry);
        self.engine()
            .update(|mut state| {
                state.add("Implements", (format!("{dialect}.{name}"), interface_name))?;
                Ok(())
            })
            .unwrap_or_else(|error| panic!("operation interface: {error}"));
        self.invalidate_indexes();
    }

    pub(crate) fn get_dyn_op(&self, op: OpHandle) -> Box<dyn Operation> {
        Box::new(op)
    }

    pub(crate) fn get_op_interface<I: ?Sized + 'static>(&self, op: OpHandle) -> Option<Box<I>> {
        let converter = self.find_op_interface::<I>(self.op_name_id(op.id))?;
        let erased = converter(op);
        downcast_op_interface::<I>(erased)
    }

    pub(crate) fn find_op_interface<I: ?Sized + 'static>(
        &self,
        name: OpNameId,
    ) -> Option<OpInterfaceConverter> {
        let interface = std::any::TypeId::of::<I>();
        self.registry()
            .interfaces
            .get(name.index())?
            .iter()
            .find(|(ty, _)| *ty == interface)
            .map(|(_, converter)| *converter)
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

    pub(crate) fn states_of(
        &self,
        values: impl IntoIterator<Item = ValueId>,
        resource: crate::builtin::StateResource,
    ) -> Vec<ValueId> {
        values
            .into_iter()
            .filter(|value| self.state_resource(self.get_value(*value).ty()) == Some(resource))
            .collect()
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
        self.invalidate_indexes();
    }

    fn delete(&self, table: &str, keys: Vec<EggValue>) {
        self.engine()
            .update(|mut state| state.remove(table, RawValues(keys)))
            .unwrap_or_else(|error| panic!("{table}: {error}"));
        self.invalidate_indexes();
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

    fn construct(&self, name: &str, fields: Vec<EggValue>) -> EggValue {
        self.engine()
            .update(|mut state| state.add(name, RawValues(fields)))
            .unwrap_or_else(|error| panic!("{name}: {error}"))
    }

    fn fields(&self, name: &str, expression: EggValue) -> Option<Vec<EggValue>> {
        constructor_fields(&mut self.engine(), name, expression)
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

    fn refresh_indexes(&self) {
        if !self.0.indexes_dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        Self::refresh_engine_indexes(&mut self.engine())
            .unwrap_or_else(|error| panic!("IR indexes: {error}"));
    }

    pub(crate) fn refresh_engine_indexes(engine: &mut EGraph) -> Result<(), tir_egglog::Error> {
        for table in [
            "TypeItem",
            "KindItem",
            "RowItem",
            "AttrItem",
            "DictItem",
            "OwnedRegion",
            "HasInterface",
            "DefinedValue",
            "Result",
            "BlockParam",
            "RegionParam",
            "Uses",
            "EdgeArgument",
            "GammaSelector",
            "EntryValue",
            "ExitValue",
            "FeedbackValue",
            "ParentOp",
            "Within",
            "Module",
            "Function",
            "OwningFunction",
            "SelectedOp",
            "GraphMember",
            "BlockMember",
            "BlocksOf",
            "OpRegion",
            "RegionUnder",
            "OpUnderRegion",
            "ReaderAt",
            "SiblingRead",
            "ScopedSiblingRead",
            "OwnedEntity",
        ] {
            engine.clear_function(table)?;
        }
        engine.seminaive = false;
        let schedule = tir_egglog::ast::Command::RunSchedule(tir_egglog::Schedule::Saturate(
            tir_egglog::span!(),
            Box::new(tir_egglog::Schedule::run("ir-index")),
        ));
        engine.run_program(vec![schedule])?;
        Ok(())
    }

    pub fn has_operation(&self, id: OpId) -> bool {
        self.lookup("OpKind", i64::from(id.raw())).is_some()
    }
    pub fn has_value(&self, id: ValueId) -> bool {
        self.lookup("ValueType", i64::from(id.number())).is_some()
    }
    pub fn has_region(&self, id: RegionId) -> bool {
        self.lookup("Mode", i64::from(id.raw())).is_some()
    }
    pub fn has_block(&self, id: BlockId) -> bool {
        self.lookup("BlockLayout", i64::from(id.raw())).is_some()
    }

    pub fn get_value(&self, id: ValueId) -> Value {
        let ty = self
            .lookup("ValueType", i64::from(id.number()))
            .unwrap_or_else(|| panic!("live value {id:?}"));
        let defining = self
            .definition(id, "ResultDef")
            .map(|fields| OpId::new(self.integer(fields[0]) as u32));
        Value::new(id, self.terms().expression_id(ty), defining)
    }
    fn definition(&self, value: ValueId, constructor: &str) -> Option<Vec<EggValue>> {
        self.fields(
            constructor,
            self.lookup("ValueDefinition", i64::from(value.number()))?,
        )
    }
    pub fn region_of_port(&self, value: ValueId) -> Option<RegionId> {
        Some(RegionId::new(
            self.integer(self.definition(value, "RegionParamDef")?[0]) as u32,
        ))
    }
    pub fn block_of_argument(&self, value: ValueId) -> Option<BlockId> {
        Some(BlockId::new(
            self.integer(self.definition(value, "BlockParamDef")?[0]) as u32,
        ))
    }
    fn placement(&self, op: OpId, constructor: &str) -> Option<Vec<EggValue>> {
        self.fields(
            constructor,
            self.lookup("OpPlacement", i64::from(op.raw()))?,
        )
    }
    pub fn parent_block(&self, op: OpId) -> Option<BlockId> {
        Some(BlockId::new(
            self.integer(self.placement(op, "InBlock")?[0]) as u32,
        ))
    }
    pub fn parent_nodes_region(&self, op: OpId) -> Option<RegionId> {
        Some(RegionId::new(
            self.integer(self.placement(op, "InGraph")?[0]) as u32,
        ))
    }
    pub fn region_of_op(&self, op: OpId) -> Option<RegionId> {
        self.parent_nodes_region(op).or_else(|| {
            self.parent_block(op)
                .and_then(|block| self.parent_region(block))
        })
    }
    pub fn parent_op(&self, op: OpId) -> Option<OpId> {
        self.region_of_op(op)
            .and_then(|region| self.region_parent_op(region))
    }
    pub fn parent_region(&self, block: BlockId) -> Option<RegionId> {
        Some(RegionId::new(
            self.integer(self.lookup("BlockRegion", i64::from(block.raw()))?) as u32,
        ))
    }
    pub(crate) fn region_parent_op(&self, region: RegionId) -> Option<OpId> {
        Some(OpId::new(
            self.integer(self.lookup("RegionOwner", i64::from(region.raw()))?) as u32,
        ))
    }
    pub(crate) fn region_is_nodes(&self, region: RegionId) -> bool {
        self.lookup("Mode", i64::from(region.raw()))
            .is_some_and(|mode| self.fields("Graph", mode).is_some())
    }
    pub(crate) fn op_operands(&self, op: OpId) -> crate::operation::ValueIds {
        self.indexed("Operand", &[0], &[self.int(op.raw())], &[1])
            .into_iter()
            .map(|row| ValueId::from_number(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn op_results(&self, op: OpId) -> crate::operation::ValueIds {
        self.refresh_indexes();
        self.indexed("Result", &[0], &[self.int(op.raw())], &[1])
            .into_iter()
            .map(|row| ValueId::from_number(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn op_regions(&self, op: OpId) -> crate::operation::RegionIds {
        self.refresh_indexes();
        self.indexed("OwnedRegion", &[0], &[self.int(op.raw())], &[1])
            .into_iter()
            .map(|row| RegionId::new(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn op_name_id(&self, op: OpId) -> OpNameId {
        let name = self.string(
            self.lookup("OpKind", i64::from(op.raw()))
                .unwrap_or_else(|| panic!("live op {op:?}")),
        );
        let (dialect, name) = name
            .split_once('.')
            .unwrap_or_else(|| panic!("qualified op kind"));
        *self
            .registry()
            .op_name_ids
            .get(&(dialect, name))
            .unwrap_or_else(|| panic!("registered op kind"))
    }
    pub fn operation_definition(&self, op: OpId) -> Option<&'static crate::OpDefSpec> {
        self.registry().op_names[self.op_name_id(op).index()].definition
    }
    pub(crate) fn op_identity(&self, op: OpId) -> (&'static str, &'static str) {
        self.registry().op_names[self.op_name_id(op).index()].identity
    }
    pub(crate) fn block_arguments(&self, block: BlockId) -> Vec<Value> {
        self.refresh_indexes();
        self.indexed("BlockParam", &[0], &[self.int(block.raw())], &[1])
            .into_iter()
            .map(|row| self.declared_value(ValueId::from_number(self.integer(row[2]) as u32)))
            .collect()
    }
    fn declared_value(&self, value: ValueId) -> Value {
        let ty = self
            .lookup("DeclaredType", i64::from(value.number()))
            .unwrap_or_else(|| panic!("declared parameter type"));
        Value::new(value, self.terms().expression_id(ty), None)
    }
    pub(crate) fn region_ports(&self, region: RegionId) -> Vec<Value> {
        self.refresh_indexes();
        self.indexed("RegionParam", &[0], &[self.int(region.raw())], &[1])
            .into_iter()
            .map(|row| self.declared_value(ValueId::from_number(self.integer(row[2]) as u32)))
            .collect()
    }
    pub(crate) fn region_results(&self, region: RegionId) -> Vec<ValueId> {
        self.indexed("Yield", &[0], &[self.int(region.raw())], &[1])
            .into_iter()
            .map(|row| ValueId::from_number(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn block_ops(&self, block: BlockId) -> Vec<OpId> {
        self.refresh_indexes();
        self.indexed("BlockMember", &[0], &[self.int(block.raw())], &[1])
            .into_iter()
            .map(|row| OpId::new(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn region_ops(&self, region: RegionId) -> Vec<OpId> {
        self.refresh_indexes();
        let mut ops = self
            .indexed("GraphMember", &[0], &[self.int(region.raw())], &[])
            .into_iter()
            .map(|row| OpId::new(self.integer(row[1]) as u32))
            .collect::<Vec<_>>();
        ops.sort_by_key(|op| {
            self.lookup("GraphLayout", i64::from(op.raw()))
                .map(|n| self.integer(n))
        });
        ops
    }
    pub(crate) fn region_blocks(&self, region: RegionId) -> Vec<BlockId> {
        self.refresh_indexes();
        self.indexed("BlocksOf", &[0], &[self.int(region.raw())], &[1])
            .into_iter()
            .map(|row| BlockId::new(self.integer(row[2]) as u32))
            .collect()
    }
    pub(crate) fn subtree_ops(&self, roots: &[OpId]) -> Vec<OpId> {
        {
            let mut engine = self.engine();
            engine
                .update(|mut state| {
                    for root in roots {
                        state.add("Scope", i64::from(root.raw()))?;
                    }
                    Ok(())
                })
                .unwrap_or_else(|error| panic!("IR scope: {error}"));
        }
        self.invalidate_indexes();
        self.refresh_indexes();
        roots
            .iter()
            .flat_map(|root| self.indexed("Within", &[0], &[self.int(root.raw())], &[]))
            .map(|row| OpId::new(self.integer(row[1]) as u32))
            .collect()
    }

    pub(crate) fn region_under(&self, region: RegionId, root: RegionId) -> bool {
        self.refresh_indexes();
        self.engine()
            .read(|state| {
                state.contains(
                    "RegionUnder",
                    (i64::from(region.raw()), i64::from(root.raw())),
                )
            })
            .unwrap_or_else(|error| panic!("IR containment: {error}"))
    }
    pub fn uses_of(&self, value: ValueId) -> Vec<Use> {
        self.refresh_indexes();
        self.indexed("Uses", &[0], &[self.int(value.number())], &[])
            .into_iter()
            .filter_map(|row| {
                let fields = self.fields("OperandUse", row[2])?;
                Some(Use::new(
                    OpId::new(self.integer(row[1]) as u32),
                    self.integer(fields[0]) as usize,
                ))
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

    pub(crate) fn sibling_reads(&self, root: RegionId, nested: bool) -> Vec<(OpId, OpId)> {
        self.refresh_indexes();
        self.indexed(
            "ScopedSiblingRead",
            &[0, 1],
            &[self.int(root.raw()), self.boolean(nested)],
            &[],
        )
        .into_iter()
        .map(|row| {
            (
                OpId::new(self.integer(row[2]) as u32),
                OpId::new(self.integer(row[3]) as u32),
            )
        })
        .collect()
    }
    fn normalize_binding(&self, region: RegionId, binding: &crate::Binding) {
        for (port, operand) in binding.ports.clone().zip(binding.operands.clone()) {
            self.put(
                "EntryBinding",
                vec![self.int(region.raw()), self.int(port as i64)],
                self.int(operand as i64),
            );
        }
        for (result, yield_slot) in binding.results.clone().zip(binding.exit.clone()) {
            self.put(
                "ExitBinding",
                vec![self.int(region.raw()), self.int(result as i64)],
                self.int(yield_slot as i64),
            );
        }
        for (port, yield_slot) in binding.ports.clone().zip(binding.continue_.clone()) {
            self.put(
                "FeedbackBinding",
                vec![self.int(region.raw()), self.int(port as i64)],
                self.int(yield_slot as i64),
            );
        }
    }
    fn normalize_op(&self, id: OpId) {
        if self.0.constructing.get() == Some(id) {
            return;
        }
        let op = self.get_op(id);
        for table in ["Successor", "SuccessorArg", "GammaArm"] {
            let arity = self
                .engine()
                .get_function(table)
                .unwrap_or_else(|| panic!("IR table"))
                .func_type()
                .input
                .len();
            for row in self.indexed(table, &[0], &[self.int(id.raw())], &[]) {
                self.delete(table, row[..arity].to_vec());
            }
        }
        for table in [
            "Gamma",
            "Theta",
            "GammaPredicate",
            "GammaDefaultArm",
            "ThetaBody",
            "ThetaPredicate",
            "ThetaContinueOn",
        ] {
            self.delete(table, vec![self.int(id.raw())]);
        }
        for region in op.regions() {
            for table in ["EntryBinding", "ExitBinding", "FeedbackBinding"] {
                for row in self.indexed(table, &[0], &[self.int(region.raw())], &[]) {
                    self.delete(table, row[..2].to_vec());
                }
            }
        }
        if let Some(branch) = op.clone().as_interface::<dyn crate::BranchTerminator>() {
            for (edge, (block, slots)) in branch.successor_operand_slots().into_iter().enumerate() {
                self.put(
                    "Successor",
                    vec![self.int(id.raw()), self.int(edge as i64)],
                    self.int(block.raw()),
                );
                for (parameter, operand) in slots.into_iter().enumerate() {
                    self.put(
                        "SuccessorArg",
                        vec![
                            self.int(id.raw()),
                            self.int(edge as i64),
                            self.int(parameter as i64),
                        ],
                        self.int(operand as i64),
                    );
                }
            }
        }
        if let Some(gamma) = op.clone().as_interface::<dyn crate::Gamma>() {
            self.construct("Gamma", vec![self.int(id.raw())]);
            self.put(
                "GammaPredicate",
                vec![self.int(id.raw())],
                self.int(gamma.predicate_slot() as i64),
            );
            let arms = gamma.arms();
            let cases = gamma.cases();
            let binding = gamma.binding();
            for region in &arms {
                self.normalize_binding(*region, &binding);
            }
            for (case, region) in cases.into_iter().zip(&arms) {
                let value = case
                    .to_string()
                    .parse::<tir_egglog::sort::BigInt>()
                    .unwrap_or_else(|_| panic!("gamma case"));
                let case = self.engine().base_to_value(value);
                self.put(
                    "GammaArm",
                    vec![self.int(id.raw()), case],
                    self.int(region.raw()),
                );
            }
            if let Some(region) = arms.last() {
                self.put(
                    "GammaDefaultArm",
                    vec![self.int(id.raw())],
                    self.int(region.raw()),
                );
            }
        }
        if let Some(theta) = op.clone().as_interface::<dyn crate::Theta>() {
            self.construct("Theta", vec![self.int(id.raw())]);
            let region = theta.body();
            self.normalize_binding(region, &theta.binding());
            self.put(
                "ThetaBody",
                vec![self.int(id.raw())],
                self.int(region.raw()),
            );
            self.put(
                "ThetaPredicate",
                vec![self.int(id.raw())],
                self.int(theta.predicate_slot() as i64),
            );
            self.put(
                "ThetaContinueOn",
                vec![self.int(id.raw())],
                self.boolean(true),
            );
        }
        if let Some(binding) = op.as_interface::<dyn crate::RegionBinding>() {
            self.normalize_binding(binding.region(), &binding.binding());
        }
    }
    fn encode_attributes(&self, attributes: &[AttributeValue]) -> EggValue {
        let mut tail = self.construct("ANil", vec![]);
        for value in attributes.iter().rev() {
            let head = self.encode_attribute(value);
            tail = self.construct("ACons", vec![head, tail]);
        }
        tail
    }
    fn decode_attributes(&self, list: EggValue) -> Vec<AttributeValue> {
        self.refresh_indexes();
        self.indexed("AttrItem", &[0], &[list], &[1])
            .into_iter()
            .map(|row| self.decode_attribute(row[2]))
            .collect()
    }
    fn encode_attribute(&self, value: &AttributeValue) -> EggValue {
        use AttributeValue::*;
        match value {
            Str(text) => self.construct("StringAttr", vec![self.text(text.to_string())]),
            Int(n) => self.construct(
                "IntegerAttr",
                vec![
                    self.construct("Ctor", vec![self.text("$attr.signed")]),
                    self.int(64_i64),
                    self.text(format!("{:016x}", *n as u64)),
                ],
            ),
            UInt(n) => self.construct(
                "IntegerAttr",
                vec![
                    self.construct("Ctor", vec![self.text("$attr.unsigned")]),
                    self.int(64_i64),
                    self.text(format!("{n:016x}")),
                ],
            ),
            Wide(limbs) => self.construct(
                "IntegerAttr",
                vec![
                    self.construct("Ctor", vec![self.text("$attr.wide")]),
                    self.int(limbs.len() as i64 * 64),
                    self.text(
                        limbs
                            .iter()
                            .rev()
                            .map(|limb| format!("{limb:016x}"))
                            .collect::<std::string::String>(),
                    ),
                ],
            ),
            F32(n) => self.construct(
                "FloatAttr",
                vec![
                    self.terms().expression(self.terms().float(8, 23)),
                    self.text("f32"),
                    self.text(format!("{:08x}", n.to_bits())),
                ],
            ),
            F64(n) => self.construct(
                "FloatAttr",
                vec![
                    self.terms().expression(self.terms().float(11, 52)),
                    self.text("f64"),
                    self.text(format!("{:016x}", n.to_bits())),
                ],
            ),
            Bool(flag) => self.construct("BoolAttr", vec![self.boolean(*flag)]),
            Array(values) => self.construct("ListAttr", vec![self.encode_attributes(values)]),
            Dict(values) => {
                let mut tail = self.construct("DNil", vec![]);
                for (key, value) in values.iter().rev() {
                    tail = self.construct(
                        "DCons",
                        vec![self.text(key.clone()), self.encode_attribute(value), tail],
                    );
                }
                self.construct("DictAttr", vec![tail])
            }
            Type(ty) => self.construct("TypeAttr", vec![self.terms().expression(*ty)]),
            Value(value) => self.construct("ValueAttr", vec![self.int(value.number())]),
            Block(block) => self.construct("BlockAttr", vec![self.int(block.raw())]),
            Predicate(predicate) => {
                self.construct("PredicateAttr", vec![self.text(predicate.name())])
            }
            Register(register) => {
                let class = register.class();
                self.note_reg_class(class);
                let dialect = self.text(class.dialect());
                let name = self.text(class.name());
                match register {
                    crate::attributes::RegisterAttr::Physical { index, .. } => self.construct(
                        "PhysicalRegisterAttr",
                        vec![dialect, name, self.int(i64::from(*index))],
                    ),
                    crate::attributes::RegisterAttr::Assigned { value, index, .. } => self
                        .construct(
                            "AssignedRegisterAttr",
                            vec![
                                self.int(value.number()),
                                dialect,
                                name,
                                self.int(i64::from(*index)),
                            ],
                        ),
                }
            }
            FpSemantics(semantics) => {
                let fields = Dict(Box::new(
                    semantics
                        .fields()
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), Str(v.into())))
                        .collect(),
                ));
                self.construct("FpSemanticsAttr", vec![self.encode_attribute(&fields)])
            }
            EvaluationContract(contract) => self.construct(
                "EvaluationContractAttr",
                vec![self.encode_attribute(&contract.attribute())],
            ),
            LayoutSpec(spec) => {
                let mut options = self.construct("ANil", vec![]);
                for (name, value) in spec.options.iter().rev() {
                    let option = self.construct(
                        "LayoutOptionAttr",
                        vec![self.text(name.clone()), self.encode_attribute(value)],
                    );
                    options = self.construct("ACons", vec![option, options]);
                }
                let mut entries = self.construct("ANil", vec![]);
                for entry in spec.entries.iter().rev() {
                    let vars = self.encode_attributes(
                        &entry
                            .vars
                            .iter()
                            .map(|name| Str(name.clone().into()))
                            .collect::<Vec<_>>(),
                    );
                    let preferred = entry
                        .preferred
                        .iter()
                        .map(|arg| {
                            self.construct(
                                "TypeArgumentAttr",
                                vec![self.terms().argument_expression(arg)],
                            )
                        })
                        .collect::<Vec<_>>();
                    let mut preferred_list = self.construct("ANil", vec![]);
                    for preferred in preferred {
                        preferred_list = self.construct("ACons", vec![preferred, preferred_list]);
                    }
                    let row = self.construct(
                        "LayoutEntryAttr",
                        vec![
                            self.terms().expression(entry.key),
                            vars,
                            self.terms().argument_expression(&entry.size),
                            self.terms().argument_expression(&entry.abi),
                            preferred_list,
                        ],
                    );
                    entries = self.construct("ACons", vec![row, entries]);
                }
                self.construct("LayoutSpecAttr", vec![options, entries])
            }
        }
    }
    fn decode_attribute(&self, expression: EggValue) -> AttributeValue {
        use AttributeValue::*;
        if let Some(fields) = self.fields("StringAttr", expression) {
            return Str(self.string(fields[0]).into());
        }
        if let Some(fields) = self.fields("IntegerAttr", expression) {
            let kind = self
                .fields("Ctor", fields[0])
                .unwrap_or_else(|| panic!("integer attribute interpretation"));
            let kind = self.string(kind[0]);
            let bits = self.string(fields[2]);
            return match kind.as_str() {
                "$attr.signed" => Int(u64::from_str_radix(&bits, 16)
                    .unwrap_or_else(|_| panic!("signed integer bits"))
                    as i64),
                "$attr.unsigned" => UInt(
                    u64::from_str_radix(&bits, 16)
                        .unwrap_or_else(|_| panic!("unsigned integer bits")),
                ),
                "$attr.wide" => Wide(
                    bits.as_bytes()
                        .rchunks(16)
                        .map(|chunk| {
                            u64::from_str_radix(
                                std::str::from_utf8(chunk).unwrap_or_else(|_| panic!("hex digits")),
                                16,
                            )
                            .unwrap_or_else(|_| panic!("wide integer bits"))
                        })
                        .collect::<Vec<_>>()
                        .into(),
                ),
                _ => panic!("unknown integer attribute interpretation"),
            };
        }
        if let Some(fields) = self.fields("FloatAttr", expression) {
            let bits = self.string(fields[2]);
            return match self.string(fields[1]).as_str() {
                "f32" => F32(f32::from_bits(
                    u32::from_str_radix(&bits, 16).unwrap_or_else(|_| panic!("f32 bits")),
                )),
                "f64" => F64(f64::from_bits(
                    u64::from_str_radix(&bits, 16).unwrap_or_else(|_| panic!("f64 bits")),
                )),
                _ => panic!("known float format"),
            };
        }
        if let Some(fields) = self.fields("BoolAttr", expression) {
            return Bool(self.engine().value_to_base(fields[0]));
        }
        if let Some(fields) = self.fields("ListAttr", expression) {
            return Array(self.decode_attributes(fields[0]).into());
        }
        if let Some(fields) = self.fields("DictAttr", expression) {
            self.refresh_indexes();
            let values = self
                .indexed("DictItem", &[0], &[fields[0]], &[1])
                .into_iter()
                .map(|row| (self.string(row[1]), self.decode_attribute(row[2])))
                .collect();
            return Dict(Box::new(values));
        }

        if let Some(fields) = self.fields("TypeAttr", expression) {
            return Type(self.terms().expression_id(fields[0]));
        }
        if let Some(fields) = self.fields("ValueAttr", expression) {
            return Value(ValueId::from_number(self.integer(fields[0]) as u32));
        }
        if let Some(fields) = self.fields("BlockAttr", expression) {
            return Block(BlockId::new(self.integer(fields[0]) as u32));
        }
        if let Some(fields) = self.fields("PredicateAttr", expression) {
            return Predicate(
                crate::attributes::Predicate::parse(&self.string(fields[0]))
                    .unwrap_or_else(|| panic!("known predicate")),
            );
        }
        if let Some(fields) = self.fields("PhysicalRegisterAttr", expression) {
            let class = self
                .reg_class(
                    self.terms().name(&self.string(fields[0])),
                    self.terms().name(&self.string(fields[1])),
                )
                .unwrap_or_else(|| panic!("registered register class"));
            return Register(crate::attributes::RegisterAttr::Physical {
                class,
                index: self.integer(fields[2]) as u16,
            });
        }
        if let Some(fields) = self.fields("AssignedRegisterAttr", expression) {
            let class = self
                .reg_class(
                    self.terms().name(&self.string(fields[1])),
                    self.terms().name(&self.string(fields[2])),
                )
                .unwrap_or_else(|| panic!("registered register class"));
            return Register(crate::attributes::RegisterAttr::Assigned {
                value: ValueId::from_number(self.integer(fields[0]) as u32),
                class,
                index: self.integer(fields[3]) as u16,
            });
        }
        if let Some(fields) = self.fields("FpSemanticsAttr", expression) {
            return FpSemantics(Arc::new(
                crate::fp::Semantics::parse_attribute(&self.decode_attribute(fields[0]))
                    .unwrap_or_else(|error| panic!("FP semantics: {error}")),
            ));
        }
        if let Some(fields) = self.fields("EvaluationContractAttr", expression) {
            return EvaluationContract(Arc::new(
                crate::fp::EvaluationContract::parse_attribute(&self.decode_attribute(fields[0]))
                    .unwrap_or_else(|error| panic!("evaluation contract: {error}")),
            ));
        }
        if let Some(fields) = self.fields("LayoutSpecAttr", expression) {
            self.refresh_indexes();
            let mut spec = crate::layout::LayoutSpec::new();
            for row in self.indexed("AttrItem", &[0], &[fields[0]], &[1]) {
                let option = self
                    .fields("LayoutOptionAttr", row[2])
                    .unwrap_or_else(|| panic!("layout option"));
                spec = spec.with_option(&self.string(option[0]), self.decode_attribute(option[1]));
            }
            for row in self.indexed("AttrItem", &[0], &[fields[1]], &[1]) {
                let fields = self
                    .fields("LayoutEntryAttr", row[2])
                    .unwrap_or_else(|| panic!("layout entry"));
                let vars = self
                    .decode_attributes(fields[1])
                    .into_iter()
                    .map(|value| {
                        let Str(name) = value else {
                            panic!("layout variable");
                        };
                        name.into_string()
                    })
                    .collect();
                let preferred = self
                    .indexed("AttrItem", &[0], &[fields[4]], &[1])
                    .first()
                    .map(|row| {
                        let fields = self
                            .fields("TypeArgumentAttr", row[2])
                            .unwrap_or_else(|| panic!("layout preferred alignment"));
                        self.terms().expression_argument(fields[0])
                    });
                spec = spec.with_entry(crate::layout::TypeEntry {
                    key: self.terms().expression_id(fields[0]),
                    vars,
                    size: self.terms().expression_argument(fields[2]),
                    abi: self.terms().expression_argument(fields[3]),
                    preferred,
                });
            }
            return LayoutSpec(Arc::new(spec));
        }
        panic!("an attribute constructor")
    }
    fn attributes(&self, table: &str, id: u32) -> Vec<NamedAttribute> {
        self.refresh_indexes();
        self.indexed(table, &[0], &[self.int(id)], &[])
            .into_iter()
            .map(|row| {
                NamedAttribute::new(
                    self.intern(&self.string(row[1])),
                    self.decode_attribute(row[2]),
                )
            })
            .collect()
    }
    pub(crate) fn op_attributes(&self, op: OpId) -> Vec<NamedAttribute> {
        self.attributes("Attribute", op.raw())
    }
    pub(crate) fn block_attributes(&self, block: BlockId) -> Vec<NamedAttribute> {
        self.attributes("BlockAttribute", block.raw())
    }
    pub(crate) fn op_attr_sym(&self, op: OpId, name: Sym) -> Option<AttributeValue> {
        Some(
            self.decode_attribute(
                self.lookup("Attribute", (i64::from(op.raw()), self.resolve(name)))?,
            ),
        )
    }
    pub(crate) fn block_attr(&self, block: BlockId, name: &str) -> Option<AttributeValue> {
        Some(self.decode_attribute(self.lookup("BlockAttribute", (i64::from(block.raw()), name))?))
    }

    pub fn with_attr<R>(
        &self,
        op: OpId,
        name: &str,
        read: impl FnOnce(&AttributeValue) -> R,
    ) -> Option<R> {
        Some(read(&self.decode_attribute(
            self.lookup("Attribute", (i64::from(op.raw()), name))?,
        )))
    }
    pub(crate) fn set_block_attr(&self, block: BlockId, name: &str, value: AttributeValue) {
        self.put(
            "BlockAttribute",
            vec![self.int(block.raw()), self.text(name)],
            self.encode_attribute(&value),
        );
    }
    pub fn set_op_attributes(&self, op: OpId, attributes: Vec<NamedAttribute>) {
        for row in self.indexed("Attribute", &[0], &[self.int(op.raw())], &[]) {
            self.delete("Attribute", row[..2].to_vec());
        }
        for attribute in attributes {
            self.put(
                "Attribute",
                vec![self.int(op.raw()), self.text(self.resolve(attribute.name))],
                self.encode_attribute(&attribute.value),
            );
        }
        self.normalize_op(op);
    }
    pub fn create_value(&self, ty: TypeId, defining_op: Option<OpId>) -> Value {
        let id = ValueId::from_number(self.mint());
        self.put(
            "ValueType",
            vec![self.int(id.number())],
            self.terms().expression(ty),
        );
        if let Some(op) = defining_op {
            self.define_value(id, "ResultDef", op.raw(), self.op_results(op).len());
        }
        Value::new(id, ty, defining_op)
    }
    fn define_value(&self, value: ValueId, constructor: &str, owner: u32, slot: usize) {
        let definition = self.construct(constructor, vec![self.int(owner), self.int(slot as i64)]);
        self.put(
            "ValueDefinition",
            vec![self.int(value.number())],
            definition,
        );
        if constructor != "ResultDef" {
            let ty = self
                .lookup("ValueType", i64::from(value.number()))
                .unwrap_or_else(|| panic!("parameter type"));
            self.put("DeclaredType", vec![self.int(value.number())], ty);
        }
    }
    pub fn add_operation(
        &self,
        insertion: crate::InsertionPoint,
        op: crate::operation::NewOp,
    ) -> OpHandle {
        let id = OpId::new(self.mint());
        self.0.constructing.set(Some(id));
        let (dialect, name) = self.registry().op_names[op.name.index()].identity;
        let kind = format!("{dialect}.{name}");
        self.put("OpKind", vec![self.int(id.raw())], self.text(kind.clone()));
        let boundary = match (dialect, name) {
            ("builtin", "module") => "ModuleBoundary",
            ("func", "func" | "lambda") => "FunctionBoundary",
            _ => "OrdinaryBoundary",
        };
        let boundary = self.construct(boundary, vec![]);
        self.put("KindBoundary", vec![self.text(kind)], boundary);
        self.insert(insertion, id);
        if let Some(location) = self.0.origin.get() {
            self.put("OpLocation", vec![self.int(id.raw())], self.int(location.0));
        }
        self.set_op_operands(id, op.operands);
        for (slot, value) in op.results.into_iter().enumerate() {
            self.define_value(value, "ResultDef", id.raw(), slot);
        }
        for (slot, region) in op.regions.into_iter().enumerate() {
            self.put(
                "RegionOwner",
                vec![self.int(region.raw())],
                self.int(id.raw()),
            );
            self.put(
                "RegionSlot",
                vec![self.int(region.raw())],
                self.int(slot as i64),
            );
        }
        self.set_op_attributes(id, op.attributes);
        self.0.constructing.set(None);
        self.normalize_op(id);
        self.get_op(id)
    }
    pub fn op_location(&self, op: OpId) -> Option<crate::parse::Span> {
        Some(crate::parse::Span(
            self.integer(self.lookup("OpLocation", i64::from(op.raw()))?) as u32,
        ))
    }
    pub fn set_origin(&self, location: Option<crate::parse::Span>) -> Option<crate::parse::Span> {
        self.0.origin.replace(location)
    }
    pub fn retype_value(&self, value: ValueId, ty: TypeId) {
        self.put(
            "ValueType",
            vec![self.int(value.number())],
            self.terms().expression(ty),
        );
    }
    pub fn retype_block_argument(&self, block: BlockId, index: usize, ty: TypeId) {
        if let Some(value) = self.block_arguments(block).get(index) {
            self.retype_value(value.id(), ty);
            self.put(
                "DeclaredType",
                vec![self.int(value.id().number())],
                self.terms().expression(ty),
            );
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
            self.delete("ValueDefinition", vec![self.int(old.number())]);
        }
        self.define_value(value, "ResultDef", op.raw(), index);
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
        self.put(
            "Mode",
            vec![self.int(id.raw())],
            self.construct("CFG", vec![]),
        );
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
        self.put(
            "Mode",
            vec![self.int(region.raw())],
            self.construct("Graph", vec![]),
        );
        for (slot, value) in ports.into_iter().enumerate() {
            self.define_value(value.id(), "RegionParamDef", region.raw(), slot);
        }
        self.set_region_results(region, results);
        for op in ops {
            self.insert(crate::InsertionPoint::Region(region), op);
        }
    }
    /// The operation's current placement. For an ordered operation, inserting
    /// here places a new operation immediately before it.
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

    /// Place an operation at `insertion`, replacing its previous placement.
    /// Block positions refer to the sequence before moving the operation.
    pub fn insert(&self, insertion: crate::InsertionPoint, op: OpId) {
        use crate::InsertionPoint;
        let placement = match insertion {
            InsertionPoint::Root => self.construct("Root", vec![]),
            InsertionPoint::Region(region) => {
                debug_assert!(self.region_is_nodes(region));
                let position = self.region_ops(region).len();
                self.put(
                    "GraphLayout",
                    vec![self.int(op.raw())],
                    self.int(position as i64),
                );
                self.construct("InGraph", vec![self.int(region.raw())])
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
                return;
            }
        };
        self.put("OpPlacement", vec![self.int(op.raw())], placement);
    }
    pub fn set_region_ops(&self, region: RegionId, ops: Vec<OpId>) {
        for (position, op) in ops.into_iter().enumerate() {
            self.put(
                "OpPlacement",
                vec![self.int(op.raw())],
                self.construct("InGraph", vec![self.int(region.raw())]),
            );
            self.put(
                "GraphLayout",
                vec![self.int(op.raw())],
                self.int(position as i64),
            );
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
        self.put("BlockLayout", vec![self.int(id.raw())], self.int(0_i64));
        for (slot, value) in arguments.into_iter().enumerate() {
            self.define_value(value.id(), "BlockParamDef", id.raw(), slot);
        }
        self.get_block(id)
    }
    pub(crate) fn add_block_to_region(&self, region: RegionId, block: BlockId) {
        let slot = self.region_blocks(region).len();
        self.put(
            "BlockRegion",
            vec![self.int(block.raw())],
            self.int(region.raw()),
        );
        self.put(
            "BlockLayout",
            vec![self.int(block.raw())],
            self.int(slot as i64),
        );
        if slot == 0 {
            self.put("Entry", vec![self.int(region.raw())], self.int(block.raw()));
        }
    }
    pub(crate) fn remove_block_from_region(&self, region: RegionId, block: BlockId) -> bool {
        if self.parent_region(block) != Some(region) {
            return false;
        }
        self.delete("BlockRegion", vec![self.int(block.raw())]);
        if self.lookup("Entry", i64::from(region.raw())) == Some(self.int(block.raw())) {
            match self.region_blocks(region).first() {
                Some(entry) => {
                    self.put("Entry", vec![self.int(region.raw())], self.int(entry.raw()))
                }
                None => self.delete("Entry", vec![self.int(region.raw())]),
            }
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
            self.put(
                "BlockLayout",
                vec![self.int(block.raw())],
                self.int(slot as i64),
            );
        }
        Ok(())
    }
    pub(crate) fn set_block_ops(&self, block: BlockId, ops: Vec<OpId>) {
        for (slot, op) in ops.into_iter().enumerate() {
            self.put(
                "OpPlacement",
                vec![self.int(op.raw())],
                self.construct(
                    "InBlock",
                    vec![self.int(block.raw()), self.int(slot as i64)],
                ),
            );
        }
    }
    pub(crate) fn remove_op_from_block(&self, block: BlockId, op: OpId) -> bool {
        if self.parent_block(op) != Some(block) {
            return false;
        }
        self.put(
            "OpPlacement",
            vec![self.int(op.raw())],
            self.construct("Root", vec![]),
        );
        true
    }
    pub(crate) fn replace_op_in_block(&self, block: BlockId, old: OpId, new: OpId) -> bool {
        let Some(fields) = self.placement(old, "InBlock") else {
            return false;
        };
        if self.integer(fields[0]) != i64::from(block.raw()) {
            return false;
        }
        self.put(
            "OpPlacement",
            vec![self.int(new.raw())],
            self.construct("InBlock", fields),
        );
        self.put(
            "OpPlacement",
            vec![self.int(old.raw())],
            self.construct("Root", vec![]),
        );
        true
    }
    pub fn remove_from_region(&self, region: RegionId, op: OpId) {
        if self.parent_nodes_region(op) == Some(region) {
            self.put(
                "OpPlacement",
                vec![self.int(op.raw())],
                self.construct("Root", vec![]),
            );
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
            "BlockParamDef",
            block.raw(),
            self.block_arguments(block).len(),
        );
    }
    pub fn remove_block_argument(&self, block: BlockId, index: usize) {
        let mut values = self.block_arguments(block);
        let removed = values.remove(index);
        self.delete("ValueDefinition", vec![self.int(removed.id().number())]);
        for (slot, value) in values.into_iter().enumerate() {
            self.define_value(value.id(), "BlockParamDef", block.raw(), slot);
        }
    }
    fn edit_segments(&self, op: OpId, edit: impl FnOnce(&mut [AttributeValue])) {
        let sym = self.registry().segment_sizes;
        if let Some(AttributeValue::Array(mut values)) = self.op_attr_sym(op, sym) {
            edit(&mut values);
            self.put(
                "Attribute",
                vec![self.int(op.raw()), self.text(self.resolve(sym))],
                self.encode_attribute(&AttributeValue::Array(values)),
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
            self.delete("ValueDefinition", vec![self.int(old.number())]);
        }
        for (slot, value) in values.iter().enumerate() {
            self.define_value(*value, "ResultDef", op.raw(), slot);
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
        self.delete("ValueDefinition", vec![self.int(removed.id().number())]);
        for (slot, value) in values.into_iter().enumerate() {
            self.define_value(value.id(), "RegionParamDef", region.raw(), slot);
        }
    }
    pub(crate) fn insert_region_port(&self, region: RegionId, index: usize, value: Value) {
        let mut values = self.region_ports(region);
        values.insert(index, value);
        for (slot, value) in values.into_iter().enumerate() {
            self.define_value(value.id(), "RegionParamDef", region.raw(), slot);
        }
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
        self.subtree_ops(&[root]);
        let owned = self.indexed("OwnedEntity", &[0], &[self.int(root.raw())], &[]);
        for row in owned {
            let category = self.string(row[1]);
            let id = row[2];
            let number = self.integer(id) as u32;
            match category.as_str() {
                "op" => {
                    for table in [
                        "Operand",
                        "Attribute",
                        "Successor",
                        "SuccessorArg",
                        "GammaArm",
                    ] {
                        for row in self.indexed(table, &[0], &[id], &[]) {
                            let arity = self
                                .engine()
                                .get_function(table)
                                .unwrap_or_else(|| panic!("IR table"))
                                .func_type()
                                .input
                                .len();
                            self.delete(table, row[..arity].to_vec());
                        }
                    }
                    for table in [
                        "OpKind",
                        "OpPlacement",
                        "OpLocation",
                        "GraphLayout",
                        "Gamma",
                        "Theta",
                        "Scope",
                        "SelectedScope",
                        "GammaPredicate",
                        "GammaDefaultArm",
                        "ThetaBody",
                        "ThetaPredicate",
                        "ThetaContinueOn",
                    ] {
                        self.delete(table, vec![id]);
                    }
                }
                "value" if !keep.contains(&ValueId::from_number(number)) => {
                    for table in ["ValueType", "ValueDefinition", "DeclaredType"] {
                        self.delete(table, vec![id]);
                    }
                }
                "block" => {
                    for row in self.indexed("BlockAttribute", &[0], &[id], &[]) {
                        self.delete("BlockAttribute", row[..2].to_vec());
                    }
                    for table in ["BlockRegion", "BlockLayout"] {
                        self.delete(table, vec![id]);
                    }
                }
                "region" => {
                    for table in ["Yield", "EntryBinding", "ExitBinding", "FeedbackBinding"] {
                        for row in self.indexed(table, &[0], &[id], &[]) {
                            self.delete(table, row[..2].to_vec());
                        }
                    }
                    for table in ["Mode", "RegionOwner", "RegionSlot", "Entry"] {
                        self.delete(table, vec![id]);
                    }
                }
                _ => {}
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
