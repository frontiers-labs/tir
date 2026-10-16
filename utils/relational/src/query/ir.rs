//! The query IR: atoms over a database's tables, lattice columns and client
//! tables, guards
//! over the words a match binds, and the host a search asks for what only the
//! client knows.

use smallvec::SmallVec;

use crate::Ref;
use crate::db::{LatticeId, SpaceId};

/// A variable of a [`Query`], numbered from zero. It binds a reference: an id
/// and an offset. A variable bound twice must bind the same reference both
/// times.
pub type Var = u32;

/// A scalar variable: one word, read off a row or computed by a guard.
pub type Scalar = u32;

/// A set of cells the host resolves per database: which cells a selected
/// column may hold, and in which tables. Shared by a query and its nots.
pub type Param = u32;

/// One of the host's functions over words.
pub type ExternId = u32;

/// An integer expression over bound scalars.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Expr {
    Lit(i64),
    Scalar(Scalar),
    Sub(Box<Expr>, Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    /// `2^e - 1`, the all-ones value of `e` bits.
    Ones(Box<Expr>),
    /// Bitwise conjunction, for masking a literal to the width the class spells
    /// it at.
    And(Box<Expr>, Box<Expr>),
    /// One when the operand is zero, zero otherwise — a comparison's negation,
    /// which is what a rule proving the complement of a settled comparison
    /// needs to spell.
    IsZero(Box<Expr>),
}

impl Expr {
    pub(crate) fn eval(&self, scalars: &[u64]) -> Option<i64> {
        Some(match self {
            Expr::Lit(value) => *value,
            Expr::Scalar(slot) => scalars[*slot as usize] as i64,
            Expr::Sub(a, b) => a.eval(scalars)?.checked_sub(b.eval(scalars)?)?,
            Expr::Add(a, b) => a.eval(scalars)?.checked_add(b.eval(scalars)?)?,
            Expr::Ones(e) => match e.eval(scalars)? {
                64 => u64::MAX as i64,
                bits @ 0..64 => ((1u64 << bits) - 1) as i64,
                _ => return None,
            },
            Expr::And(a, b) => a.eval(scalars)? & b.eval(scalars)?,
            Expr::IsZero(e) => i64::from(e.eval(scalars)? == 0),
        })
    }

    pub(super) fn lone(&self) -> Option<Scalar> {
        match self {
            Expr::Scalar(slot) => Some(*slot),
            _ => None,
        }
    }

