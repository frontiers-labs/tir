//! Structural type terms in the shared egglog IR engine.
//!
//! A type handle names a `TypeExpr`. Constructors and arguments remain
//! inspectable as egglog terms. Structural terms are never unioned.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{OnceLock, RwLock};
use std::{cell::RefCell, rc::Rc};

use smallvec::SmallVec;
use std::sync::Arc;
use tir_egglog::{Core, EGraph, Read, Value as EggValue, Write};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

impl TypeId {
    /// `builtin.state<memory>`, the first term the store interns, so a memory
    /// state is told from a value without a lookup.
    pub const STATE: TypeId = TypeId(0);
    /// `builtin.state<fp.env>`, the second term the store interns.
    pub const FP_ENV: TypeId = TypeId(1);
    pub fn number(self) -> u32 {
        self.0
    }

    pub fn from_number(n: u32) -> Self {
        Self(n)
    }

    pub fn as_index(self) -> usize {
        self.0 as usize
    }
}

impl From<u32> for TypeId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl fmt::Debug for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TypeId({})", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CtorId(u32);

impl CtorId {
    pub fn number(self) -> u32 {
        self.0
    }
}

/// A name a term argument holds: a definition, a resource, a register class.
/// Its identity comes from the shared context entity allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeArg {
    /// A width, a length, a field offset.
    Nat(u64),
    /// A definition name, a resource, a register class.
    Sym(Name),
    /// A child type, by id: equality never expands a child.
    Type(TypeId),
    /// A variable: a de Bruijn index under a [`TypeArg::Lam`], or a variable
    /// of inference. An IR value never has a type with a free variable.
    Var(u32),
    /// A binder with its arity: generic definitions, signature schemes.
    Lam(u32, Box<TypeArg>),
    List(Box<[TypeArg]>),
}

impl TypeArg {
    /// The child type, when the argument is one.
    pub fn as_type(&self) -> Option<TypeId> {
        match self {
            TypeArg::Type(id) => Some(*id),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Data,
    /// States and floating-point environments: they order effects and carry no
    /// value.
    Ghost,
    Nat,
    Sym,
    /// A flag: `Nat(0)` or `Nat(1)`.
    Bool,
}

/// The shape of one constructor parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    One(Kind),
    /// Present or absent. Only the last parameter can be optional.
    Opt(Kind),
    List(Kind),
    /// A list of `(name, type)` pairs, each a two-element list.
    Fields,
    /// Not checked by shape: the constructor's `validate` checks it.
    Any,
}

pub struct TypeCtor {
    pub dialect: &'static str,
    /// `"i"`, `"f"`, `"p"`, `"named"`, …
    pub name: &'static str,
    pub params: &'static [(&'static str, Param)],
    pub kind: Kind,
    /// Checks what the shapes in `params` do not.
    pub validate: fn(&Terms, &[TypeArg]) -> Result<(), String>,
    /// The width of the bit vector a value of the type is, or `None` for a
    /// type that is not one bit vector.
    pub carrier: fn(&Terms, &[TypeArg]) -> Option<TypeArg>,
}

pub fn valid(_: &Terms, _: &[TypeArg]) -> Result<(), String> {
    Ok(())
}

pub fn no_carrier(_: &Terms, _: &[TypeArg]) -> Option<TypeArg> {
    None
}

fn first_arg(_: &Terms, args: &[TypeArg]) -> Option<TypeArg> {
    args.first().cloned()
}

fn float_carrier(terms: &Terms, args: &[TypeArg]) -> Option<TypeArg> {
    let [exponent, mantissa] = args else {
        return None;
    };
    let fraction = terms.nat_add(mantissa.clone(), TypeArg::Nat(1));
    Some(terms.nat_add(exponent.clone(), fraction))
}

/// The constructors the semantic layer reads, registered first and in this
/// order, so their ids are constants.
pub mod ctor {
    use super::{
        CtorId, Kind, Param, TypeArg, TypeCtor, first_arg, float_carrier, no_carrier, valid,
    };

