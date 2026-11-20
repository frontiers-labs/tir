//! Context-owned type terms, symbols and constructor declarations.

mod tables;

use std::{cell::RefCell, fmt, rc::Rc};
use tables::{Arguments, Declaration, DefaultArg, Tables, load, replace, store};
use tir_egglog::{Database, EGraph};

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use smallvec::SmallVec;
use tir_adt::Sym;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

impl TypeId {
    /// `builtin.state<memory>`, the first term the store interns, so a memory
    /// state is told from a value without a lookup.
    pub const STATE: TypeId = TypeId(0);
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(u32);

impl Name {
    /// The symbol's position in this context.
    pub fn number(self) -> u32 {
        self.0
    }

    /// Recover a symbol handle from its context-local number.
    pub fn from_number(number: u32) -> Self {
        Self(number)
    }
}

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
#[repr(i64)]
pub enum Kind {
    Data,
    /// State dependencies order effects and carry no value.
    Ghost,
    Nat,
    Sym,
    /// A flag: `Nat(0)` or `Nat(1)`.
    Bool,
}

impl Kind {
    fn from_number(n: i64) -> Self {
        [Self::Data, Self::Ghost, Self::Nat, Self::Sym, Self::Bool][n as usize]
    }
}

/// The shape of one constructor parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Param {
    One(Kind),
    /// Present or absent. Only the last parameter can be optional.
    Opt(Kind),
    List(Kind),
    /// A list of `(name, type)` pairs, each a two-element list.
    Fields,
    /// An index whose kind is the named source domain.
    Source(&'static str),
    /// A list whose elements have a source-defined shape.
    ListOf(&'static Param),
    /// An omitted trailing source argument, encoded without an Option tag.
    Optional(&'static Param),
    /// An untagged source sum, encoded as one of its payloads.
    Encoded(&'static str, &'static [Param]),
    /// Not checked by shape: the constructor's `validate` checks it.
    Any,
}

#[derive(Debug)]
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
    args: Arc<[TypeArg]>,
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

/// The integer widths every store interns up front.
pub const FIXED_INT_WIDTHS: u64 = 128;

/// Context-local type handles are positions in insertion order.
pub struct Terms {
    engine: Rc<RefCell<EGraph>>,
    tables: Tables,
    next_entity: Arc<AtomicU32>,
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
        Self::with_entity_ids(Arc::default())
    }

    /// Allocate type names from the context's shared entity sequence.
    pub fn with_entity_ids(next_entity: Arc<AtomicU32>) -> Self {
        let mut engine = EGraph::default();
        tir_egglog::Program::from_bytes(tir_ir_schema::IR_SCHEMA, engine.database_mut())
            .expect("the compiled structural IR schema loads");
        Self::with_engine(Rc::new(RefCell::new(engine)), next_entity)
    }

    /// Install the metadata tables in the context's engine.
    pub fn with_engine(engine: Rc<RefCell<EGraph>>, next_entity: Arc<AtomicU32>) -> Self {
        let tables = Tables::new(engine.borrow_mut().database_mut());
        let terms = Self {
            engine,
            next_entity,
            tables,
        };
        if !terms.is_empty() {
            let engine = terms.engine.borrow();
            let db = engine.database();
            if let Some(last) = db
                .rows(terms.tables.symbol)
                .map(|r| db.base_value::<i64>(r[2]))
                .max()
            {
                terms
                    .next_entity
                    .fetch_max((last + 1) as u32, Ordering::Relaxed);
            }
            drop(engine);
            return terms;
        }
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

    /// Register a constructor under its qualified name.
    pub fn register(&self, ctor: &'static TypeCtor) -> CtorId {
        let mut engine = self.engine.borrow_mut();
        let db = engine.database_mut();
        let name = [
            db.base(ctor.dialect.to_owned()),
            db.base(ctor.name.to_owned()),
        ];
        if let Some(row) = db.listed(self.tables.ctor_name, &name).next() {
            let id = CtorId(db.base_value::<i64>(row[0]) as u32);
            let mut decl = load::<Declaration>(db, row[3]);
            if decl.info.params != ctor.params && !decl.additional.contains(&ctor.params) {
                decl.additional.push(ctor.params);
                let old = std::array::from_fn::<_, 5, _>(|i| row[i]);
                replace(db, self.tables.ctor, old, 3, store(db, decl));
            }
            return id;
        }
        let id = db.row_count(self.tables.ctor) as u32;
        let decl = Declaration {
            info: ctor,
            additional: Vec::new(),
        };
        db.insert(
            self.tables.ctor,
            &[
                db.base(i64::from(id)),
                name[0],
                name[1],
                store(db, decl),
                store(db, DefaultArg(None)),
            ],
        )
        .expect("type constructor");
        CtorId(id)
    }

    pub fn ctor_named(&self, dialect: &str, name: &str) -> Option<CtorId> {
        let engine = self.engine.borrow();
        let db = engine.database();
        let row = db
            .listed(
                self.tables.ctor_name,
                &[db.base(dialect.to_owned()), db.base(name.to_owned())],
            )
            .next()?;
        Some(CtorId(db.base_value::<i64>(row[0]) as u32))
    }

    fn ctor_row<'a>(&self, db: &'a Database, ctor: CtorId) -> tir_egglog::Row<'a> {
        db.listed_nth(self.tables.ctor_id, &[db.base(i64::from(ctor.0))], 0)
            .expect("registered type constructor")
    }

    pub fn ctor_info(&self, ctor: CtorId) -> &'static TypeCtor {
        let engine = self.engine.borrow();
        let db = engine.database();
        load::<Declaration>(db, self.ctor_row(db, ctor)[3]).info
    }

