//! Rules as conjunctive queries: a rule's left-hand side is a set of atoms over
//! the engine's rows, and matching it is a join rather than a backtracking walk
//! over a goal stack.
//!
//! A [`Query`] is the rule IR; a [`Plan`] is that query with an evaluation order
//! chosen once, and [`Plan::search`] is the evaluator. The order a plan produces
//! matches in is part of the contract: heads are applied in match order, so the
//! ids a saturation mints depend on it.

use smallvec::SmallVec;

use crate::engine::{Candidates, Groups};
use crate::store::Table;
use crate::{ClassId, Engine, Label, LabelId, Ref};

/// A class variable of a [`Query`], numbered from zero.
pub type Var = u32;

/// A scalar variable: one word, read off a label or computed by a guard.
pub type Scalar = u32;

/// A field of a label the language lets a rule read or fill.
pub type Field = u32;

/// One of the host's primitive functions over bound scalars.
pub type ExternId = u32;

/// A lattice column of the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnId {
    /// The constant a class is known to be.
    Const,
    /// The type its terms carry.
    Type,
}

/// The host's primitive functions over what a match bound: labels an atom
/// matched or a fact named, and words a guard computed. A guard never sees the
/// graph — a label is an e-node stripped of its children, so there is nowhere to
/// navigate from it — which is what makes a match's existence a function of its
/// atoms, and so what makes the delta exact.
pub trait Externs<L> {
    /// Fill `out` and report success; failure fails the match.
    fn call(&self, id: ExternId, labels: &[&L], args: &[u64], out: &mut [u64]) -> bool;
}

/// No externs: every call fails.
pub struct NoExterns;

impl<L> Externs<L> for NoExterns {
    fn call(&self, _id: ExternId, _labels: &[&L], _args: &[u64], _out: &mut [u64]) -> bool {
        false
    }
}

/// An integer expression over bound scalars.
#[derive(Clone, Debug)]
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

    /// Whether every scalar the expression reads is `known`.
    fn ready(&self, known: &[bool]) -> bool {
        match self {
            Expr::Lit(_) => true,
            Expr::Scalar(slot) => known[*slot as usize],
            Expr::Sub(a, b) | Expr::Add(a, b) | Expr::And(a, b) => a.ready(known) && b.ready(known),
            Expr::Ones(e) | Expr::IsZero(e) => e.ready(known),
        }
    }
}

/// A term a guard reads: the e-node a row atom matched, or the label a fact
/// named. A row keeps everything the node was interned with — its provenance,
/// which label identity drops — so the two are not interchangeable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Row(Scalar),
    Label(Scalar),
}

impl Source {
    fn slot(self) -> Scalar {
        match self {
            Source::Row(slot) | Source::Label(slot) => slot,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Eq,
    Ne,
}

/// A total or partial predicate over bound scalars.
#[derive(Clone, Debug)]
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
    /// Read `field` off the label a fact column bound, so a constant's value and
    /// width reach the guards as words. The engine owns the label table; this is
    /// a decode, not a look at the graph.
    Read {
        term: Source,
        field: Field,
        out: Scalar,
    },
    /// A host function over the labels and words named by `labels` and `args`;
    /// it binds `out`, and failing fails the match.
    Extern {
        call: ExternId,
        terms: SmallVec<[Source; 2]>,
        args: SmallVec<[Expr; 4]>,
        out: SmallVec<[Scalar; 2]>,
    },
}

impl Guard {
    /// Whether every scalar the guard reads is `known` and every class
    /// variable it reads (only a disequality has any) is `bound`.
    fn ready(&self, known: &[bool], bound: &[bool]) -> bool {
        match self {
            Guard::Cmp(_, a, b) => a.ready(known) && b.ready(known),
            Guard::Let { value, .. } => value.ready(known),
            Guard::Distinct(pairs) => pairs
                .iter()
                .all(|&(a, b)| bound[a as usize] && bound[b as usize]),
            Guard::Read { term, .. } => known[term.slot() as usize],
            Guard::Extern { terms, args, .. } => {
                terms.iter().all(|term| known[term.slot() as usize])
                    && args.iter().all(|arg| arg.ready(known))
            }
        }
    }

    #[inline]
    fn writes(&self) -> &[Scalar] {
        match self {
            Guard::Cmp(..) | Guard::Distinct(..) => &[],
            Guard::Let { out, .. } | Guard::Read { out, .. } => std::slice::from_ref(out),
            Guard::Extern { out, .. } => out,
        }
    }
}

/// One conjunct of a [`Query`]. Variables bind references: a class and an
/// offset. A variable bound twice must bind the same reference both times.
#[derive(Clone, Debug)]
pub enum Atom<L> {
    /// A row whose label matches `template`, whose value is `class` and whose
    /// operands bind `args`, one per operand. `row` binds the matched row,
    /// whose e-node a guard reads fields off or hands to a host function.
    ///
    /// The root variable, before an atom fixes its offset, stands for its
    /// class at whatever offset the row it meets is at, so a search from a
    /// class meets every e-node of it.
    Node {
        template: L,
        args: SmallVec<[Var; 4]>,
        class: Var,
        row: Option<Scalar>,
    },
    /// `class` is the constant `value`: its carrier's zero at that offset,
    /// or, outside a carrier, a class the constant column holds it for.
    Literal { value: L, class: Var },
    /// `key` has a value in `column`; bind it to `value`.
    Fact {
        column: ColumnId,
        key: Var,
        value: Scalar,
    },
    /// `key` is `base`, its class at offset zero, plus the scalar `offset`.
    Offset { key: Var, base: Var, offset: Scalar },
    /// `key` is an integer constant of a carrier: bind its value to `value`.
    Const { key: Var, value: Scalar },
    /// `key`'s class has a carrier: bind its width to `width`.
    Width { key: Var, width: Scalar },
    /// A union tried to put `key`'s class at a non-zero offset from itself:
    /// it is placed two ways at once, so nowhere. A law that refuses what it
    /// cannot place matches this, in a negated conjunction as well.
    Conflicted { key: Var },
    /// `key` holds a row of operator `op`, whatever its operands — the one
    /// reading that does not fix an arity.
    Holds { key: Var, op: u64 },
    /// `key` has no value in `column`. The complement of [`Atom::Fact`], for the
    /// negated conjunction that must match an access it cannot measure.
    Unknown { column: ColumnId, key: Var },
}

/// A negated sub-conjunction: no solution of it exists, given what the outer
/// match bound.
///
/// Evaluated against the whole relation, never a delta. Rows and facts only
/// accumulate and classes only merge, so the conjunction can only go from
/// unsatisfied to satisfied over rounds: a match blocked once stays blocked and
/// needs no re-check, and a match that fired cannot be un-fired. The second half
/// is the contract the memory laws already live with — a law that has fired
/// cannot be taken back — stated by the engine rather than left to a comment.
#[derive(Clone, Debug)]
pub struct Nested<L> {
    pub atoms: Vec<Atom<L>>,
    pub guards: Vec<Guard>,
}

impl<L> Atom<L> {
    /// The variable this atom's rows are looked up under; bound before the atom
    /// is stepped.
    pub fn class(&self) -> Var {
        match self {
            Atom::Node { class, .. } | Atom::Literal { class, .. } => *class,
            Atom::Fact { key, .. }
            | Atom::Offset { key, .. }
            | Atom::Const { key, .. }
            | Atom::Width { key, .. }
            | Atom::Conflicted { key }
            | Atom::Holds { key, .. }
            | Atom::Unknown { key, .. } => *key,
        }
    }

    /// The scalars this atom binds.
    fn writes(&self) -> SmallVec<[Scalar; 2]> {
        match self {
            Atom::Node { row, .. } => row.iter().copied().collect(),
            Atom::Literal { .. } => SmallVec::new(),
            Atom::Fact { value, .. } => SmallVec::from_slice(&[*value]),
            Atom::Offset { offset, .. } => SmallVec::from_slice(&[*offset]),
            Atom::Const { value, .. } => SmallVec::from_slice(&[*value]),
            Atom::Width { width, .. } => SmallVec::from_slice(&[*width]),
            Atom::Conflicted { .. } | Atom::Holds { .. } | Atom::Unknown { .. } => SmallVec::new(),
        }
    }
}

/// A conjunctive query. Every atom's class variable is reachable from
/// [`Self::root`] through the atoms' `args`, which is what lets a plan bind
/// them in one downward pass.
#[derive(Clone, Debug)]
pub struct Query<L> {
    pub vars: u32,
    pub scalars: u32,
    pub root: Var,
    pub atoms: Vec<Atom<L>>,
    pub guards: Vec<Guard>,
    /// Sub-conjunctions that must have no solution. Placed once everything they
    /// read is bound, and so last.
    pub nots: Vec<Nested<L>>,
}

impl<L> Query<L> {
    /// A query with no scalars and no guards: a bare structural pattern.
    pub fn tree(vars: u32, root: Var, atoms: Vec<Atom<L>>) -> Self {
        Self {
            vars,
            scalars: 0,
            root,
            atoms,
            guards: Vec::new(),
            nots: Vec::new(),
        }
    }
}

/// One match: the class the root variable bound to, and the reference every
/// variable bound to, read through [`Match::binding`].
#[derive(Clone, Debug)]
pub struct Match {
    pub root: ClassId,
    /// Per variable, its reference, or a class no graph has for a variable
    /// no atom reached.
    pub bindings: SmallVec<[Ref; 8]>,
    pub scalars: SmallVec<[u64; 8]>,
}

impl Match {
    /// What `var` bound to; `None` for a variable no atom reached.
    pub fn binding(&self, var: Var) -> Option<Ref> {
        bound(self.bindings[var as usize])
    }
}

/// An evaluation order: the steps, the template levels below the root, and
/// the plans of the negated conjunctions.
type Ordered<L> = (Vec<Step>, usize, Vec<Plan<L>>);

/// The matches of one search, stored flat: a search that finds thousands builds
/// no vector per match.
#[derive(Default)]
pub(crate) struct Matches {
    roots: Vec<ClassId>,
    /// `roots.len()` rows of bindings back to back, [`UNBOUND`] for a
    /// variable no atom reached, and the same of scalars.
    bindings: Vec<Ref>,
    scalars: Vec<u64>,
}

impl Matches {
    pub(crate) fn len(&self) -> usize {
        self.roots.len()
    }

