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

/// One conjunct of a query.
#[derive(Clone, Debug)]
pub enum Atom<L> {
    /// A row of `class` whose label matches `template` and whose children bind
    /// `args`, one per operand.
    Node {
        template: L,
        args: SmallVec<[Var; 4]>,
        class: Var,
    },
    /// `class` holds `value` as a childless row, or is assumed to evaluate to it.
    Literal { value: L, class: Var },
}

impl<L> Atom<L> {
    /// The variable this atom's rows are looked up under; bound before the atom
    /// is stepped.
    pub fn class(&self) -> Var {
        match self {
            Atom::Node { class, .. } | Atom::Literal { class, .. } => *class,
        }
    }
}

/// A conjunctive query. Every atom's class variable is reachable from
/// [`Self::root`] through the atoms' `args`, which is what lets a plan bind
/// them in one downward pass.
#[derive(Clone, Debug)]
pub struct Query<L> {
    pub vars: u32,
    pub root: Var,
    pub atoms: Vec<Atom<L>>,
}

/// One match: the class the root variable bound to, and the class every
/// variable bound to. `None` for a variable no atom reached.
#[derive(Clone, Debug)]
pub struct Match {
    pub root: ClassId,
    pub bindings: SmallVec<[Option<ClassId>; 8]>,
}

/// A query with its atoms ordered for evaluation: each step's class variable is
/// bound by the root or by an earlier step, so evaluation is a loop nest with no
/// search over orders.
#[derive(Clone, Debug)]
pub struct Plan<L> {
    query: Query<L>,
    /// Atom indices in evaluation order.
    steps: Vec<usize>,
    /// Variables in the order the nest first binds them.
    bind_order: Vec<Var>,
}

impl<L: Label> Plan<L> {
    /// Order `query`'s atoms: the root's atoms first, then whatever their
    /// operands bound, breadth by query order and depth first — the order the
    /// query was written in, since an atom becomes steppable as soon as the atom
    /// naming its class has run.
    ///
    /// Panics if an atom's class variable is unreachable from the root.
    pub fn compile(query: Query<L>) -> Self {
        let mut bound = vec![false; query.vars as usize];
        bound[query.root as usize] = true;
        let mut bind_order = vec![query.root];
        let mut steps = Vec::with_capacity(query.atoms.len());
        let mut taken = vec![false; query.atoms.len()];
        while steps.len() < query.atoms.len() {
            let next = (0..query.atoms.len())
                .find(|&i| !taken[i] && bound[query.atoms[i].class() as usize])
                .expect("every atom's class is reachable from the root");
            taken[next] = true;
            steps.push(next);
            if let Atom::Node { args, .. } = &query.atoms[next] {
                for &arg in args {
                    if !std::mem::replace(&mut bound[arg as usize], true) {
                        bind_order.push(arg);
                    }
                }
            }
        }
        Self {
            query,
            steps,
            bind_order,
        }
    }

    pub fn query(&self) -> &Query<L> {
        &self.query
    }

    /// The atom indices in evaluation order.
    pub fn steps(&self) -> &[usize] {
        &self.steps
    }

    /// The variables in the order evaluation first binds them.
    pub fn bind_order(&self) -> &[Var] {
        &self.bind_order
    }

    /// Every match at `roots`, each root canonicalized and visited once.
    ///
    /// `allowed(var, class)` prunes a binding the caller rejects — the hook for
    /// operand constraints instruction selection carries outside the query.
    /// When `only_new` is set, a match none of whose rows the previous round
    /// touched is dropped: it existed a round earlier at a root that was
    /// searched then, so its head ran then.
    pub fn search(
        &self,
        eg: &Engine<L>,
        roots: impl IntoIterator<Item = ClassId>,
        allowed: &dyn Fn(Var, ClassId) -> bool,
        only_new: bool,
    ) -> Vec<Match> {
        let mut eval = Eval {
            eg,
            allowed,
            only_new: only_new
                && self
                    .query
                    .atoms
                    .iter()
                    .any(|atom| matches!(atom, Atom::Node { .. })),
            bound: SmallVec::from_elem(None, self.query.vars as usize),
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
        let Some(&atom) = self.steps.get(index) else {
            if !eval.only_new || eval.fresh > 0 {
                eval.out.push(Match {
                    root,
                    bindings: eval.bound.clone(),
                });
            }
            return;
        };
        let atom = &self.query.atoms[atom];
        let class = eval.bound[atom.class() as usize].expect("class bound by an earlier step");
        match atom {
            Atom::Literal { value, .. } => self.literal(eval, root, index, value, class),
            Atom::Node { template, args, .. } => {
                self.node(eval, root, index, template, args, class)
            }
        }
    }

    fn node(
        &self,
        eval: &mut Eval<'_, L>,
        root: ClassId,
        index: usize,
        template: &L,
        args: &[Var],
        class: ClassId,
    ) {
        let eg = eval.eg;
        for row in eg.rows(class) {
            let children = eg.children(row);
            if children.len() != args.len() || !template.matches_template(eg.node(row)) {
                continue;
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
        let fresh = usize::from(eg.const_is_new(class));
        eval.fresh += fresh;
        self.step(eval, root, index + 1);
        eval.fresh -= fresh;
    }
}

/// State one [`Plan::search`] threads through the loop nest.
struct Eval<'a, L: Label> {
    eg: &'a Engine<L>,
    allowed: &'a dyn Fn(Var, ClassId) -> bool,
    only_new: bool,
    bound: SmallVec<[Option<ClassId>; 8]>,
    /// Variables this branch bound, to undo on the way out.
    trail: SmallVec<[Var; 8]>,
    /// How many rows of the partial match the previous round touched.
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
        }
    }

    /// `f(g(?1), ?2)` with the atoms written parent-last, so ordering has to do
    /// something.
    fn f_of_g() -> Query<Term> {
        Query {
            vars: 4,
            root: 0,
            atoms: vec![
                node(Term::op("g", &[ClassId(0)]), &[3], 1),
                node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0),
            ],
        }
    }

    #[test]
    fn plan_steps_the_root_atom_first_then_what_it_bound() {
        let plan = Plan::compile(f_of_g());
        assert_eq!(plan.steps(), &[1, 0]);
        assert_eq!(plan.bind_order(), &[0, 1, 2, 3]);
    }

    #[test]
    #[should_panic(expected = "reachable from the root")]
    fn plan_rejects_an_atom_no_operand_reaches() {
        Plan::compile(Query {
            vars: 2,
            root: 0,
            atoms: vec![node(Term::leaf("x"), &[], 1)],
        });
    }

    #[test]
    fn search_binds_every_variable_of_the_pattern() {
        let mut eg = Engine::new();
        let x = eg.add(Term::leaf("x"));
        let y = eg.add(Term::leaf("y"));
        let g = eg.add(Term::op("g", &[x]));
        let f = eg.add(Term::op("f", &[g, y]));
        let found = Plan::compile(f_of_g()).search(&eg, [f], &|_, _| true, false);
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
                0 => Query { vars: 3, root: 0, atoms: vec![
                    node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0)] },
                1 => Query { vars: 3, root: 0, atoms: vec![
                    node(Term::comm("h", &[ClassId(0), ClassId(0)]), &[1, 2], 0)] },
                // One variable in two operand slots: the match must agree on it.
                2 => Query { vars: 2, root: 0, atoms: vec![
                    node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 1], 0)] },
                _ => f_of_g(),
            };
            let roots: Vec<ClassId> = eg.class_ids().collect();
            let found = Plan::compile(query.clone()).search(&eg, roots, &|_, _| true, false);
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
