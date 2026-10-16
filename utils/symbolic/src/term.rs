//! Type terms: every IR type and every semantic type is one interned term of
//! one store.
//!
//! A term is a constructor applied to arguments. A [`TypeId`] is the position
//! of the term's row in the store; rows are minted once, never move and are
//! never reused, so two equal terms have one id and equality is id equality.
//! The rows are a chunked relation: a column of constructors and a clustered
//! table of argument cells. A writer appends under a lock and publishes the
//! rows; a reader reads the rows last published under a shared lock. An id
//! means a type only in the store that minted it; whoever interns owns the
//! store and hands it to every reader.
//!
//! A term interns in normal form. A sum of widths is kept as one sorted
//! [`ctor::NAT_ADD`] term with its constants folded, so equal sums have one id
//! without associative or commutative rewriting.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use smallvec::SmallVec;
use tir_relational::chunk::{CHUNK_ROWS, Chunk, Space, chunk_of, id_at, row_of};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

impl TypeId {
    /// `builtin.state<memory>`, the first term the store interns, so a memory
    /// state is told from a value without a lookup.
    pub const STATE: TypeId = TypeId(0);
    /// `builtin.state<fp.env>`, the second term the store interns.
    pub const FP_ENV: TypeId = TypeId(1);
    /// `builtin.i<1>`; the integers up to [`FIXED_INT_WIDTHS`] bits follow it
    /// in width order, so [`Terms::int`] finds them without a lookup.
    const FIRST_INT: u32 = 2;

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
/// It is interned by the term store, not by a context.
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

// Argument cells. A term's arguments are one stream of cells in the clustered
// table: a tag cell, then the payload.
const NAT: u32 = 0;
const SYM: u32 = 1;
const TYPE: u32 = 2;
const VAR: u32 = 3;
const LAM: u32 = 4;
const LIST: u32 = 5;

fn encode(arg: &TypeArg, cells: &mut SmallVec<[u32; 16]>) {
    match arg {
        TypeArg::Nat(n) => cells.extend([NAT, *n as u32, (*n >> 32) as u32]),
        TypeArg::Sym(Name(name)) => cells.extend([SYM, *name]),
        TypeArg::Type(id) => cells.extend([TYPE, id.0]),
        TypeArg::Var(index) => cells.extend([VAR, *index]),
        TypeArg::Lam(arity, body) => {
            cells.extend([LAM, *arity]);
            encode(body, cells);
        }
        TypeArg::List(items) => {
            cells.extend([LIST, items.len() as u32]);
            for item in items {
                encode(item, cells);
            }
        }
    }
}

fn decode(cells: &[u32], at: &mut usize) -> TypeArg {
    let tag = cells[*at];
    let first = cells[*at + 1];
    *at += 2;
    match tag {
        NAT => {
            let high = cells[*at];
            *at += 1;
            TypeArg::Nat(u64::from(first) | u64::from(high) << 32)
        }
        SYM => TypeArg::Sym(Name(first)),
        TYPE => TypeArg::Type(TypeId(first)),
        VAR => TypeArg::Var(first),
        LAM => TypeArg::Lam(first, Box::new(decode(cells, at))),
        LIST => TypeArg::List((0..first).map(|_| decode(cells, at)).collect()),
        _ => unreachable!("an argument tag"),
    }
}

fn skip(cells: &[u32], at: &mut usize) {
    let tag = cells[*at];
    let first = cells[*at + 1];
    *at += 2;
    match tag {
        NAT => *at += 1,
        LAM => skip(cells, at),
        LIST => (0..first).for_each(|_| skip(cells, at)),
        _ => {}
    }
}

/// The arguments of one term, read out of its cells: a term of up to four
/// flat arguments allocates nothing.
pub type Args = SmallVec<[TypeArg; 4]>;

fn decode_all(cells: &[u32]) -> Args {
    let mut at = 0;
    let mut args = Args::new();
    while at < cells.len() {
        args.push(decode(cells, &mut at));
    }
    args
}

// Every argument among `cells`, in a slice of exactly their count.
fn decode_boxed(cells: &[u32]) -> Box<[TypeArg]> {
    let mut count = 0;
    let mut at = 0;
    while at < cells.len() {
        skip(cells, &mut at);
        count += 1;
    }
    let mut args = Vec::with_capacity(count);
    at = 0;
    while at < cells.len() {
        args.push(decode(cells, &mut at));
    }
    args.into_boxed_slice()
}

// Argument `index` among `cells` when it is a constant width, read in place.
fn nat_at(cells: &[u32], index: usize) -> Option<u64> {
    let mut at = 0;
    for _ in 0..index {
        if at >= cells.len() {
            return None;
        }
        skip(cells, &mut at);
    }
    match cells.get(at..at + 3)? {
        &[NAT, low, high] => Some(u64::from(low) | u64::from(high) << 32),
        _ => None,
    }
}

// The width of one of the integers every store interns up front.
fn fixed_int(id: TypeId) -> Option<u64> {
    let width = u64::from(id.0.checked_sub(TypeId::FIRST_INT)?) + 1;
    (width <= FIXED_INT_WIDTHS).then_some(width)
}

// Argument `index` among `cells`, decoded alone.
fn decode_at(cells: &[u32], index: usize) -> Option<TypeArg> {
    let mut at = 0;
    for _ in 0..index {
        if at >= cells.len() {
            return None;
        }
        skip(cells, &mut at);
    }
    (at < cells.len()).then(|| decode(cells, &mut at))
}

type Rows = Space<1, 1>;

const SHARDS: usize = 16;

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

