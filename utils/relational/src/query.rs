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

/// The host's primitive functions. A guard may only read bound scalars, never
/// the graph — that is what makes a match's existence a function of its atoms,
/// and so what makes the delta exact.
pub trait Externs {
    /// `None` fails the match.
    fn call(&self, id: ExternId, args: &[u64]) -> Option<u64>;
}

/// No externs: every call fails.
pub struct NoExterns;

impl Externs for NoExterns {
    fn call(&self, _id: ExternId, _args: &[u64]) -> Option<u64> {
        None
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
            Expr::Ones(e) => e.reads(out),
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
    /// Read `field` off the label a fact column bound, so a constant's value and
    /// width reach the guards as words. The engine owns the label table; this is
    /// a decode, not a look at the graph.
    Read {
        label: Scalar,
        field: Field,
        out: Scalar,
    },
    /// A host function; `out` binds its result, and `None` fails the match.
    Extern {
        call: ExternId,
        args: SmallVec<[Scalar; 4]>,
        out: Option<Scalar>,
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
            Guard::Read { label, .. } => out.push(*label),
            Guard::Extern { args, .. } => out.extend(args.iter().copied()),
        }
        out
    }

    fn writes(&self) -> Option<Scalar> {
        match self {
            Guard::Cmp(..) => None,
            Guard::Read { out, .. } | Guard::Extern { out: Some(out), .. } => Some(*out),
            Guard::Extern { out: None, .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Atom<L> {
    /// A row of `class` whose label matches `template` and whose children bind
    /// `args`, one per operand. `reads` pulls scalars off the matched label.
    Node {
        template: L,
        args: SmallVec<[Var; 4]>,
        class: Var,
        reads: SmallVec<[(Field, Scalar); 2]>,
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
            Atom::Node { reads, .. } => reads.iter().map(|&(_, slot)| slot).collect(),
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Atom(usize),
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
        loop {
            let guard = (0..query.guards.len()).find(|&i| {
                !checked[i] && query.guards[i].reads().iter().all(|&s| known[s as usize])
            });
            if let Some(guard) = guard {
                checked[guard] = true;
                if let Some(out) = query.guards[guard].writes() {
                    known[out as usize] = true;
                }
                steps.push(Step::Guard(guard));
                continue;
            }
            if taken.iter().all(|&t| t) {
                break;
            }
            let next = (0..query.atoms.len())
                .find(|&i| !taken[i] && bound[query.atoms[i].class() as usize])
                .expect("every atom's class is reachable from the root");
            taken[next] = true;
            steps.push(Step::Atom(next));
            if let Atom::Node { args, .. } = &query.atoms[next] {
                for &arg in args {
                    bound[arg as usize] = true;
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
        Self { query, steps }
    }

    pub fn query(&self) -> &Query<L> {
        &self.query
    }

    /// The atoms and guards in evaluation order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
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
        externs: &dyn Externs,
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
                reads,
                ..
            } => self.node(eval, root, index, template, args, reads, class),
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
        reads: &[(Field, Scalar)],
        class: ClassId,
    ) {
        let eg = eval.eg;
        for row in eg.rows(class) {
            let children = eg.children(row);
            if children.len() != args.len() || !template.matches_template(eg.node(row)) {
                continue;
            }
            let Some(()) = eval.read(reads, row) else {
                continue;
            };
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
    externs: &'a dyn Externs,
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

impl<L: Label> Eval<'_, L> {
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
            Guard::Read { label, field, out } => {
                let label = crate::LabelId(self.scalars[*label as usize] as u32);
                let Some(value) = self.eg.label_scalar(label, *field) else {
                    return false;
                };
                self.scalars[*out as usize] = value;
                true
            }
            Guard::Extern { call, args, out } => {
                let args: SmallVec<[u64; 4]> = args
                    .iter()
                    .map(|&slot| self.scalars[slot as usize])
                    .collect();
                let Some(value) = self.externs.call(*call, &args) else {
                    return false;
                };
                if let Some(slot) = out {
                    self.scalars[*slot as usize] = value;
                }
                true
            }
        }
    }

    /// Pull `reads` off a matched row's label. `None` when the label has no such
    /// field, which fails the atom.
    fn read(&mut self, reads: &[(Field, Scalar)], row: crate::RowId) -> Option<()> {
        for &(field, slot) in reads {
            self.scalars[slot as usize] = self.eg.node(row).scalar(field)?;
        }
        Some(())
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
            reads: SmallVec::new(),
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
    }

    #[test]
    #[should_panic(expected = "reachable from the root")]
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
                    label: 0,
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
        fn query_equals_brute_force(recipe in term_strategy(), which in 0usize..4) {
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