    fn push(&mut self, root: ClassId, bindings: &[Ref], scalars: &[u64]) {
        self.roots.push(root);
        self.bindings.extend_from_slice(bindings);
        self.scalars.extend_from_slice(scalars);
    }

    /// The root, bindings and scalars of match `index`.
    #[inline]
    pub(crate) fn get(&self, index: usize) -> (ClassId, &[Ref], &[u64]) {
        let vars = self.bindings.len() / self.roots.len();
        let scalars = self.scalars.len() / self.roots.len();
        (
            self.roots[index],
            &self.bindings[index * vars..(index + 1) * vars],
            &self.scalars[index * scalars..(index + 1) * scalars],
        )
    }

    /// Append the matches of another search of the same plan.
    pub(crate) fn extend(&mut self, other: Matches) {
        self.roots.extend(other.roots);
        self.bindings.extend(other.bindings);
        self.scalars.extend(other.scalars);
    }

    fn clear(&mut self) {
        self.roots.clear();
        self.bindings.clear();
        self.scalars.clear();
    }

    fn to_vec(&self) -> Vec<Match> {
        (0..self.len())
            .map(|index| {
                let (root, bindings, scalars) = self.get(index);
                Match {
                    root,
                    bindings: SmallVec::from_slice(bindings),
                    scalars: SmallVec::from_slice(scalars),
                }
            })
            .collect()
    }
}

/// A query with its atoms ordered for evaluation: each step's class variable is
/// bound by the root or by an earlier step, so evaluation is a loop nest with no
/// search over orders. Guards run as soon as their inputs are bound.
#[derive(Clone, Debug)]
pub struct Plan<L> {
    query: Query<L>,
    steps: Vec<Step>,
    height: usize,
    /// One compiled plan per negated sub-conjunction, in the query's order.
    nots: Vec<Plan<L>>,
    /// One evaluation order per row atom, starting at that atom rather than at
    /// the root: what a round uses to search from the rows the round before
    /// wrote. Empty for a plan that cannot be searched that way. Worked out on
    /// first use: a plan compiled and never saturated with does not pay for it.
    anchors: std::sync::OnceLock<Vec<Anchor>>,
    /// One evaluation order per fact atom, starting at the class the fact is
    /// of: what a round uses to search from the facts the round before raised.
    /// Empty for a plan that cannot be searched that way.
    fact_anchors: std::sync::OnceLock<Vec<FactAnchor>>,
    /// Names the plan to an engine's caches. Zero for a plan that is part of
    /// another.
    id: u64,
    /// Per atom, the operator bucket of the rows it reads, hashed once here
    /// rather than once per search. Zero for an atom that reads no row.
    ops: Vec<u64>,
}

/// An evaluation order that starts at one row atom. Its first step is that
/// atom, which the search scans; the rest reach every other atom from what it
/// bound, up through operands and down through classes.
#[derive(Clone, Debug)]
struct Anchor {
    atom: usize,
    steps: Vec<Step>,
}

/// An evaluation order that starts at the class a fact atom reads: its first
/// step is that atom.
#[derive(Clone, Debug)]
struct FactAnchor {
    column: ColumnId,
    key: Var,
    steps: Vec<Step>,
    /// The fact atoms that come before this one, as a mask over atom indices.
    /// A match with several new facts is found from the first of them, so
    /// from this anchor those must be old.
    earlier: u64,
    /// Whether no earlier fact atom reads the same class. A class minted with
    /// its facts has all of them new at once, so only such an anchor can find
    /// a match there.
    first_on_key: bool,
}

/// What a saturation keeps between rounds for one plan: the tables each atom
/// reads, which only change when the graph meets a new label.
#[derive(Default)]
pub struct PlanCache {
    labels: usize,
    tables: Vec<Vec<Candidates>>,
    /// Whether some atom of the plan reads nothing the graph holds, so the plan
    /// has no match until the graph meets another label.
    dead: bool,
    /// The evaluator's working arrays, kept so a search allocates nothing.
    scratch: Scratch,
}

impl PlanCache {
    /// Hand back the matches of a search once they are spent, so the next
    /// search fills their arrays rather than growing new ones.
    pub(crate) fn recycle(&mut self, matches: Matches) {
        self.scratch.out = matches;
    }
}

#[derive(Default)]
struct Scratch {
    bound: Vec<Ref>,
    scalars: Vec<u64>,
    trail: Vec<Var>,
    pool: Vec<Vec<u32>>,
    out: Matches,
}

/// A search expecting at most this many starting points looks rows up by
/// scanning the column instead of through a grouping: the scans cost less than
/// building one.
const SCAN_LIMIT: usize = 16;

/// Run `$body` for each row of `$table` holding `$cell` in `$column`: read off
/// the grouping, or found by a scan when the search is too small to pay for
/// one.
macro_rules! lookup {
    ($eval:ident, $groups:ident, $slot:expr, $column:expr, $table:ident, $cell:expr, |$row:ident| $body:block) => {
        if $eval.scan {
            let rows = $eval.scan_rows($table.column($column), $cell);
            for &$row in &rows $body
            $eval.pool.push(rows);
        } else {
            for &$row in $groups.get($slot, $column).rows($cell) $body
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Atom(usize),
    /// A negated sub-conjunction, by index into the plan's nested plans.
    Not(usize),
    /// A sideways atom: its class is not bound, but its operand `slot` is, so it
    /// is reached through that class's parent back-edges rather than by walking
    /// down from the root.
    Parents {
        atom: usize,
        slot: u8,
    },
    Guard(usize),
}

impl<L: Label> Plan<L> {
    /// Order `query`'s atoms — the root's atom first, then whatever its operands
    /// bound, in the order the query was written — with each guard placed at the
    /// first point its inputs are all bound.
    ///
    /// Panics if an atom's class variable is unreachable from the root, or a
    /// guard reads a scalar nothing binds.
    pub fn compile(query: Query<L>) -> Self {
        let mut bound = vec![false; query.vars as usize];
        bound[query.root as usize] = true;
        let known = vec![false; query.scalars as usize];
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut plan = Self::compile_from(query, bound, known, None)
            .expect("every atom is reached and every guard read");
        plan.id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        plan
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn anchors(&self) -> &[Anchor] {
        self.anchors.get_or_init(|| self.anchor())
    }

    /// Whether a round can search this plan from the rows the round before
    /// wrote, instead of from every class near a change.
    pub fn anchorable(&self) -> bool {
        !self.anchors().is_empty()
    }

    /// An evaluation order from each row atom. Empty for a plan a round cannot
    /// search from its new rows: one that negates, whose root binds no row,
    /// that asks only whether a class holds an operator — a reading a second
    /// row of that operator does not make new — or some atom of which cannot
    /// be reached from another.
    fn anchor(&self) -> Vec<Anchor> {
        let query = &self.query;
        let rooted = query
            .atoms
            .iter()
            .any(|atom| matches!(atom, Atom::Node { class, .. } if *class == query.root));
        let holds = query
            .atoms
            .iter()
            .any(|atom| matches!(atom, Atom::Holds { .. }));
        if !rooted || holds || !self.nots.is_empty() {
            return Vec::new();
        }
        let mut anchors = Vec::new();
        for (index, atom) in query.atoms.iter().enumerate() {
            let Atom::Node {
                args, class, row, ..
            } = atom
            else {
                continue;
            };
            let mut bound = vec![false; query.vars as usize];
            bound[*class as usize] = true;
            for &arg in args {
                bound[arg as usize] = true;
            }
            let mut known = vec![false; query.scalars as usize];
            if let Some(row) = row {
                known[*row as usize] = true;
            }
            let Some((rest, _, _)) = Self::order(query, bound, known, Some(index)) else {
                return Vec::new();
            };
            let mut steps = vec![Step::Atom(index)];
            steps.extend(rest);
            anchors.push(Anchor { atom: index, steps });
        }
        anchors
    }

    /// Whether the plan reads no row at all: every atom is a fact.
    pub(crate) fn rowless(&self) -> bool {
        !self
            .query
            .atoms
            .iter()
            .any(|atom| matches!(atom, Atom::Node { .. } | Atom::Holds { .. }))
    }

    /// Whether a round can search this plan from the facts the round before
    /// raised.
    pub(crate) fn fact_anchorable(&self) -> bool {
        !self.fact_anchors().is_empty()
    }

    fn fact_anchors(&self) -> &[FactAnchor] {
        self.fact_anchors.get_or_init(|| self.fact_anchor())
    }

    /// An evaluation order from each fact atom. Empty for a plan that negates,
    /// reads no fact, or some atom of which cannot be reached from one.
    fn fact_anchor(&self) -> Vec<FactAnchor> {
        let query = &self.query;
        if !self.nots.is_empty() {
            return Vec::new();
        }
        // Facts are tried in order of how rarely their column rises: a
        // constant before a type, which nearly every new class is minted with.
        let rank = |column: ColumnId| match column {
            ColumnId::Const => 0,
            ColumnId::Type => 1,
        };
        let mut facts: Vec<(usize, ColumnId, Var)> = query
            .atoms
            .iter()
            .enumerate()
            .filter_map(|(index, atom)| match atom {
                Atom::Literal { class, .. } => Some((index, ColumnId::Const, *class)),
                Atom::Fact { column, key, .. } => Some((index, *column, *key)),
                _ => None,
            })
            .collect();
        if facts.iter().any(|&(index, _, _)| index >= 64) {
            return Vec::new();
        }
        facts.sort_by_key(|&(index, column, _)| (rank(column), index));
        let mut anchors = Vec::new();
        let mut earlier = 0u64;
        for (position, &(index, column, key)) in facts.iter().enumerate() {
            let mut bound = vec![false; query.vars as usize];
            bound[key as usize] = true;
            let known = vec![false; query.scalars as usize];
            // No atom is skipped, but the order is not from the root.
            let Some((steps, _, _)) = Self::order(query, bound, known, Some(usize::MAX)) else {
                return Vec::new();
            };
            let first_on_key = !facts[..position].iter().any(|&(_, _, other)| other == key);
            anchors.push(FactAnchor {
                column,
                key,
                steps,
                earlier,
                first_on_key,
            });
            earlier |= 1 << index;
        }
        anchors
    }

    /// Every match one of whose facts the previous round raised and whose rows
    /// are all older than that, each once, found from those facts: what
    /// [`Self::search_new`] leaves out. `minted` adds the facts classes were
    /// minted with, which only a plan that reads no row needs. For a
    /// [`Self::fact_anchorable`] plan.
    pub(crate) fn search_risen(
        &self,
        eg: &Engine<L>,
        minted: bool,
        externs: &dyn Externs<L>,
        cache: &mut PlanCache,
    ) -> Matches {
        self.refresh(eg, cache);
        if cache.dead {
            return Matches::default();
        }
        let lists = |anchor: &FactAnchor| match minted && anchor.first_on_key {
            true => &[false, true][..],
            false => &[false][..],
        };
        let starts = |anchor: &FactAnchor| {
            lists(anchor)
                .iter()
                .map(|&minted| eg.risen(anchor.column, minted).count())
                .sum::<usize>()
        };
        let total: usize = self.fact_anchors().iter().map(starts).sum();
        if total == 0 {
            return Matches::default();
        }
        let scan = total <= SCAN_LIMIT;
        if !scan {
            for anchor in self.fact_anchors() {
                Self::group(eg, &cache.tables[0], &anchor.steps);
            }
        }
        let groups = eg.groups();
        let scratch = std::mem::take(&mut cache.scratch);
        let mut eval = self.eval(eg, &groups, &cache.tables, None, externs, scratch);
        eval.scan = scan;
        eval.only_new = true;
        eval.old_below = usize::MAX;
        for (index, anchor) in self.fact_anchors().iter().enumerate() {
            eval.fact_anchor = Some(index);
            eval.facts_old = anchor.earlier;
            for &minted in lists(anchor) {
                for class in eg.risen(anchor.column, minted) {
                    eval.loosen(anchor.key, class);
                    self.step(&mut eval, class, 0);
                    eval.bound[anchor.key as usize] = UNBOUND;
                    eval.loose = None;
                }
            }
        }
        let (out, scratch) = eval.finish();
        cache.scratch = scratch;
        out
    }

    /// The operator bucket of each row atom, with repeats.
    pub(crate) fn row_ops(&self) -> impl Iterator<Item = u64> + '_ {
        self.query
            .atoms
            .iter()
            .zip(&self.ops)
            .filter(|(atom, _)| matches!(atom, Atom::Node { .. }))
            .map(|(_, &op)| op)
    }

    /// The columns this plan reads a fact of, as a mask of
    /// [`crate::engine::column_bit`]. A match can be new without any of its
    /// rows being new when a fact rises in one of them, on a class whose rows
    /// stand still.
    pub(crate) fn fact_columns(&self) -> u8 {
        self.query.atoms.iter().fold(0, |mask, atom| {
            let column = match atom {
                Atom::Literal { .. } => ColumnId::Const,
                Atom::Fact { column, .. } => *column,
                _ => return mask,
            };
            mask | 1 << crate::engine::column_bit(column)
        })
    }

    fn compile_from(
        query: Query<L>,
        bound: Vec<bool>,
        known: Vec<bool>,
        skip: Option<usize>,
    ) -> Option<Self> {
        let (steps, height, nots) = Self::order(&query, bound, known, skip)?;
        Some(Self {
            ops: query
                .atoms
                .iter()
                .map(|atom| match atom {
                    Atom::Node { template, .. } => template.op_key(),
                    Atom::Holds { op, .. } => *op,
                    _ => 0,
                })
                .collect(),
            query,
            steps,
            height,
            nots,
            anchors: std::sync::OnceLock::new(),
            fact_anchors: std::sync::OnceLock::new(),
            id: 0,
        })
    }

    /// Order what `bound` and `known` do not already give: the steps, the
    /// template levels below the root, and the negated conjunctions' plans.
    /// `None` when an atom is reachable neither down from a bound class nor
    /// sideways from a bound operand, or a guard reads a scalar nothing binds.
    ///
    /// `skip` says the order does not start at the root, and names the atom the
    /// caller has matched already when there is one.
    fn order(
        query: &Query<L>,
        mut bound: Vec<bool>,
        mut known: Vec<bool>,
        skip: Option<usize>,
    ) -> Option<Ordered<L>> {
        let mut steps = Vec::with_capacity(query.atoms.len() + query.guards.len());
        let mut taken = vec![false; query.atoms.len()];
        if let Some(taken) = skip.and_then(|atom| taken.get_mut(atom)) {
            *taken = true;
        }
        let mut checked = vec![false; query.guards.len()];
        let mut nots: Vec<Plan<L>> = Vec::new();
        let mut placed = vec![false; query.nots.len()];
        let mut depth = vec![0usize; query.vars as usize];
        let mut height = 0usize;
        loop {
            // A host call that binds nothing may be an assertion about a whole
            // match rather than a filter, and a rule's author sees to it that
            // every filter runs first when the match is built from the root. An
            // order that starts elsewhere keeps that promise by running such a
            // call only once every atom is matched.
            let all_taken = taken.iter().all(|&t| t);
            let guard = (0..query.guards.len()).find(|&i| {
                let held_back = skip.is_some()
                    && !all_taken
                    && matches!(&query.guards[i], Guard::Extern { out, .. } if out.is_empty());
                !checked[i] && !held_back && query.guards[i].ready(&known, &bound)
            });
            if let Some(guard) = guard {
                checked[guard] = true;
                for &out in query.guards[guard].writes() {
                    known[out as usize] = true;
                }
                steps.push(Step::Guard(guard));
                continue;
            }
            // A negated conjunction reads and never writes, so it goes as soon
            // as everything it names is bound — the earliest it can prune.
            let not = (0..query.nots.len()).find_map(|i| {
                if placed[i] {
                    return None;
                }
                let nested = Query {
                    vars: query.vars,
                    scalars: query.scalars,
                    root: query.root,
                    atoms: query.nots[i].atoms.clone(),
                    guards: query.nots[i].guards.clone(),
                    nots: Vec::new(),
                };
                Plan::compile_from(nested, bound.clone(), known.clone(), None).map(|plan| (i, plan))
            });
            if let Some((index, plan)) = not {
                placed[index] = true;
                steps.push(Step::Not(nots.len()));
                nots.push(plan);
                continue;
            }
            if taken.iter().all(|&t| t) {
                break;
            }
            let downward = (0..query.atoms.len())
                .find(|&i| !taken[i] && bound[query.atoms[i].class() as usize]);
            // Nothing left to walk down to: reach an atom whose class is still
            // free through an operand that is not, which is the one shape a
            // root-first plan cannot cover.
            let (next, step) = match downward {
                Some(next) => (next, Step::Atom(next)),
                None => {
                    let (next, slot) =
                        (0..query.atoms.len())
                            .filter(|&i| !taken[i])
                            .find_map(|i| match &query.atoms[i] {
                                Atom::Node { args, .. } => args
                                    .iter()
                                    .position(|&arg| bound[arg as usize])
                                    .map(|slot| (i, slot as u8)),
                                _ => None,
                            })?;
                    (next, Step::Parents { atom: next, slot })
                }
            };
            taken[next] = true;
            steps.push(step);
            bound[query.atoms[next].class() as usize] = true;
            let below = depth[query.atoms[next].class() as usize] + 1;
            match &query.atoms[next] {
                Atom::Node { args, .. } => {
                    height = height.max(below);
                    for &arg in args {
                        if !bound[arg as usize] {
                            bound[arg as usize] = true;
                            depth[arg as usize] = below;
                        }
                    }
                }
                Atom::Offset { base, .. } => bound[*base as usize] = true,
                _ => {}
            }
            for slot in query.atoms[next].writes() {
                known[slot as usize] = true;
            }
        }
        (checked.iter().all(|&c| c) && placed.iter().all(|&p| p)).then_some((steps, height, nots))
    }

    pub fn query(&self) -> &Query<L> {
        &self.query
    }

    /// The atoms and guards in evaluation order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Template levels below the root this plan binds. A class outside the
    /// frontier at this depth has an unchanged cone down to it, so its matches
    /// are the previous round's.
    ///
    /// Only meaningful for a plan that walks down: see [`Self::sideways`].
    pub fn height(&self) -> usize {
        self.height
    }

    /// Whether the match depends on rows the root's downward cone does not
    /// contain, so neither narrowing a round's roots to the change frontier nor
    /// skipping a match with no new row of its own is licensed.
    ///
    /// Two shapes do this. A sideways atom sits in a sibling class sharing a
    /// child, which no upward closure of the change log reaches from the root.
    /// And a negated conjunction is read against the whole relation: what
    /// satisfies it can change — a class placed consistently in one round is
    /// [`Atom::Conflicted`] in the next — without anything in the match moving
    /// at all.
    pub fn unbounded(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, Step::Parents { .. } | Step::Not(_)))
            || self.nots.iter().any(Plan::unbounded)
    }

    /// The classes the root atom can match at: those holding its operator, or
    /// every class when the root binds no row.
    pub fn roots(&self, eg: &Engine<L>) -> Vec<ClassId> {
        match self.root_op() {
            Some(op) => eg.classes_with_op(op),
            None => eg.class_ids().collect(),
        }
    }

    /// The operator the root atom binds, or `None` when the root binds no row.
    pub fn root_op(&self) -> Option<u64> {
        let root = self
            .query
            .atoms
            .iter()
            .position(|atom| atom.class() == self.query.root)?;
        matches!(self.query.atoms[root], Atom::Node { .. }).then_some(self.ops[root])
    }

    /// Every match at `roots`, each root canonicalized and visited once.
    ///
    /// `allowed(var, class)` prunes a binding the caller rejects — the hook for
    /// operand constraints instruction selection carries outside the query.
    /// When `only_new` is set, a match nothing of whose rows or facts the
    /// previous round touched is dropped: it existed a round earlier at a root
    /// that was searched then, so its head ran then.
    pub fn search(
        &self,
        eg: &Engine<L>,
        roots: impl IntoIterator<Item = ClassId>,
        allowed: &dyn Fn(Var, ClassId) -> bool,
        only_new: bool,
        externs: &dyn Externs<L>,
    ) -> Vec<Match> {
        // A plan none of whose rows the graph can hold is settled without
        // touching a cache.
        if self.row_ops().any(|op| eg.labels_with_op(op).is_empty()) {
            return Vec::new();
        }
        let mut cache = eg.take_cache(self.id);
        let fresh = (only_new, 0);
        let found = self.search_roots(eg, roots, Some(allowed), fresh, externs, &mut cache.plan);
        let matches = found.to_vec();
        cache.plan.recycle(found);
        eg.put_cache(self.id, cache);
        matches
    }

    /// [`Self::search`], with every row atom below `fresh.1` held to rows the
    /// previous round did not write; `fresh.0` is `only_new`.
    pub(crate) fn search_roots(
        &self,
        eg: &Engine<L>,
        roots: impl IntoIterator<Item = ClassId>,
        allowed: Option<&dyn Fn(Var, ClassId) -> bool>,
        fresh: (bool, usize),
        externs: &dyn Externs<L>,
        cache: &mut PlanCache,
    ) -> Matches {
        self.refresh(eg, cache);
        if cache.dead {
            return Matches::default();
        }
        // Each root canonical and once, in the order first given.
        let mut roots: SmallVec<[ClassId; 16]> =
            roots.into_iter().map(|root| eg.root(root)).collect();
        let mut marks = eg.marks();
        roots.retain(|root| marks.insert(root.index()));
        drop(marks);
        let scan = roots.len() <= SCAN_LIMIT;
        if !scan {
            self.prepare(eg, cache);
        }
        let groups = eg.groups();
        let scratch = std::mem::take(&mut cache.scratch);
        let mut eval = self.eval(eg, &groups, &cache.tables, allowed, externs, scratch);
        eval.scan = scan;
        eval.old_below = fresh.1;
        eval.only_new = fresh.0
            && self
                .query
                .atoms
                .iter()
                .any(|atom| matches!(atom, Atom::Node { .. }));
        for root in roots {
            if allowed.is_some_and(|allowed| !allowed(self.query.root, root)) {
                continue;
            }
            eval.loosen(self.query.root, root);
            self.step(&mut eval, root, 0);
            eval.bound[self.query.root as usize] = UNBOUND;
            eval.loose = None;
        }
        let (out, scratch) = eval.finish();
        cache.scratch = scratch;
        out
    }

    /// Every match, found by scanning the root atom's tables rather than by
    /// walking classes. For an [`Self::anchorable`] plan.
    pub(crate) fn search_all(
        &self,
        eg: &Engine<L>,
        externs: &dyn Externs<L>,
        cache: &mut PlanCache,
    ) -> Matches {
        self.refresh(eg, cache);
        if cache.dead {
            return Matches::default();
        }
        let root = self
            .anchors()
            .iter()
            .position(|anchor| self.query.atoms[anchor.atom].class() == self.query.root)
            .expect("an anchorable plan has a row at its root");
        // The scan reads the root's rows by label; the rest are looked up.
        for (slot, _) in &cache.tables[0][self.anchors()[root].atom] {
            eg.prepare_group(*slot, 0);
        }
        Self::group(eg, &cache.tables[0], &self.anchors()[root].steps[1..]);
        let groups = eg.groups();
        let scratch = std::mem::take(&mut cache.scratch);
        let mut eval = self.eval(eg, &groups, &cache.tables, None, externs, scratch);
        self.scan(&mut eval, root, false);
        let (out, scratch) = eval.finish();
        cache.scratch = scratch;
        out
    }

    /// Every match with a row the previous round wrote, each once, found from
    /// those rows. For an [`Self::anchorable`] plan.
    ///
    /// A match with several new rows is found from the first of them in atom
    /// order: the search from a later one holds the earlier atoms to old rows.
    /// A match made new by a fact alone is not found here; see
    /// [`Self::search_facts`].
    pub(crate) fn search_new(
        &self,
        eg: &Engine<L>,
        externs: &dyn Externs<L>,
        cache: &mut PlanCache,
    ) -> Matches {
        self.refresh(eg, cache);
        if cache.dead {
            return Matches::default();
        }
        // Most rounds give most rules nothing to start from, so that is settled
        // before any grouping is built.
        for anchor in self.anchors() {
            for (slot, _) in &cache.tables[0][anchor.atom] {
                eg.prepare_new_rows(*slot);
            }
        }
        let mut starts = 0;
        let live: SmallVec<[usize; 4]> = {
            let groups = eg.groups();
            (0..self.anchors().len())
                .filter(|&anchor| {
                    let rows: usize = cache.tables[0][self.anchors()[anchor].atom]
                        .iter()
                        .flat_map(|(slot, wanted)| {
                            wanted
                                .iter()
                                .map(|&label| groups.new_rows(*slot, label).len())
                        })
                        .sum();
                    starts += rows;
                    rows != 0
                })
                .collect()
        };
        if live.is_empty() {
            return Matches::default();
        }
        let scan = starts <= SCAN_LIMIT;
        if !scan {
            for &anchor in &live {
                Self::group(eg, &cache.tables[0], &self.anchors()[anchor].steps[1..]);
            }
        }
        let groups = eg.groups();
        let scratch = std::mem::take(&mut cache.scratch);
        let mut eval = self.eval(eg, &groups, &cache.tables, None, externs, scratch);
        eval.scan = scan;
        for anchor in live {
            self.scan(&mut eval, anchor, true);
        }
        let (out, scratch) = eval.finish();
        cache.scratch = scratch;
        out
    }

    /// Every match at `roots` whose rows are all older than the previous round
    /// and one of whose facts is not: what [`Self::search_new`] leaves out.
    pub(crate) fn search_facts(
        &self,
        eg: &Engine<L>,
        roots: impl IntoIterator<Item = ClassId>,
        externs: &dyn Externs<L>,
        cache: &mut PlanCache,
    ) -> Matches {
        self.search_roots(eg, roots, None, (true, usize::MAX), externs, cache)
    }

    fn eval<'a>(
        &self,
        eg: &'a Engine<L>,
        groups: &'a Groups,
        labels: &'a [Vec<Candidates>],
        allowed: Option<&'a dyn Fn(Var, ClassId) -> bool>,
        externs: &'a dyn Externs<L>,
        mut scratch: Scratch,
    ) -> Eval<'a, L> {
        // A probe looks a representative up in the cells, which name the
        // absorbed class until a rebuild.
        debug_assert!(eg.rebuilt(), "search needs a rebuilt graph");
        scratch.bound.clear();
        scratch.bound.resize(self.query.vars as usize, UNBOUND);
        scratch.scalars.clear();
        scratch.scalars.resize(self.query.scalars as usize, 0);
        scratch.trail.clear();
        let mut out = std::mem::take(&mut scratch.out);
        out.clear();
        Eval {
            eg,
            groups,
            labels,
            plan: 0,
            anchor: None,
            fact_anchor: None,
            facts_old: 0,
            old_below: 0,
            externs,
            allowed,
            only_new: false,
            counting: false,
            hits: 0,
            bound: scratch.bound,
            scalars: scratch.scalars,
            trail: scratch.trail,
            scan: false,
            pool: scratch.pool,
            fresh: 0,
            loose: None,
            out,
        }
    }

    /// Search from the rows of one anchor's atom: the ones the previous round
    /// wrote, or all of them.
    fn scan(&self, eval: &mut Eval<'_, L>, anchor: usize, new: bool) {
        let atom = self.anchors()[anchor].atom;
        let Atom::Node {
            args, class, row, ..
        } = &self.query.atoms[atom]
        else {
            unreachable!("an anchor is a row atom")
        };
        let (eg, groups, labels) = (eval.eg, eval.groups, eval.labels);
        eval.anchor = Some(anchor);
        eval.old_below = if new { atom } else { 0 };
        let bind = (*row, *class);
        for (slot, wanted) in &labels[0][atom] {
            let Some(table) = eg.slot_table(*slot) else {
                continue;
            };
            for &label in wanted {
                if new {
                    for &(_, found) in groups.new_rows(*slot, label) {
                        self.row(eval, ClassId(0), 0, atom, table, found, args, bind, 2);
                    }
                    continue;
                }
                for &found in groups.get(*slot, 0).rows(label.0) {
                    self.row(eval, ClassId(0), 0, atom, table, found, args, bind, 2);
                }
            }
        }
        eval.anchor = None;
    }

    /// Build the grouping each step of a search from the root looks rows up
    /// in.
    fn prepare(&self, eg: &Engine<L>, cache: &mut PlanCache) {
        Self::group(eg, &cache.tables[0], &self.steps);
        for (not, tables) in self.nots.iter().zip(&cache.tables[1..]) {
            Self::group(eg, tables, &not.steps);
        }
    }

    /// Bring `cache` up to the graph's labels: what each atom reads, from
    /// scratch the first time and from the labels met since after that.
    fn refresh(&self, eg: &Engine<L>, cache: &mut PlanCache) {
        if cache.labels == eg.labels_len() && !cache.tables.is_empty() {
            return;
        }
        let since = if cache.tables.is_empty() {
            None
        } else {
            Some(cache.labels)
        };
        cache.labels = eg.labels_len();
        cache.tables.resize_with(1 + self.nots.len(), Vec::new);
        let plans = std::iter::once(self).chain(&self.nots);
        for (plan, tables) in plans.zip(&mut cache.tables) {
            tables.resize_with(plan.query.atoms.len(), Candidates::new);
            for ((atom, &op), reads) in plan.query.atoms.iter().zip(&plan.ops).zip(tables) {
                match atom {
                    Atom::Node { template, args, .. } => {
                        eg.labels_matching(op, Some((template, args.len())), since, reads);
                    }
                    Atom::Holds { .. } => eg.labels_matching(op, None, since, reads),
                    _ => {}
                }
            }
        }
        cache.dead = self
            .query
            .atoms
            .iter()
            .zip(&cache.tables[0])
            .any(|(atom, reads)| {
                matches!(atom, Atom::Node { .. } | Atom::Holds { .. }) && reads.is_empty()
            });
    }

    /// Build the grouping each of `steps` looks rows up in: by class for an
    /// atom walked down to, by operand for one reached sideways.
    fn group(eg: &Engine<L>, labels: &[Candidates], steps: &[Step]) {
        for step in steps {
            let (atom, operand) = match *step {
                Step::Atom(atom) => (atom, None),
                Step::Parents { atom, slot } => (atom, Some(slot as usize)),
                _ => continue,
            };
            for (slot, _) in &labels[atom] {
                let Some(table) = eg.slot_table(*slot) else {
                    continue;
                };
                match operand {
                    None => eg.prepare_group(*slot, table.arity()),
                    Some(operand) => {
                        eg.prepare_group(*slot, 1 + operand);
                        if table.arity() == 3 {
                            eg.prepare_group(*slot, 2 - operand);
                        }
                    }
                }
            }
        }
    }

    fn step(&self, eval: &mut Eval<'_, L>, root: ClassId, index: usize) {
        let steps = match (eval.anchor, eval.fact_anchor) {
            (Some(anchor), _) => &self.anchors()[anchor].steps,
            (None, Some(anchor)) => &self.fact_anchors()[anchor].steps,
            (None, None) => &self.steps,
        };
        let Some(&step) = steps.get(index) else {
            // A negated conjunction asks only whether a solution exists.
            if eval.counting {
                eval.hits += 1;
                return;
            }
            if !eval.only_new || eval.fresh > 0 {
                let root =
                    bound(eval.bound[self.query.root as usize]).map_or(root, |root| root.class);
                eval.out.push(root, &eval.bound, &eval.scalars);
            }
            return;
        };
        if eval.counting && eval.hits > 0 {
            return;
        }
        let index_of_atom = match step {
            Step::Parents { atom, slot } => {
                let Atom::Node {
                    args, class, row, ..
                } = &self.query.atoms[atom]
                else {
                    unreachable!("only a row atom is reached sideways")
                };
                let child = eval.bound[args[slot as usize] as usize];
                debug_assert_ne!(child, UNBOUND, "bound operand");
                self.parents(
                    eval,
                    root,
                    index,
                    atom,
                    args,
                    *class,
                    *row,
                    slot as usize,
                    child,
                );
                return;
            }
            Step::Not(not) => {
                let outer = std::mem::replace(&mut eval.counting, true);
                let hits = std::mem::replace(&mut eval.hits, 0);
                let plan = std::mem::replace(&mut eval.plan, 1 + not);
                let mark = eval.trail.len();
                self.nots[not].step(eval, root, 0);
                eval.unbind(mark);
                let blocked = eval.hits > 0;
                eval.counting = outer;
                eval.hits = hits;
                eval.plan = plan;
                if !blocked {
                    self.step(eval, root, index + 1);
                }
                return;
            }
            Step::Guard(guard) => {
                self.guard(eval, root, index, &self.query.guards[guard]);
                return;
            }
            Step::Atom(atom) => atom,
        };
        let atom = &self.query.atoms[index_of_atom];
        let var = atom.class();
        let reference = eval.bound[var as usize];
        debug_assert_ne!(reference, UNBOUND, "class bound by an earlier step");
        let class = reference.class;
        if eval.facts_old >> index_of_atom.min(63) & 1 == 1 {
            let column = match atom {
                Atom::Literal { .. } => Some(ColumnId::Const),
                Atom::Fact { column, .. } => Some(*column),
                _ => None,
            };
            if column.is_some_and(|column| eval.eg.fact_is_new(column, class)) {
                return;
            }
        }
        match atom {
            Atom::Literal { value, .. } => self.literal(eval, root, index, value, var, reference),
            Atom::Fact { column, value, .. } => {
                self.fact(eval, root, index, *column, *value, class)
            }
            Atom::Offset { base, offset, .. } => {
                // The offset is read, so it is fixed from here on.
                let mark = eval.trail.len();
                eval.tighten(var);
                eval.scalars[*offset as usize] = reference.offset;
                if eval.bind_one(*base, class.into()) {
                    self.step(eval, root, index + 1);
                }
                eval.unbind(mark);
            }
            Atom::Const { value, .. } => {
                if let Some(constant) = eval.eg.int_const(reference) {
                    let mark = eval.trail.len();
                    eval.tighten(var);
                    eval.scalars[*value as usize] = constant;
                    self.step(eval, root, index + 1);
                    eval.unbind(mark);
                }
            }
            // A class's carrier never changes, so there is no freshness to
            // track either.
            Atom::Width { width, .. } => {
                if let Some(bits) = eval.eg.width(class) {
                    eval.scalars[*width as usize] = u64::from(bits);
                    self.step(eval, root, index + 1);
                }
            }
            // A class stops being placed once and for all, so there is no
            // freshness to track.
            Atom::Conflicted { .. } => {
                if eval.eg.conflicted(class) {
                    self.step(eval, root, index + 1);
                }
            }
            Atom::Unknown { column, .. } => {
                if eval.eg.fact(*column, class).is_none() {
                    self.step(eval, root, index + 1);
                }
            }
            Atom::Holds { .. } => {
                let (eg, groups, labels) = (eval.eg, eval.groups, eval.labels);
                let mut held = None;
                for (slot, wanted) in &labels[eval.plan][index_of_atom] {
                    let Some(table) = eg.slot_table(*slot) else {
                        continue;
                    };
                    lookup!(eval, groups, *slot, table.arity(), table, class.0, |row| {
                        if held.is_none() && reads(wanted, table.column(0)[row as usize]) {
                            held = Some(table.stamps()[row as usize] + 1 == eg.epoch());
                        }
                    });
                }
                let Some(fresh) = held.map(usize::from) else {
                    return;
                };
                eval.fresh += fresh;
                self.step(eval, root, index + 1);
                eval.fresh -= fresh;
            }
            Atom::Node { args, row, .. } => {
                self.node(eval, root, index, index_of_atom, args, (*row, var), class)
            }
        }
    }

    /// Run `guard`, and the steps after it if it holds.
    ///
    /// A rule may write a guard's result into the slot an earlier scalar lives
    /// in. From the root that is harmless: every match recomputes the earlier
    /// one first. An order that starts elsewhere can put a loop over rows
    /// between the two, so what the guard overwrote is put back for the next
    /// row.
    fn guard(&self, eval: &mut Eval<'_, L>, root: ClassId, index: usize, guard: &Guard) {
        let written = guard.writes();
        // Most guards write one scalar or none, which needs no list.
        let before: SmallVec<[u64; 2]> = match written {
            [] | [_] => SmallVec::new(),
            _ => written
                .iter()
                .map(|&slot| eval.scalars[slot as usize])
                .collect(),
        };
        let one = written.first().map(|&slot| eval.scalars[slot as usize]);
        if eval.holds(guard) {
            self.step(eval, root, index + 1);
        }
        match (written, one) {
            ([slot], Some(value)) => eval.scalars[*slot as usize] = value,
            _ => {
                for (&slot, value) in written.iter().zip(before) {
                    eval.scalars[slot as usize] = value;
                }
            }
        }
    }

    /// Every row of `class` the atom reads.
    #[allow(clippy::too_many_arguments)]
    fn node(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        atom: usize,
        args: &[Var],
        bind: (Option<Scalar>, Var),
        class: ClassId,
    ) {
        let (eg, groups, labels) = (eval.eg, eval.groups, eval.labels);
        for (slot, wanted) in &labels[eval.plan][atom] {
            let Some(table) = eg.slot_table(*slot) else {
                continue;
            };
            lookup!(eval, groups, *slot, table.arity(), table, class.0, |row| {
                if reads(wanted, table.column(0)[row as usize]) {
                    self.row(eval, root, index, atom, table, row, args, bind, 2);
                }
            });
        }
    }

    /// Try one row of an atom's table. `bind` names the scalar that takes the
    /// row and the variable that takes the row's value, or is checked against
    /// it when bound. `orders` picks the operand orders to try: zero or one
    /// for that order alone, two for both where the row's operator commutes.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn row(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        atom: usize,
        table: &Table,
        row: u32,
        args: &[Var],
        bind: (Option<Scalar>, Var),
        orders: usize,
    ) {
        let fresh = usize::from(table.stamps()[row as usize] + 1 == eval.eg.epoch());
        if fresh == 1 && eval.plan == 0 && atom < eval.old_below {
            return;
        }
        if let Some(slot) = bind.0 {
            eval.scalars[slot as usize] = table.tags()[row as usize] as u64;
        }
        let orders = match orders {
            // A commutative binary operator matches in both operand orders.
            2 if eval.eg.commutes(LabelId(table.column(0)[row as usize])) => 0..2,
            2 => 0..1,
            order => order..order + 1,
        };
        eval.fresh += fresh;
        for order in orders {
            let mark = eval.trail.len();
            let weighted = table.weighted();
            let class = ClassId(table.values()[row as usize]);
            let prior = eval.bound[bind.1 as usize];
            // A row looked up by its class, in a table without weights, has
            // the value the variable already holds at offset zero.
            let settled = !weighted
                && prior.class == class
                && prior.offset == 0
                && eval.loose != Some(bind.1);
            let value = || match weighted {
                true => Ref::new(class, table.weight(table.arity(), row)),
                false => class.into(),
            };
            if (settled || eval.bind_one(bind.1, value()))
                && eval.bind_row(args, table, row, order, weighted)
            {
                self.step(eval, root, index + 1);
            }
            eval.unbind(mark);
        }
        eval.fresh -= fresh;
    }

    /// Reach a row through a bound operand: the rows the atom reads that hold
    /// `child` at `slot`. Everything already bound is checked against the row;
    /// the rest, including the row's own class, is bound from it.
    #[allow(clippy::too_many_arguments)]
    fn parents(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        atom: usize,
        args: &[Var],
        class: Var,
        bind_row: Option<Scalar>,
        slot: usize,
        child: Ref,
    ) {
        let (eg, groups, labels) = (eval.eg, eval.groups, eval.labels);
        for (table_slot, wanted) in &labels[eval.plan][atom] {
            let Some(table) = eg.slot_table(*table_slot) else {
                continue;
            };
            // In the swapped order of a commutative operator the bound operand
            // sits in the other column.
            let orders = if args.len() == 2 { 2 } else { 1 };
            for order in 0..orders {
                let column = 1 + if order == 1 { 1 - slot } else { slot };
                lookup!(
                    eval,
                    groups,
                    *table_slot,
                    column,
                    table,
                    child.class.0,
                    |row| {
                        let label = table.column(0)[row as usize];
                        if reads(wanted, label) && (order == 0 || eg.commutes(LabelId(label))) {
                            let bind = (bind_row, class);
                            self.row(eval, root, index, atom, table, row, args, bind, order);
                        }
                    }
                );
            }
        }
    }

    /// A literal reads what a class is *known* to be rather than its rows:
    /// for a carrier's constant, the reference itself; otherwise the constant
    /// column, which covers both a class's own literal and what a scope
    /// assumed of it, and whose stamp says which round proved it.
    fn literal(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        value: &L,
        var: Var,
        reference: Ref,
    ) {
        let eg = eval.eg;
        let mark = eval.trail.len();
        match eg.is_literal(reference, value) {
            Some(true) => eval.tighten(var),
            Some(false) => return,
            None => {
                let class = reference.class;
                if !eg.const_label_matches(class, value) {
                    return;
                }
            }
        }
        let fresh = usize::from(eg.fact_is_new(ColumnId::Const, reference.class));
        eval.fresh += fresh;
        self.step(eval, root, index + 1);
        eval.fresh -= fresh;
        eval.unbind(mark);
    }

    fn fact(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        column: ColumnId,
        slot: Scalar,
        class: ClassId,
    ) {
        let Some(value) = eval.eg.fact(column, class) else {
            return;
        };
        eval.scalars[slot as usize] = value;
        let fresh = usize::from(eval.eg.fact_is_new(column, class));
        eval.fresh += fresh;
        self.step(eval, root, index + 1);
        eval.fresh -= fresh;
    }
}