    /// Slot `index`, its segment allocated if it is not yet.
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

/// A store of type terms. Every store registers the base constructors in one
/// order and interns the two states, then the integers up to
/// [`FIXED_INT_WIDTHS`] bits, first, so [`ctor`] ids, [`TypeId::STATE`],
/// [`TypeId::FP_ENV`] and those integers' ids hold in each.
pub struct Terms {
    /// The rows a writer appends to.
    rows: Mutex<Rows>,
    /// The rows as the last append left them, which a reader reads.
    published: RwLock<Rows>,
    /// Ids published so far.
    minted: AtomicU32,
    // Sharded by constructor so parallel interning of different constructors
    // takes different locks.
    lookup: [RwLock<HashMap<u32, Vec<TypeId>, tir_adt::FxBuildHasher>>; SHARDS],
    ctors: Segmented<&'static TypeCtor>,
    // Each registered constructor's id by the address of its description, so
    // a dialect's static finds its id without hashing its name.
    by_address: RwLock<HashMap<usize, CtorId, tir_adt::FxBuildHasher>>,
    // By constructor: the value its omitted optional last argument stands for.
    defaults: Segmented<TypeArg>,
    registered: RwLock<HashMap<(&'static str, &'static str), CtorId>>,
    names: RwLock<Names>,
    defined: RwLock<HashMap<Name, Kind>>,
}

#[derive(Default)]
struct Names {
    text: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32>,
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
        let terms = Terms {
            rows: Mutex::default(),
            published: RwLock::default(),
            minted: AtomicU32::new(0),
            lookup: std::array::from_fn(|_| RwLock::default()),
            ctors: Segmented::new(),
            by_address: RwLock::default(),
            defaults: Segmented::new(),
            registered: RwLock::default(),
            names: RwLock::default(),
            defined: RwLock::default(),
        };
        for ctor in &ctor::BASE {
            terms.register(ctor);
        }
        for (resource, id) in [("memory", TypeId::STATE), ("fp.env", TypeId::FP_ENV)] {
            let resource = TypeArg::Sym(terms.name(resource));
            let state = terms.intern(ctor::STATE, vec![resource]);
            assert_eq!(state, Ok(id));
        }
        // Found by `try_intern` from their width, so the lookup holds none.
        for width in 1..=FIXED_INT_WIDTHS {
            let cells = [NAT, width as u32, 0];
            let int = terms.push(&mut terms.lock_rows(), ctor::INT, &cells);
            debug_assert_eq!(int, TypeId(TypeId::FIRST_INT + width as u32 - 1));
        }
        terms.publish(&terms.lock_rows());
        terms
    }

