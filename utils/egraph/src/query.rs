//! Rules as conjunctive queries over the e-graph: a rule's left-hand side is a
//! set of atoms over e-nodes and the facts known of classes, and matching it is
//! a join rather than a backtracking walk over a goal stack.
//!
//! A [`Query`] is the rule IR rules are written in; a [`Plan`] compiles it onto
//! the database's query IR, one generic atom per atom written, and
//! [`Plan::search`] runs it. Each label is a table's selected cell, so a
//! template reads the labels it matches, and a commutative label lets a binary
//! atom match its row swapped. The order a plan produces matches in is part of
//! the contract: heads are applied in match order, so the ids a saturation
//! mints depend on it.

use smallvec::SmallVec;

use tir_relational::query::{
    self as rel, Calls, Cmp, Expr, ExternId, Host, Out, Param, Resolution, Scalar, Var,
};

use crate::engine::{CLASSES, Terms, lattice, leaf_constant};
use crate::{ClassId, Engine, Label, LabelId, Ref, RowId};

/// A field of a label the language lets a rule read or fill.
pub type Field = u32;

/// A lattice column of the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnId {
    /// The constant a class is known to be.
    Const,
    /// The type its terms carry.
    Type,
    /// A column a client registered, numbered in the order
    /// [`Engine::add_column`] made them.
    Client(u32),
}

/// The host's primitive functions over what a match bound: labels an atom
/// matched or a fact named, and words a guard computed. A guard never sees the
/// graph — a label is an e-node stripped of its children, so there is nowhere to
/// navigate from it — which is what makes a match's existence a function of its
/// atoms, and so what makes the delta exact.
pub trait Externs<L> {
    /// Fill `out` and report success; failure fails the match.
    fn call(&self, id: ExternId, labels: &[&L], args: &[u64], out: &mut [u64]) -> bool;

    /// These functions as several threads may call them at once: `None`
    /// where they read what is bound to one thread, which keeps every search
    /// that calls them on the saturation's own.
    fn shared(&self) -> Option<&(dyn Externs<L> + Sync)> {
        None
    }
}

/// No externs: every call fails.
pub struct NoExterns;

