//! A rule as data: a query, and a head that only writes.
//!
//! Nothing in a head reads the graph and nothing outside an atom reads it
//! either, so a match exists in round `t` and not `t-1` exactly when one of its
//! rows or facts is from round `t-1`'s delta. That is what a hand-asserted
//! "this applier reads no deeper than its pattern" used to promise and nothing
//! checked.

use smallvec::SmallVec;

use tir_relational::Database;
use tir_relational::query::{Expr, Matches, Scalar, Var};
use tir_relational::rule::{self as rel, Client, Head, Key, Keyed};

use crate::engine::{CLASSES, Lists, Shaped, lattice};
use crate::query::{Externs, Field, Match, NoExterns, Plan};
use crate::{ClassId, ColumnId, Engine, Label, LabelId, Ref};

/// A node a head builds: a template with scalars written into named fields.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelFill<L> {
    pub template: L,
    pub fills: SmallVec<[(Field, Scalar); 2]>,
}

impl<L> LabelFill<L> {
    pub fn plain(template: L) -> Self {
        Self {
            template,
            fills: SmallVec::new(),
        }
    }
}

/// One write of a rule's right-hand side. Variables hold references.
#[derive(Clone, Debug, PartialEq)]
pub enum HeadOp<L> {
    /// Hash-cons `label(args)` and bind its value to `into`.
    Insert {
        label: LabelFill<L>,
        args: SmallVec<[Var; 4]>,
        into: Var,
    },
    Union(Var, Var),
    /// Bind `into` to `base` plus `offset`.
    Offset {
        base: Var,
        offset: Expr,
        into: Var,
    },
    /// Union `class` with the variable `offset + scalars[index]` — the one head
    /// that chooses among bound variables, for a rule that picks an operand by
    /// a guard's answer (which arm of a decided gate).
    UnionIndexed {
        class: Var,
        offset: Var,
        index: Scalar,
    },
    /// Say `value` of `key`'s class in `column`, joined onto what the column
    /// holds for it: how an analysis of classes is a rule like any other.
    Raise {
        column: ColumnId,
        key: Var,
        value: Expr,
    },
}

/// A rule: what to look for, and what it proves.
#[derive(Clone, Debug)]
pub struct Rule<L> {
    pub name: String,
    pub plan: Plan<L>,
    pub head: Vec<HeadOp<L>>,
    /// Variables the head binds, numbered above the query's. A match carries
    /// only what it matched; a head's own classes exist for as long as it runs.
    pub head_vars: u32,
    /// Applied once after the iterative fixpoint rather than in it, for a law
    /// whose right-hand side would feed back through the others.
    pub post_saturation: bool,
}

/// `head` as generic writes, appended to `out`. The operands of a commutative
/// operator are sorted once canonical: both of a binary one as written, all
/// of a filled one.
pub(crate) fn compile<L: Label>(head: &[HeadOp<L>], out: &mut Vec<Head>) {
    out.extend(head.iter().map(|op| match op {
        HeadOp::Insert { label, args, into } => {
            let template = &label.template;
            debug_assert_eq!(template.children().len(), args.len());
            Head::Insert {
                args: args.clone(),
                sorted: template.commutative() && (!label.fills.is_empty() || args.len() == 2),
                into: *into,
            }
        }
        HeadOp::Union(a, b) => Head::Equate(*a, *b),
        HeadOp::Offset { base, offset, into } => Head::Offset {
            base: *base,
            offset: offset.clone(),
            into: *into,
        },
        HeadOp::UnionIndexed {
            class,
            offset,
            index,
        } => Head::EquateIndexed {
            var: *class,
            offset: *offset,
            index: *index,
        },
        HeadOp::Raise { column, key, value } => Head::Raise {
            column: lattice(*column),
            key: *key,
            value: value.clone(),
        },
    }));
}

pub(crate) type Cached = Option<(SmallVec<[(Field, u64); 2]>, LabelId)>;