    pub(super) fn ready(&self, known: &[bool]) -> bool {
        match self {
            Expr::Lit(_) => true,
            Expr::Scalar(slot) => known[*slot as usize],
            Expr::Sub(a, b) | Expr::Add(a, b) | Expr::And(a, b) => a.ready(known) && b.ready(known),
            Expr::Ones(e) | Expr::IsZero(e) => e.ready(known),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cmp {
    Lt,
    Le,
    Eq,
    Ne,
}

/// A conjunctive query. Every atom's key variable is reachable from
/// [`Self::root`] through the variables earlier atoms bind, which is what
/// lets a plan bind them in one pass.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Query {
    /// The id space every variable ranges over.
    pub space: SpaceId,
    pub vars: u32,
    pub scalars: u32,
    pub params: u32,
    pub root: Var,
    pub atoms: Vec<Atom>,
    pub guards: Vec<Guard>,
    /// Sub-conjunctions that must have no solution. Placed once everything they
    /// read is bound.
    pub nots: Vec<Nested>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Atom {
    Rows(Rows),
    Lattice(Lattice),
    Place(Place),
    Words(Tuples),
}

impl Atom {
    /// The variable this atom is looked up under: a row atom's value, a
    /// lattice or place atom's key. A client table has rows of words alone.
    pub fn key(&self) -> Option<Var> {
        match self {
            Atom::Rows(rows) => Some(rows.value()),
            Atom::Lattice(lattice) => Some(lattice.key),
            Atom::Place(place) => Some(place.key),
            Atom::Words(_) => None,
        }
    }

    pub(crate) fn writes(&self) -> SmallVec<[Scalar; 4]> {
        match self {
            Atom::Rows(rows) => rows.tag.into_iter().chain(rows.cell).collect(),
            Atom::Lattice(lattice) => match lattice.out {
                Out::Word(slot) => SmallVec::from_slice(&[slot]),
                Out::In(_) | Out::Host(_) => SmallVec::new(),
            },
            Atom::Place(place) => match place.read {
                Read::Offset { offset, .. } | Read::Pinned { offset } => {
                    SmallVec::from_slice(&[offset])
                }
                Read::Width { width } => SmallVec::from_slice(&[width]),
                Read::Conflicted => SmallVec::new(),
            },
            Atom::Words(tuples) => tuples.terms.iter().filter_map(Expr::lone).collect(),
        }
    }
}

/// Rows of the client tables `table` resolves to (see
/// [`Database::add_words`](crate::Core::add_words)), one term per
/// column, at most 32. A term that is a lone scalar nothing has bound when
/// the atom is reached is bound to the row's cell. Any other term is
/// evaluated, and the row must hold its value in that column: a value no
/// cell is wide enough for is in no row. So an atom
/// ranges over the rows that agree with what the match has bound, tables in
/// the order resolved and rows in the order the client added them.
///
/// Placed once its other terms can be evaluated and no guard or atom but
/// another of these is left to bind a lone scalar it reads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tuples {
    pub table: Param,
    pub terms: Vec<Expr>,
}

impl Tuples {
    pub(super) fn keys(&self, known: &[bool]) -> u32 {
        let ready = |column: &usize| self.terms[*column].ready(known);
        (0..self.terms.len())
            .filter(ready)
            .fold(0, |keys, column| keys | 1 << column)
    }
}

/// Rows of the tables `select.param` names. In each table, column
/// `select.column` must hold one of the cells listed for that table. A cell
/// binds its variable to the reference it and its weight are.
///
/// The variable a search starts at, before an atom fixes its offset, stands
/// for its id at whatever offset the row it meets is at, so a search from an
/// id meets every row whose value is in its set.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rows {
    pub select: Select,
    /// One per column, the value column last, which must be a variable.
    /// `terms[select.column]` is [`Term::Any`].
    pub terms: SmallVec<[Term; 6]>,
    /// Binds the row's tag.
    pub tag: Option<Scalar>,
    /// Binds the row's selected cell, written like the tag: whatever the
    /// scalar held before is not compared with it.
    pub cell: Option<Scalar>,
    pub swap: Option<Swap>,
    /// Holds the tag [`Self::tag`] binds to `[lo, hi)`, two expressions over
    /// scalars other atoms and guards bind. The atom is reached through its
    /// value only once those are bound, and then reads the rows of the
    /// interval alone instead of every row with that value; reached any
    /// other way, it reads its rows and the interval filters them.
    pub tag_within: Option<(Expr, Expr)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Select {
    pub column: u8,
    pub param: Param,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Term {
    Any,
    Var(Var),
}

/// The atom also matches a row with columns `columns[0]` and `columns[1]`
/// exchanged, if the row's selected cell is among `when`'s cells for its
/// table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Swap {
    pub columns: [u8; 2],
    pub when: Param,
}

impl Rows {
    /// The variable the value column binds.
    pub fn value(&self) -> Var {
        match self.terms.last() {
            Some(Term::Var(var)) => *var,
            _ => panic!("a row atom binds its value column"),
        }
    }

    pub(crate) fn args(&self) -> impl Iterator<Item = (usize, Var)> + '_ {
        let keys = &self.terms[..self.terms.len() - 1];
        keys.iter()
            .enumerate()
            .filter_map(|(column, term)| match term {
                Term::Var(var) => Some((column, *var)),
                Term::Any => None,
            })
    }

    pub(crate) fn swapped(&self, column: usize) -> usize {
        match self.swap {
            Some(Swap {
                columns: [a, b], ..
            }) if column == a as usize => b as usize,
            Some(Swap {
                columns: [a, b], ..
            }) if column == b as usize => a as usize,
            _ => column,
        }
    }
}

/// The value of `key`'s id in a lattice column.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Lattice {
    pub column: LatticeId,
    pub key: Var,
    pub out: Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Out {
    /// The value is written to the scalar.
    Word(Scalar),
    /// The value must be one of the parameter's cells.
    In(Param),
    /// The value is not read, and need not exist: [`Calls::accepts`] decides
    /// from `key`'s reference. The column still says which epoch the atom is
    /// from.
    Host(ExternId),
}

/// What the id space knows of `key`'s reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Place {
    pub key: Var,
    pub read: Read,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Read {
    /// `key` is `base`, its id at offset zero, plus the scalar `offset`.
    Offset { base: Var, offset: Scalar },
    /// `key`'s id is pinned: bind the offset `key` sits at from it, which is
    /// the constant `key` is.
    Pinned { offset: Scalar },
    /// `key`'s id has a carrier: bind its width in bits.
    Width { width: Scalar },
    /// An equate tried to put `key`'s id at a non-zero offset from itself: it
    /// is placed two ways at once, so nowhere.
    Conflicted,
}

/// A predicate over bound scalars.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Guard {
    Cmp(Cmp, Expr, Expr),
    /// Bind `out` to what `value` evaluates to.
    Let {
        out: Scalar,
        value: Expr,
    },
    /// The pairs are not all bound to the same reference. One pair is plain
    /// disequality; several say "these two terms are not the same term".
    Distinct(SmallVec<[(Var, Var); 4]>),
    /// A host function over the words `args` evaluate to; it binds `out`, and
    /// failing fails the match.
    Extern {
        call: ExternId,
        args: SmallVec<[Expr; 4]>,
        out: SmallVec<[Scalar; 2]>,
    },
}

