// The rule trie of a ruleset (thesis §3.4): at each node, every variant
// offers its best-scored joins, and the join the most variants offer becomes
// a child, modulo variable renaming. A greedy set cover; the variants that
// did not take it repeat the choice. A variant whose premise is done adds
// its actions to the node; a lookup or primitive among its actions starts a
// chain of private children.

use super::Symbols;
use super::hir::{Act, Rule, Var};
use super::order::{Arg, Candidate, Join, State};
use super::permute::Symmetries;
use super::seminaive::Variant;
use crate::vm::code::{Action, Bind, Node, Reg, Slice, Step};
use crate::{Schema, TableKind};

pub(crate) struct Trie<'a> {
    pub(crate) schema: Schema<'a>,
    pub(crate) syms: &'a Symmetries,
    pub(crate) rules: &'a [Rule],
    pub(crate) variants: &'a [Variant],
    pub(crate) symbols: &'a mut Symbols,
    pub(crate) nodes: &'a mut Vec<Node>,
    // The register of the constant `()`, the operand of `Join::Always`.
    pub(crate) unit: Reg,
}

impl Trie<'_> {
    // The children of a node whose batch has `width` registers. Nodes are
    // stored after their children.
    pub(crate) fn children(&mut self, mut states: Vec<State>, width: u32) -> Vec<u32> {
        let mut children = Vec::new();
        while !states.is_empty() {
            let offers: Vec<Vec<Candidate>> = states
                .iter()
                .map(|state| {
                    let variant = &self.variants[state.variant];
                    let rule = &self.rules[variant.rule];
                    state.candidates(rule, &variant.slices, self.schema, self.syms)
                })
                .collect();
            let mut counts: Vec<(&Join, usize)> = Vec::new();
            for offer in &offers {
                for (i, candidate) in offer.iter().enumerate() {
                    if offer[..i].iter().any(|other| other.join == candidate.join) {
                        continue;
                    }
                    match counts.iter_mut().find(|(join, _)| **join == candidate.join) {
                        Some((_, count)) => *count += 1,
                        None => counts.push((&candidate.join, 1)),
                    }
                }
            }
            let most = counts.iter().map(|(_, count)| *count).max().unwrap_or(0);
            let best = counts
                .iter()
                .find(|(_, count)| *count == most)
                .map(|(join, _)| (*join).clone())
                .expect("a variant always offers a join");
            let (mut taken, mut rest) = (Vec::new(), Vec::new());
            for (state, offer) in states.into_iter().zip(offers) {
                match offer.into_iter().find(|candidate| candidate.join == best) {
                    Some(candidate) => taken.push((state, candidate)),
                    None => rest.push(state),
                }
            }
            children.push(self.node(&best, taken, width));
            states = rest;
        }
        children
    }

    fn node(&mut self, join: &Join, taken: Vec<(State, Candidate)>, width: u32) -> u32 {
        let (step, after) = self.step(join, width);
        let (mut done, mut open) = (Vec::new(), Vec::new());
        for (mut state, candidate) in taken {
            state.take(&candidate, width);
            if state.complete() {
                done.push(state);
            } else {
                open.push(state);
            }
        }
        let mut width = after;
        let (mut actions, mut rules, mut heads) = (Vec::new(), Vec::new(), Vec::new());
        for mut state in done {
            let rule = self.variants[state.variant].rule;
            rules.push(rule as u32);
            let next = self.actions(&mut state, 0, &mut actions, &mut width);
            heads.push((state, next));
        }
        let mut children = Vec::new();
        for (state, next) in heads {
            if next < self.rule(&state).actions.len() {
                children.push(self.head(state, next, width));
            }
        }
        children.extend(self.children(open, width));
        // A semi-join right before a join that implies it does nothing.
        if let [child] = children[..]
            && self.implied(&step, child)
        {
            return child;
        }
        self.push(Node {
            step,
            actions,
            children,
            rules,
        })
    }

    // A chain node for the lookup or primitive at `at` among the actions.
    fn head(&mut self, mut state: State, at: usize, width: u32) -> u32 {
        let rules = self.rules;
        let rule = &rules[self.variants[state.variant].rule];
        let (step, mut width) = match &rule.actions[at] {
            Act::Lookup(table, args) => {
                let (out, key) = args.split_last().expect("a lookup has an output");
                let key = regs(&state, key);
                let (out, width) = bind(&mut state, *out, width);
                let table = self.symbols.table(*table);
                let (out, slice) = (Some(out), Slice::Any);
                (
                    Step::Lookup {
                        table,
                        key,
                        out,
                        slice,
                    },
                    width,
                )
            }
            Act::Prim(prim, args) => {
                let (out, inputs) = args.split_last().expect("a primitive has an output");
                let args = regs(&state, inputs);
                let (out, width) = bind(&mut state, *out, width);
                let prim = self.symbols.prim(*prim);
                (Step::Prim { prim, args, out }, width)
            }
            _ => unreachable!("chains start at lookups and primitives"),
        };
        let mut actions = Vec::new();
        let next = self.actions(&mut state, at + 1, &mut actions, &mut width);
        let children = if next < rule.actions.len() {
            vec![self.head(state, next, width)]
        } else {
            Vec::new()
        };
        self.push(Node {
            step,
            actions,
            children,
            rules: Vec::new(),
        })
    }

    // Lowers the actions from `from` until the next lookup or primitive, and
    // returns where it stopped.
    fn actions(
        &mut self,
        state: &mut State,
        from: usize,
        out: &mut Vec<Action>,
        width: &mut u32,
    ) -> usize {
        let rules = self.rules;
        let acts = &rules[self.variants[state.variant].rule].actions;
        for (k, act) in acts.iter().enumerate().skip(from) {
            out.push(match act {
                Act::Lookup(table, args)
                    if self.schema.table(*table).default_value().is_none()
                        || args
                            .last()
                            .is_some_and(|&v| state.regs[v as usize].is_some()) =>
                {
                    return k;
                }
                Act::Prim(..) => return k,
                Act::Insert(table, args) | Act::Lookup(table, args) => {
                    let schema = self.schema.table(*table);
                    let keys = schema.key_arity();
                    let table = self.symbols.table(*table);
                    let makes =
                        schema.kind() == TableKind::Constructor || schema.default_value().is_some();
                    match args.get(keys) {
                        Some(&v) if makes && state.regs[v as usize].is_none() => {
                            state.regs[v as usize] = Some(*width);
                            *width += 1;
                            let out = *width - 1;
                            let args = regs(state, &args[..keys]);
                            Action::Entry { table, args, out }
                        }
                        _ => Action::Set {
                            table,
                            args: regs(state, args),
                        },
                    }
                }
                Act::FindOrMake(tables, args) => {
                    let (&group, mut cells) = args.split_last().expect("a group");
                    let rows = (tables.iter())
                        .map(|&table| {
                            let len = self.schema.table(table).columns().len() - 1;
                            let (row, rest) = cells.split_at(len);
                            cells = rest;
                            (self.symbols.table(table), regs(state, row))
                        })
                        .collect();
                    state.regs[group as usize] = Some(*width);
                    *width += 1;
                    Action::FindOrMake {
                        rows,
                        out: *width - 1,
                    }
                }
                Act::Union(a, b) => Action::Union {
                    a: reg(state, *a),
                    b: reg(state, *b),
                },
                Act::Delete(table, args) => Action::Delete {
                    table: self.symbols.table(*table),
                    args: regs(state, args),
                },
                Act::Subsume(table, args) => Action::Subsume {
                    table: self.symbols.table(*table),
                    args: regs(state, args),
                },
                Act::Call(prim, args) => Action::Call {
                    prim: self.symbols.prim(*prim),
                    args: regs(state, args),
                },
                Act::Panic(message) => Action::Panic {
                    message: message.clone(),
                },
            });
        }
        acts.len()
    }

    fn step(&mut self, join: &Join, width: u32) -> (Step, u32) {
        let (binds, after) = lower(join, width);
        let step = match join {
            Join::Always => Step::Same {
                a: self.unit,
                b: self.unit,
            },
            Join::Prim { prim, .. } => {
                let (out, inputs) = binds.split_last().expect("a primitive has an output");
                Step::Prim {
                    prim: self.symbols.prim(*prim),
                    args: inputs.iter().map(same).collect(),
                    out: *out,
                }
            }
            Join::Semi { table, key } => {
                let cols: Vec<usize> = (0..key.len()).filter(|&c| key[c].is_some()).collect();
                let key = cols.iter().filter_map(|&c| key[c]).collect();
                let index = self.symbols.index(*table, cols);
                Step::Check { index, key }
            }
            Join::Primary { table, slice, args } => {
                let keys = self.schema.table(*table).key_arity();
                let bound: Vec<usize> = (0..args.len())
                    .filter(|&c| matches!(args[c], Arg::Reg(_)))
                    .collect();
                let file_table = self.symbols.table(*table);
                let slice = *slice;
                if slice != Slice::New
                    && bound.len() >= keys
                    && bound[..keys].iter().copied().eq(0..keys)
                {
                    let key = binds[..keys].iter().map(same).collect();
                    let out = binds.get(keys).copied();
                    Step::Lookup {
                        table: file_table,
                        key,
                        out,
                        slice,
                    }
                } else if slice == Slice::New || bound.is_empty() {
                    Step::Scan {
                        table: file_table,
                        slice,
                        cols: binds,
                    }
                } else {
                    let key = bound.iter().map(|&c| same(&binds[c])).collect();
                    let outs = (0..args.len())
                        .filter(|c| !bound.contains(c))
                        .map(|c| binds[c])
                        .collect();
                    let index = self.symbols.index(*table, bound);
                    Step::Expand {
                        index,
                        key,
                        outs,
                        slice,
                    }
                }
            }
        };
        (step, after)
    }

    // Whether `step` is a semi-join that the join of `child` implies: one on
    // the same table whose key holds the same registers in the same columns.
    fn implied(&self, step: &Step, child: u32) -> bool {
        let Step::Check { index, key } = step else {
            return false;
        };
        let check = &self.symbols.indexes[*index as usize];
        let (table, pairs): (u32, Vec<(usize, Reg)>) = match &self.nodes[child as usize].step {
            Step::Expand { index, key, .. } => {
                let decl = &self.symbols.indexes[*index as usize];
                let pairs = decl.order.iter().copied().zip(key.iter().copied());
                (decl.table, pairs.collect())
            }
            Step::Lookup { table, key, .. } => (*table, key.iter().copied().enumerate().collect()),
            _ => return false,
        };
        let mut wanted = check.order.iter().copied().zip(key.iter().copied());
        table == check.table && wanted.all(|pair| pairs.contains(&pair))
    }

    fn rule(&self, state: &State) -> &Rule {
        &self.rules[self.variants[state.variant].rule]
    }

    fn push(&mut self, node: Node) -> u32 {
        self.nodes.push(node);
        (self.nodes.len() - 1) as u32
    }
}