impl<L> Externs<L> for NoExterns {
    fn call(&self, _id: ExternId, _labels: &[&L], _args: &[u64], _out: &mut [u64]) -> bool {
        false
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

    fn kind(self) -> Kind {
        match self {
            Source::Row(_) => Kind::Row,
            Source::Label(_) => Kind::Label,
        }
    }
}

/// A total or partial predicate over bound scalars.
#[derive(Clone, Debug, PartialEq)]
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

/// One conjunct of a [`Query`]. Variables bind references: a class and an
/// offset. A variable bound twice must bind the same reference both times.
// Two predicates compare by address: equal ones behave alike, and unequal
// ones only cost a caller that pools equal queries a duplicate.
#[allow(unpredictable_function_pointer_comparisons)]
#[derive(Clone, Debug, PartialEq)]
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
    /// [`Atom::Node`] over every label `accept` holds of, whatever its
    /// operator, at the arity `args` gives: one atom for a rule that reads
    /// what the label is from a client table rather than from a template.
    /// `label` binds the id of the label the row has, the word
    /// [`Engine::label`] gives: the row's selected cell, so a table atom
    /// reading it joins with the row. An engine never renumbers a label, so
    /// the id keys a client table for the life of that engine.
    Any {
        accept: fn(&L) -> bool,
        args: SmallVec<[Var; 4]>,
        class: Var,
        row: Option<Scalar>,
        label: Option<Scalar>,
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
    /// A row of client table `table` (see [`Engine::add_words`]), one term
    /// per column. A term that is a lone scalar nothing has bound is bound
    /// to the row's cell; any other is evaluated and the row must hold its
    /// value there. Several rows may agree with what is bound: each is a
    /// match, in the order the client added them. See [`rel::Tuples`].
    Table { table: u32, terms: Vec<Expr> },
}

/// A negated sub-conjunction: no solution of it exists, given what the outer
/// match bound. See [`rel::Nested`].
#[derive(Clone, Debug)]
pub struct Nested<L> {
    pub atoms: Vec<Atom<L>>,
    pub guards: Vec<Guard>,
}

impl<L> Atom<L> {
    /// The variable this atom's rows are looked up under; bound before the atom
    /// is stepped. A client table has rows of words alone.
    pub fn class(&self) -> Option<Var> {
        match self {
            Atom::Node { class, .. } | Atom::Any { class, .. } | Atom::Literal { class, .. } => {
                Some(*class)
            }
            Atom::Fact { key, .. }
            | Atom::Offset { key, .. }
            | Atom::Const { key, .. }
            | Atom::Width { key, .. }
            | Atom::Conflicted { key } => Some(*key),
            Atom::Table { .. } => None,
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
        rel::bound(self.bindings[var as usize].into()).map(Ref::from)
    }
}

/// A query compiled onto the database's query IR, with what its parameters
/// and host calls mean in e-graph terms.
#[derive(Clone, Debug)]
pub struct Plan<L> {
    query: Query<L>,
    plan: rel::Plan,
    params: Vec<ParamSpec<L>>,
    calls: Vec<Call>,
    /// The carrier constants its literal atoms test a reference against.
    literals: Vec<L>,
    /// The operator the root atom binds, or `None` when the root binds no row
    /// or a template of no one bucket.
    root_op: Option<u64>,
}

/// What a parameter of the compiled query resolves to.
#[derive(Clone, Debug)]
enum ParamSpec<L> {
    /// The labels in the `op` bucket, or of any bucket for `None`, that
    /// `template` matches at its arity, by the table their rows live in.
    Template {
        template: L,
        arity: usize,
        op: Option<u64>,
    },
    Any {
        arity: usize,
        accept: fn(&L) -> bool,
    },
    /// The labels of that template or any-label parameter that commute, by
    /// table.
    Commuting(Param),
    /// The labels in the `op` bucket that `value` matches: the constants a
    /// literal accepts.
    Equal {
        value: L,
        op: u64,
    },
    // The client table at this slot.
    Table(u32),
}

/// What a host call of the compiled query decodes its words as.
#[derive(Clone, Debug)]
enum Call {
    /// Read `field` off the term the one argument names.
    Read { kind: Kind, field: Field },
    /// The user's extern, its first arguments naming terms of these kinds.
    User {
        call: ExternId,
        kinds: SmallVec<[Kind; 2]>,
    },
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Row,
    Label,
}

/// Collects what [`Plan::compile`] lowers to.
struct Lower<L> {
    params: Vec<ParamSpec<L>>,
    calls: Vec<Call>,
    literals: Vec<L>,
}

impl<L: Label> Lower<L> {
    fn param(&mut self, spec: ParamSpec<L>) -> Param {
        self.params.push(spec);
        self.params.len() as Param - 1
    }

    fn rows(
        &mut self,
        labels: Param,
        args: &[Var],
        class: Var,
        row: Option<Scalar>,
        label: Option<Scalar>,
    ) -> rel::Atom {
        // Every binary atom may read a row swapped: which labels it reads
        // decides which of them commute.
        let swap = (args.len() == 2).then(|| rel::Swap {
            columns: [1, 2],
            when: self.param(ParamSpec::Commuting(labels)),
        });
        let mut terms: SmallVec<[rel::Term; 6]> = SmallVec::new();
        terms.push(rel::Term::Any);
        terms.extend(args.iter().map(|&arg| rel::Term::Var(arg)));
        terms.push(rel::Term::Var(class));
        rel::Atom::Rows(rel::Rows {
            select: rel::Select {
                column: 0,
                param: labels,
            },
            terms,
            tag: row,
            cell: label,
            swap,
            tag_within: None,
        })
    }