/// The fills of `label` with the words a match bound.
fn fills<L>(label: &LabelFill<L>, scalars: &[u64]) -> SmallVec<[(Field, u64); 2]> {
    let fills = label.fills.iter();
    fills
        .map(|&(field, slot)| (field, scalars[slot as usize]))
        .collect()
}

fn label_fill<L>(op: &HeadOp<L>) -> &LabelFill<L> {
    match op {
        HeadOp::Insert { label, .. } => label,
        _ => unreachable!("only an insert has a key the client fills"),
    }
}

impl<L: Label> Engine<L> {
    /// Run `head` on `matches`, which one search found, write by write. A
    /// head that cannot be spelled for a match — a fill the language has no
    /// term for — writes no more for it; everything before it stands, which
    /// is sound because a head only ever adds.
    pub fn apply_head(&mut self, head: &[HeadOp<L>], head_vars: u32, found: &[Match]) {
        let mut compiled = Vec::new();
        compile(head, &mut compiled);
        let mut matches = Matches::default();
        let mut bindings: SmallVec<[tir_relational::Ref; 8]> = SmallVec::new();
        for matched in found {
            bindings.clear();
            bindings.extend(matched.bindings.iter().map(|&binding| binding.into()));
            matches.push(matched.root.0, &bindings, &matched.scalars);
        }
        let mut run = Run {
            eg: self,
            rules: &[],
            externs: &NoExterns,
            starts: vec![0],
            ops: head.iter().map(|op| (op, None)).collect(),
        };
        let mut batch = rel::Batch::default();
        rel::apply(
            &mut run, CLASSES, 0, &compiled, head_vars, &matches, &mut batch,
        );
    }

    /// What insert `op` over `operands`, which are canonical, comes to before
    /// a table is probed. A label as written is remembered in `cached`, so a
    /// node the graph already holds costs a key lookup and no term is built
    /// for it; a filled one is spelled from the match's scalars and remembered
    /// until a match fills it differently, `None` when the language cannot
    /// spell it. A carrier's integer constant is a reference and interns
    /// nothing.
    #[inline]
    pub(crate) fn head_key(
        &mut self,
        op: &HeadOp<L>,
        cached: &mut Cached,
        operands: &[tir_relational::Ref],
        scalars: &[u64],
        key: &mut Key,
    ) -> Option<Keyed> {
        let label = label_fill(op);
        let template = &label.template;
        let id = if label.fills.is_empty() {
            let id = match cached {
                Some((_, id)) => *id,
                None => cached.insert((SmallVec::new(), self.intern(template))).1,
            };
            if self.is_int_constant(id)
                && let Some(found) = self.int_constant_ref(template)
            {
                return Some(Keyed::Is(found.into()));
            }
            id
        } else {
            // Matches come grouped by what they fill the label with, so the
            // last fill is most often the next one.
            let words = fills(label, scalars);
            match cached {
                Some((filled, id)) if *filled == words => *id,
                _ => {
                    let node = L::fill(template, self.names(), &words)?;
                    debug_assert_eq!(node.commutative(), template.commutative());
                    if let Some(found) = self.int_constant_ref(&node) {
                        return Some(Keyed::Is(found.into()));
                    }
                    cached.insert((words, self.intern(&node))).1
                }
            }
        };
        let operands: SmallVec<[Ref; 4]> = operands.iter().map(|&o| o.into()).collect();
        Some(match self.shape(id, &operands, key) {
            Shaped::Ref(found) => Keyed::Is(found.into()),
            Shaped::Key { shift, .. } => Keyed::At {
                table: self.table_slot(id),
                shift,
            },
        })
    }