    fn lock_rows(&self) -> std::sync::MutexGuard<'_, Rows> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }

    // Publish `rows` for readers: a clone copies a pointer per page, and the
    // next append copies only the chunk it writes.
    fn publish(&self, rows: &Rows) {
        *self.published.write().unwrap_or_else(|e| e.into_inner()) = rows.clone();
        // Counted once readable: an id below `len` is in what a reader loads.
        let minted = match rows.chunks() {
            0 => 0,
            n => id_at(n - 1, rows.chunk(n - 1).len()),
        };
        self.minted.store(minted, Ordering::Release);
    }

    // Append a row under the next id; it is read once published.
    fn push(&self, rows: &mut Rows, ctor: CtorId, cells: &[u32]) -> TypeId {
        let last = rows.chunks().checked_sub(1);
        let index = match last {
            Some(index) if !rows.chunk(index).is_full() => index,
            _ => {
                let index = rows.chunks();
                rows.put(index, std::sync::Arc::new(Chunk::new(0)));
                index
            }
        };
        let chunk = rows.chunk_mut(index);
        let row = chunk.push([ctor.0], 0);
        debug_assert!(row < CHUNK_ROWS);
        chunk.seg_mut(0).replace(row, cells);
        TypeId(id_at(index, row))
    }

    // `read` of the constructor and argument cells of `id`, in the rows last
    // published: an id is handed out once the rows holding it are.
    fn read<R>(&self, id: TypeId, read: impl FnOnce(CtorId, &[u32]) -> R) -> R {
        let (index, row) = (chunk_of(id.0), row_of(id.0));
        fn cells(rows: &Rows, index: u32, row: usize) -> Option<(CtorId, &[u32])> {
            let chunk = rows.find(index).filter(|chunk| row < chunk.len())?;
            Some((CtorId(chunk.cell(0, row)), chunk.seg(0).of(row)))
        }
        // `read` takes no lock, so holding this one cannot deadlock.
        let rows = self.published.read().unwrap_or_else(|e| e.into_inner());
        let (ctor, found) = cells(&rows, index, row).expect("an id this store minted");
        read(ctor, found)
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
        let mut known = self.by_address.write().unwrap_or_else(|e| e.into_inner());
        known.insert(address, id);
        id
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
        if let Some(&id) = self
            .names
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .ids
            .get(text)
        {
            return Name(id);
        }
        let mut names = self.names.write().unwrap_or_else(|e| e.into_inner());
        let next = names.text.len() as u32;
        let id = *names.ids.entry(text.into()).or_insert(next);
        if id == next {
            names.text.push(text.into());
        }
        Name(id)
    }

    pub fn name_text(&self, name: Name) -> String {
        let names = self.names.read().unwrap_or_else(|e| e.into_inner());
        names.text[name.0 as usize].to_string()
    }

    /// Intern `ctor` applied to `args`; see [`Terms::try_intern`].
    pub fn intern(&self, ctor: CtorId, args: Vec<TypeArg>) -> Result<TypeId, String> {
        self.try_intern(ctor, args.into_boxed_slice())
    }