    fn atom(&mut self, atom: &Atom<L>) -> rel::Atom {
        match atom {
            Atom::Node {
                template,
                args,
                class,
                row,
            } => {
                let labels = self.param(ParamSpec::Template {
                    template: template.clone(),
                    arity: args.len(),
                    op: template.template_key(),
                });
                self.rows(labels, args, *class, *row, None)
            }
            Atom::Any {
                accept,
                args,
                class,
                row,
                label,
            } => {
                let labels = self.param(ParamSpec::Any {
                    arity: args.len(),
                    accept: *accept,
                });
                self.rows(labels, args, *class, *row, *label)
            }
            // A carrier's constant is a reference, which the host tests; any
            // other is what the constant column holds.
            Atom::Literal { value, class } if leaf_constant(value).is_some() => {
                self.literals.push(value.clone());
                let call = self.literals.len() as ExternId - 1;
                lattice_atom(ColumnId::Const, *class, Out::Host(call))
            }
            Atom::Literal { value, class } => {
                let op = value.op_key();
                let value = value.clone();
                let param = self.param(ParamSpec::Equal { value, op });
                lattice_atom(ColumnId::Const, *class, Out::In(param))
            }
            Atom::Fact { column, key, value } => lattice_atom(*column, *key, Out::Word(*value)),
            Atom::Offset { key, base, offset } => place(
                *key,
                rel::Read::Offset {
                    base: *base,
                    offset: *offset,
                },
            ),
            Atom::Const { key, value } => place(*key, rel::Read::Pinned { offset: *value }),
            Atom::Width { key, width } => place(*key, rel::Read::Width { width: *width }),
            Atom::Conflicted { key } => place(*key, rel::Read::Conflicted),
            Atom::Table { table, terms } => rel::Atom::Words(rel::Tuples {
                table: self.param(ParamSpec::Table(*table)),
                terms: terms.clone(),
            }),
        }
    }

    fn guard(&mut self, guard: &Guard) -> rel::Guard {
        match guard {
            Guard::Cmp(cmp, a, b) => rel::Guard::Cmp(*cmp, a.clone(), b.clone()),
            Guard::Let { out, value } => rel::Guard::Let {
                out: *out,
                value: value.clone(),
            },
            Guard::Distinct(pairs) => rel::Guard::Distinct(pairs.clone()),
            // One extern with an output, as a read was: never held back.
            Guard::Read { term, field, out } => {
                self.calls.push(Call::Read {
                    kind: term.kind(),
                    field: *field,
                });
                rel::Guard::Extern {
                    call: self.calls.len() as ExternId - 1,
                    args: SmallVec::from_elem(Expr::Scalar(term.slot()), 1),
                    out: SmallVec::from_slice(&[*out]),
                }
            }
            Guard::Extern {
                call,
                terms,
                args,
                out,
            } => {
                self.calls.push(Call::User {
                    call: *call,
                    kinds: terms.iter().map(|term| term.kind()).collect(),
                });
                let words = terms.iter().map(|term| Expr::Scalar(term.slot()));
                rel::Guard::Extern {
                    call: self.calls.len() as ExternId - 1,
                    args: words.chain(args.iter().cloned()).collect(),
                    out: out.clone(),
                }
            }
        }
    }
}

fn lattice_atom(column: ColumnId, key: Var, out: Out) -> rel::Atom {
    rel::Atom::Lattice(rel::Lattice {
        column: lattice(column),
        key,
        out,
    })
}

fn place(key: Var, read: rel::Read) -> rel::Atom {
    rel::Atom::Place(rel::Place { key, read })
}

impl<L: Label> Plan<L> {
    /// Order `query`'s atoms — the root's atom first, then whatever its operands
    /// bound, in the order the query was written — with each guard placed at the
    /// first point its inputs are all bound.
    ///
    /// Panics if an atom's class variable is unreachable from the root, or a
    /// guard reads a scalar nothing binds.
    pub fn compile(query: Query<L>) -> Self {
        let mut lower = Lower {
            params: Vec::new(),
            calls: Vec::new(),
            literals: Vec::new(),
        };
        let atoms = query.atoms.iter().map(|atom| lower.atom(atom)).collect();
        let guards = query
            .guards
            .iter()
            .map(|guard| lower.guard(guard))
            .collect();
        let nots = query
            .nots
            .iter()
            .map(|nested| rel::Nested {
                atoms: nested.atoms.iter().map(|atom| lower.atom(atom)).collect(),
                guards: nested
                    .guards
                    .iter()
                    .map(|guard| lower.guard(guard))
                    .collect(),
            })
            .collect();
        let root_op = match query
            .atoms
            .iter()
            .find(|atom| atom.class() == Some(query.root))
        {
            Some(Atom::Node { template, .. }) => template.template_key(),
            _ => None,
        };
        let plan = rel::Plan::compile(rel::Query {
            space: CLASSES,
            vars: query.vars,
            scalars: query.scalars,
            params: lower.params.len() as u32,
            root: query.root,
            atoms,
            guards,
            nots,
        });
        Self {
            query,
            plan,
            params: lower.params,
            calls: lower.calls,
            literals: lower.literals,
            root_op,
        }
    }