/// What a variable no atom has reached holds.
pub(crate) const UNBOUND: Ref = Ref {
    class: ClassId(u32::MAX),
    offset: 0,
};

/// `binding` unless it is [`UNBOUND`].
#[inline]
pub(crate) fn bound(binding: Ref) -> Option<Ref> {
    (binding.class != UNBOUND.class).then_some(binding)
}

/// Marks a trail entry that put the loose variable back.
const LOOSE: Var = 1 << 31;

/// Whether `label` is one of `wanted`, which is ascending.
#[inline]
fn reads(wanted: &[LabelId], label: u32) -> bool {
    match wanted {
        [only] => only.0 == label,
        _ => wanted.binary_search(&LabelId(label)).is_ok(),
    }
}

/// State one [`Plan::search`] threads through the loop nest.
struct Eval<'a, L: Label> {
    eg: &'a Engine<L>,
    groups: &'a Groups,
    /// Per plan and atom, the tables the atom reads. Plan zero is the query;
    /// the rest are its negated sub-conjunctions.
    labels: &'a [Vec<Candidates>],
    /// The plan being stepped.
    plan: usize,
    /// The anchor whose order is being stepped, when the search started at a
    /// row atom rather than at the root.
    anchor: Option<usize>,
    /// The fact anchor whose order is being stepped, when the search started
    /// at a risen fact.
    fact_anchor: Option<usize>,
    /// The fact atoms, as a mask over atom indices, that match only facts older
    /// than the previous round.
    facts_old: u64,
    /// Row atoms below this index match only rows older than the previous
    /// round.
    old_below: usize,
    externs: &'a dyn Externs<L>,
    /// `None` allows every binding.
    allowed: Option<&'a dyn Fn(Var, ClassId) -> bool>,
    only_new: bool,
    /// Inside a negated conjunction: count solutions and stop at the first,
    /// rather than emit them.
    counting: bool,
    hits: usize,
    /// Per variable, its reference or [`UNBOUND`].
    bound: Vec<Ref>,
    scalars: Vec<u64>,
    /// Variables this branch bound, to undo on the way out. One with
    /// [`LOOSE`] set was the loose variable and had its offset fixed.
    trail: Vec<Var>,
    /// The variable the search started at, bound to its class at offset zero
    /// until an atom fixes its offset: a row atom to the row's value, or one
    /// that reads the offset to the offset it read.
    loose: Option<Var>,
    /// Whether rows are looked up by scanning, with no grouping built.
    scan: bool,
    /// Vectors a scan's hits go in, reused.
    pool: Vec<Vec<u32>>,
    /// How many rows and facts of the partial match the previous round touched.
    fresh: usize,
    out: Matches,
}

