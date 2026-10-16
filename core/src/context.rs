use std::{
    any::Any,
    cell::{Ref, RefCell, RefMut},
    collections::{HashMap, HashSet},
    sync::{
        Arc, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU32, Ordering},
    },
};

use tir_adt::{Interner, Sym};

use crate::store::Db;

pub use crate::store::{Census, Parent};

use crate::{
    Dialect, Error, OpId, Operation, OperationParser, TypeId,
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
    ty::{Spellings, Terms, TypeParser, TypeSpelling},
    value::{Use, Value, ValueId},
    vector::VectorDialect,
};

/// What every reader and editor of the IR holds.
///
/// A context reads and edits the IR's chunks directly, so every edit is
/// visible to the next read. A chunk another context still reads (a fork, a
/// task of a parallel epoch) is copied on its first write, so no context ever
/// sees another's edits. [`Context::commit`] closes an epoch. Ids are minted
/// once and never move or get reused, so an id names the same entity for as
/// long as the entity lives.
///
/// The context also owns what is not IR: the dialects, interned names and
/// types, and interface registrations.
///
/// A context belongs to one thread: it is neither `Send` nor `Sync`, and its
/// reads and edits take no lock.
///
/// # Example
///
/// ```rust
/// let context = tir::Context::with_default_dialects();
/// ```
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

/// Entities an erase reclaims, gathered before the storage is borrowed for
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
    reg_classes: HashMap<(crate::ty::Name, crate::ty::Name), crate::backend::regalloc::RegClassId>,
    /// The interfaces each op name implements, by [`OpNameId`]: an op's name
    /// is a column, so a lookup indexes by it and compares a few type ids.
    interfaces: Vec<Vec<(std::any::TypeId, OpInterfaceConverter)>>,
    spellings: Spellings,
    fp_semantics: HashMap<crate::fp::Semantics, Arc<crate::fp::Semantics>>,
    evaluation_contracts:
        HashMap<crate::fp::EvaluationContract, Arc<crate::fp::EvaluationContract>>,
    /// The names attributes are keyed by, so an op carries four bytes per
    /// attribute name instead of a heap `String` per instance.
    names: Interner,
    /// The op identities, by [`OpNameId`]. Ids are dense and handed out in
    /// construction order.
    op_names: Vec<OpName>,
    op_name_ids: HashMap<(&'static str, &'static str), OpNameId>,
    segment_sizes: Sym,
    // The core type definitions (`dialects/layout/types.rs`), and their
    // definitions by name: the layout regions of the aggregate constructors.
    constructors: Option<(Db, Arc<HashMap<String, OpId>>)>,
    // Whether this context has the core type definitions, built on first
    // need.
    core_types: bool,
    // The layouts under each op carrying a spec, and each module's
    // `type.def`s by name, at the version of the module partition they were
    // read at.
    layouts: HashMap<OpId, (u32, Arc<crate::layout::ModuleLayouts>)>,
    // Rule sets compiled against this context's names and types, by kind:
    // every pass instance of the context shares one.
    rules: HashMap<std::any::TypeId, Arc<dyn std::any::Any + Send + Sync>>,
    type_definitions: HashMap<OpId, (u32, Arc<HashMap<String, OpId>>)>,
}

struct OpName {
    identity: (&'static str, &'static str),
    schema: Option<&'static crate::OpSchema>,
    scheme: Option<Arc<crate::typecheck::SchemeRows>>,
}

/// The names and types a context interned, readable from any thread. An id
/// the registry handed out says in what order things were met; whoever must
/// not depend on that order reads what the id stands for through this.
#[derive(Clone)]
pub struct Names(Arc<RwLock<Registry>>);

impl Names {
    /// The name `sym` was interned from.
    pub fn name(&self, sym: Sym) -> String {
        let registry = self.0.read().unwrap_or_else(|e| e.into_inner());
        registry.names.resolve(sym).to_string()
    }

    /// The name whose [`Sym::index`] is `index`, if one is.
    pub fn name_at(&self, index: usize) -> Option<String> {
        let registry = self.0.read().unwrap_or_else(|e| e.into_inner());
        registry.names.at(index).map(str::to_string)
    }

    /// Intern `name`, as [`Context::intern`] does.
    pub fn intern(&self, name: &str) -> Sym {
        let known = self.0.read().unwrap_or_else(|e| e.into_inner());
        if let Some(sym) = known.names.lookup(name) {
            return sym;
        }
        drop(known);
        let mut registry = self.0.write().unwrap_or_else(|e| e.into_inner());
        registry.names.intern(name)
    }

    /// `ty` as the IR spells it.
    pub fn type_spelling(&self, ty: TypeId) -> String {
        let registry = self.0.read().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        registry
            .spellings
            .print(ty, &mut IRFormatter::new(&mut out))
            .expect("type print must succeed");
        out
    }
}

/// What another context over the IR this one reads is opened from; see
/// [`Context::snapshot`].
#[derive(Clone)]
pub(crate) struct Snapshot {
    registry: Arc<RwLock<Registry>>,
    terms: Arc<Terms>,
    db: Db,
    epoch: u32,
}

impl Snapshot {
    /// The partition each of `targets` mints into.
    pub(crate) fn partitions(&self, targets: &[OpId]) -> Vec<u32> {
        targets
            .iter()
            .map(|target| self.db.partition_of(*target))
            .collect()
    }
}

/// What a task of a parallel epoch wrote: its chunks, to be merged into the
/// base it was opened over.
pub(crate) struct EditBatch(Db);

impl EditBatch {
    /// Bytes of the chunks the batch wrote.
    pub(crate) fn bytes(&self) -> usize {
        self.0.census().touched_bytes
    }
}

struct Inner {
    /// Shared by every context over one module: a worker interns into the
    /// same tables the module was built with.
    registry: Arc<RwLock<Registry>>,
    // Shared like the registry so an id names one type in every fork and task.
    terms: Arc<Terms>,
    db: RefCell<Db>,
    /// Advances at every commit.
    epoch: AtomicU32,
    frozen: bool,
    // Whether this is the IR of the core type definitions, which are
    // the type constructors themselves.
    defines_types: bool,
}

/// Mints into a partition of its own until dropped; see
/// [`Context::function_scope`].
pub struct FunctionScope {
    context: Context,
    previous: u32,
}