    /// `builtin.i<W>`: a bit vector of `W` bits, any width.
    pub const INT: CtorId = CtorId(0);
    /// `builtin.f<E, M>`: a binary float with `E` exponent and `M` stored
    /// mantissa bits; its carrier is `i<E+M+1>`.
    pub const FLOAT: CtorId = CtorId(1);
    pub const UNIT: CtorId = CtorId(2);
    pub const TUPLE: CtorId = CtorId(3);
    /// `builtin.state<R>`: an ordering dependency on resource `R`.
    pub const STATE: CtorId = CtorId(4);
    /// `nat.add<[W]>`: a sum of widths in normal form.
    pub const NAT_ADD: CtorId = CtorId(5);
    /// `sem.raw<W>`: `W` bits with no arithmetic reading yet; a float carrier.
    pub const RAW: CtorId = CtorId(6);
    /// `sem.iter<T>`: a sequence of `T` a semantic loop walks.
    pub const ITER: CtorId = CtorId(7);
    /// `sem.state`: the state of a semantic description, any resource.
    pub const SEM_STATE: CtorId = CtorId(8);
    /// `type.named<N, [A]>`: the type a `type.def !N` declares, applied to
    /// `A`. Its kind is its definition's; see [`super::Terms::define`].
    pub const NAMED: CtorId = CtorId(9);

    pub(super) const BASE: [TypeCtor; 10] = [
        TypeCtor {
            dialect: "builtin",
            name: "i",
            params: &[("width", Param::One(Kind::Nat))],
            kind: Kind::Data,
            validate: valid,
            carrier: first_arg,
        },
        TypeCtor {
            dialect: "builtin",
            name: "f",
            params: &[
                ("exponent", Param::One(Kind::Nat)),
                ("mantissa", Param::One(Kind::Nat)),
            ],
            kind: Kind::Data,
            validate: valid,
            carrier: float_carrier,
        },
        TypeCtor {
            dialect: "builtin",
            name: "unit",
            params: &[],
            kind: Kind::Data,
            validate: valid,
            carrier: no_carrier,
        },
        TypeCtor {
            dialect: "builtin",
            name: "tuple",
            params: &[("elements", Param::List(Kind::Data))],
            kind: Kind::Data,
            validate: valid,
            carrier: no_carrier,
        },
        TypeCtor {
            dialect: "builtin",
            name: "state",
            params: &[("resource", Param::One(Kind::Sym))],
            kind: Kind::Ghost,
            validate: valid,
            carrier: no_carrier,
        },
        TypeCtor {
            dialect: "nat",
            name: "add",
            params: &[("terms", Param::List(Kind::Nat))],
            kind: Kind::Nat,
            validate: valid,
            carrier: no_carrier,
        },
        TypeCtor {
            dialect: "sem",
            name: "raw",
            params: &[("width", Param::One(Kind::Nat))],
            kind: Kind::Data,
            validate: valid,
            carrier: first_arg,
        },
        TypeCtor {
            dialect: "sem",
            name: "iter",
            params: &[("element", Param::One(Kind::Data))],
            kind: Kind::Data,
            validate: valid,
            carrier: no_carrier,
        },
        TypeCtor {
            dialect: "sem",
            name: "state",
            params: &[],
            kind: Kind::Ghost,
            validate: valid,
            carrier: no_carrier,
        },
        TypeCtor {
            dialect: "type",
            name: "named",
            params: &[("name", Param::One(Kind::Sym)), ("args", Param::Any)],
            kind: Kind::Data,
            validate: |_, args| match args.get(1) {
                Some(TypeArg::List(_)) => Ok(()),
                _ => Err("type 'type.named' takes a list of arguments".to_string()),
            },
            carrier: no_carrier,
        },
    ];
}

/// A term read back from the store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeTerm {
    ctor: CtorId,
    args: Box<[TypeArg]>,
}

impl TypeTerm {
    pub fn ctor(&self) -> CtorId {
        self.ctor
    }

    pub fn args(&self) -> &[TypeArg] {
        &self.args
    }
}

/// The arguments of one term.
pub type Args = SmallVec<[TypeArg; 4]>;

/// The normalized structural IR and type schema.
pub const IR_SCHEMA: &str = include_str!("ir-schema.egg");

/// Build-time compiled rules and declarations of the structural IR.
pub const IR_PROGRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ir-schema.bin"));

/// The integer widths every store interns up front.
pub const FIXED_INT_WIDTHS: u64 = 128;

/// The first segment of a [`Segmented`] table holds `1 << FIRST_SEGMENT_BITS`
/// slots; each next one twice the one before.
const FIRST_SEGMENT_BITS: u32 = 5;
const SEGMENTS: usize = 32 - FIRST_SEGMENT_BITS as usize;

/// Slots written once that never move: segments are allocated on first use
/// and never reallocated, so a reader takes no lock.
struct Segmented<T> {
    segments: [OnceLock<Box<[OnceLock<T>]>>; SEGMENTS],
}