    /// The query as written.
    pub fn query(&self) -> &Query<L> {
        &self.query
    }

    pub(crate) fn generic(&self) -> &rel::Plan {
        &self.plan
    }

    /// The atoms and guards in evaluation order; an atom or guard is named by
    /// its index in the query as written.
    pub fn steps(&self) -> &[rel::Step] {
        self.plan.steps()
    }

    /// See [`rel::Plan::unbounded`].
    pub fn unbounded(&self) -> bool {
        self.plan.unbounded()
    }

    /// The classes the root atom can match at: those holding its operator, or
    /// every class when the root binds no row.
    pub fn roots(&self, eg: &Engine<L>) -> Vec<ClassId> {
        match self.root_op {
            Some(op) => eg.classes_with_op(op),
            None => eg.class_ids().collect(),
        }
    }

    pub(crate) fn root_op(&self) -> Option<u64> {
        self.root_op
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
        let host = Lookup {
            eg,
            plan: self,
            externs,
        };
        let mut caches = eg.caches.lock().expect("the plan caches");
        let Some(cache) = caches.plan(&self.plan, &eg.db, &host) else {
            return Vec::new();
        };
        let allowed = |var, class| allowed(var, ClassId(class));
        let roots = roots.into_iter().map(|root| root.0);
        let fresh = (only_new, 0);
        let found = self
            .plan
            .search_roots(&eg.db, &host, roots, Some(&allowed), fresh, cache);
        let matches = (0..found.len())
            .map(|index| {
                let (root, bindings, scalars) = found.get(index);
                Match {
                    root: ClassId(root),
                    bindings: bindings.iter().map(|&binding| binding.into()).collect(),
                    scalars: SmallVec::from_slice(scalars),
                }
            })
            .collect();
        cache.recycle(found);
        matches
    }

    pub(crate) fn resolve(
        &self,
        eg: &Engine<L>,
        param: Param,
        since: Option<u64>,
        out: &mut Resolution,
    ) {
        let since = since.map(|since| since as usize);
        let rows =
            |labels: Param, commuting, out: &mut Resolution| match &self.params[labels as usize] {
                ParamSpec::Template {
                    template,
                    arity,
                    op,
                } => {
                    let matches = |node: &L| template.matches_template(node);
                    eg.labels_matching(*op, *arity, matches, commuting, since, out)
                }
                ParamSpec::Any { arity, accept } => {
                    eg.labels_matching(None, *arity, accept, commuting, since, out)
                }
                _ => unreachable!("commutativity is of a row atom's labels"),
            };
        match &self.params[param as usize] {
            ParamSpec::Template { .. } | ParamSpec::Any { .. } => rows(param, false, out),
            ParamSpec::Commuting(labels) => rows(*labels, true, out),
            ParamSpec::Equal { value, op } => eg.labels_equal(*op, value, since, out),
            ParamSpec::Table(slot) if since.is_none() => out.tables.push((*slot, SmallVec::new())),
            ParamSpec::Table(_) => {}
        }
    }