impl Drop for FunctionScope {
    fn drop(&mut self) {
        self.context.db_mut().mint_into(self.previous);
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

/// Every op an `operation!` declared, with its schema, ahead of any IR.
fn declared_op_names() -> (Vec<OpName>, HashMap<(&'static str, &'static str), OpNameId>) {
    let mut names = Vec::new();
    let mut ids = HashMap::new();
    for schema in crate::schema::OP_SCHEMAS {
        ids.entry((schema.dialect, schema.name)).or_insert_with(|| {
            names.push(OpName {
                identity: (schema.dialect, schema.name),
                schema: Some(schema),
                scheme: None,
            });
            OpNameId::new(names.len() as u32 - 1)
        });
    }
    (names, ids)
}

pub(crate) struct Refused(pub(crate) String);

fn refuse(what: String) -> ! {
    std::panic::resume_unwind(Box::new(Refused(what)))
}

impl Context {
    /// Create a new empty context with no registered dialects.
    pub fn new() -> Self {
        let mut names = schema_vocabulary();
        let segment_sizes = names.intern("operand_segment_sizes");
        let (op_names, op_name_ids) = declared_op_names();
        let terms = Arc::new(Terms::new());
        let context = Context(std::rc::Rc::new(Inner {
            registry: Arc::new(RwLock::new(Registry {
                dialects: HashMap::new(),
                reg_classes: HashMap::new(),
                interfaces: Vec::new(),
                spellings: Spellings::new(terms.clone()),
                fp_semantics: HashMap::new(),
                evaluation_contracts: HashMap::new(),
                names,
                op_names,
                op_name_ids,
                segment_sizes,
                constructors: None,
                core_types: false,
                layouts: HashMap::new(),
                rules: HashMap::new(),
                type_definitions: HashMap::new(),
            })),
            terms,
            db: RefCell::new(Db::new()),
            epoch: AtomicU32::new(0),
            frozen: false,
            defines_types: false,
        }));
        context.register_type("backend", &crate::backend::RegClassType::spelling(&context));
        context
    }

    /// Create a new context with default dialects.
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
        context.register_dialect::<VectorDialect>();
        context.declare_core_types();

        context
    }

    // The core type definitions are built on first need, in `constructors`;
    // the defaults every term of their constructors interns under hold now.
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

    fn define_core_types(&self) -> (Db, Arc<HashMap<String, OpId>>) {
        let definitions = self.over(Db::new());
        let module = crate::dialects::layout::types::define(&definitions);
        definitions.commit();
        if cfg!(debug_assertions)
            && let Err(error) = crate::operation::verify_op_tree(&definitions, module.id())
        {
            panic!("the core type definitions must verify: {error}");
        }
        let names = crate::layout::definitions_in(&definitions, module.id());
        (definitions.db().clone(), Arc::new(names))
    }

    // Whether this context holds the core type definitions, which define the
    // type constructors.
    pub(crate) fn defines_types(&self) -> bool {
        self.0.defines_types
    }

    // The IR of the core type definitions over `db`, sharing this context's
    // registry and types.
    fn over(&self, db: Db) -> Self {
        Context(std::rc::Rc::new(Inner {
            registry: self.0.registry.clone(),
            terms: self.0.terms.clone(),
            db: RefCell::new(db),
            epoch: AtomicU32::new(0),
            frozen: self.0.frozen,
            defines_types: true,
        }))
    }

    /// The rules of kind `T` compiled for this context, `compile`d the first
    /// time any pass instance asks.
    pub(crate) fn compiled_rules<T: std::any::Any + Send + Sync>(
        &self,
        compile: impl FnOnce() -> T,
    ) -> Arc<T> {
        let kind = std::any::TypeId::of::<T>();
        let known = self.registry().rules.get(&kind).cloned();
        let rules = known.unwrap_or_else(|| {
            let compiled: Arc<dyn std::any::Any + Send + Sync> = Arc::new(compile());
            let mut registry = self.registry_mut();
            registry.rules.entry(kind).or_insert(compiled).clone()
        });
        rules
            .downcast()
            .expect("rules of the kind they are filed under")
    }

    // The names the core type definitions define.
    pub(crate) fn core_type_names(&self) -> Option<&'static [&'static str]> {
        self.registry()
            .core_types
            .then_some(&crate::dialects::layout::types::NAMES[..])
    }

    // The IR of the core type definitions and its definitions by name, built
    // the first time anything asks.
    pub(crate) fn constructors(&self) -> Option<(Context, Arc<HashMap<String, OpId>>)> {
        let built = self.registry().constructors.clone();
        let (db, names) = match built {
            Some(built) => built,
            None if self.registry().core_types => {
                let built = self.define_core_types();
                let mut registry = self.registry_mut();
                registry.constructors.get_or_insert(built).clone()
            }
            None => return None,
        };
        Some((self.over(db), names))
    }

    // The layouts under `scope`, an op carrying a spec, kept until the
    // module partition is next edited.
    pub(crate) fn scope_layouts(
        &self,
        scope: OpId,
        build: impl FnOnce() -> crate::layout::ModuleLayouts,
    ) -> Arc<crate::layout::ModuleLayouts> {
        let version = self.db().module_version();
        if let Some((at, laid)) = self.registry().layouts.get(&scope)
            && *at == version
        {
            return laid.clone();
        }
        let laid = Arc::new(build());
        let entry = (version, laid.clone());
        self.registry_mut().layouts.insert(scope, entry);
        laid
    }

    // The `type.def`s of `module` by name, kept like its layouts.
    pub(crate) fn module_type_definitions(&self, module: OpId) -> Arc<HashMap<String, OpId>> {
        let version = self.db().module_version();
        if let Some((at, names)) = self.registry().type_definitions.get(&module)
            && *at == version
        {
            return names.clone();
        }
        let names = Arc::new(crate::layout::definitions_in(self, module));
        let entry = (version, names.clone());
        self.registry_mut().type_definitions.insert(module, entry);
        names
    }