    /// Check `ctor` applied to `args` and intern it in normal form.
    pub fn try_intern(&self, ctor: CtorId, args: Box<[TypeArg]>) -> Result<TypeId, String> {
        let args = self.normal_form(ctor, args);
        if let (ctor::INT, [TypeArg::Nat(width @ 1..=FIXED_INT_WIDTHS)]) = (ctor, &*args) {
            return Ok(TypeId(TypeId::FIRST_INT + *width as u32 - 1));
        }
        let mut cells = SmallVec::<[u32; 16]>::new();
        for arg in args.iter() {
            encode(arg, &mut cells);
        }
        let key = tir_adt::simd::hash_row(&cells) ^ ctor.0.wrapping_mul(0x9e37_79b9);
        let shard = &self.lookup[ctor.0 as usize % SHARDS];
        let same =
            |id: &&TypeId| self.read(**id, |found, held| found == ctor && held == &cells[..]);
        {
            let lookup = shard.read().unwrap_or_else(|e| e.into_inner());
            if let Some(&id) = lookup.get(&key).and_then(|found| found.iter().find(same)) {
                return Ok(id);
            }
        }
        // A default spelled out is the term without it, so both name one id.
        if let Some(args) = self.without_default(ctor, &args) {
            return self.try_intern(ctor, args);
        }
        // Only a term not interned yet is checked: one that is passed.
        self.check(ctor, &args)?;
        let mut lookup = shard.write().unwrap_or_else(|e| e.into_inner());
        // Another thread can have interned it between the two locks.
        if let Some(&id) = lookup.get(&key).and_then(|found| found.iter().find(same)) {
            return Ok(id);
        }
        let mut rows = self.lock_rows();
        let id = self.push(&mut rows, ctor, &cells);
        self.publish(&rows);
        drop(rows);
        lookup.entry(key).or_default().push(id);
        Ok(id)
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
        if fixed_int(id).is_some() {
            return ctor::INT;
        }
        self.read(id, |ctor, _| ctor)
    }

    pub fn kind(&self, id: TypeId) -> Kind {
        let ctor = self.ctor(id);
        if ctor == ctor::NAMED
            && let Some(TypeArg::Sym(name)) = self.arg(id, 0)
        {
            let defined = self.defined.read().unwrap_or_else(|e| e.into_inner());
            // A name with no definition yet is an incomplete data type.
            return defined.get(&name).copied().unwrap_or(Kind::Data);
        }
        self.ctor_info(ctor).kind
    }

    /// Record that `type.def !name` has a body of `kind`, the kind every
    /// `type.named<name, ..>` has. The definition built last decides: two
    /// modules of one store that define a name with different kinds keep
    /// theirs apart only once a merge renames one.
    pub fn define(&self, name: Name, kind: Kind) {
        let mut defined = self.defined.write().unwrap_or_else(|e| e.into_inner());
        defined.insert(name, kind);
    }

    pub fn term(&self, id: TypeId) -> TypeTerm {
        let (ctor, args) = self.read(id, |ctor, cells| (ctor, decode_boxed(cells)));
        TypeTerm { ctor, args }
    }

    /// The arguments of `id`.
    pub fn args(&self, id: TypeId) -> Args {
        self.read(id, |_, cells| decode_all(cells))
    }

    /// Argument `index` of `id`, read without decoding the others.
    pub fn arg(&self, id: TypeId, index: usize) -> Option<TypeArg> {
        self.read(id, |_, cells| decode_at(cells, index))
    }

    /// Argument `index` of `id` when it is a constant [`TypeArg::Nat`].
    pub fn nat(&self, id: TypeId, index: usize) -> Option<u64> {
        if let Some(width) = fixed_int(id) {
            return (index == 0).then_some(width);
        }
        self.read(id, |_, cells| nat_at(cells, index))
    }

    /// Argument `index` of `id` when it is a child type.
    pub fn child(&self, id: TypeId, index: usize) -> Option<TypeId> {
        self.arg(id, index)?.as_type()
    }

    /// The width of `builtin.i<W>` when `W` is a constant.
    pub fn int_width(&self, id: TypeId) -> Option<u64> {
        if let Some(width) = fixed_int(id) {
            return Some(width);
        }
        self.read(id, |ctor, cells| {
            (ctor == ctor::INT).then(|| nat_at(cells, 0))?
        })
    }

    /// The carrier width of `id`; see [`TypeCtor::carrier`].
    pub fn carrier(&self, id: TypeId) -> Option<TypeArg> {
        if let Some(width) = fixed_int(id) {
            return Some(TypeArg::Nat(width));
        }
        let (ctor, args) = self.read(id, |ctor, cells| (ctor, decode_all(cells)));
        (self.ctor_info(ctor).carrier)(self, &args)
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
        if (1..=FIXED_INT_WIDTHS).contains(&width) {
            return TypeId(TypeId::FIRST_INT + width as u32 - 1);
        }
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