    pub(crate) fn call(
        &self,
        eg: &Terms<'_, L>,
        externs: &dyn Externs<L>,
        id: ExternId,
        args: &[u64],
        out: &mut [u64],
    ) -> bool {
        let term = |kind: Kind, word: u64| match kind {
            Kind::Row => Some(eg.node(RowId(word as u32))),
            Kind::Label => eg.label_node(LabelId(word as u32)),
        };
        match &self.calls[id as usize] {
            Call::Read { kind, field } => {
                let read = |node: &L| node.scalar(eg.names, *field);
                let Some(value) = term(*kind, args[0]).and_then(read) else {
                    return false;
                };
                out[0] = value;
                true
            }
            Call::User { call, kinds } => {
                let Some(terms): Option<SmallVec<[&L; 2]>> = kinds
                    .iter()
                    .zip(args)
                    .map(|(&kind, &word)| term(kind, word))
                    .collect()
                else {
                    return false;
                };
                externs.call(*call, &terms, &args[kinds.len()..], out)
            }
        }
    }
}

/// A plan's host: the engine's labels and the user's externs.
pub(crate) struct Lookup<'a, L: Label> {
    pub(crate) eg: &'a Engine<L>,
    pub(crate) plan: &'a Plan<L>,
    pub(crate) externs: &'a dyn Externs<L>,
}

impl<L: Label> Calls for Lookup<'_, L> {
    fn call(&self, id: ExternId, args: &[u64], out: &mut [u64]) -> bool {
        self.plan
            .call(&self.eg.terms(), self.externs, id, args, out)
    }

    fn accepts(&self, id: ExternId, reference: tir_relational::Ref) -> bool {
        let value = &self.plan.literals[id as usize];
        self.eg.is_literal(reference.into(), value) == Some(true)
    }
}

impl<L: Label> Host for Lookup<'_, L> {
    /// Labels only accumulate, so what a parameter resolves to is fixed
    /// between two readings that agree on how many there are.
    fn version(&self) -> u64 {
        self.eg.labels_len() as u64
    }

    fn resolve(&self, param: Param, since: Option<u64>, out: &mut Resolution) {
        self.plan.resolve(self.eg, param, since, out)
    }

    /// A template whose operator the graph has never met matches nothing.
    fn empty(&self, param: Param) -> bool {
        match &self.plan.params[param as usize] {
            ParamSpec::Template { op: Some(op), .. } => !self.eg.holds_op(*op),
            _ => false,
        }
    }
}

pub(crate) struct SharedLookup<'a, L: Label> {
    pub(crate) terms: Terms<'a, L>,
    pub(crate) plan: &'a Plan<L>,
    pub(crate) externs: &'a (dyn Externs<L> + Sync),
}