impl<'a, L: Label> Eval<'a, L> {
    /// The matches found, and the working arrays for the next search.
    fn finish(self) -> (Matches, Scratch) {
        let scratch = Scratch {
            bound: self.bound,
            scalars: self.scalars,
            trail: self.trail,
            pool: self.pool,
            out: Matches::default(),
        };
        (self.out, scratch)
    }

    /// The rows of `column` holding `cell`, in a vector from the pool.
    fn scan_rows(&mut self, column: &[u32], cell: u32) -> Vec<u32> {
        let mut rows = self.pool.pop().unwrap_or_default();
        rows.clear();
        tir_adt::simd::select_eq(column, cell, &mut rows);
        rows
    }

    /// Start a search at `class`, with `var` bound to it and loose.
    fn loosen(&mut self, var: Var, class: ClassId) {
        self.bound[var as usize] = class.into();
        self.loose = Some(var);
    }

    /// Fix the offset of `var` where it is, if it is the loose variable.
    fn tighten(&mut self, var: Var) {
        if self.loose == Some(var) {
            self.loose = None;
            self.trail.push(var | LOOSE);
        }
    }

    /// Bind `args` to the cells of a table row, or report the row inconsistent
    /// with what is already bound. Whatever it bound before failing is on the
    /// trail.
    #[inline(always)]
    fn bind_row(
        &mut self,
        args: &[Var],
        table: &Table,
        row: u32,
        order: usize,
        weighted: bool,
    ) -> bool {
        for (slot, &var) in args.iter().enumerate() {
            let column = 1 + if order == 1 { 1 - slot } else { slot };
            // A rebuilt table names every class by its representative.
            let class = ClassId(table.column(column)[row as usize]);
            let cell = match weighted {
                true => Ref::new(class, table.weight(column, row)),
                false => class.into(),
            };
            if !self.bind_one(var, cell) {
                return false;
            }
        }
        true
    }