impl Guard {
    pub(crate) fn ready(&self, known: &[bool], bound: &[bool]) -> bool {
        match self {
            Guard::Cmp(_, a, b) => a.ready(known) && b.ready(known),
            Guard::Let { value, .. } => value.ready(known),
            Guard::Distinct(pairs) => pairs
                .iter()
                .all(|&(a, b)| bound[a as usize] && bound[b as usize]),
            Guard::Extern { args, .. } => args.iter().all(|arg| arg.ready(known)),
        }
    }

    #[inline]
    pub(crate) fn writes(&self) -> &[Scalar] {
        match self {
            Guard::Cmp(..) | Guard::Distinct(..) => &[],
            Guard::Let { out, .. } => std::slice::from_ref(out),
            Guard::Extern { out, .. } => out,
        }
    }
}

/// A negated sub-conjunction: no solution of it exists, given what the outer
/// match bound.
///
/// Evaluated against the whole database, never a delta. Rows and lattice
/// values only accumulate and ids only merge, so the conjunction can only go
/// from unsatisfied to satisfied over epochs: a match blocked once stays
/// blocked and needs no re-check, and a match that fired cannot be un-fired.
/// An id placed consistently in one epoch can be [`Read::Conflicted`] in the
/// next without anything in the match moving at all.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Nested {
    pub atoms: Vec<Atom>,
    pub guards: Vec<Guard>,
}

/// What a parameter resolves to: for a row atom's selection, tables in the
/// client's order and each table's cells ascending; for a client table
/// atom, the client tables' slots, with no cells; for a lattice atom's
/// [`Out::In`], cells in any order. Resolutions only grow: a table's cells
/// at the end, a table anywhere.
#[derive(Clone, Debug, Default)]
pub struct Resolution {
    pub tables: SmallVec<[(u32, SmallVec<[u32; 4]>); 2]>,
    /// Per table of `tables`, its cells in the order a scan reads them.
    /// Empty when no table's is anything but ascending.
    pub scans: SmallVec<[SmallVec<[u32; 4]>; 2]>,
    pub cells: SmallVec<[u32; 4]>,
}

impl Resolution {
    pub(crate) fn scan(&self, position: usize) -> &[u32] {
        match self.scans.get(position) {
            Some(scan) => scan,
            None => &self.tables[position].1,
        }
    }

    pub(crate) fn cells_of(&self, table: u32) -> &[u32] {
        let found = self.tables.iter().find(|(known, _)| *known == table);
        found.map_or(&[], |(_, cells)| cells.as_slice())
    }
}

/// What a search asks its client while it runs over rows: the client's
/// functions, with what the plan's parameters resolve to already read.
pub trait Calls {
    /// A guard's host function over words. Failing fails the match.
    fn call(&self, id: ExternId, args: &[u64], out: &mut [u64]) -> bool;
    /// Whether an [`Out::Host`] atom holds of `reference`, which is canonical.
    fn accepts(&self, _id: ExternId, _reference: Ref) -> bool {
        false
    }
}

/// What a search asks its client.
pub trait Host: Calls {
    /// Grows whenever some parameter's resolution may have grown.
    fn version(&self) -> u64;
    /// Extend `out` by what `param` resolves to among what appeared since
    /// version `since` (`None`: from scratch, into an empty `out`), leaving
    /// the order a from-scratch resolution would give.
    fn resolve(&self, param: Param, since: Option<u64>, out: &mut Resolution);
    /// Whether `param` resolves to nothing, when the host can tell without
    /// resolving it: a plan reading no table is settled before it gets a
    /// cache.
    fn empty(&self, _param: Param) -> bool {
        false
    }
}
