//! Rules as conjunctive queries: a rule's left-hand side is a set of atoms over
//! the engine's rows, and matching it is a join rather than a backtracking walk
//! over a goal stack.
//!
//! A [`Query`] is the rule IR; a [`Plan`] is that query with an evaluation order
//! chosen once, and [`Plan::search`] is the evaluator. The order a plan produces
//! matches in is part of the contract: heads are applied in match order, so the
//! ids a saturation mints depend on it.

use smallvec::SmallVec;

use crate::{ClassId, Engine, Label};

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
    /// One when the operand is zero, zero otherwise — a comparison's negation,
    /// which is what a rule proving the complement of a settled comparison
    /// needs to spell.
    IsZero(Box<Expr>),
}

impl Expr {
    fn eval(&self, scalars: &[u64]) -> Option<i64> {
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
            Expr::IsZero(e) => i64::from(e.eval(scalars)? == 0),
        })
    }

    fn reads(&self, out: &mut Vec<Scalar>) {
        match self {
            Expr::Lit(_) => {}
            Expr::Scalar(slot) => out.push(*slot),
            Expr::Sub(a, b) | Expr::Add(a, b) => {
                a.reads(out);
                b.reads(out);
            }
            Expr::Ones(e) | Expr::IsZero(e) => e.reads(out),
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
    fn reads(&self) -> Vec<Scalar> {
        let mut out = Vec::new();
        match self {
            Guard::Cmp(_, a, b) => {
                a.reads(&mut out);
                b.reads(&mut out);
            }
            Guard::Let { value, .. } => value.reads(&mut out),
            Guard::Read { term, .. } => out.push(term.slot()),
            Guard::Extern { terms, args, .. } => {
                out.extend(terms.iter().map(|term| term.slot()));
                for arg in args {
                    arg.reads(&mut out);
                }
            }
        }
        out
    }

    fn writes(&self) -> SmallVec<[Scalar; 2]> {
        match self {
            Guard::Cmp(..) => SmallVec::new(),
            Guard::Let { out, .. } | Guard::Read { out, .. } => SmallVec::from_slice(&[*out]),
            Guard::Extern { out, .. } => out.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Atom<L> {
    /// A row of `class` whose label matches `template` and whose children bind
    /// `args`, one per operand. `row` binds the matched row, whose e-node a
    /// guard reads fields off or hands to a host function.
    Node {
        template: L,
        args: SmallVec<[Var; 4]>,
        class: Var,
        row: Option<Scalar>,
    },
    /// `class` holds `value` as a childless row, or is assumed to evaluate to it.
    Literal { value: L, class: Var },
    /// `key` has a value in `column`; bind it to `value`.
    Fact {
        column: ColumnId,
        key: Var,
        value: Scalar,
    },
}

impl<L> Atom<L> {
    /// The variable this atom's rows are looked up under; bound before the atom
    /// is stepped.
    pub fn class(&self) -> Var {
        match self {
            Atom::Node { class, .. } | Atom::Literal { class, .. } => *class,
            Atom::Fact { key, .. } => *key,
        }
    }

    /// The scalars this atom binds.
    fn writes(&self) -> SmallVec<[Scalar; 2]> {
        match self {
            Atom::Node { row, .. } => row.iter().copied().collect(),
            Atom::Literal { .. } => SmallVec::new(),
            Atom::Fact { value, .. } => SmallVec::from_slice(&[*value]),
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
        }
    }
}

/// One match: the class the root variable bound to, and the class every
/// variable bound to. `None` for a variable no atom reached.
#[derive(Clone, Debug)]
pub struct Match {
    pub root: ClassId,
    pub bindings: SmallVec<[Option<ClassId>; 8]>,
    pub scalars: SmallVec<[u64; 4]>,
}

/// A query with its atoms ordered for evaluation: each step's class variable is
/// bound by the root or by an earlier step, so evaluation is a loop nest with no
/// search over orders. Guards run as soon as their inputs are bound.
#[derive(Clone, Debug)]
pub struct Plan<L> {
    query: Query<L>,
    steps: Vec<Step>,
    height: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Atom(usize),
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
        let mut known = vec![false; query.scalars as usize];
        let mut steps = Vec::with_capacity(query.atoms.len() + query.guards.len());
        let mut taken = vec![false; query.atoms.len()];
        let mut checked = vec![false; query.guards.len()];
        let mut depth = vec![0usize; query.vars as usize];
        let mut height = 0usize;
        loop {
            let guard = (0..query.guards.len()).find(|&i| {
                !checked[i] && query.guards[i].reads().iter().all(|&s| known[s as usize])
            });
            if let Some(guard) = guard {
                checked[guard] = true;
                for out in query.guards[guard].writes() {
                    known[out as usize] = true;
                }
                steps.push(Step::Guard(guard));
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
                    let (next, slot) = (0..query.atoms.len())
                        .filter(|&i| !taken[i])
                        .find_map(|i| match &query.atoms[i] {
                            Atom::Node { args, .. } => args
                                .iter()
                                .position(|&arg| bound[arg as usize])
                                .map(|slot| (i, slot as u8)),
                            _ => None,
                        })
                        .expect("every atom is reached from the root or from an operand");
                    (next, Step::Parents { atom: next, slot })
                }
            };
            taken[next] = true;
            steps.push(step);
            bound[query.atoms[next].class() as usize] = true;
            let below = depth[query.atoms[next].class() as usize] + 1;
            if let Atom::Node { args, .. } = &query.atoms[next] {
                height = height.max(below);
                for &arg in args {
                    if !bound[arg as usize] {
                        bound[arg as usize] = true;
                        depth[arg as usize] = below;
                    }
                }
            }
            for slot in query.atoms[next].writes() {
                known[slot as usize] = true;
            }
        }
        assert!(
            checked.iter().all(|&c| c),
            "every guard reads scalars the atoms bind"
        );
        Self {
            query,
            steps,
            height,
        }
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
    pub fn height(&self) -> usize {
        self.height
    }

    /// The classes the root atom can match at: those holding its operator, or
    /// every class when the root binds no row.
    pub fn roots(&self, eg: &Engine<L>) -> Vec<ClassId> {
        match self
            .query
            .atoms
            .iter()
            .find(|atom| atom.class() == self.query.root)
        {
            Some(Atom::Node { template, .. }) => eg.classes_with_op(template.op_key()),
            _ => eg.class_ids().collect(),
        }
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
        let mut eval = Eval {
            eg,
            externs,
            allowed,
            only_new: only_new
                && self
                    .query
                    .atoms
                    .iter()
                    .any(|atom| matches!(atom, Atom::Node { .. })),
            bound: SmallVec::from_elem(None, self.query.vars as usize),
            scalars: SmallVec::from_elem(0, self.query.scalars as usize),
            trail: SmallVec::new(),
            fresh: 0,
            out: Vec::new(),
        };
        let mut seen = crate::label::FxHashMap::<ClassId, ()>::default();
        for root in roots {
            let root = eg.find(root);
            if seen.insert(root, ()).is_some() || !allowed(self.query.root, root) {
                continue;
            }
            eval.bound[self.query.root as usize] = Some(root);
            self.step(&mut eval, root, 0);
            eval.bound[self.query.root as usize] = None;
        }
        eval.out
    }

    fn step(&self, eval: &mut Eval<'_, L>, root: ClassId, index: usize) {
        let Some(&step) = self.steps.get(index) else {
            if !eval.only_new || eval.fresh > 0 {
                eval.out.push(Match {
                    root,
                    bindings: eval.bound.clone(),
                    scalars: eval.scalars.clone(),
                });
            }
            return;
        };
        let atom = match step {
            Step::Parents { atom, slot } => {
                let Atom::Node {
                    template,
                    args,
                    class,
                    row,
                } = &self.query.atoms[atom]
                else {
                    unreachable!("only a row atom is reached sideways")
                };
                let child = eval.bound[args[slot as usize] as usize].expect("bound operand");
                self.parents(eval, root, index, template, args, *class, *row, child);
                return;
            }
            Step::Guard(guard) => {
                // A guard writes at most one scalar and is placed once, so the
                // branch that fails it cannot have clobbered a live value.
                if eval.holds(&self.query.guards[guard]) {
                    self.step(eval, root, index + 1);
                }
                return;
            }
            Step::Atom(atom) => &self.query.atoms[atom],
        };
        let class = eval.bound[atom.class() as usize].expect("class bound by an earlier step");
        match atom {
            Atom::Literal { value, .. } => self.literal(eval, root, index, value, class),
            Atom::Fact { column, value, .. } => {
                self.fact(eval, root, index, *column, *value, class)
            }
            Atom::Node {
                template,
                args,
                row,
                ..
            } => self.node(eval, root, index, template, args, *row, class),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        template: &L,
        args: &[Var],
        bind_row: Option<Scalar>,
        class: ClassId,
    ) {
        let eg = eval.eg;
        for row in eg.rows(class) {
            let children = eg.children(row);
            if children.len() != args.len() || !template.matches_template(eg.node(row)) {
                continue;
            }
            if let Some(slot) = bind_row {
                eval.scalars[slot as usize] = row.0 as u64;
            }
            // A commutative binary operator matches in both operand orders.
            let orders = if children.len() == 2 && eg.node(row).commutative() {
                2
            } else {
                1
            };
            let fresh = usize::from(eg.row_is_new(row));
            eval.fresh += fresh;
            for order in 0..orders {
                let mark = eval.trail.len();
                if eval.bind(args, children, order) {
                    self.step(eval, root, index + 1);
                }
                eval.unbind(mark);
            }
            eval.fresh -= fresh;
        }
    }

    /// Reach a row through a bound operand's back-edges. Everything already
    /// bound is checked against the row; the rest, including the row's own
    /// class, is bound from it.
    #[allow(clippy::too_many_arguments)]
    fn parents(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        template: &L,
        args: &[Var],
        class: Var,
        bind_row: Option<Scalar>,
        child: ClassId,
    ) {
        let eg = eval.eg;
        for row in eg.parents(child) {
            let children = eg.children(row);
            if children.len() != args.len() || !template.matches_template(eg.node(row)) {
                continue;
            }
            if let Some(slot) = bind_row {
                eval.scalars[slot as usize] = row.0 as u64;
            }
            let orders = if children.len() == 2 && eg.node(row).commutative() {
                2
            } else {
                1
            };
            let fresh = usize::from(eg.row_is_new(row));
            eval.fresh += fresh;
            for order in 0..orders {
                let mark = eval.trail.len();
                let owner = eg.find(eg.owner(row));
                if eval.bind(&[class], &[owner], 0) && eval.bind(args, children, order) {
                    self.step(eval, root, index + 1);
                }
                eval.unbind(mark);
            }
            eval.fresh -= fresh;
        }
    }

    /// A literal reads the constant column rather than the class's rows: what a
    /// class is *known* to be covers both its own literal and what a scope
    /// assumed of it, and the column's stamp says which round proved it.
    fn literal(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        value: &L,
        class: ClassId,
    ) {
        let eg = eval.eg;
        if !eg.const_of(class).is_some_and(|known| value.matches(known)) {
            return;
        }
        let fresh = usize::from(eg.fact_is_new(ColumnId::Const, class));
        eval.fresh += fresh;
        self.step(eval, root, index + 1);
        eval.fresh -= fresh;
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

/// State one [`Plan::search`] threads through the loop nest.
struct Eval<'a, L: Label> {
    eg: &'a Engine<L>,
    externs: &'a dyn Externs<L>,
    allowed: &'a dyn Fn(Var, ClassId) -> bool,
    only_new: bool,
    bound: SmallVec<[Option<ClassId>; 8]>,
    scalars: SmallVec<[u64; 4]>,
    /// Variables this branch bound, to undo on the way out.
    trail: SmallVec<[Var; 8]>,
    /// How many rows and facts of the partial match the previous round touched.
    fresh: usize,
    out: Vec<Match>,
}

impl<'a, L: Label> Eval<'a, L> {
    /// Bind `args` to a row's children, or report the row inconsistent with what
    /// is already bound. Whatever it bound before failing is on the trail.
    fn bind(&mut self, args: &[Var], children: &[ClassId], order: usize) -> bool {
        for (slot, &var) in args.iter().enumerate() {
            let child = children[if order == 1 { 1 - slot } else { slot }];
            let child = self.eg.find(child);
            match self.bound[var as usize] {
                // A variable shared by two atoms must bind the same class.
                Some(prior) => {
                    if prior != child {
                        return false;
                    }
                }
                None => {
                    if !(self.allowed)(var, child) {
                        return false;
                    }
                    self.bound[var as usize] = Some(child);
                    self.trail.push(var);
                }
            }
        }
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
                let Some(terms): Option<SmallVec<[&L; 2]>> =
                    terms.iter().map(|&term| self.term(term)).collect()
                else {
                    return false;
                };
                let Some(args): Option<SmallVec<[u64; 4]>> = args
                    .iter()
                    .map(|arg| arg.eval(&self.scalars).map(|value| value as u64))
                    .collect()
                else {
                    return false;
                };
                let mut values: SmallVec<[u64; 2]> = SmallVec::from_elem(0, out.len());
                if !self.externs.call(*call, &terms, &args, &mut values) {
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
        for var in self.trail.drain(mark..) {
            self.bound[var as usize] = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[should_panic(expected = "reached from the root or from an operand")]
    fn plan_rejects_an_atom_no_operand_reaches() {
        Plan::compile(Query::tree(2, 0, vec![node(Term::leaf("x"), &[], 1)]));
    }

    #[test]
    fn search_binds_every_variable_of_the_pattern() {
        let mut eg = Engine::new();
        let x = eg.add(Term::leaf("x"));
        let y = eg.add(Term::leaf("y"));
        let g = eg.add(Term::op("g", &[x]));
        let f = eg.add(Term::op("f", &[g, y]));
        let found = Plan::compile(f_of_g()).search(&eg, [f], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].bindings.as_slice(),
            &[Some(f), Some(g), Some(y), Some(x)]
        );
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
            Atom::Literal { value, .. } => {
                eg.const_of(class).is_some_and(|known| value.matches(known))
            }
            Atom::Fact { column, .. } => eg.fact(*column, class).is_some(),
            Atom::Node { template, args, .. } => eg.rows(class).any(|row| {
                let children: Vec<ClassId> = eg.children(row).iter().map(|&c| eg.find(c)).collect();
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

    /// A second root reached from an operand the first bound.
    #[test]
    fn a_sideways_atom_is_reached_through_a_bound_operand() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let c = eg.add(Term::leaf("c"));
        let f = eg.add(Term::op("f", &[a, b]));
        let g = eg.add(Term::op("g", &[a, b]));
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
        assert_eq!(found[0].bindings[3], Some(g));
    }

    /// `f(?1, ?2)` where `?1` is a literal below ten.
    #[test]
    fn a_guard_runs_as_soon_as_its_scalars_are_bound() {
        let mut eg = Engine::new();
        let small = eg.add(Term::int(3));
        let big = eg.add(Term::int(30));
        let y = eg.add(Term::leaf("y"));
        let hit = eg.add(Term::op("f", &[small, y]));
        let miss = eg.add(Term::op("f", &[big, y]));
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
            made.push(eg.add(term));
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
                .map(|m| m.bindings.iter().map(|b| b.expect("bound")).collect())
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