    #[inline(always)]
    fn bind_one(&mut self, var: Var, reference: Ref) -> bool {
        let prior = self.bound[var as usize];
        // A variable shared by two atoms must bind the same reference.
        if prior == reference {
            return true;
        }
        if prior.class != UNBOUND.class {
            if self.loose != Some(var) || prior.class != reference.class {
                return false;
            }
            self.bound[var as usize] = reference;
            self.loose = None;
            self.trail.push(var | LOOSE);
            return true;
        }
        if self
            .allowed
            .is_some_and(|allowed| !allowed(var, reference.class))
        {
            return false;
        }
        self.bound[var as usize] = reference;
        self.trail.push(var);
        true
    }

    /// The e-node a scalar names.
    fn term(&self, term: Source) -> Option<&'a L> {
        let word = self.scalars[term.slot() as usize];
        match term {
            Source::Row(_) => Some(self.eg.node(crate::RowId(word as u32))),
            Source::Label(_) => self.eg.label_node(crate::LabelId(word as u32)),
        }
    }

    /// Whether `guard` holds of the scalars bound so far, binding its own
    /// output if it does.
    fn holds(&mut self, guard: &Guard) -> bool {
        match guard {
            Guard::Cmp(cmp, a, b) => {
                let (Some(a), Some(b)) = (a.eval(&self.scalars), b.eval(&self.scalars)) else {
                    return false;
                };
                match cmp {
                    Cmp::Lt => a < b,
                    Cmp::Le => a <= b,
                    Cmp::Eq => a == b,
                    Cmp::Ne => a != b,
                }
            }
            Guard::Distinct(pairs) => !pairs
                .iter()
                .all(|&(a, b)| self.bound[a as usize] == self.bound[b as usize]),
            Guard::Let { out, value } => {
                let Some(value) = value.eval(&self.scalars) else {
                    return false;
                };
                self.scalars[*out as usize] = value as u64;
                true
            }
            Guard::Read { term, field, out } => {
                let Some(value) = self.term(*term).and_then(|node| node.scalar(*field)) else {
                    return false;
                };
                self.scalars[*out as usize] = value;
                true
            }
            Guard::Extern {
                call,
                terms,
                args,
                out,
            } => {
                let mut nodes: SmallVec<[&L; 2]> = SmallVec::new();
                for &term in terms {
                    let Some(node) = self.term(term) else {
                        return false;
                    };
                    nodes.push(node);
                }
                let mut words: SmallVec<[u64; 4]> = SmallVec::new();
                for arg in args {
                    let Some(value) = arg.eval(&self.scalars) else {
                        return false;
                    };
                    words.push(value as u64);
                }
                let mut values: SmallVec<[u64; 2]> = SmallVec::from_elem(0, out.len());
                if !self.externs.call(*call, &nodes, &words, &mut values) {
                    return false;
                }
                for (&slot, value) in out.iter().zip(values) {
                    self.scalars[slot as usize] = value;
                }
                true
            }
        }
    }

    fn unbind(&mut self, mark: usize) {
        while self.trail.len() > mark {
            let var = self.trail.pop().expect("above the mark");
            if var & LOOSE == 0 {
                self.bound[var as usize] = UNBOUND;
                continue;
            }
            let var = var & !LOOSE;
            let class = self.bound[var as usize].class;
            self.bound[var as usize] = class.into();
            self.loose = Some(var);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Nested;
    use crate::testing::Term;
    use proptest::prelude::*;

    fn node(template: Term, args: &[Var], class: Var) -> Atom<Term> {
        Atom::Node {
            template,
            args: args.iter().copied().collect(),
            class,
            row: None,
        }
    }

    /// `f(g(?1), ?2)` with the atoms written parent-last, so ordering has to do
    /// something.
    fn f_of_g() -> Query<Term> {
        Query::tree(
            4,
            0,
            vec![
                node(Term::op("g", &[ClassId(0)]), &[3], 1),
                node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0),
            ],
        )
    }

    #[test]
    fn plan_steps_the_root_atom_first_then_what_it_bound() {
        let plan = Plan::compile(f_of_g());
        assert_eq!(plan.steps(), &[Step::Atom(1), Step::Atom(0)]);
        assert_eq!(plan.height(), 2);
    }

    #[test]
    #[should_panic(expected = "every atom is reached")]
    fn plan_rejects_an_atom_no_operand_reaches() {
        Plan::compile(Query::tree(2, 0, vec![node(Term::leaf("x"), &[], 1)]));
    }

    #[test]
    fn search_binds_every_variable_of_the_pattern() {
        let mut eg = Engine::new();
        let x = eg.add(Term::leaf("x")).class;
        let y = eg.add(Term::leaf("y")).class;
        let g = eg.add(Term::op("g", &[x])).class;
        let f = eg.add(Term::op("f", &[g, y])).class;
        let found = Plan::compile(f_of_g()).search(&eg, [f], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].bindings.as_slice(), &[f, g, y, x].map(Ref::from));
    }

    /// Every assignment of `query`'s variables to classes that satisfies every
    /// atom, found by trying all of them.
    fn brute_force(eg: &Engine<Term>, query: &Query<Term>) -> Vec<Vec<ClassId>> {
        let classes: Vec<ClassId> = eg.class_ids().collect();
        let mut out = Vec::new();
        let mut assignment = vec![ClassId(0); query.vars as usize];
        let total = classes.len().pow(query.vars);
        for mut code in 0..total {
            for slot in assignment.iter_mut() {
                *slot = classes[code % classes.len()];
                code /= classes.len();
            }
            if query.atoms.iter().all(|atom| holds(eg, atom, &assignment)) {
                out.push(assignment.clone());
            }
        }
        out
    }

    fn holds(eg: &Engine<Term>, atom: &Atom<Term>, assignment: &[ClassId]) -> bool {
        let class = assignment[atom.class() as usize];
        match atom {
            Atom::Literal { value, .. } => eg
                .const_of(class)
                .is_some_and(|known| value.matches(&known)),
            Atom::Fact { column, .. } => eg.fact(*column, class).is_some(),
            Atom::Offset { base, .. } => assignment[*base as usize] == class,
            Atom::Const { .. } => eg.int_const(class).is_some(),
            Atom::Width { .. } => eg.width(class).is_some(),
            Atom::Conflicted { .. } => eg.conflicted(class),
            Atom::Holds { op, .. } => eg.rows(class).any(|row| eg.node(row).op_key() == *op),
            Atom::Unknown { column, .. } => eg.fact(*column, class).is_none(),
            Atom::Node { template, args, .. } => eg.rows(class).any(|row| {
                let children: Vec<ClassId> = eg.children(row).iter().map(|c| c.class).collect();
                if children.len() != args.len() || !template.matches_template(eg.node(row)) {
                    return false;
                }
                let want: Vec<ClassId> = args.iter().map(|&a| assignment[a as usize]).collect();
                children == want
                    || (eg.node(row).commutative()
                        && children.len() == 2
                        && children == [want[1], want[0]])
            }),
        }
    }

    /// A negated conjunction blocks a match whose blocker no round created —
    /// it is read against the whole relation, never a delta.
    #[test]
    fn a_negated_atom_blocks_a_match_whatever_round_made_it() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a")).class;
        let b = eg.add(Term::leaf("b")).class;
        let blocked = eg.add(Term::op("f", &[a, b])).class;
        let free = eg.add(Term::op("f", &[b, a])).class;
        eg.add(Term::op("g", &[a]));
        eg.rebuild();

        let query = Query {
            vars: 4,
            scalars: 0,
            root: 0,
            atoms: vec![node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0)],
            guards: Vec::new(),
            nots: vec![Nested {
                atoms: vec![node(Term::op("g", &[ClassId(0)]), &[1], 3)],
                guards: Vec::new(),
            }],
        };
        let plan = Plan::compile(query);
        assert_eq!(plan.steps(), &[Step::Atom(0), Step::Not(0)]);
        let found = plan.search(&eg, [blocked, free], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].root, free);
    }

    /// A second root reached from an operand the first bound.
    #[test]
    fn a_sideways_atom_is_reached_through_a_bound_operand() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a")).class;
        let b = eg.add(Term::leaf("b")).class;
        let c = eg.add(Term::leaf("c")).class;
        let f = eg.add(Term::op("f", &[a, b])).class;
        let g = eg.add(Term::op("g", &[a, b])).class;
        eg.add(Term::op("g", &[a, c]));
        eg.rebuild();

        let plan = Plan::compile(Query::tree(
            4,
            0,
            vec![
                node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0),
                node(Term::op("g", &[ClassId(0), ClassId(0)]), &[1, 2], 3),
            ],
        ));
        assert_eq!(
            plan.steps(),
            &[Step::Atom(0), Step::Parents { atom: 1, slot: 0 }]
        );
        let found = plan.search(&eg, [f], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].binding(3), Some(g.into()));
    }

    /// `f(?1, ?2)` where `?1` is a literal below ten.
    #[test]
    fn a_guard_runs_as_soon_as_its_scalars_are_bound() {
        let mut eg = Engine::new();
        let small = eg.add(Term::int(3)).class;
        let big = eg.add(Term::int(30)).class;
        let y = eg.add(Term::leaf("y")).class;
        let hit = eg.add(Term::op("f", &[small, y])).class;
        let miss = eg.add(Term::op("f", &[big, y])).class;
        eg.rebuild();

        let query = Query {
            vars: 3,
            scalars: 2,
            root: 0,
            atoms: vec![
                node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0),
                Atom::Fact {
                    column: ColumnId::Const,
                    key: 1,
                    value: 0,
                },
            ],
            guards: vec![
                Guard::Read {
                    term: Source::Label(0),
                    field: 0,
                    out: 1,
                },
                Guard::Cmp(Cmp::Lt, Expr::Scalar(1), Expr::Lit(10)),
            ],
            nots: Vec::new(),
        };
        let plan = Plan::compile(query);
        assert_eq!(
            plan.steps(),
            &[Step::Atom(0), Step::Atom(1), Step::Guard(0), Step::Guard(1)]
        );
        let found = plan.search(&eg, [hit, miss], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].root, hit);
        assert_eq!(found[0].scalars[1], 3);
    }

    /// `g(?a)` at a 32-bit carrier, with `?a` read as a base and an offset.
    fn g_at_offset(
        extra: Vec<Atom<Term>>,
        guards: Vec<Guard>,
        nots: Vec<Nested<Term>>,
    ) -> Plan<Term> {
        let mut atoms = vec![
            node(Term::typed("g", 32, &[ClassId(0)]), &[1], 0),
            Atom::Offset {
                key: 1,
                base: 2,
                offset: 0,
            },
        ];
        atoms.extend(extra);
        Plan::compile(Query {
            vars: 3,
            scalars: 2,
            root: 0,
            atoms,
            guards,
            nots,
        })
    }

    fn typed_graph() -> (Engine<Term>, Ref, Ref) {
        let mut eg = Engine::new();
        let x = eg.add(Term::typed("x", 32, &[]));
        let y = eg.add(Term::typed("y", 32, &[]));
        (eg, x, y)
    }

    fn g_of(eg: &mut Engine<Term>, operand: Ref) -> Ref {
        eg.insert(Term::typed("g", 32, &[ClassId(0)]), &[operand])
    }

    #[test]
    fn a_guard_filters_on_the_offset_an_operand_sits_at() {
        let (mut eg, x, y) = typed_graph();
        let near = g_of(&mut eg, Ref::new(x.class, 2));
        let far = g_of(&mut eg, Ref::new(x.class, 9));
        let plain = g_of(&mut eg, y);
        eg.rebuild();
        let plan = g_at_offset(
            Vec::new(),
            vec![Guard::Cmp(Cmp::Lt, Expr::Scalar(0), Expr::Lit(4))],
            Vec::new(),
        );
        let found = plan.search(
            &eg,
            [near.class, far.class, plain.class],
            &|_, _| true,
            false,
            &NoExterns,
        );
        let got: Vec<(ClassId, Option<Ref>, u64)> = found
            .iter()
            .map(|m| (m.root, m.binding(2), m.scalars[0]))
            .collect();
        assert_eq!(
            got,
            vec![(near.class, Some(x), 2), (plain.class, Some(y), 0)]
        );
    }

    #[test]
    fn a_constant_operand_binds_its_value_without_a_row() {
        let (mut eg, x, _) = typed_graph();
        let seven = eg.add(Term::num(32, 7));
        let constant = g_of(&mut eg, seven);
        let variable = g_of(&mut eg, x);
        eg.rebuild();
        let plan = g_at_offset(
            vec![Atom::Const { key: 1, value: 1 }],
            Vec::new(),
            Vec::new(),
        );
        let found = plan.search(
            &eg,
            [constant.class, variable.class],
            &|_, _| true,
            false,
            &NoExterns,
        );
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].root, found[0].scalars[1]), (constant.class, 7));
    }

    #[test]
    fn a_width_atom_binds_the_carrier_of_any_class_that_has_one() {
        let (mut eg, x, _) = typed_graph();
        let seven = eg.add(Term::num(8, 7));
        let wide = g_of(&mut eg, x);
        let narrow = g_of(&mut eg, seven);
        let plain = eg.add(Term::leaf("p"));
        let untyped = g_of(&mut eg, plain);
        eg.rebuild();
        let plan = g_at_offset(
            vec![Atom::Width { key: 1, width: 1 }],
            Vec::new(),
            Vec::new(),
        );
        let found = plan.search(
            &eg,
            [wide.class, narrow.class, untyped.class],
            &|_, _| true,
            false,
            &NoExterns,
        );
        let got: Vec<(ClassId, u64)> = found.iter().map(|m| (m.root, m.scalars[1])).collect();
        assert_eq!(got, vec![(wide.class, 32), (narrow.class, 8)]);
    }

    #[test]
    fn a_conflicted_operand_matches_and_blocks() {
        let (mut eg, x, y) = typed_graph();
        let placed = g_of(&mut eg, x);
        let unplaced = g_of(&mut eg, y);
        eg.union(y, Ref::new(y.class, 1)).unwrap_err();
        eg.rebuild();
        let roots = [placed.class, unplaced.class];
        let conflicted = || Atom::Conflicted { key: 1 };
        let positive = g_at_offset(vec![conflicted()], Vec::new(), Vec::new());
        let found = positive.search(&eg, roots, &|_, _| true, false, &NoExterns);
        assert_eq!(
            found.iter().map(|m| m.root).collect::<Vec<_>>(),
            vec![unplaced.class]
        );
        let nots = vec![Nested {
            atoms: vec![conflicted()],
            guards: Vec::new(),
        }];
        let negative = g_at_offset(Vec::new(), Vec::new(), nots);
        let found = negative.search(&eg, roots, &|_, _| true, false, &NoExterns);
        assert_eq!(
            found.iter().map(|m| m.root).collect::<Vec<_>>(),
            vec![placed.class]
        );
    }

    /// A class whose e-node sits at an offset from it is matched at that
    /// offset, and a nested atom matches only the reference its parent names.
    #[test]
    fn the_root_binds_each_row_at_its_own_offset() {
        let (mut eg, x, y) = typed_graph();
        let g_y = g_of(&mut eg, y);
        // g(y) == x + 3.
        eg.union(g_y, Ref::new(x.class, 3)).unwrap();
        let h = Term::typed("h", 32, &[ClassId(0)]);
        let on_node = eg.insert(h.clone(), &[Ref::new(x.class, 3)]);
        let off_node = eg.insert(h, &[x]);
        eg.rebuild();
        let root = eg.root(x.class);
        let found = Plan::compile(Query::tree(
            2,
            0,
            vec![node(Term::typed("g", 32, &[ClassId(0)]), &[1], 0)],
        ))
        .search(&eg, [root], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].binding(0), Some(eg.find(g_y)));
        let nested = Plan::compile(Query::tree(
            3,
            0,
            vec![
                node(Term::typed("h", 32, &[ClassId(0)]), &[1], 0),
                node(Term::typed("g", 32, &[ClassId(0)]), &[2], 1),
            ],
        ));
        let found = nested.search(
            &eg,
            [on_node.class, off_node.class],
            &|_, _| true,
            false,
            &NoExterns,
        );
        assert_eq!(
            found.iter().map(|m| m.root).collect::<Vec<_>>(),
            vec![on_node.class]
        );
    }

    /// A term over three leaves and three operators, one of them commutative.
    fn term_strategy() -> impl Strategy<Value = Vec<(String, Vec<usize>)>> {
        prop::collection::vec(
            (
                prop_oneof![
                    Just("x".to_string()),
                    Just("y".to_string()),
                    Just("f".to_string()),
                    Just("g".to_string()),
                    Just("h".to_string()),
                ],
                prop::collection::vec(0usize..6, 0..3),
            ),
            1..8,
        )
    }

    /// Build an engine from `recipe`: each entry names an operator and the
    /// earlier entries its operands, wrapped into range.
    fn build(recipe: &[(String, Vec<usize>)]) -> (Engine<Term>, Vec<ClassId>) {
        let mut eg = Engine::new();
        let mut made: Vec<ClassId> = Vec::new();
        for (op, operands) in recipe {
            let arity = match op.as_str() {
                "x" | "y" => 0,
                "g" => 1,
                _ => 2,
            };
            let children: Vec<ClassId> = (0..arity)
                .map(|i| {
                    made.get(operands.get(i).copied().unwrap_or(0) % made.len().max(1))
                        .copied()
                        .unwrap_or(ClassId(0))
                })
                .collect();
            if made.is_empty() && arity > 0 {
                continue;
            }
            let term = if op == "h" {
                Term::comm(op, &children)
            } else {
                Term::op(op, &children)
            };
            made.push(eg.add(term).class);
        }
        eg.rebuild();
        (eg, made)
    }

    proptest! {
        /// The evaluator's bindings are exactly the satisfying assignments.
        #[test]
        fn query_equals_brute_force(recipe in term_strategy(), which in 0usize..5) {
            let (eg, made) = build(&recipe);
            prop_assume!(!made.is_empty());
            let query = match which {
                0 => Query::tree(3, 0, vec![
                    node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0)]),
                1 => Query::tree(3, 0, vec![
                    node(Term::comm("h", &[ClassId(0), ClassId(0)]), &[1, 2], 0)]),
                // One variable in two operand slots: the match must agree on it.
                2 => Query::tree(2, 0, vec![
                    node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 1], 0)]),
                // A second root, reached sideways through a shared operand.
                3 => Query::tree(4, 0, vec![
                    node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0),
                    node(Term::op("g", &[ClassId(0), ClassId(0)]), &[1, 2], 3)]),
                _ => f_of_g(),
            };
            let roots: Vec<ClassId> = eg.class_ids().collect();
            let found = Plan::compile(query.clone()).search(&eg, roots, &|_, _| true, false, &NoExterns);
            let mut got: Vec<Vec<ClassId>> = found
                .iter()
                .map(|m| m.bindings.iter().map(|b| b.class).collect())
                .collect();
            got.sort();
            got.dedup();
            let mut want = brute_force(&eg, &query);
            want.sort();
            want.dedup();
            prop_assert_eq!(got, want);
        }
    }
}