impl<T> Segmented<T> {
    fn new() -> Self {
        Self {
            segments: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    // The segment holding `index` and its slot there.
    fn locate(index: u32) -> (usize, usize) {
        let scaled = (u64::from(index) >> FIRST_SEGMENT_BITS) + 1;
        let segment = 63 - scaled.leading_zeros() as usize;
        let first = ((1u64 << segment) - 1) << FIRST_SEGMENT_BITS;
        (segment, (u64::from(index) - first) as usize)
    }

    fn slot(&self, index: u32) -> &OnceLock<T> {
        let (segment, slot) = Self::locate(index);
        let slots = self.segments[segment].get_or_init(|| {
            let len = 1usize << (FIRST_SEGMENT_BITS as usize + segment);
            (0..len).map(|_| OnceLock::new()).collect()
        });
        &slots[slot]
    }

    fn get(&self, index: u32) -> Option<&T> {
        let (segment, slot) = Self::locate(index);
        self.segments[segment].get()?[slot].get()
    }
}

/// The type-algebra API over the context's structural engine. Base type
/// handles are interned in a fixed order for the two resource-state constants.
pub struct Terms {
    engine: Rc<RefCell<EGraph>>,
    minted: AtomicU32,
    entities: Arc<AtomicU32>,
    indexes_dirty: Arc<AtomicBool>,
    ctors: Segmented<&'static TypeCtor>,
    // Each registered constructor's id by the address of its description, so
    // a dialect's static finds its id without hashing its name.
    by_address: RwLock<HashMap<usize, CtorId, tir_adt::FxBuildHasher>>,
    // By constructor: the value its omitted optional last argument stands for.
    defaults: Segmented<TypeArg>,
    registered: RwLock<HashMap<(&'static str, &'static str), CtorId>>,
}

impl AsRef<Terms> for Terms {
    fn as_ref(&self) -> &Terms {
        self
    }
}

impl Default for Terms {
    fn default() -> Self {
        Self::new()
    }
}

impl Terms {
    pub fn new() -> Self {
        let mut engine = EGraph::default();
        engine
            .load_program(IR_PROGRAM)
            .expect("the compiled structural IR schema loads");
        Self::with_engine(
            Rc::new(RefCell::new(engine)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicBool::new(true)),
        )
    }

    /// Use the context's structural engine and shared entity allocator.
    pub fn with_engine(
        engine: Rc<RefCell<EGraph>>,
        entities: Arc<AtomicU32>,
        indexes_dirty: Arc<AtomicBool>,
    ) -> Self {
        let terms = Terms {
            engine,
            entities,
            indexes_dirty,
            minted: AtomicU32::new(0),
            ctors: Segmented::new(),
            by_address: RwLock::default(),
            defaults: Segmented::new(),
            registered: RwLock::default(),
        };
        for ctor in &ctor::BASE {
            terms.register(ctor);
        }
        for resource in ["memory", "fp.env"] {
            let resource = TypeArg::Sym(terms.name(resource));
            terms
                .intern(ctor::STATE, vec![resource])
                .expect("a state type");
        }
        for width in 1..=FIXED_INT_WIDTHS {
            terms
                .intern(ctor::INT, vec![TypeArg::Nat(width)])
                .expect("an integer type");
        }
        terms
    }

    /// The structural expression named by a type handle.
    pub fn expression(&self, id: TypeId) -> EggValue {
        self.engine.borrow_mut().read(|state| {
            state
                .lookup("TypeHandle", i64::from(id.0))
                .expect("type handle table")
                .expect("a type this engine interned")
        })
    }

    /// Find the handle of a structural expression already interned here.
    pub fn expression_id(&self, expression: EggValue) -> TypeId {
        let engine = self.engine.borrow_mut();
        let id = engine
            .read(|state| state.lookup("TypeHandleOf", expression))
            .expect("type handle table")
            .expect("an interned type expression");
        TypeId(engine.value_to_base::<i64>(id) as u32)
    }

    fn read<R>(&self, id: TypeId, read: impl FnOnce(CtorId, &[TypeArg]) -> R) -> R {
        let expression = self.expression(id);
        let mut engine = self.engine.borrow_mut();
        let (ctor, args) = if let Some(fields) =
            constructor_fields(&mut engine, "FunctionType", expression)
        {
            let inputs = self.decode_list(&mut engine, fields[0]);
            let outputs = self.decode_list(&mut engine, fields[1]);
            (
                self.ctor_named("builtin", "fn")
                    .expect("function type constructor"),
                vec![
                    TypeArg::List(inputs.into()),
                    outputs
                        .into_iter()
                        .next()
                        .expect("one functional result type"),
                ],
            )
        } else if let Some(fields) = constructor_fields(&mut engine, "ProductType", expression) {
            (
                ctor::TUPLE,
                vec![TypeArg::List(
                    self.decode_list(&mut engine, fields[0]).into(),
                )],
            )
        } else if let Some(fields) = constructor_fields(&mut engine, "Nominal", expression) {
            let name = Name(engine.value_to_base::<i64>(fields[0]) as u32);
            (
                ctor::NAMED,
                vec![
                    TypeArg::Sym(name),
                    TypeArg::List(self.decode_list(&mut engine, fields[1]).into()),
                ],
            )
        } else {
            let fields = constructor_fields(&mut engine, "Apply", expression)
                .expect("a constructor application");
            let ctor =
                constructor_fields(&mut engine, "Ctor", fields[0]).expect("a named constructor");
            let name = engine.value_to_base::<String>(ctor[0]);
            let (dialect, name) = name.split_once('.').expect("a qualified constructor");
            (
                self.ctor_named(dialect, name)
                    .expect("a registered constructor"),
                self.decode_list(&mut engine, fields[1]),
            )
        };
        drop(engine);
        read(ctor, &args)
    }

    fn encode_arg(&self, engine: &mut EGraph, arg: &TypeArg) -> EggValue {
        match arg {
            TypeArg::Nat(n) => engine
                .update(|mut state| {
                    let n = state.base_to_value(
                        n.to_string()
                            .parse::<tir_egglog::sort::BigInt>()
                            .expect("a natural number"),
                    );
                    state.add("NatLit", n)
                })
                .expect("natural expression"),
            TypeArg::Sym(name) => engine
                .update(|mut state| state.add("SymbolParam", i64::from(name.0)))
                .expect("symbol expression"),
            TypeArg::Type(id) => engine
                .read(|state| state.lookup("TypeHandle", i64::from(id.0)))
                .expect("type handle")
                .expect("an interned child"),
            TypeArg::Var(index) => engine
                .update(|mut state| state.add("UnkindedBound", i64::from(*index)))
                .expect("bound argument"),
            TypeArg::Lam(arity, body) => {
                let body = self.encode_arg(engine, body);
                engine
                    .update(|mut state| state.add("UnkindedLambda", (i64::from(*arity), body)))
                    .expect("argument binder")
            }
            TypeArg::List(items) => {
                let items = self.encode_list(engine, items);
                engine
                    .update(|mut state| state.add("ListArg", items))
                    .expect("type argument list")
            }
        }
    }

    fn encode_list(&self, engine: &mut EGraph, args: &[TypeArg]) -> EggValue {
        let mut tail = engine
            .update(|mut state| state.add("TNil", tir_egglog::RawValues(vec![])))
            .expect("type list");
        for arg in args.iter().rev() {
            let head = self.encode_arg(engine, arg);
            tail = engine
                .update(|mut state| state.add("TCons", (head, tail)))
                .expect("type list");
        }
        tail
    }

    fn decode_list(&self, engine: &mut EGraph, list: EggValue) -> Vec<TypeArg> {
        let schedule = tir_egglog::ast::Command::RunSchedule(tir_egglog::Schedule::Saturate(
            tir_egglog::span!(),
            Box::new(tir_egglog::Schedule::run("ir-type-index")),
        ));
        engine.seminaive = false;
        engine
            .run_program(vec![schedule])
            .expect("structural type projections");
        let db = engine.database_mut();
        let table = db.table("TypeItem").expect("type item relation");
        let index = db.add_kept_index(table, &[0], &[1]);
        let items = db
            .listed(index, &[list])
            .map(|row| row[2])
            .collect::<Vec<_>>();
        items
            .into_iter()
            .map(|item| self.decode_arg(engine, item))
            .collect()
    }

    fn decode_arg(&self, engine: &mut EGraph, expression: EggValue) -> TypeArg {
        if let Some(fields) = constructor_fields(engine, "NatLit", expression) {
            let value = engine.value_to_base::<tir_egglog::sort::BigInt>(fields[0]);
            return TypeArg::Nat(value.to_string().parse().expect("a u64 natural argument"));
        }
        if let Some(fields) = constructor_fields(engine, "SymbolParam", expression) {
            return TypeArg::Sym(Name(engine.value_to_base::<i64>(fields[0]) as u32));
        }
        if let Some(fields) = constructor_fields(engine, "UnkindedBound", expression) {
            return TypeArg::Var(engine.value_to_base::<i64>(fields[0]) as u32);
        }
        if let Some(fields) = constructor_fields(engine, "ListArg", expression) {
            return TypeArg::List(self.decode_list(engine, fields[0]).into());
        }
        if let Some(fields) = constructor_fields(engine, "UnkindedLambda", expression) {
            let arity = engine.value_to_base::<i64>(fields[0]) as u32;
            return TypeArg::Lam(arity, Box::new(self.decode_arg(engine, fields[1])));
        }
        let id = engine
            .read(|state| state.lookup("TypeHandleOf", expression))
            .expect("type handle table")
            .expect("a child type handle");
        TypeArg::Type(TypeId(engine.value_to_base::<i64>(id) as u32))
    }

    /// Register `ctor`, or find the id it was registered under before.
    pub fn register(&self, ctor: &'static TypeCtor) -> CtorId {
        let address = std::ptr::from_ref(ctor) as usize;
        let known = self.by_address.read().unwrap_or_else(|e| e.into_inner());
        if let Some(&id) = known.get(&address) {
            return id;
        }
        drop(known);
        let mut registered = self.registered.write().unwrap_or_else(|e| e.into_inner());
        let id = match registered.get(&(ctor.dialect, ctor.name)) {
            Some(&id) => id,
            None => {
                let id = CtorId(registered.len() as u32);
                let set = self.ctors.slot(id.0).set(ctor);
                assert!(set.is_ok(), "a constructor id is registered once");
                registered.insert((ctor.dialect, ctor.name), id);
                id
            }
        };
        drop(registered);
        let mut engine = self.engine.borrow_mut();
        let mut kind = Self::kind_expression(&mut engine, ctor.kind);
        for (_, param) in ctor.params.iter().rev() {
            let input = match param {
                Param::One(kind) | Param::Opt(kind) => Self::kind_expression(&mut engine, *kind),
                Param::List(kind) => {
                    let kind = Self::kind_expression(&mut engine, *kind);
                    engine
                        .update(|mut state| state.add("KList", kind))
                        .expect("list kind")
                }
                Param::Fields => engine
                    .update(|mut state| state.add("KRow", tir_egglog::RawValues(vec![])))
                    .expect("row kind"),
                Param::Any => engine
                    .update(|mut state| state.add("KAny", tir_egglog::RawValues(vec![])))
                    .expect("argument kind"),
            };
            kind = engine
                .update(|mut state| state.add("KArrow", (input, kind)))
                .expect("constructor kind");
        }
        let name = format!("{}.{}", ctor.dialect, ctor.name);
        engine
            .update(|mut state| state.set("ConstructorKind", name, kind))
            .expect("registered constructor kind");
        self.indexes_dirty.store(true, Ordering::Relaxed);
        drop(engine);
        let mut known = self.by_address.write().unwrap_or_else(|e| e.into_inner());
        known.insert(address, id);
        id
    }

    fn kind_expression(engine: &mut EGraph, kind: Kind) -> EggValue {
        let name = match kind {
            Kind::Data => "KType",
            Kind::Ghost => "KGhost",
            Kind::Nat => "KNat",
            Kind::Sym => "KSym",
            Kind::Bool => "KBool",
        };
        engine
            .update(|mut state| state.add(name, tir_egglog::RawValues(vec![])))
            .expect("registered kind")
    }

    fn decode_kind(engine: &mut EGraph, value: EggValue) -> Kind {
        for (name, kind) in [
            ("KType", Kind::Data),
            ("KGhost", Kind::Ghost),
            ("KNat", Kind::Nat),
            ("KSym", Kind::Sym),
            ("KBool", Kind::Bool),
        ] {
            if constructor_fields(engine, name, value).is_some() {
                return kind;
            }
        }
        panic!("a registered scalar kind")
    }

    pub fn ctor_named(&self, dialect: &str, name: &str) -> Option<CtorId> {
        let registered = self.registered.read().unwrap_or_else(|e| e.into_inner());
        registered.get(&(dialect, name)).copied()
    }

    pub fn ctor_info(&self, ctor: CtorId) -> &'static TypeCtor {
        self.ctors
            .get(ctor.0)
            .expect("a constructor this store registered")
    }

    /// Every constructor registered so far, in registration order.
    pub fn ctors(&self) -> Vec<CtorId> {
        let count = self
            .registered
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        (0..count as u32).map(CtorId).collect()
    }

    pub fn name(&self, text: &str) -> Name {
        let mut engine = self.engine.borrow_mut();
        if let Some(id) = engine
            .read(|state| state.lookup("SymbolId", text))
            .expect("symbol identity table")
        {
            return Name(engine.value_to_base::<i64>(id) as u32);
        }
        self.indexes_dirty.store(true, Ordering::Relaxed);
        let id = self.entities.fetch_add(1, Ordering::Relaxed);
        engine
            .update(|mut state| {
                state.set("SymbolName", i64::from(id), text)?;
                state.set("SymbolId", text, i64::from(id))
            })
            .expect("symbol identity");
        Name(id)
    }

    pub fn name_text(&self, name: Name) -> String {
        let engine = self.engine.borrow_mut();
        let value = engine
            .read(|state| state.lookup("SymbolName", i64::from(name.0)))
            .expect("symbol name table")
            .expect("an interned symbol");
        engine.value_to_base(value)
    }

    /// A structural term argument, including binders and lists.
    pub fn argument_expression(&self, arg: &TypeArg) -> EggValue {
        let mut engine = self.engine.borrow_mut();
        self.encode_arg(&mut engine, arg)
    }

    /// Decode an argument stored structurally in this engine.
    pub fn expression_argument(&self, expression: EggValue) -> TypeArg {
        let mut engine = self.engine.borrow_mut();
        self.decode_arg(&mut engine, expression)
    }

    /// Intern `ctor` applied to `args`; see [`Terms::try_intern`].
    pub fn intern(&self, ctor: CtorId, args: Vec<TypeArg>) -> Result<TypeId, String> {
        self.try_intern(ctor, args.into_boxed_slice())
    }

    /// Check `ctor` applied to `args` and intern it in normal form.
    pub fn try_intern(&self, ctor: CtorId, args: Box<[TypeArg]>) -> Result<TypeId, String> {
        let args = self.normal_form(ctor, args);
        if let Some(args) = self.without_default(ctor, &args) {
            return self.try_intern(ctor, args);
        }
        self.check(ctor, &args)?;
        let info = self.ctor_info(ctor);
        let mut engine = self.engine.borrow_mut();
        let expression = match (info.dialect, info.name, &*args) {
            ("builtin", "fn", [TypeArg::List(inputs), output]) => {
                let inputs = self.encode_list(&mut engine, inputs);
                let outputs = self.encode_list(&mut engine, std::slice::from_ref(output));
                engine.update(|mut state| state.add("FunctionType", (inputs, outputs)))
            }
            ("builtin", "tuple", [TypeArg::List(items)]) => {
                let items = self.encode_list(&mut engine, items);
                engine.update(|mut state| state.add("ProductType", items))
            }
            ("type", "named", [TypeArg::Sym(name), TypeArg::List(args)]) => {
                let args = self.encode_list(&mut engine, args);
                engine.update(|mut state| state.add("Nominal", (i64::from(name.0), args)))
            }
            _ => {
                let name = format!("{}.{}", info.dialect, info.name);
                let head = engine
                    .update(|mut state| state.add("Ctor", name))
                    .map_err(|e| e.to_string())?;
                let args = self.encode_list(&mut engine, &args);
                engine.update(|mut state| state.add("Apply", (head, args)))
            }
        }
        .map_err(|e| e.to_string())?;
        if let Some(id) = engine
            .read(|state| state.lookup("TypeHandleOf", expression))
            .map_err(|e| e.to_string())?
        {
            return Ok(TypeId(engine.value_to_base::<i64>(id) as u32));
        }
        self.indexes_dirty.store(true, Ordering::Relaxed);
        let id = self.minted.fetch_add(1, Ordering::Relaxed);
        engine
            .update(|mut state| {
                state.set("TypeHandle", i64::from(id), expression)?;
                state.set("TypeHandleOf", expression, i64::from(id))
            })
            .map_err(|e| e.to_string())?;
        Ok(TypeId(id))
    }

    /// Let the optional last argument of `ctor` default to `value`: a term
    /// that spells `value` out interns as the one that omits it. A default is
    /// set once.
    pub fn set_default(&self, ctor: CtorId, value: TypeArg) -> Result<(), String> {
        let set = self.defaults.slot(ctor.0).get_or_init(|| value.clone());
        match *set == value {
            true => Ok(()),
            false => Err(format!(
                "type '{}' already defaults to {}",
                self.ctor_info(ctor).name,
                self.display_arg(set)
            )),
        }
    }

    fn without_default(&self, ctor: CtorId, args: &[TypeArg]) -> Option<Box<[TypeArg]>> {
        let (last, rest) = args.split_last()?;
        let full = args.len() == self.ctor_info(ctor).params.len();
        let default = self.defaults.get(ctor.0);
        (full && default == Some(last)).then(|| rest.into())
    }

    fn check(&self, ctor: CtorId, args: &[TypeArg]) -> Result<(), String> {
        let info = self.ctor_info(ctor);
        let fixed = info
            .params
            .iter()
            .filter(|(_, p)| !matches!(p, Param::Opt(_)));
        let arity_ok = args.len() == info.params.len() || args.len() == fixed.count();
        if !arity_ok {
            return Err(format!(
                "type '{}.{}' takes {} arguments, got {}",
                info.dialect,
                info.name,
                info.params.len(),
                args.len()
            ));
        }
        for ((param, shape), arg) in info.params.iter().zip(args) {
            let fits = match shape {
                Param::One(kind) | Param::Opt(kind) => self.fits(*kind, arg),
                Param::List(kind) => match arg {
                    TypeArg::List(items) => items.iter().all(|item| self.fits(*kind, item)),
                    TypeArg::Var(_) => true,
                    _ => false,
                },
                Param::Fields => match arg {
                    TypeArg::List(fields) => fields.iter().all(|field| {
                        matches!(field, TypeArg::List(pair) if pair.len() == 2
                            && self.fits(Kind::Sym, &pair[0])
                            && self.fits(Kind::Data, &pair[1]))
                    }),
                    TypeArg::Var(_) => true,
                    _ => false,
                },
                Param::Any => true,
            };
            if !fits {
                return Err(format!(
                    "type '{}.{}' parameter '{param}' does not take {}",
                    info.dialect,
                    info.name,
                    self.display_arg(arg)
                ));
            }
        }
        (info.validate)(self, args)
    }

    fn fits(&self, kind: Kind, arg: &TypeArg) -> bool {
        match (kind, arg) {
            (_, TypeArg::Var(_)) => true,
            (Kind::Nat, TypeArg::Nat(_)) | (Kind::Sym, TypeArg::Sym(_)) => true,
            (Kind::Bool, TypeArg::Nat(flag)) => *flag <= 1,
            (kind, TypeArg::Type(id)) => self.kind(*id) == kind,
            _ => false,
        }
    }

    fn normal_form(&self, ctor: CtorId, args: Box<[TypeArg]>) -> Box<[TypeArg]> {
        if ctor != ctor::NAT_ADD {
            return args;
        }
        let [TypeArg::List(items)] = &*args else {
            return args;
        };
        let (constant, mut parts) = self.flatten_sum(items);
        parts.sort_by_key(sum_order);
        if constant != 0 {
            parts.push(TypeArg::Nat(constant));
        }
        Box::new([TypeArg::List(parts.into())])
    }

    fn flatten_sum(&self, items: &[TypeArg]) -> (u64, Vec<TypeArg>) {
        let mut constant = 0u64;
        let mut parts = Vec::new();
        for item in items {
            match item {
                TypeArg::Nat(n) => constant += n,
                TypeArg::Type(id) if self.ctor(*id) == ctor::NAT_ADD => {
                    let [TypeArg::List(inner)] = &*self.term(*id).args else {
                        unreachable!("a sum holds one list")
                    };
                    let (more, rest) = self.flatten_sum(inner);
                    constant += more;
                    parts.extend(rest);
                }
                other => parts.push(other.clone()),
            }
        }
        (constant, parts)
    }

    /// The width `a + b`: a constant when both are, else a sum term.
    pub fn nat_add(&self, a: TypeArg, b: TypeArg) -> TypeArg {
        let (constant, mut parts) = self.flatten_sum(&[a, b]);
        if parts.is_empty() {
            return TypeArg::Nat(constant);
        }
        if constant == 0 && parts.len() == 1 {
            return parts.pop().unwrap();
        }
        parts.push(TypeArg::Nat(constant));
        let id = self
            .intern(ctor::NAT_ADD, vec![TypeArg::List(parts.into())])
            .expect("a sum of widths");
        TypeArg::Type(id)
    }

    pub fn ctor(&self, id: TypeId) -> CtorId {
        self.read(id, |ctor, _| ctor)
    }

    pub fn kind(&self, id: TypeId) -> Kind {
        let ctor = self.ctor(id);
        if ctor == ctor::NAMED
            && let Some(TypeArg::Sym(name)) = self.arg(id, 0)
        {
            let mut engine = self.engine.borrow_mut();
            if let Some(kind) = engine
                .read(|state| state.lookup("NominalKind", i64::from(name.0)))
                .expect("nominal kind")
            {
                return Self::decode_kind(&mut engine, kind);
            }
            return Kind::Data;
        }
        self.ctor_info(ctor).kind
    }

    /// Record that `type.def !name` has a body of `kind`, the kind every
    /// `type.named<name, ..>` has. The definition built last decides: two
    /// modules of one store that define a name with different kinds keep
    /// theirs apart only once a merge renames one.
    pub fn define(&self, name: Name, kind: Kind) {
        let mut engine = self.engine.borrow_mut();
        let kind = Self::kind_expression(&mut engine, kind);
        engine
            .update(|mut state| state.remove("NominalKind", i64::from(name.0)))
            .expect("nominal kind");
        engine
            .update(|mut state| state.set("NominalKind", i64::from(name.0), kind))
            .expect("nominal kind");
        self.indexes_dirty.store(true, Ordering::Relaxed);
    }

    pub fn term(&self, id: TypeId) -> TypeTerm {
        let (ctor, args) = self.read(id, |ctor, args| (ctor, args.into()));
        TypeTerm { ctor, args }
    }

    /// The arguments of `id`.
    pub fn args(&self, id: TypeId) -> Args {
        self.read(id, |_, args| args.iter().cloned().collect())
    }

    /// Argument `index` of `id`, read without decoding the others.
    pub fn arg(&self, id: TypeId, index: usize) -> Option<TypeArg> {
        self.read(id, |_, args| args.get(index).cloned())
    }

    /// Argument `index` of `id` when it is a constant [`TypeArg::Nat`].
    pub fn nat(&self, id: TypeId, index: usize) -> Option<u64> {
        match self.arg(id, index)? {
            TypeArg::Nat(n) => Some(n),
            _ => None,
        }
    }

    /// Argument `index` of `id` when it is a child type.
    pub fn child(&self, id: TypeId, index: usize) -> Option<TypeId> {
        self.arg(id, index)?.as_type()
    }

    /// The width of `builtin.i<W>` when `W` is a constant.
    pub fn int_width(&self, id: TypeId) -> Option<u64> {
        (self.ctor(id) == ctor::INT).then(|| self.nat(id, 0))?
    }

    /// The carrier width of `id`; see [`TypeCtor::carrier`].
    pub fn carrier(&self, id: TypeId) -> Option<TypeArg> {
        let term = self.term(id);
        (self.ctor_info(term.ctor).carrier)(self, &term.args)
    }

    /// The carrier width of `id` when it is a constant.
    pub fn bit_width(&self, id: TypeId) -> Option<u64> {
        match self.carrier(id)? {
            TypeArg::Nat(width) => Some(width),
            _ => None,
        }
    }

    /// `builtin.i<width>`.
    pub fn int(&self, width: u64) -> TypeId {
        self.intern(ctor::INT, vec![TypeArg::Nat(width)])
            .expect("an integer type")
    }

    /// `builtin.f<exponent, mantissa>`.
    pub fn float(&self, exponent: u64, mantissa: u64) -> TypeId {
        let args = vec![TypeArg::Nat(exponent), TypeArg::Nat(mantissa)];
        self.intern(ctor::FLOAT, args).expect("a float type")
    }

    /// The exponent and mantissa widths of `builtin.f<E, M>` when both are
    /// constants.
    pub fn float_format(&self, id: TypeId) -> Option<(u64, u64)> {
        if self.ctor(id) != ctor::FLOAT {
            return None;
        }
        Some((self.nat(id, 0)?, self.nat(id, 1)?))
    }

    /// Ids published so far: every id below it names a row a reader loads.
    pub fn len(&self) -> u32 {
        self.minted.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `id` in the generic spelling `!dialect.name<args>`.
    pub fn display(&self, id: TypeId) -> String {
        let term = self.term(id);
        let info = self.ctor_info(term.ctor);
        let mut out = format!("!{}.{}", info.dialect, info.name);
        if !term.args.is_empty() {
            let args: Vec<_> = term.args.iter().map(|arg| self.display_arg(arg)).collect();
            out += &format!("<{}>", args.join(", "));
        }
        out
    }

    pub fn display_arg(&self, arg: &TypeArg) -> String {
        match arg {
            TypeArg::Nat(n) => n.to_string(),
            TypeArg::Sym(name) => self.name_text(*name),
            TypeArg::Type(id) => self.display(*id),
            TypeArg::Var(index) => format!("${index}"),
            TypeArg::Lam(arity, body) => format!("\\{arity}. {}", self.display_arg(body)),
            TypeArg::List(items) => {
                let items: Vec<_> = items.iter().map(|item| self.display_arg(item)).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }
}

// Summands sort by variable, then by term id, so equal sums have one order.
fn sum_order(arg: &TypeArg) -> (u8, u32) {
    match arg {
        TypeArg::Var(index) => (0, *index),
        TypeArg::Type(id) => (1, id.0),
        _ => (2, 0),
    }
}

/// Read a structural constructor by its output index. No structural terms are unioned.
pub fn constructor_fields(
    engine: &mut EGraph,
    name: &str,
    expression: EggValue,
) -> Option<Vec<EggValue>> {
    let db = engine.database_mut();
    let table = db.table(name).ok()?;
    let arity = db.schema().table(table).key_arity();
    let index = db.add_kept_index(table, &[arity], &[]);
    let row = db.listed(index, &[expression]).next()?;
    Some(row.values().take(arity).collect())
}
