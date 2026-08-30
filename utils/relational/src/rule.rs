//! A rule as data: a query, and a head that only writes.
//!
//! Nothing in a head reads the graph and nothing outside an atom reads it
//! either, so a match exists in round `t` and not `t-1` exactly when one of its
//! rows or facts is from round `t-1`'s delta. That is what a hand-asserted
//! "this applier reads no deeper than its pattern" used to promise and nothing
//! checked.

use smallvec::SmallVec;

use crate::query::{Expr, Field, Match, Plan, Scalar, Var};
use crate::{ClassId, Engine, Label};

/// A node a head builds: a template with scalars written into named fields.
#[derive(Clone, Debug)]
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

/// One write of a rule's right-hand side.
#[derive(Clone, Debug)]
pub enum HeadOp<L> {
    /// Hash-cons `label(args)` and bind its class to `into`.
    Insert {
        label: LabelFill<L>,
        args: SmallVec<[Var; 4]>,
        into: Var,
    },
    Union(Var, Var),
    /// Record that `key` is `base` plus `offset`.
    RaiseObject {
        key: Var,
        base: Var,
        offset: Expr,
    },
    /// Union `class` with the variable `offset + scalars[index]` — the one head
    /// that chooses among bound variables, for a rule that picks an operand by
    /// a guard's answer (which arm of a decided gate).
    UnionIndexed {
        class: Var,
        offset: Var,
        index: Scalar,
    },
}

/// A rule: what to look for, and what it proves.
#[derive(Clone, Debug)]
pub struct Rule<L> {
    pub name: String,
    pub plan: Plan<L>,
    pub head: Vec<HeadOp<L>>,
    /// Applied once after the iterative fixpoint rather than in it, for a law
    /// whose right-hand side would feed back through the others.
    pub post_saturation: bool,
}