    /// Mint the class of insert `op`, whose row is stored under `key`: its
    /// label and then its operands. The node is the head's own spelling with
    /// those children, not the label's stored node, which is bare.
    pub(crate) fn head_mint(&mut self, op: &HeadOp<L>, key: &Key, scalars: &[u64]) -> u32 {
        let label = label_fill(op);
        let mut node = match label.fills.is_empty() {
            true => label.template.clone(),
            false => {
                let fills = fills(label, scalars);
                L::fill(&label.template, self.names(), &fills).expect("spelled for its label")
            }
        };
        for (child, &class) in node.children_mut().iter_mut().zip(&key.cells[1..]) {
            *child = ClassId(class);
        }
        self.list_class(node, LabelId(key.cells[0]), &key.cells, key.weights())
            .0
    }
}

/// The e-graph running the heads of a rule set.
pub(super) struct Run<'a, L: Label> {
    pub(super) eg: &'a mut Engine<L>,
    pub(super) rules: &'a [&'a Rule<L>],
    pub(super) externs: &'a dyn Externs<L>,
    /// Per rule, where its head ops start in `ops`.
    pub(super) starts: Vec<usize>,
    /// Every head op of the rule set, and the label it last interned.
    pub(super) ops: Vec<(&'a HeadOp<L>, Cached)>,
}