    /// Every constructor registered so far, in registration order.
    pub fn ctors(&self) -> Vec<CtorId> {
        (0..self.engine.borrow().database().row_count(self.tables.ctor) as u32)
            .map(CtorId)
            .collect()
    }

    /// Intern a spelling for use in this context.
    pub fn intern_symbol(&self, text: &str) -> Sym {
        let mut engine = self.engine.borrow_mut();
        let db = engine.database_mut();
        let spelling = db.base(text.to_owned());
        if let Some(row) = db.listed_nth(self.tables.spelling, &[spelling], 0) {
            return Sym::from_number(db.base_value::<i64>(row[0]) as u32);
        }
        let id = db.row_count(self.tables.symbol) as u32;
        db.insert(
            self.tables.symbol,
            &[
                db.base(i64::from(id)),
                spelling,
                db.base(-1i64),
                db.base(Kind::Data as i64),
            ],
        )
        .expect("symbol");
        Sym::from_number(id)
    }

    /// Find a spelling already interned in this context.
    pub fn symbol_lookup(&self, text: &str) -> Option<Sym> {
        let engine = self.engine.borrow();
        let db = engine.database();
        let row = db.listed_nth(self.tables.spelling, &[db.base(text.to_owned())], 0)?;
        Some(Sym::from_number(db.base_value::<i64>(row[0]) as u32))
    }

    /// Read a spelling by its symbol handle.
    pub fn symbol_text(&self, symbol: Sym) -> String {
        self.symbol_at(symbol.index()).expect("interned symbol")
    }

    /// Read a spelling by its position in this context.
    pub fn symbol_at(&self, index: usize) -> Option<String> {
        let engine = self.engine.borrow();
        let db = engine.database();
        let key = db.base(i64::try_from(index).ok()?);
        Some(db.base_value(db.listed_nth(self.tables.symbol_id, &[key], 0)?[1]))
    }

    pub fn name(&self, text: &str) -> Name {
        let sym = self.intern_symbol(text);
        let mut engine = self.engine.borrow_mut();
        let db = engine.database_mut();
        let row = db
            .listed_nth(self.tables.symbol_id, &[db.base(sym.index() as i64)], 0)
            .expect("interned symbol");
        let entity = db.base_value::<i64>(row[2]);
        if entity >= 0 {
            return Name(entity as u32);
        }
        let old = std::array::from_fn::<_, 4, _>(|i| row[i]);
        let id = self.next_entity.fetch_add(1, Ordering::Relaxed);
        replace(db, self.tables.symbol, old, 2, db.base(i64::from(id)));
        Name(id)
    }

