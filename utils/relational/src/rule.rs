//! A rule as data: a query, and a head that only writes.
//!
//! Nothing in a head reads the graph and nothing outside an atom reads it
//! either, so a match exists in round `t` and not `t-1` exactly when one of its
//! rows or facts is from round `t-1`'s delta. That is what a hand-asserted
//! "this applier reads no deeper than its pattern" used to promise and nothing
//! checked.

use smallvec::SmallVec;

use crate::query::{Expr, Field, Match, Plan, Scalar, UNBOUND, Var};
use crate::{ClassId, Engine, Label, LabelId, Ref};

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

/// One write of a rule's right-hand side. Variables hold references.
#[derive(Clone, Debug)]
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
}

/// What a head's variables hold: the match's, then the head's own.
struct Bound<'a>(&'a mut [Ref]);

impl Bound<'_> {
    /// The reference `var` holds, if any.
    #[inline]
    fn get(&self, var: usize) -> Option<Ref> {
        crate::query::bound(*self.0.get(var)?)
    }

    #[inline]
    fn set(&mut self, var: Var, reference: Ref) {
        self.0[var as usize] = reference;
    }
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

impl<L: Label> Engine<L> {
    /// Run `head` on one match. A head that cannot be spelled — a fill the
    /// language has no term for — writes nothing; everything before it stands,
    /// which is sound because a head only ever adds.
    pub fn apply_head(&mut self, head: &[HeadOp<L>], head_vars: u32, matched: &Match) {
        let (bindings, scalars) = (&matched.bindings, &matched.scalars);
        self.apply_head_cached(head, head_vars, bindings, scalars, &mut Vec::new());
    }

    /// [`Self::apply_head`] for a head run many times: `labels` keeps the label
    /// of each insert whose node is the template as written, so a node the
    /// graph already holds costs a key lookup and no term is built for it.
    pub(crate) fn apply_head_cached(
        &mut self,
        head: &[HeadOp<L>],
        head_vars: u32,
        bindings: &[Ref],
        scalars: &[u64],
        labels: &mut Vec<Option<LabelId>>,
    ) {
        if labels.len() < head.len() {
            labels.resize(head.len(), None);
        }
        // The match's bindings and the head's own, in one array the engine
        // keeps between heads.
        let mut bound = std::mem::take(&mut self.head_bound);
        bound.clear();
        bound.extend_from_slice(bindings);
        bound.resize(bound.len() + head_vars as usize, UNBOUND);
        self.run_head(head, scalars, &mut Bound(&mut bound), labels);
        self.head_bound = bound;
    }

    fn run_head(
        &mut self,
        head: &[HeadOp<L>],
        scalars: &[u64],
        bound: &mut Bound<'_>,
        labels: &mut [Option<LabelId>],
    ) {
        for (index, op) in head.iter().enumerate() {
            match op {
                HeadOp::Insert { label, args, into } if label.fills.is_empty() => {
                    let template = &label.template;
                    debug_assert_eq!(template.children().len(), args.len());
                    // The cache is keyed by the rule's plan, and nothing stops
                    // two rules from sharing one, so a remembered label is
                    // checked against the template before it is trusted.
                    let id = match labels[index] {
                        Some(id) if self.label_is(id, template) => id,
                        _ => {
                            let id = self.intern(template);
                            labels[index] = Some(id);
                            id
                        }
                    };
                    let build = |cells: &[u32]| {
                        let mut node = template.clone();
                        for (child, &cell) in node.children_mut().iter_mut().zip(cells) {
                            *child = ClassId(cell);
                        }
                        node
                    };
                    if self.is_plain(id) {
                        // The key is the label and then the operand classes;
                        // all but the widest operators fit on the stack.
                        let mut inline = [0u32; 8];
                        let mut spilled = Vec::new();
                        let key = match inline.get_mut(..=args.len()) {
                            Some(key) => key,
                            None => {
                                spilled.resize(args.len() + 1, 0);
                                &mut spilled[..]
                            }
                        };
                        key[0] = id.0;
                        let mut at_zero = true;
                        for (cell, &arg) in key[1..].iter_mut().zip(args) {
                            let Some(operand) = bound.get(arg as usize) else {
                                return;
                            };
                            let operand = self.find(operand);
                            at_zero &= operand.offset == 0;
                            *cell = operand.class.0;
                        }
                        if at_zero {
                            if self.commutes(id) && key[2] < key[1] {
                                key.swap(1, 2);
                            }
                            let reference = self.insert_plain(id, key, build);
                            bound.set(*into, reference);
                            continue;
                        }
                    }
                    let reference = if self.is_int_constant(id) {
                        self.insert(template.clone(), &[])
                    } else {
                        let Some(mut operands) = args
                            .iter()
                            .map(|&arg| bound.get(arg as usize))
                            .collect::<Option<SmallVec<[Ref; 4]>>>()
                        else {
                            return;
                        };
                        let sort = self.commutes(id);
                        self.insert_labelled(id, &mut operands, sort, build)
                    };
                    bound.set(*into, reference);
                }
                HeadOp::Insert { label, args, into } => {
                    let Some(mut operands) = args
                        .iter()
                        .map(|&arg| bound.get(arg as usize))
                        .collect::<Option<SmallVec<[Ref; 4]>>>()
                    else {
                        return;
                    };
                    let fills: SmallVec<[(Field, u64); 2]> = label
                        .fills
                        .iter()
                        .map(|&(field, slot)| (field, scalars[slot as usize]))
                        .collect();
                    let Some(node) = L::fill(&label.template, &fills) else {
                        return;
                    };
                    debug_assert_eq!(node.children().len(), args.len());
                    if node.commutative() {
                        for operand in operands.iter_mut() {
                            *operand = self.find(*operand);
                        }
                        operands.sort_unstable();
                    }
                    let reference = self.insert(node, &operands);
                    bound.set(*into, reference);
                }
                HeadOp::Union(a, b) => {
                    let (Some(a), Some(b)) = (bound.get(*a as usize), bound.get(*b as usize))
                    else {
                        return;
                    };
                    // A contradiction is recorded by the engine.
                    let _ = self.union(a, b);
                }
                HeadOp::Offset { base, offset, into } => {
                    let (Some(base), Some(offset)) =
                        (bound.get(*base as usize), offset.eval(scalars))
                    else {
                        return;
                    };
                    let shifted = Ref::new(base.class, base.offset.wrapping_add(offset as u64));
                    bound.set(*into, self.find(shifted));
                }
                HeadOp::UnionIndexed {
                    class,
                    offset,
                    index,
                } => {
                    let chosen = *offset as usize + scalars[*index as usize] as usize;
                    let (Some(a), Some(b)) = (bound.get(*class as usize), bound.get(chosen)) else {
                        return;
                    };
                    let _ = self.union(a, b);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClassId;
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
    fn a_head_inserts_and_unions() {
        let mut eg = Engine::new();
        let a = eg.add(Term::leaf("a")).class;
        let b = eg.add(Term::leaf("b")).class;
        let f = eg.add(Term::op("f", &[a, b])).class;
        eg.rebuild();

        let rule = rule();
        let found = rule.plan.search(&eg, [f], &|_, _| true, false, &NoExterns);
        assert_eq!(found.len(), 1);
        for matched in &found {
            eg.apply_head(&rule.head, rule.head_vars, matched);
        }
        eg.rebuild();

        let g = eg.add(Term::op("g", &[a])).class;
        assert!(eg.connected(f, g));
    }
}