impl<L: Label> Client<Lists, 1> for Run<'_, L> {
    fn db(&self) -> &Database<Lists, 1> {
        &self.eg.db
    }

    fn db_mut(&mut self) -> &mut Database<Lists, 1> {
        &mut self.eg.db
    }

    #[inline]
    fn key(
        &mut self,
        rule: usize,
        op: usize,
        operands: &[tir_relational::Ref],
        scalars: &[u64],
        key: &mut Key,
    ) -> Option<Keyed> {
        let (op, cached) = &mut self.ops[self.starts[rule] + op];
        self.eg.head_key(op, cached, operands, scalars, key)
    }

    fn tag(&self) -> u32 {
        self.eg.total_size() as u32
    }

    fn mint(&mut self, rule: usize, op: usize, _table: u32, key: &Key, scalars: &[u64]) -> u32 {
        let (op, _) = self.ops[self.starts[rule] + op];
        self.eg.head_mint(op, key, scalars)
    }

    fn equate(&mut self, a: tir_relational::Ref, b: tir_relational::Ref) {
        // A contradiction is recorded by the engine.
        let _ = self.eg.union(Ref::from(a), Ref::from(b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Term;
    use crate::{Atom, NoExterns, Query};

    /// `f(?1, ?2)` proves `f(?1, ?2) = g(?1)`.
    fn rule() -> Rule<Term> {
        Rule {
            name: "f-to-g".into(),
            plan: Plan::compile(Query::tree(
                4,
                0,
                vec![Atom::Node {
                    template: Term::op("f", &[ClassId(0), ClassId(0)]),
                    args: SmallVec::from_slice(&[1, 2]),
                    class: 0,
                    row: None,
                }],
            )),
            head: vec![
                HeadOp::Insert {
                    label: LabelFill::plain(Term::op("g", &[ClassId(0)])),
                    args: SmallVec::from_slice(&[1]),
                    into: 3,
                },
                HeadOp::Union(0, 3),
            ],
            head_vars: 0,
            post_saturation: false,
        }
    }

    /// A caller may union and saturate without a rebuild in between: the first
    /// round still matches through the class the union absorbed.
    #[test]
    fn saturation_matches_through_a_union_nobody_rebuilt_for() {
        let mut eg = Engine::new();
        // The older class survives a union, so the `f` row names the absorbed one.
        let older = eg.add(Term::leaf("older")).class;
        let x = eg.add(Term::leaf("x")).class;
        let y = eg.add(Term::leaf("y")).class;
        let g = eg.add(Term::op("g", &[x])).class;
        let f = eg.add(Term::op("f", &[g, y])).class;
        eg.rebuild();
        eg.union(older, g).unwrap();

        // `f(g(?3), ?2)` proves `f(g(?3), ?2) = ?2`.
        let atom = |op: &str, children: usize, args: &[u32], class| Atom::Node {
            template: Term::op(op, &vec![ClassId(0); children]),
            args: SmallVec::from_slice(args),
            class,
            row: None,
        };
        let rule = Rule {
            name: "f-of-g".into(),
            plan: Plan::compile(Query::tree(
                4,
                0,
                vec![atom("g", 1, &[3], 1), atom("f", 2, &[1, 2], 0)],
            )),
            head: vec![HeadOp::Union(0, 2)],
            head_vars: 0,
            post_saturation: false,
        };
        eg.saturate_rules(&[rule], &NoExterns, 1, usize::MAX);
        assert!(eg.connected(f, y));
    }

    #[test]
    fn a_vocabulary_serves_the_next_graph() {
        let rule = rule();
        let saturated = |eg: &mut Engine<Term>| {
            let a = eg.add(Term::leaf("a")).class;
            let b = eg.add(Term::leaf("b")).class;
            let f = eg.add(Term::op("f", &[a, b]));
            eg.saturate_rules(std::slice::from_ref(&rule), &NoExterns, 4, usize::MAX);
            let g = eg.lookup(&Term::op("g", &[a]));
            assert_eq!(g, Some(eg.find(f)));
            let row = eg.rows(f.class).next().expect("the seeded node");
            eg.label(row)
        };
        let mut first = Engine::new();
        let label = saturated(&mut first);
        let mut second = Engine::over(first.release());
        assert!(second.is_empty());
        second.add(Term::leaf("before"));
        assert_eq!(saturated(&mut second), label);
        let mut copy = Engine::over(second.release().clone());
        assert_eq!(saturated(&mut copy), label);
    }

    #[test]
    fn a_head_raises_a_fact_to_its_fixpoint() {
        let mut eg = Engine::new();
        let depth = eg.add_column(tir_relational::Join::Max);
        let a = eg.add(Term::leaf("a")).class;
        let once = eg.add(Term::op("g", &[a])).class;
        let twice = eg.add(Term::op("g", &[once])).class;
        let b = eg.add(Term::leaf("b")).class;
        let alone = eg.add(Term::op("g", &[b])).class;
        eg.raise_fact(depth, a, 0);
        eg.raise_fact(depth, b, 5);
        eg.raise_fact(depth, b, 3);
        let below = Expr::Scalar(0);
        let rule = Rule {
            name: "deeper".into(),
            plan: Plan::compile(Query {
                vars: 2,
                scalars: 1,
                root: 0,
                atoms: vec![
                    Atom::Node {
                        template: Term::op("g", &[ClassId(0)]),
                        args: SmallVec::from_slice(&[1]),
                        class: 0,
                        row: None,
                    },
                    Atom::Fact {
                        column: depth,
                        key: 1,
                        value: 0,
                    },
                ],
                guards: Vec::new(),
                nots: Vec::new(),
            }),
            head: vec![HeadOp::Raise {
                column: depth,
                key: 0,
                value: Expr::Add(Box::new(below), Box::new(Expr::Lit(1))),
            }],
            head_vars: 0,
            post_saturation: false,
        };
        eg.saturate_rules(&[rule], &NoExterns, usize::MAX, usize::MAX);
        let depths = [once, twice, alone].map(|class| eg.fact(depth, class));
        assert_eq!(depths, [Some(1), Some(2), Some(6)]);
        eg.union(twice, alone).unwrap();
        assert_eq!(eg.fact(depth, twice), Some(6));
    }

    #[test]
    fn a_head_inserts_and_unions() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a")).class;
        let b = eg.add(Term::leaf("b")).class;
        let f = eg.add(Term::op("f", &[a, b])).class;
        eg.rebuild();

        let rule = rule();
        let found = rule.plan.search(&eg, [f], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        eg.apply_head(&rule.head, rule.head_vars, &found);
        eg.rebuild();

        let g = eg.add(Term::op("g", &[a])).class;
        assert!(eg.connected(f, g));
    }

    fn insert(op: &str, args: &[u32], into: u32) -> HeadOp<Term> {
        HeadOp::Insert {
            label: LabelFill::plain(Term::op(op, &vec![ClassId(0); args.len()])),
            args: SmallVec::from_slice(args),
            into,
        }
    }

    // A rule over every `f(?1, ?2)`, with `head`.
    fn over_f(head: Vec<HeadOp<Term>>) -> Rule<Term> {
        Rule {
            head,
            head_vars: 2,
            ..rule()
        }
    }

    // What running `rule`'s head leaves of a graph `seed` builds, on all the
    // matches at once and on a match at a time: per term `seed` names, the
    // first one it is equal to, and how many classes there are.
    fn written(
        seed: impl Fn(&mut Engine<Term>) -> Vec<ClassId>,
        rule: &Rule<Term>,
        layered: bool,
    ) -> [(Vec<usize>, usize); 2] {
        [true, false].map(|at_once| {
            let mut eg = Engine::new();
            let terms = seed(&mut eg);
            eg.rebuild();
            if layered {
                eg.push_context();
            }
            let roots = rule.plan.roots(&eg);
            let found = rule
                .plan
                .search(&eg, roots, &|_, _| true, false, &NoExterns);
            assert!(found.len() > 1);
            match at_once {
                true => eg.apply_head(&rule.head, rule.head_vars, &found),
                false => found.iter().for_each(|matched| {
                    eg.apply_head(&rule.head, rule.head_vars, std::slice::from_ref(matched))
                }),
            }
            eg.rebuild();
            let first = |term: &ClassId| terms.iter().position(|other| eg.connected(*other, *term));
            let equal = terms.iter().map(|term| first(term).expect("itself"));
            (equal.collect(), eg.num_classes())
        })
    }

    // `f(a, b)`, `f(a, c)` and `f(b, c)` over three leaves, and `g(c)`.
    fn three_fs(eg: &mut Engine<Term>) -> Vec<ClassId> {
        let [a, b, c] = ["a", "b", "c"].map(|leaf| eg.add(Term::leaf(leaf)).class);
        let fs = [(a, b), (a, c), (b, c)].map(|(x, y)| eg.add(Term::op("f", &[x, y])).class);
        let g = eg.add(Term::op("g", &[c])).class;
        [a, b, c, g].into_iter().chain(fs).collect()
    }

    #[test]
    fn matches_inserting_one_key_mint_it_once() {
        // `f(a, b)` and `f(a, c)` both insert `g(a)`.
        let rule = rule();
        let [at_once, one_by_one] = written(three_fs, &rule, false);
        assert_eq!(at_once, one_by_one);
        assert_eq!(at_once.0[4], at_once.0[5]);
    }

    #[test]
    fn a_write_reads_what_an_earlier_write_minted() {
        let rule = over_f(vec![
            insert("g", &[2], 3),
            insert("h", &[3, 1], 4),
            HeadOp::Union(0, 4),
        ]);
        let [at_once, one_by_one] = written(three_fs, &rule, false);
        assert_eq!(at_once, one_by_one);
    }

    #[test]
    fn a_match_keyed_through_an_earlier_matchs_union_ends_up_congruent() {
        // One at a time `f(a, c)` finds the `g(a)` that `f(a, b)` equated
        // with `g(b)`; at once every `g` is minted before any is equated.
        let rule = over_f(vec![
            insert("g", &[1], 3),
            HeadOp::Union(1, 2),
            insert("h", &[2], 4),
            HeadOp::Union(0, 4),
        ]);
        let [at_once, one_by_one] = written(three_fs, &rule, false);
        assert_eq!(at_once, one_by_one);
    }

    #[test]
    fn a_layer_looks_a_batch_up_in_the_base_and_in_its_own_rows() {
        // `g(c)` is the base's; `g(a)` and `g(b)` become the layer's.
        let rule = over_f(vec![
            insert("g", &[2], 3),
            insert("g", &[1], 4),
            HeadOp::Union(1, 2),
            insert("h", &[4], 5),
            HeadOp::Union(0, 5),
        ]);
        let [at_once, one_by_one] = written(three_fs, &rule, true);
        assert_eq!(at_once, one_by_one);
    }
}