    fn name_row<'a>(&self, db: &'a Database, name: Name) -> tir_egglog::Row<'a> {
        db.listed_nth(self.tables.entity, &[db.base(i64::from(name.0))], 0)
            .expect("interned type name")
    }

    pub fn name_text(&self, name: Name) -> String {
        let engine = self.engine.borrow();
        let db = engine.database();
        db.base_value(self.name_row(db, name)[1])
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
        Ok(self.intern_application(ctor, args))
    }

    /// Intern a partial constructor used only as a type pattern.
    pub fn intern_pattern(&self, ctor: CtorId, args: Vec<TypeArg>) -> Result<TypeId, String> {
        if args.len() > self.ctor_info(ctor).params.len() {
            return Err("type pattern has too many arguments".into());
        }
        Ok(self.intern_application(ctor, args.into_boxed_slice()))
    }

    fn intern_application(&self, ctor: CtorId, args: Box<[TypeArg]>) -> TypeId {
        let mut engine = self.engine.borrow_mut();
        let db = engine.database_mut();
        let arguments = store(db, Arguments(args.into()));
        let constructor = db.base(i64::from(ctor.0));
        if let Some(row) = db
            .listed(self.tables.application, &[arguments, constructor])
            .next()
        {
            return TypeId(db.base_value::<i64>(row[0]) as u32);
        }
        let id = db.row_count(self.tables.term) as u32;
        db.insert(
            self.tables.term,
            &[db.base(i64::from(id)), constructor, arguments],
        )
        .expect("type application");
        TypeId(id)
    }

    /// Let the optional last argument of `ctor` default to `value`. A term
    /// that spells the default out interns as the one that omits it.
    pub fn set_default(&self, ctor: CtorId, value: TypeArg) -> Result<(), String> {
        let mut engine = self.engine.borrow_mut();
        let db = engine.database_mut();
        let row = self.ctor_row(db, ctor);
        if let Some(previous) = load::<DefaultArg>(db, row[4]).0 {
            if previous == value {
                return Ok(());
            }
            let name = load::<Declaration>(db, row[3]).info.name;
            drop(engine);
            return Err(format!(
                "type '{name}' already defaults to {}",
                self.display_arg(&previous)
            ));
        }
        let old = std::array::from_fn::<_, 5, _>(|i| row[i]);
        replace(
            db,
            self.tables.ctor,
            old,
            4,
            store(db, DefaultArg(Some(value))),
        );
        Ok(())
    }

    fn without_default(&self, ctor: CtorId, args: &[TypeArg]) -> Option<Box<[TypeArg]>> {
        let (last, rest) = args.split_last()?;
        let engine = self.engine.borrow();
        let db = engine.database();
        let row = self.ctor_row(db, ctor);
        let info = load::<Declaration>(db, row[3]).info;
        let full = args.len() == info.params.len()
            && matches!(
                info.params.last(),
                Some((_, Param::Opt(_) | Param::Optional(_)))
            );
        (full && load::<DefaultArg>(db, row[4]).0.as_ref() == Some(last)).then(|| rest.into())
    }

    fn check(&self, ctor: CtorId, args: &[TypeArg]) -> Result<(), String> {
        let info = self.ctor_info(ctor);
        let fixed = info
            .params
            .iter()
            .filter(|(_, p)| !matches!(p, Param::Opt(_) | Param::Optional(_)));
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
            if !self.fits_param(shape, arg) {
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

    /// Check one argument against its declared parameter shape.
    pub fn fits_param(&self, shape: &Param, arg: &TypeArg) -> bool {
        self.matches_param(shape, arg, &mut Vec::new())
    }

    fn matches_param<'a>(
        &self,
        shape: &Param,
        arg: &'a TypeArg,
        visiting: &mut Vec<(&'static str, &'a TypeArg)>,
    ) -> bool {
        if matches!(arg, TypeArg::Var(_)) {
            return true;
        }
        match (shape, arg) {
            (Param::One(kind) | Param::Opt(kind), arg) => self.fits(*kind, arg),
            (Param::List(kind), TypeArg::List(items)) => {
                items.iter().all(|item| self.fits(*kind, item))
            }
            (Param::Fields, TypeArg::List(fields)) => fields.iter().all(|field| {
                let TypeArg::List(pair) = field else {
                    return false;
                };
                let [name, ty] = pair.as_ref() else {
                    return false;
                };
                self.fits(Kind::Sym, name)
                    && self.matches_param(&Param::Source("std::ir::Ty"), ty, visiting)
            }),
            (Param::Source(domain) | Param::Encoded(domain, _), arg) => {
                if self.fits_builtin_source(domain, arg) {
                    return true;
                }
                if visiting.contains(&(domain, arg)) {
                    return false;
                }
                visiting.push((domain, arg));
                let engine = self.engine.borrow();
                let db = engine.database();
                let registered = db
                    .rows(self.tables.ctor)
                    .map(|row| load::<Declaration>(db, row[3]));
                let declared = match shape {
                    Param::Encoded(_, alternatives) => *alternatives,
                    _ => &[],
                };
                let valid = declared
                    .iter()
                    .any(|param| self.matches_param(param, arg, visiting))
                    || registered.into_iter().any(|decl| {
                        std::iter::once(decl.info.params)
                            .chain(decl.additional)
                            .flatten()
                            .any(|(_, param)| {
                                has_encoding(param, domain, &mut |alt| {
                                    self.matches_param(alt, arg, visiting)
                                })
                            })
                    });
                visiting.pop();
                valid
            }
            (Param::Optional(element), arg) => self.matches_param(element, arg, visiting),
            (Param::ListOf(element), TypeArg::List(items)) => items
                .iter()
                .all(|item| self.matches_param(element, item, visiting)),
            (Param::Any, _) => true,
            _ => false,
        }
    }

    fn fits_builtin_source(&self, domain: &str, arg: &TypeArg) -> bool {
        match arg {
            TypeArg::Type(ty) => {
                domain == "std::ir::Ty" && matches!(self.kind(*ty), Kind::Data | Kind::Ghost)
            }
            TypeArg::Sym(name) => {
                let spelling = self.name_text(*name);
                matches!(
                    (domain, spelling.as_str()),
                    ("String", _)
                        | ("std::ir::EffectSet", "std::ir::Pure")
                        | ("std::ir::RegionKind", "std::ir::Cfg" | "std::ir::Nodes")
                        | ("std::ir::Resource", "memory")
                )
            }
            _ => false,
        }
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
        let engine = self.engine.borrow();
        let db = engine.database();
        CtorId(db.base_value::<i64>(self.term_row(db, id)[1]) as u32)
    }

    pub fn kind(&self, id: TypeId) -> Kind {
        let ctor = self.ctor(id);
        if ctor == ctor::NAMED
            && let Some(TypeArg::Sym(name)) = self.arg(id, 0)
        {
            let engine = self.engine.borrow();
            let db = engine.database();
            return Kind::from_number(db.base_value(self.name_row(db, name)[3]));
        }
        self.ctor_info(ctor).kind
    }

    /// Record the kind of the most recent definition of `name`.
    pub fn define(&self, name: Name, kind: Kind) {
        let mut engine = self.engine.borrow_mut();
        let db = engine.database_mut();
        let row = self.name_row(db, name);
        let old = std::array::from_fn::<_, 4, _>(|i| row[i]);
        replace(db, self.tables.symbol, old, 3, db.base(kind as i64));
    }

    fn term_row<'a>(&self, db: &'a Database, id: TypeId) -> tir_egglog::Row<'a> {
        db.listed_nth(self.tables.term_id, &[db.base(i64::from(id.0))], 0)
            .expect("interned type")
    }

    pub fn term(&self, id: TypeId) -> TypeTerm {
        let engine = self.engine.borrow();
        let db = engine.database();
        let row = self.term_row(db, id);
        TypeTerm {
            ctor: CtorId(db.base_value::<i64>(row[1]) as u32),
            args: load::<Arguments>(db, row[2]).0,
        }
    }

    /// The arguments of `id`.
    pub fn args(&self, id: TypeId) -> Args {
        self.term(id).args.iter().cloned().collect()
    }

    /// Argument `index` of `id`, read without cloning the others.
    pub fn arg(&self, id: TypeId, index: usize) -> Option<TypeArg> {
        self.term(id).args.get(index).cloned()
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
        self.engine.borrow().database().row_count(self.tables.term) as u32
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

fn has_encoding(shape: &Param, domain: &str, accepts: &mut impl FnMut(&Param) -> bool) -> bool {
    match shape {
        Param::Encoded(source, alternatives) => {
            (*source == domain && alternatives.iter().any(&mut *accepts))
                || alternatives
                    .iter()
                    .any(|alt| has_encoding(alt, domain, accepts))
        }
        Param::ListOf(element) | Param::Optional(element) => has_encoding(element, domain, accepts),
        _ => false,
    }
}

/// Read a single natural argument from a checked integer type's argument row.
pub fn integer_width(p: &tir_egglog::PrimCtx, encoded: tir_egglog::Value) -> Option<i64> {
    let raw = u32::try_from(p.base_value::<i64>(encoded)).ok()?;
    let args = p.base_value::<tables::Arguments>(tir_egglog::Value::new(raw));
    match args.0.as_ref() {
        [TypeArg::Nat(width)] if *width > 0 => i64::try_from(*width).ok(),
        _ => None,
    }
}