impl<L: Label> Engine<L> {
    /// Run `head` on one match. A head that cannot be spelled — a fill the
    /// language has no term for — writes nothing; everything before it stands,
    /// which is sound because a head only ever adds.
    pub fn apply_head(&mut self, head: &[HeadOp<L>], matched: &Match) {
        let mut bound: SmallVec<[Option<ClassId>; 8]> = matched.bindings.clone();
        for op in head {
            match op {
                HeadOp::Insert { label, args, into } => {
                    let fills: SmallVec<[(Field, u64); 2]> = label
                        .fills
                        .iter()
                        .map(|&(field, slot)| (field, matched.scalars[slot as usize]))
                        .collect();
                    let Some(mut node) = L::fill(&label.template, &fills) else {
                        return;
                    };
                    debug_assert_eq!(node.children().len(), args.len());
                    for (slot, &arg) in node.children_mut().iter_mut().zip(args) {
                        let Some(class) = bound[arg as usize] else {
                            return;
                        };
                        *slot = class;
                    }
                    let class = self.add(node);
                    bound[*into as usize] = Some(class);
                }
                HeadOp::Union(a, b) => {
                    let (Some(a), Some(b)) = (bound[*a as usize], bound[*b as usize]) else {
                        return;
                    };
                    self.union(a, b);
                }
                HeadOp::RaiseObject { key, base, offset } => {
                    let (Some(key), Some(base), Some(offset)) = (
                        bound[*key as usize],
                        bound[*base as usize],
                        offset.eval(&matched.scalars),
                    ) else {
                        return;
                    };
                    self.raise_object(key, base, offset);
                }
                HeadOp::UnionIndexed {
                    class,
                    offset,
                    index,
                } => {
                    let chosen = *offset as usize + matched.scalars[*index as usize] as usize;
                    let (Some(a), Some(Some(b))) = (bound[*class as usize], bound.get(chosen))
                    else {
                        return;
                    };
                    self.union(a, *b);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Atom, NoExterns, Query};
    use crate::testing::Term;

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
            post_saturation: false,
        }
    }

    /// `add(p, k)` with `k` a literal is `p` plus `k` — a rule whose head raises
    /// the column its own atoms read, so a chain of them reaches a fixpoint.
    fn derivation() -> Rule<Term> {
        Rule {
            name: "derivation".into(),
            plan: Plan::compile(Query {
                vars: 4,
                scalars: 3,
                root: 0,
                atoms: vec![
                    Atom::Node {
                        template: Term::op("add", &[ClassId(0), ClassId(0)]),
                        args: SmallVec::from_slice(&[1, 2]),
                        class: 0,
                        row: None,
                    },
                    Atom::Fact {
                        column: crate::ColumnId::Const,
                        key: 2,
                        value: 0,
                    },
                    Atom::Object {
                        key: 1,
                        base: 3,
                        offset: 1,
                    },
                ],
                guards: vec![crate::Guard::Read {
                    term: crate::Source::Label(0),
                    field: 0,
                    out: 2,
                }],
                nots: Vec::new(),
            }),
            head: vec![HeadOp::RaiseObject {
                key: 0,
                base: 3,
                offset: crate::Expr::Add(
                    Box::new(crate::Expr::Scalar(1)),
                    Box::new(crate::Expr::Scalar(2)),
                ),
            }],
            post_saturation: false,
        }
    }

    #[test]
    fn a_chain_of_derivations_reaches_one_base_and_one_offset() {
        let mut eg = Engine::new();
        let p = eg.add(Term::leaf("p"));
        let four = eg.add(Term::int(4));
        let first = eg.add(Term::op("add", &[p, four]));
        let second = eg.add(Term::op("add", &[first, four]));
        eg.rebuild();

        let rule = derivation();
        for _ in 0..2 {
            let found = rule.plan.search(
                &eg,
                eg.class_ids().collect::<Vec<_>>(),
                &|_, _| true,
                false,
                &NoExterns,
            );
            for matched in &found {
                eg.apply_head(&rule.head, matched);
            }
        }
        assert_eq!(eg.object_of(p), Some((p, 0)));
        assert_eq!(eg.object_of(first), Some((p, 4)));
        assert_eq!(eg.object_of(second), Some((p, 8)));
    }

    /// A chain placed before its own base was placed says the same thing once
    /// the base is: what the column joins is where the two land, not the step
    /// each was written with.
    #[test]
    fn a_derivation_written_early_agrees_with_the_one_written_late() {
        let mut eg = Engine::new();
        let p = eg.add(Term::leaf("p"));
        let first = eg.add(Term::leaf("q"));
        let second = eg.add(Term::leaf("r"));
        eg.rebuild();

        eg.raise_object(second, first, 4);
        eg.raise_object(first, p, 4);
        eg.raise_object(second, p, 8);
        assert_eq!(eg.object_of(second), Some((p, 8)));
    }

    /// The same fact raised again after its base was absorbed is the same fact.
    #[test]
    fn a_derivation_survives_its_base_being_merged_away() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let x = eg.add(Term::leaf("x"));
        eg.rebuild();

        eg.raise_object(x, b, 4);
        let survivor = eg.union(a, b);
        eg.rebuild();
        eg.raise_object(x, survivor, 4);
        assert_eq!(eg.object_of(x), Some((survivor, 4)));
    }

    #[test]
    fn two_derivations_that_disagree_leave_the_class_unplaceable() {
        let mut eg = Engine::new();
        let p = eg.add(Term::leaf("p"));
        let q = eg.add(Term::leaf("q"));
        eg.rebuild();
        assert!(eg.raise_object(p, q, 4));
        assert!(eg.raise_object(p, q, 8));
        assert_eq!(eg.object_of(p), None);
    }

    #[test]
    fn a_head_inserts_and_unions() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a"));
        let b = eg.add(Term::leaf("b"));
        let f = eg.add(Term::op("f", &[a, b]));
        eg.rebuild();

        let rule = rule();
        let found = rule.plan.search(&eg, [f], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        for matched in &found {
            eg.apply_head(&rule.head, matched);
        }
        eg.rebuild();

        let g = eg.add(Term::op("g", &[a]));
        assert!(eg.connected(f, g));
    }
}