// The binds of a join's columns: a register bound before, or the k-th new
// variable from register `width` on. Returns the width after the join.
fn lower(join: &Join, width: u32) -> (Vec<Bind>, u32) {
    let args: &[Arg] = match join {
        Join::Primary { args, .. } | Join::Prim { args, .. } => args,
        Join::Semi { .. } | Join::Always => return (Vec::new(), width),
    };
    let mut fresh = 0;
    let binds = args
        .iter()
        .map(|arg| match *arg {
            Arg::Reg(r) => Bind::Same(r),
            Arg::New(k) if k < fresh => Bind::Same(width + k),
            Arg::New(k) => {
                fresh = k + 1;
                Bind::New(width + k)
            }
        })
        .collect();
    (binds, width + fresh)
}

// Binds `v` from register `width` on unless it is bound already.
fn bind(state: &mut State, v: Var, width: u32) -> (Bind, u32) {
    match state.regs[v as usize] {
        Some(r) => (Bind::Same(r), width),
        None => {
            state.regs[v as usize] = Some(width);
            (Bind::New(width), width + 1)
        }
    }
}

fn same(bind: &Bind) -> Reg {
    match *bind {
        Bind::Same(r) => r,
        Bind::New(_) => unreachable!("a key column is bound"),
    }
}

fn reg(state: &State, v: Var) -> Reg {
    state.regs[v as usize].expect("an action reads bound variables")
}

fn regs(state: &State, args: &[Var]) -> Vec<Reg> {
    args.iter().map(|&v| reg(state, v)).collect()
}