    fn registry(&self) -> RwLockReadGuard<'_, Registry> {
        self.0.registry.read().unwrap_or_else(|e| e.into_inner())
    }

    fn registry_mut(&self) -> RwLockWriteGuard<'_, Registry> {
        self.0.registry.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The store the types of this context's IR are interned in.
    pub fn terms(&self) -> &Terms {
        &self.0.terms
    }

    fn db(&self) -> Ref<'_, Db> {
        self.0.db.borrow()
    }

    fn db_mut(&self) -> RefMut<'_, Db> {
        self.0.db.borrow_mut()
    }

    /// See [`Names`].
    pub fn names(&self) -> Names {
        Names(self.0.registry.clone())
    }

    /// Intern an attribute name.
    pub fn intern(&self, name: &str) -> Sym {
        if self.0.frozen {
            return self
                .sym(name)
                .unwrap_or_else(|| refuse(format!("the name `{name}`")));
        }
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

    pub fn intern_fp_semantics(
        &self,
        semantics: impl Into<crate::fp::Semantics>,
    ) -> Arc<crate::fp::Semantics> {
        let semantics = semantics.into();
        let mut registry = self.registry_mut();
        registry
            .fp_semantics
            .entry(semantics)
            .or_insert_with(|| Arc::new(semantics))
            .clone()
    }

    pub fn intern_evaluation_contract(
        &self,
        contract: impl Into<crate::fp::EvaluationContract>,
    ) -> Arc<crate::fp::EvaluationContract> {
        let contract = contract.into();
        let mut registry = self.registry_mut();
        registry
            .evaluation_contracts
            .entry(contract)
            .or_insert_with_key(|contract| Arc::new(contract.clone()))
            .clone()
    }

    /// The dense id of an op identity, minted on first sight.
    pub(crate) fn intern_op_name(&self, dialect: &'static str, name: &'static str) -> OpNameId {
        let mut registry = self.registry_mut();
        if let Some(id) = registry.op_name_ids.get(&(dialect, name)) {
            return *id;
        }
        if self.0.frozen {
            drop(registry);
            refuse(format!("the operation `{dialect}.{name}`"));
        }
        let id = OpNameId::new(registry.op_names.len() as u32);
        registry.op_names.push(OpName {
            identity: (dialect, name),
            schema: None,
            scheme: None,
        });
        registry.op_name_ids.insert((dialect, name), id);
        id
    }

    pub fn named_attribute(&self, name: &str, value: AttributeValue) -> NamedAttribute {
        NamedAttribute::new(self.intern(name), value)
    }

    /// Live rows, chunks and bytes of the IR's storage, for the memory census.
    pub fn census(&self) -> Census {
        self.db().census()
    }

    // Epochs.

    /// Which epoch this is; advances at every commit.
    pub fn epoch(&self) -> u32 {
        self.0.epoch.load(Ordering::Relaxed)
    }

    /// What this context wrote since it was opened, for the context it was
    /// opened from to merge.
    pub(crate) fn finish(&self) -> EditBatch {
        EditBatch(self.db().clone())
    }

    /// Close the epoch. Ids do not move.
    pub fn commit(&self) {
        self.db_mut().commit();
        self.0.epoch.store(self.epoch() + 1, Ordering::Relaxed);
    }

    /// Merge `batches`, each finished by a context opened over this one as
    /// committed, in the order given. Two batches writing one row are refused
    /// before anything changes, naming the row.
    pub(crate) fn commit_batches(&self, batches: Vec<EditBatch>) -> Result<(), String> {
        self.db_mut()
            .merge(batches.into_iter().map(|batch| batch.0).collect())?;
        self.0.epoch.store(self.epoch() + 1, Ordering::Relaxed);
        Ok(())
    }

    /// What another context over the IR this one reads needs. `Send`, so the
    /// context can be opened on another thread. Every edit must be committed
    /// first.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let db = self.db();
        assert!(db.is_clean(), "a snapshot is taken of a committed base");
        Snapshot {
            registry: self.0.registry.clone(),
            terms: self.0.terms.clone(),
            db: db.clone(),
            epoch: self.epoch(),
        }
    }

    /// A context over `snapshot` for task `index` of the `count` an epoch
    /// runs on `target`s of their own. It mints into `target`'s partition
    /// through chunk indices no other task takes; `exclusive` says no other
    /// task of the epoch mints into that partition.
    pub(crate) fn open(
        snapshot: &Snapshot,
        index: usize,
        count: usize,
        target: OpId,
        exclusive: bool,
    ) -> Self {
        let part = snapshot.db.partition_of(target);
        Context(std::rc::Rc::new(Inner {
            registry: snapshot.registry.clone(),
            terms: snapshot.terms.clone(),
            db: RefCell::new(
                snapshot
                    .db
                    .lane(index as u32, count as u32, part, exclusive),
            ),
            epoch: AtomicU32::new(snapshot.epoch),
            frozen: false,
            defines_types: false,
        }))
    }

    pub(crate) fn detach(self) -> Snapshot {
        Snapshot {
            registry: self.0.registry.clone(),
            terms: self.0.terms.clone(),
            db: self.db().clone(),
            epoch: self.epoch(),
        }
    }

    // Two threads interning at once would number the registry's entries by
    // their race.
    pub(crate) fn open_frozen(snapshot: Snapshot) -> Self {
        Context(std::rc::Rc::new(Inner {
            registry: snapshot.registry,
            terms: snapshot.terms,
            db: RefCell::new(snapshot.db),
            epoch: AtomicU32::new(snapshot.epoch),
            frozen: true,
            defines_types: false,
        }))
    }

    /// A copy of this context to try an edit in. Ids minted in the fork
    /// continue from this context's, so [`Context::adopt`] hands them back
    /// unchanged.
    pub(crate) fn fork(&self) -> Self {
        Context(std::rc::Rc::new(Inner {
            registry: self.0.registry.clone(),
            terms: self.0.terms.clone(),
            db: RefCell::new(self.db().clone()),
            epoch: AtomicU32::new(self.epoch()),
            frozen: self.0.frozen,
            defines_types: self.0.defines_types,
        }))
    }

    /// Take the IR of `fork`, a [`Context::fork`] of this context, as this
    /// context's own. Handles into `fork` die with it; its ids stay valid
    /// here.
    pub(crate) fn adopt(&self, fork: Context) {
        let db = fork.db().clone();
        drop(fork);
        *self.db_mut() = db;
    }

    /// Mint what is created until the scope drops into a partition of its
    /// own. A frontend brackets the construction of one function with this,
    /// so the function's rows share chunks with no other function's.
    pub fn function_scope(&self) -> FunctionScope {
        FunctionScope {
            context: self.clone(),
            previous: self.db_mut().enter_partition(),
        }
    }

    /// [`Context::function_scope`] into the partition `op` already has, for
    /// a pass that walks a module and builds inside one function of it.
    /// `None` when `op` has no partition of its own or is already the one
    /// minted into.
    pub fn partition_scope(&self, op: OpId) -> Option<FunctionScope> {
        let previous = self.db_mut().mint_under(op)?;
        Some(FunctionScope {
            context: self.clone(),
            previous,
        })
    }

    /// Record that `old` was replaced by `new` in place, so a pipeline can
    /// follow its root across the swap.
    pub(crate) fn record_replaced_op(&self, old: OpId, new: OpId) {
        self.db_mut().replaced_ops.insert(old, new);
    }

    /// The op that took `id`'s place, if a rewrite replaced it.
    pub(crate) fn replaced_op(&self, id: OpId) -> Option<OpId> {
        self.db().replaced_ops.get(&id).copied()
    }

    // Dialects, interfaces and types.

    // A scheme naming a constructor no dialect has registered yet fails, and
    // is built again next time.
    pub(crate) fn scheme_rows(
        &self,
        name: OpNameId,
    ) -> Option<Result<Arc<crate::typecheck::SchemeRows>, String>> {
        let schema = {
            let registry = self.registry();
            let entry = &registry.op_names[name.index()];
            if let Some(rows) = &entry.scheme {
                return Some(Ok(rows.clone()));
            }
            entry.schema?
        };
        let rows = match crate::typecheck::SchemeRows::new(self.terms(), schema) {
            Ok(rows) => Arc::new(rows),
            Err(error) => return Some(Err(error)),
        };
        self.registry_mut().op_names[name.index()].scheme = Some(rows.clone());
        Some(Ok(rows))
    }

    /// The schema `operation!` declared for `dialect.name`.
    pub(crate) fn op_schema(&self, dialect: &str, name: &str) -> Option<&'static crate::OpSchema> {
        let registry = self.registry();
        let id = registry.op_name_ids.get(&(dialect, name))?;
        registry.op_names[id.index()].schema
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
        self.registry_mut().dialects.insert(D::name(), dialect);
    }

    /// Register a target's register classes so the attribute parser can resolve a
    /// `CLASS[n]` register's class name back to its
    /// [`crate::backend::regalloc::RegClassId`]. Backends call this from
    /// `register_dialects` with their generated `register_info().classes`.
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

    /// Resolve a register-class name to its
    /// [`crate::backend::regalloc::RegClassId`], if a target that defines it
    /// has been registered (see [`Context::register_reg_classes`]).
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

    pub fn register_op_interface<I: ?Sized + 'static>(
        &self,
        dialect: &'static str,
        op_name: &'static str,
        converter: OpInterfaceConverter,
    ) {
        let name = self.intern_op_name(dialect, op_name).index();
        let interface = std::any::TypeId::of::<I>();
        let mut registry = self.registry_mut();
        if registry.interfaces.len() <= name {
            registry.interfaces.resize_with(name + 1, Vec::new);
        }
        let held = &mut registry.interfaces[name];
        match held.iter_mut().find(|(ty, _)| *ty == interface) {
            Some(entry) => entry.1 = converter,
            None => held.push((interface, converter)),
        }
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

    /// Spell the terms of `spelling.ctor` the short way, parsed under
    /// `dialect`.
    pub fn register_type(&self, dialect: &'static str, spelling: &TypeSpelling) {
        self.registry_mut().spellings.register(dialect, spelling);
    }

    /// Parse `!dialect.key` with `parser`, for a mnemonic no constructor's
    /// spelling lists, such as a register class of a target.
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

    /// The ids of the interned types `keep` holds of, in the order they were
    /// interned.
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

    /// Whether `ty` stands for "any number of further arguments" when it ends
    /// a function signature: overload resolution then matches the fixed
    /// prefix and accepts any tail, including an empty one.
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

    /// A term argument as the IR spells it, its variables named by `names`.
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

    // Handles.

    /// The handle naming `id`. Panics for an id no live operation has: a handle
    /// reads the operation as it stands, and an erased one does not stand.
    pub fn get_op(&self, id: OpId) -> OpHandle {
        assert!(self.has_operation(id), "live operation {id:?}");
        OpHandle {
            context: self.clone(),
            id,
        }
    }

    /// The handle naming `id`. Panics for an id no live block has.
    pub fn get_block(&self, id: BlockId) -> BlockHandle {
        assert!(self.has_block(id), "live block {id:?}");
        BlockHandle {
            context: self.clone(),
            id,
        }
    }

    /// The handle naming `id`; see [`Context::get_block`].
    pub fn get_region(&self, id: RegionId) -> RegionHandle {
        assert!(self.has_region(id), "live region {id:?}");
        RegionHandle {
            context: self.clone(),
            id,
        }
    }

    fn find_op(&self, id: OpId) -> Option<OpHandle> {
        self.has_operation(id).then(|| self.get_op(id))
    }

    fn find_block(&self, id: BlockId) -> Option<BlockHandle> {
        self.has_block(id).then(|| self.get_block(id))
    }

    fn find_region(&self, id: RegionId) -> Option<RegionHandle> {
        self.has_region(id).then(|| self.get_region(id))
    }

    // Reads.

    pub fn has_operation(&self, id: OpId) -> bool {
        self.db().op_live(id)
    }

    pub fn has_value(&self, id: ValueId) -> bool {
        self.db().value_live(id)
    }

    pub fn has_region(&self, id: RegionId) -> bool {
        self.db().region_live(id)
    }

    pub fn has_block(&self, id: BlockId) -> bool {
        self.db().block_live(id)
    }

    /// A counter that moves with every edit under `op`, and may move with an
    /// edit elsewhere: it counts the writes to the rows of `op`'s partition
    /// and of the module's. Analyses cached against a version are stale as
    /// soon as it moves; see [`crate::analysis::AnalysisManager`].
    pub fn op_version(&self, op: OpId) -> u32 {
        self.db().version(op)
    }

    /// Whether `pass` already ran on `op` as it stands now and edited nothing.
    pub(crate) fn is_settled(&self, pass: &'static str, op: OpId) -> bool {
        self.db().is_settled(pass, op)
    }

    /// Record that `pass` ran on `op` as it stands now and edited nothing.
    pub(crate) fn settle(&self, pass: &'static str, op: OpId) {
        self.db_mut().settle(pass, op);
    }

    /// The subtrees edited since the last call, innermost-dirtied op per edit and
    /// deduplicated. The pass manager drains this to scope post-pass verification.
    pub(crate) fn take_dirty_ops(&self) -> Vec<OpId> {
        self.db_mut().take_dirty()
    }

    pub fn get_value(&self, id: ValueId) -> Value {
        self.db().value(id).expect("live value")
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
        let db = self.db();
        ids.iter()
            .copied()
            .filter(|&id| {
                db.value(id)
                    .is_some_and(|value| self.is_state_type(value.ty()))
                    == states
            })
            .collect()
    }

    /// Every operand slot holding `value`: ascending by reading operation,
    /// with the operations edited most recently last.
    ///
    /// This is the def-use chain: it lists what live operation storage holds,
    /// whether or not the reading op sits in the tree. Attributes naming a
    /// value are not uses: they record where the ABI places it, not a read.
    pub fn uses_of(&self, value: ValueId) -> Vec<Use> {
        self.db_mut().uses(value)
    }

    /// The operations reading `value`, one entry per operand slot.
    pub fn users_of(&self, value: ValueId) -> Vec<OpId> {
        self.uses_of(value)
            .into_iter()
            .map(|r#use| r#use.op)
            .collect()
    }

    pub fn is_used(&self, value: ValueId) -> bool {
        self.db_mut().is_used(value)
    }

    pub fn use_count(&self, value: ValueId) -> usize {
        self.uses_of(value).len()
    }

    /// Every region result slot naming `value`, as `(region, index)`: where
    /// an unordered region publishes it. Region results sit in no use list.
    pub fn result_uses(&self, value: ValueId) -> Vec<(RegionId, usize)> {
        self.db_mut().result_uses(value)
    }

    /// Whether `region` or a region nested in it names `value` as a result.
    pub(crate) fn published_under(&self, region: RegionId, value: ValueId) -> bool {
        let mut db = self.db_mut();
        let uses = db.result_uses(value);
        uses.into_iter()
            .any(|(naming, _)| db.region_under(naming, region))
    }

    /// Whether `region` is `root` or nested in it, at any depth.
    pub(crate) fn region_under(&self, region: RegionId, root: RegionId) -> bool {
        self.db().region_under(region, root)
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
        self.db().value_region(id)
    }

    /// The block `id` is an argument of, or `None` when an operation defines it.
    pub fn block_of_argument(&self, id: ValueId) -> Option<BlockId> {
        self.db().value_block(id)
    }

    /// The block currently holding `op`, or `None` for an op not in any block
    /// (the root op, or one detached by a rewrite).
    pub fn parent_block(&self, op: OpId) -> Option<BlockId> {
        match self.db().op_parent(op) {
            Some(Parent::Block(block)) => Some(block),
            _ => None,
        }
    }

    /// The unordered region holding `op` directly, or `None` for an op that
    /// sits in a block or in no region at all.
    pub fn parent_nodes_region(&self, op: OpId) -> Option<RegionId> {
        match self.db().op_parent(op) {
            Some(Parent::Region(region)) => Some(region),
            _ => None,
        }
    }

    /// The region holding `op`, through its block where it has one.
    pub fn region_of_op(&self, op: OpId) -> Option<RegionId> {
        let db = self.db();
        match db.op_parent(op)? {
            Parent::Region(region) => Some(region),
            Parent::Block(block) => db.block_parent(block),
        }
    }

    /// The operation enclosing `op`: the owner of the region holding `op`'s
    /// block. `None` for a root op or one detached by a rewrite.
    pub fn parent_op(&self, op: OpId) -> Option<OpId> {
        self.db().enclosing_op_of(op)
    }

    /// The region currently holding `block`, or `None` for a detached block.
    pub fn parent_region(&self, block: BlockId) -> Option<RegionId> {
        self.db().block_parent(block)
    }

    // `read` must not edit the context: the storage is borrowed for the read.
    pub(crate) fn with_attributes<R>(
        &self,
        id: OpId,
        read: impl FnOnce(&[crate::attributes::NamedAttribute]) -> R,
    ) -> R {
        read(self.db().op_attrs(id))
    }

    // `read` must not edit the context, nor ask it for a use list or for what
    // a block or a region holds:
    // those bring a grouping up to date, and the storage is borrowed for the
    // read.
    pub(crate) fn with_rows<R>(&self, read: impl FnOnce(&Db) -> R) -> R {
        read(&self.db())
    }

    /// Read an attribute of `op` in place. For an attribute large enough that
    /// cloning it per lookup would matter.
    ///
    /// `read` must not edit the context: the storage is borrowed for the read.
    pub fn with_attr<R>(
        &self,
        id: OpId,
        name: &str,
        read: impl FnOnce(&AttributeValue) -> R,
    ) -> Option<R> {
        let name = self.sym(name)?;
        self.db()
            .op_attrs(id)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| read(&attribute.value))
    }

    pub(crate) fn op_operands(&self, id: OpId) -> crate::operation::ValueIds {
        self.db().operands(id)
    }

    pub(crate) fn op_results(&self, id: OpId) -> crate::operation::ValueIds {
        self.db().results(id)
    }

    pub(crate) fn op_regions(&self, id: OpId) -> crate::operation::RegionIds {
        self.db().regions(id)
    }

    pub(crate) fn op_attributes(&self, id: OpId) -> Vec<NamedAttribute> {
        self.db().op_attrs(id).to_vec()
    }

    /// [`OpHandle::attr_sym`]: the lookup is a `u32` compare per attribute.
    pub(crate) fn op_attr_sym(&self, id: OpId, name: Sym) -> Option<AttributeValue> {
        self.db()
            .op_attrs(id)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.clone())
    }

    /// The ops named one of `names` under `root`, read off the name column of
    /// the partitions holding what is under it rather than by a walk.
    pub(crate) fn ops_named(&self, root: OpId, names: &[OpNameId]) -> Vec<OpId> {
        self.db().ops_named(root, names)
    }

    /// The interned name of `id`.
    pub(crate) fn op_name_id(&self, id: OpId) -> OpNameId {
        self.db().op_name(id)
    }

    /// The `(dialect, name)` pair `id` is spelled by.
    pub(crate) fn op_identity(&self, id: OpId) -> (&'static str, &'static str) {
        let name = self.db().op_name(id);
        self.registry().op_names[name.index()].identity
    }

    pub(crate) fn block_arguments(&self, id: BlockId) -> Vec<Value> {
        self.db().block_args(id)
    }

    pub(crate) fn block_attributes(&self, id: BlockId) -> Vec<NamedAttribute> {
        self.db().block_attrs(id).to_vec()
    }

    /// [`BlockHandle::attr`].
    pub(crate) fn block_attr(&self, block: BlockId, name: &str) -> Option<AttributeValue> {
        let name = self.sym(name)?;
        self.db()
            .block_attrs(block)
            .iter()
            .find(|attribute| attribute.name == name)
            .map(|attribute| attribute.value.clone())
    }

    /// The operations `block` holds, in order.
    pub(crate) fn block_ops(&self, id: BlockId) -> Vec<OpId> {
        self.db_mut().block_ops(id)
    }

    pub(crate) fn region_parent_op(&self, id: RegionId) -> Option<OpId> {
        self.db().region_parent(id)
    }

    pub(crate) fn region_is_nodes(&self, id: RegionId) -> bool {
        self.db().region_is_nodes(id)
    }

    /// The blocks an ordered region holds, in order; none for an unordered one.
    pub(crate) fn region_blocks(&self, id: RegionId) -> Vec<BlockId> {
        self.db_mut().region_blocks(id)
    }

    /// The operations an unordered region holds directly, in the order they
    /// joined it; none for an ordered one.
    pub(crate) fn region_ops(&self, id: RegionId) -> Vec<OpId> {
        self.db_mut().region_ops(id)
    }

    /// Every op under `roots`, the roots included.
    pub(crate) fn subtree_ops(&self, roots: &[OpId]) -> Vec<OpId> {
        self.db_mut().subtree_ops(roots)
    }

    /// `region` and every region nested in it.
    pub(crate) fn nested_regions(&self, region: RegionId) -> Vec<RegionId> {
        self.db_mut().nested_regions(region)
    }

    pub(crate) fn sibling_reads(&self, root: RegionId, nested: bool) -> Vec<(OpId, OpId)> {
        self.db_mut().sibling_reads(root, nested)
    }

    pub(crate) fn sibling_read(&self, reader: OpId, value: ValueId) -> Option<(OpId, OpId)> {
        self.db().sibling_read(reader, value, None)
    }

    /// The ports an unordered region owns itself; none for an ordered one.
    pub(crate) fn region_ports(&self, id: RegionId) -> Vec<Value> {
        self.db().region_ports(id)
    }

    pub(crate) fn region_results(&self, id: RegionId) -> Vec<ValueId> {
        self.db().region_results(id)
    }

    // Edits. Each writes the rows it names and reports the subtree it
    // touched, for verification after the pass.

    pub fn add_operation(&self, op: crate::operation::NewOp) -> OpHandle {
        let id = self.db_mut().add_operation(op);
        self.get_op(id)
    }

    /// Where `op` came from: the byte offset of its text in the source the
    /// parser read, or of the op a pass built it for.
    pub fn op_location(&self, op: OpId) -> Option<crate::parse::Span> {
        self.db().op_location(op).map(crate::parse::Span)
    }

    /// Locate every op added until the next call at `location`; returns the
    /// location ops were added at so far.
    pub fn set_origin(&self, location: Option<crate::parse::Span>) -> Option<crate::parse::Span> {
        let previous = self.db_mut().set_origin(location.map(|span| span.0));
        previous.map(crate::parse::Span)
    }

    /// Whether `refine`'s obligation was proved for the op as it stands.
    pub(crate) fn refine_proved(&self, refine: OpId) -> bool {
        let db = self.db();
        db.proved_refines.get(&refine) == Some(&db.op_stamp(refine))
    }

    /// Record that `refine`'s obligation holds for the op as it stands.
    pub(crate) fn note_refine_proved(&self, refine: OpId) {
        let mut db = self.db_mut();
        let stamp = db.op_stamp(refine);
        db.proved_refines.insert(refine, stamp);
    }

    /// Forget the proofs of the refines that are gone.
    pub(crate) fn forget_erased_refines(&self) {
        let mut db = self.db_mut();
        let mut proved = std::mem::take(&mut db.proved_refines);
        proved.retain(|&op, _| db.op_live(op));
        db.proved_refines = proved;
    }

    /// Replace an operation's attributes in place, keeping its id, position, and
    /// regions.
    pub fn set_op_attributes(&self, id: OpId, attributes: Vec<NamedAttribute>) {
        let mut db = self.db_mut();
        if db.set_op_attrs(id, attributes) {
            db.edit_op(id);
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
        let mut db = self.db_mut();
        if db.set_operand(id, index, new) {
            db.edit_op(id);
        }
    }

    /// Replace all of an operation's SSA operands.
    pub fn set_op_operands(&self, id: OpId, operands: Vec<ValueId>) {
        let mut db = self.db_mut();
        let count = db.operands(id).len();
        if db.splice_operands(id, 0, count, &operands) {
            db.edit_op(id);
        }
    }

    /// Replace a single operation's SSA result at `index`, moving the
    /// definition of `new` onto this op. `new` is a value no other operation
    /// defines.
    pub fn set_op_result(&self, id: OpId, index: usize, new: ValueId) {
        if self.op_results(id).get(index).is_none_or(|old| *old == new) {
            return;
        }
        let mut db = self.db_mut();
        if db.splice_results(id, index, 1, &[new]) {
            db.edit_op(id);
        }
    }

    /// Give a value a new type, keeping its id and every use of it.
    pub fn retype_value(&self, value: ValueId, ty: TypeId) {
        let mut db = self.db_mut();
        if !db.set_value_type(value, ty) {
            return;
        }
        let record = db.value(value).expect("live value");
        match (record.defining_op(), db.value_block(value)) {
            (Some(op), _) => db.edit_op(op),
            (None, Some(block)) => db.edit_block(block),
            (None, None) => {
                if let Some(region) = db.value_region(value) {
                    db.edit_region(region);
                }
            }
        }
    }

    /// [`Context::retype_value`] for a block argument, whose type the block
    /// declares alongside the value's own.
    pub fn retype_block_argument(&self, block: BlockId, index: usize, ty: TypeId) {
        let mut db = self.db_mut();
        if !db.block_live(block) {
            return;
        }
        db.retype_block_argument(block, index, ty);
        db.edit_block(block);
    }

    pub fn create_value(&self, ty: TypeId, defining_op: Option<OpId>) -> Value {
        self.db_mut().create_value(ty, defining_op)
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
        let mut db = self.db_mut();
        let mut edited = db.replace_value_uses(old, new);
        edited.sort_unstable();
        edited.dedup();
        for op in edited {
            db.edit_op(op);
        }
    }

    pub fn create_region(&self) -> RegionHandle {
        let id = self.db_mut().create_region();
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
        let mut db = self.db_mut();
        assert!(
            db.region_is_nodes(region),
            "only an unordered region takes an operation without a position"
        );
        debug_assert!(
            db.op_parent(op).is_none(),
            "an operation joins an unordered region from nowhere else",
        );
        db.append_held(Parent::Region(region), op);
        db.edit_region(region);
    }

    /// Choose another insertion order for the operations the unordered
    /// `region` already holds; `ops` must be a permutation of them.
    pub fn set_region_ops(&self, region: RegionId, ops: Vec<OpId>) {
        let mut db = self.db_mut();
        assert!(
            db.region_is_nodes(region),
            "only an unordered region holds an insertion order"
        );
        debug_assert_eq!(db.region_ops(region).len(), ops.len());
        db.set_held(Parent::Region(region), &ops);
        db.edit_region(region);
    }

    /// Name the values the unordered `region` produces.
    pub fn set_region_results(&self, region: RegionId, results: Vec<ValueId>) {
        let mut db = self.db_mut();
        db.set_region_results(region, &results);
        db.edit_region(region);
    }

    /// Make an empty region unordered; see [`Context::create_nodes_region`].
    pub(crate) fn set_region_nodes(
        &self,
        region: RegionId,
        ports: Vec<Value>,
        ops: Vec<OpId>,
        results: Vec<ValueId>,
    ) {
        let mut db = self.db_mut();
        db.make_nodes(region, &ports, &results);
        for op in ops {
            debug_assert!(
                db.op_parent(op).is_none(),
                "an operation joins an unordered region from nowhere else",
            );
            db.append_held(Parent::Region(region), op);
        }
    }

    pub fn create_block(&self, arguments: Vec<Value>) -> BlockHandle {
        let id = self.db_mut().create_block(&arguments);
        self.get_block(id)
    }

    /// Append an argument of type `ty` to `block` and return it.
    pub fn append_block_argument(&self, block: BlockId, ty: TypeId) -> Value {
        let value = self.create_value(ty, None);
        self.place_block_argument(block, value.id());
        value
    }

    /// Make `value` an entry argument of `block`, in place of the definition it
    /// had. Nothing is renamed: the value keeps its identity, so every reader
    /// goes on naming it.
    pub fn adopt_block_argument(&self, block: BlockId, value: ValueId) {
        if self.has_value(value) && self.place_block_argument(block, value) {
            self.db_mut().set_value_def(value, None);
        }
    }

    /// Drop the `index`th argument of `block`. Nothing is renamed: the value
    /// it named stays, so the caller retires its readers and the branches
    /// that pass it first.
    pub fn remove_block_argument(&self, block: BlockId, index: usize) {
        let mut db = self.db_mut();
        if db.remove_block_argument(block, index).is_some() {
            db.edit_block(block);
        }
    }

    fn place_block_argument(&self, block: BlockId, argument: ValueId) -> bool {
        let mut db = self.db_mut();
        if !db.push_block_argument(block, argument) {
            return false;
        }
        db.edit_block(block);
        true
    }

    /// Append `value` to `op`'s operands, keeping the segment sizes that
    /// describe the trailing variadic group in step.
    pub fn append_operand(&self, op: OpId, value: ValueId) {
        let segment_sizes = self.registry().segment_sizes;
        let mut db = self.db_mut();
        let index = db.operands(op).len();
        if !db.splice_operands(op, index, 0, &[value]) {
            return;
        }
        db.edit_segment_sizes(op, segment_sizes, |sizes| {
            if let Some(AttributeValue::UInt(last)) = sizes.last_mut() {
                *last += 1;
            }
        });
        db.edit_op(op);
    }

    /// Append `value` to `op`'s results, moving its definition onto `op`.
    pub fn adopt_result(&self, op: OpId, value: ValueId) {
        let index = self.op_results(op).len();
        self.insert_result_at(op, index, value);
    }

    /// Drop the operand at `index`, and with it the use it made.
    pub(crate) fn remove_operand(&self, op: OpId, index: usize) {
        let segment_sizes = self.registry().segment_sizes;
        let mut db = self.db_mut();
        if !db.op_live(op) {
            return;
        }
        // Shrink by one the declared operand group holding the operand. A
        // fixed-arity op tracks no segments.
        db.edit_segment_sizes(op, segment_sizes, |sizes| {
            let mut start = 0;
            for size in sizes.iter_mut() {
                if let AttributeValue::UInt(size) = size {
                    if index < start + *size as usize {
                        *size -= 1;
                        return;
                    }
                    start += *size as usize;
                }
            }
        });
        db.splice_operands(op, index, 1, &[]);
        db.edit_op(op);
    }

    /// Drop the result at `index`. The value it named is left with no
    /// definition, so a caller drops one nothing reads.
    pub(crate) fn remove_result(&self, op: OpId, index: usize) {
        let mut db = self.db_mut();
        if db.splice_results(op, index, 1, &[]) {
            db.edit_op(op);
        }
    }

    /// Drop the port at `index` of an unordered region.
    pub(crate) fn remove_region_port(&self, region: RegionId, index: usize) {
        let mut db = self.db_mut();
        db.splice_region_ports(region, index, 1, &[]);
        db.edit_region(region);
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
        let mut db = self.db_mut();
        if !db.splice_operands(op, index, 0, &[value]) {
            return;
        }
        // Grow by one the declared group whose operands ended at `group_end`
        // before the insert, the last such group when several end there (an
        // empty variadic group after a fixed one).
        db.edit_segment_sizes(op, segment_sizes, |sizes| {
            let mut end = 0;
            let mut chosen = None;
            for (position, size) in sizes.iter().enumerate() {
                if let AttributeValue::UInt(size) = size {
                    end += *size as usize;
                    if end == group_end {
                        chosen = Some(position);
                    }
                }
            }
            if let Some(AttributeValue::UInt(size)) =
                chosen.and_then(|position| sizes.get_mut(position))
            {
                *size += 1;
            }
        });
        db.edit_op(op);
    }

    /// Put `port` at position `index` of the unordered `region`'s ports.
    pub(crate) fn insert_region_port(&self, region: RegionId, index: usize, port: Value) {
        let mut db = self.db_mut();
        db.splice_region_ports(region, index, 0, &[port]);
        db.edit_region(region);
    }

    /// Name `value` at position `index` of the unordered `region`'s results.
    pub(crate) fn insert_region_result(&self, region: RegionId, index: usize, value: ValueId) {
        let mut db = self.db_mut();
        db.splice_region_results(region, index, 0, &[value]);
        db.edit_region(region);
    }

    /// Take `op` out of the unordered `region` without erasing it; the inverse
    /// of [`Context::add`].
    pub fn remove_from_region(&self, region: RegionId, op: OpId) {
        let mut db = self.db_mut();
        assert!(
            db.region_is_nodes(region),
            "only an unordered region holds an operation without a position"
        );
        db.remove_held(Parent::Region(region), op);
        db.edit_region(region);
    }

    /// Put `value` at position `index` of `op`'s results, moving its
    /// definition onto `op`.
    pub(crate) fn insert_result_at(&self, op: OpId, index: usize, value: ValueId) {
        let mut db = self.db_mut();
        if db.splice_results(op, index, 0, &[value]) {
            db.edit_op(op);
        }
    }

    /// Give `op` one more result of type `ty`.
    pub fn append_result(&self, op: OpId, ty: TypeId) -> ValueId {
        let result = self.create_value(ty, Some(op)).id();
        let index = self.op_results(op).len();
        self.insert_result_at(op, index, result);
        result
    }

    /// Make the ordered `region` the unordered region `staged` was built as:
    /// the old blocks leave the tree with whatever still sits in them, and
    /// `staged`'s ports, operations and results become `region`'s own. `staged`
    /// is gone afterwards.
    pub fn replace_region_with_nodes(&self, region: RegionId, staged: RegionId) {
        let handle = self.get_region(region);
        let owner = handle.parent_op();
        self.detach_subtree(&handle.block_ids());
        let mut db = self.db_mut();
        db.take_nodes(region, staged);
        if let Some(owner) = owner {
            db.mark_dirty(owner);
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
        let mut db = self.db_mut();
        db.make_blocks(region, &blocks);
        if let Some(owner) = owner {
            db.mark_dirty(owner);
        }
    }

    /// Take the subtree under `blocks` out of the live IR.
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
    /// A region is reclaimed only while its parent link still points at the
    /// op being erased: a replacement that took the region over owns it now.
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
                        ops.extend(handle.op_ids());
                        continue;
                    }
                    blocks.extend(handle.block_ids());
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
            ops.extend(block.op_ids());
        }
    }

    /// Take entities that have left the IR out of the visible graph.
    fn free(&self, owned: Owned) {
        let mut db = self.db_mut();
        for op in owned.ops {
            db.kill_op(op);
        }
        for value in owned.values {
            db.kill_value(value);
        }
        for block in owned.blocks {
            db.kill_block(block);
        }
        for region in owned.regions {
            db.kill_region(region);
        }
    }

    /// Insert `op` into `block` at `index`, recording the new parent.
    pub(crate) fn insert_op(&self, block: BlockId, index: usize, op: OpId) {
        let mut db = self.db_mut();
        db.insert_held(block, index, op);
        db.edit_block(block);
    }

    /// Insert `op` after everything `block` currently holds.
    pub(crate) fn append_op(&self, block: BlockId, op: OpId) {
        let mut db = self.db_mut();
        db.append_held(Parent::Block(block), op);
        db.edit_block(block);
    }

    pub(crate) fn replace_op_in_block(&self, block: BlockId, old: OpId, new: OpId) -> bool {
        let mut db = self.db_mut();
        if !db.block_live(block) || !db.replace_held(block, old, new) {
            return false;
        }
        db.edit_block(block);
        true
    }

    pub(crate) fn remove_op_from_block(&self, block: BlockId, op: OpId) -> bool {
        let mut db = self.db_mut();
        if !db.block_live(block) || !db.remove_held(Parent::Block(block), op) {
            return false;
        }
        db.edit_block(block);
        true
    }

    /// [`BlockHandle::set_ops`].
    pub(crate) fn set_block_ops(&self, block: BlockId, ops: Vec<OpId>) {
        let mut db = self.db_mut();
        debug_assert_eq!(
            db.block_ops(block).len(),
            ops.len(),
            "a reordering holds the block's own operations",
        );
        db.set_held(Parent::Block(block), &ops);
        db.edit_block(block);
    }

    pub(crate) fn set_block_attr(&self, block: BlockId, name: &str, value: AttributeValue) {
        let name = self.intern(name);
        let mut db = self.db_mut();
        if !db.block_live(block) {
            return;
        }
        let mut attributes = db.block_attrs(block).to_vec();
        match attributes.iter_mut().find(|a| a.name == name) {
            Some(attribute) => attribute.value = value,
            None => attributes.push(NamedAttribute::new(name, value)),
        }
        db.set_block_attrs(block, attributes);
        db.edit_block(block);
    }

    /// Reorder an ordered region without changing its entry, membership, or block identities.
    /// Invalid permutations leave the region and its analysis versions unchanged.
    pub fn reorder_region_blocks(
        &self,
        region: RegionId,
        blocks: Vec<BlockId>,
    ) -> Result<(), String> {
        let handle = self.get_region(region);
        if handle.is_nodes() {
            return Err("cannot reorder blocks of an unordered region".into());
        }
        let original = handle.block_ids();
        let mut remaining: HashSet<_> = original.iter().copied().collect();
        if blocks.len() != original.len()
            || blocks.first() != original.first()
            || blocks.iter().any(|block| !remaining.remove(block))
        {
            return Err("block order must be a permutation preserving the entry".into());
        }
        if blocks == original {
            return Ok(());
        }
        let mut db = self.db_mut();
        db.set_region_blocks(region, &blocks);
        db.edit_region(region);
        Ok(())
    }

    pub(crate) fn add_block_to_region(&self, region: RegionId, block: BlockId) {
        let mut db = self.db_mut();
        db.append_block(region, block);
        db.edit_region(region);
    }

    pub(crate) fn remove_block_from_region(&self, region: RegionId, block: BlockId) -> bool {
        let mut db = self.db_mut();
        if !db.region_live(region) || !db.remove_block(region, block) {
            return false;
        }
        db.edit_region(region);
        true
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