impl<L: Label> Calls for SharedLookup<'_, L> {
    fn call(&self, id: ExternId, args: &[u64], out: &mut [u64]) -> bool {
        self.plan.call(&self.terms, self.externs, id, args, out)
    }

    fn accepts(&self, id: ExternId, reference: tir_relational::Ref) -> bool {
        let value = &self.plan.literals[id as usize];
        self.terms.is_literal(reference.into(), value) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Term;
    use proptest::prelude::*;
    use tir_relational::Step;

    fn node(template: Term, args: &[Var], class: Var) -> Atom<Term> {
        Atom::Node {
            template,
            args: args.iter().copied().collect(),
            class,
            row: None,
        }
    }

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
        assert_eq!(plan.generic().height(), 2);
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

    // Four unary labels in two tables, met by the pass in the order `met`
    // and then `later`, an earlier graph having searched in between, before
    // a graph of five rows over them is seeded, two of them of one class in
    // different tables: every match of "any unary node", first by visiting
    // the classes, then by scanning.
    fn any_unary(met: &[usize], later: &[usize]) -> [Vec<(u32, tir_relational::Ref)>; 2] {
        let labels = [
            Term::op("g", &[ClassId(0)]),
            Term::op("f", &[ClassId(0)]),
            Term::typed("neg", 8, &[ClassId(0)]),
            Term::typed("p", 8, &[ClassId(0)]),
        ];
        let plan = Plan::compile(Query::tree(
            2,
            0,
            vec![Atom::Any {
                accept: |_| true,
                args: smallvec::smallvec![1],
                class: 0,
                row: None,
                label: None,
            }],
        ));
        let mut vocabulary = crate::Vocabulary::default();
        for &at in met {
            vocabulary.intern(&labels[at]);
        }
        if !later.is_empty() {
            let mut earlier = Engine::over(vocabulary);
            let x = earlier.add(Term::leaf("x"));
            earlier.insert(labels[met[0]].clone(), &[x]);
            let found = plan.search(
                &earlier,
                plan.roots(&earlier),
                &|_, _| true,
                false,
                &NoExterns,
            );
            assert_eq!(found.len(), 1);
            vocabulary = earlier.release();
        }
        for &at in later {
            vocabulary.intern(&labels[at]);
        }
        let mut eg = Engine::over(vocabulary);
        let x = eg.add(Term::leaf("x"));
        let w = eg.add(Term::typed("w", 8, &[]));
        let rows = [(3, x), (0, x), (2, w), (1, x), (3, w)]
            .map(|(label, operand)| eg.insert(labels[label].clone(), &[operand]));
        eg.union(rows[2], rows[4]).unwrap();
        eg.rebuild();
        let visited = plan
            .search(&eg, plan.roots(&eg), &|_, _| true, false, &NoExterns)
            .iter()
            .map(|found| (found.root.0, found.bindings[1].into()))
            .collect();
        let host = Lookup {
            eg: &eg,
            plan: &plan,
            externs: &NoExterns,
        };
        let mut caches = eg.caches.lock().unwrap();
        let cache = caches.plan(plan.generic(), &eg.db, &host).unwrap();
        let found = plan.generic().search_all(&eg.db, &host, cache);
        let scanned = (0..found.len())
            .map(|at| (found.get(at).0, found.get(at).1[1]))
            .collect();
        [visited, scanned]
    }

    #[test]
    fn a_search_finds_matches_in_one_order_whatever_order_the_pass_met_the_labels_in() {
        let found = any_unary(&[0, 1, 2, 3], &[]);
        assert_eq!(found[0].len(), 5);
        assert_eq!(found[1].len(), 5);
        assert_eq!(any_unary(&[3, 2, 1, 0], &[]), found);
        assert_eq!(any_unary(&[2, 0, 3, 1], &[]), found);
        assert_eq!(any_unary(&[3, 0], &[2, 1]), found);
        assert_eq!(any_unary(&[2], &[3, 1, 0]), found);
    }

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
        let Some(class) = atom.class().map(|class| assignment[class as usize]) else {
            return true;
        };
        match atom {
            Atom::Literal { value, .. } => eg
                .const_of(class)
                .is_some_and(|known| value.matches(&known)),
            Atom::Fact { column, .. } => eg.fact(*column, class).is_some(),
            Atom::Offset { base, .. } => assignment[*base as usize] == class,
            Atom::Const { .. } => eg.int_const(class).is_some(),
            Atom::Width { .. } => eg.width(class).is_some(),
            Atom::Conflicted { .. } => eg.conflicted(class),
            Atom::Table { .. } => true,
            Atom::Any { .. } => unreachable!("no property queries one"),
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

    // A table atom is placed once the guard binding its key has run, and
    // each row the key names is a match, in the order the rows were added.
    #[test]
    fn a_table_atom_matches_once_per_row_of_its_key() {
        let mut eg = Engine::new();
        let three = eg.add(Term::int(3)).class;
        let y = eg.add(Term::leaf("y")).class;
        let f = eg.add(Term::op("f", &[three, y])).class;
        eg.rebuild();
        let mut table = tir_relational::Words::new(2);
        for row in [[3, 7], [4, 8], [3, 9]] {
            table.push(&row);
        }
        let table = eg.add_words(std::sync::Arc::new(table));

        let plan = Plan::compile(Query {
            vars: 3,
            scalars: 3,
            root: 0,
            atoms: vec![
                Atom::Table {
                    table,
                    terms: vec![Expr::Scalar(1), Expr::Scalar(2)],
                },
                node(Term::op("f", &[ClassId(0), ClassId(0)]), &[1, 2], 0),
                Atom::Fact {
                    column: ColumnId::Const,
                    key: 1,
                    value: 0,
                },
            ],
            guards: vec![Guard::Read {
                term: Source::Label(0),
                field: 0,
                out: 1,
            }],
            nots: Vec::new(),
        });
        let table = Step::Table { atom: 0, keys: 1 };
        assert_eq!(
            plan.steps(),
            &[Step::Atom(1), Step::Atom(2), Step::Guard(0), table]
        );
        let found = plan.search(&eg, [f], &|_, _| true, false, &NoExterns);
        let values: Vec<u64> = found.iter().map(|found| found.scalars[2]).collect();
        assert_eq!(values, [7, 9]);
    }

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

    // `g(?1)`, `?1 == ?2 + d`, `st(?3)` in `?2`: the operand's base holds the
    // loop over itself, whichever offset the operand names the base at.
    #[test]
    fn a_self_loop_binds_its_operand_to_the_class_it_is_read_in() {
        let (mut eg, x, y) = typed_graph();
        let st = Term::typed("st", 32, &[ClassId(0)]);
        for class in [x, y] {
            let seed = eg.insert(st.clone(), &[class]);
            eg.union(seed, class).unwrap();
        }
        let near = g_of(&mut eg, x);
        let far = g_of(&mut eg, y);
        eg.union(y, Ref::new(x.class, 5)).unwrap();
        eg.rebuild();
        let plan = Plan::compile(Query {
            vars: 4,
            scalars: 1,
            root: 0,
            atoms: vec![
                node(Term::typed("g", 32, &[ClassId(0)]), &[1], 0),
                Atom::Offset {
                    key: 1,
                    base: 2,
                    offset: 0,
                },
                node(st, &[3], 2),
            ],
            guards: Vec::new(),
            nots: Vec::new(),
        });
        let found = plan.search(
            &eg,
            [near.class, far.class],
            &|_, _| true,
            false,
            &NoExterns,
        );
        let got: Vec<(ClassId, Option<Ref>, Option<Ref>, u64)> = found
            .iter()
            .map(|m| (m.root, m.binding(2), m.binding(3), m.scalars[0]))
            .collect();
        assert_eq!(
            got,
            vec![
                (near.class, Some(x), Some(x), 0),
                (far.class, Some(x), Some(x), 5)
            ]
        );
    }

    fn not_st(term: &Term) -> bool {
        term.op != "st"
    }

    // `?(?1)` over every unary label but `st`, each priced by a table keyed
    // by label and built before the graph had a row.
    #[test]
    fn any_label_of_an_arity_binds_its_id_for_a_client_table() {
        let mut eg = Engine::new();
        let g = Term::typed("g", 32, &[ClassId(0)]);
        let k = Term::op("k", &[ClassId(0)]);
        let mut prices = tir_relational::Words::new(2);
        prices.push(&[eg.intern(&g).0, 10]);
        prices.push(&[eg.intern(&k).0, 20]);
        let m = Term::op("m", &[ClassId(0)]);
        prices.push(&[eg.intern(&m).0, 30]);
        let table = eg.add_words(std::sync::Arc::new(prices));
        let x = eg.add(Term::typed("x", 32, &[]));
        let on_g = eg.insert(g, &[x]);
        let on_k = eg.insert(k, &[x]);
        eg.insert(Term::op("st", &[ClassId(0)]), &[x]);
        eg.insert(Term::op("k", &[ClassId(0), ClassId(0)]), &[x, x]);
        eg.rebuild();
        let plan = Plan::compile(Query {
            vars: 2,
            scalars: 3,
            root: 0,
            atoms: vec![
                Atom::Any {
                    accept: not_st,
                    args: SmallVec::from_slice(&[1]),
                    class: 0,
                    row: None,
                    label: Some(1),
                },
                Atom::Table {
                    table,
                    terms: vec![Expr::Scalar(1), Expr::Scalar(2)],
                },
            ],
            guards: Vec::new(),
            nots: Vec::new(),
        });
        let priced = |eg: &Engine<Term>| -> Vec<(ClassId, u64)> {
            let roots: Vec<ClassId> = eg.class_ids().collect();
            let found = plan.search(eg, roots, &|_, _| true, false, &NoExterns);
            found.iter().map(|m| (m.root, m.scalars[2])).collect()
        };
        assert_eq!(priced(&eg), vec![(on_g.class, 10), (on_k.class, 20)]);
        // A label the graph meets after the plan first ran is read as well.
        let on_m = eg.insert(m, &[x]);
        eg.rebuild();
        assert_eq!(
            priced(&eg),
            vec![(on_g.class, 10), (on_k.class, 20), (on_m.class, 30)]
        );
    }

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
