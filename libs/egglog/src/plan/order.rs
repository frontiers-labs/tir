// Join order (thesis §3.4, listing 18): each next join of a variant is one
// with the best score given the variables bound so far.

use super::hir::{Rule, Target, Var};
use super::permute::Symmetries;
use crate::vm::code::Slice;
use crate::{ExternId, Schema, TableId};

// Join priorities in ascending order. The planner reads arity, keys and
// symmetries, never sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Score {
    // No bound variable: iterate the whole table.
    Disconnected,
    // Primary join on All.
    AllConnected,
    // Primary join on Old, which is smaller than All.
    OldConnected,
    SemiJoinOld,
    // Semi-joins prefer All so that the primary join after them can take Old.
    SemiJoinAll,
    // One atom left: no semi-join needed.
    LastPrimary,
    // Key columns bound: at most one row.
    SingleElementConnected,
    // Every column bound: a check.
    AllBound,
    // A nullary table: one row.
    Global,
    // New has no index, so it goes first.
    New,
}

// A column of a join: a register bound before it, or the k-th variable the
// join binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Arg {
    Reg(u32),
    New(u32),
}

// A join in terms of registers, so two variants that bind their variables
// alike share it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Join {
    Primary {
        table: TableId,
        slice: Slice,
        args: Vec<Arg>,
    },
    Semi {
        table: TableId,
        key: Vec<Option<u32>>,
    },
    Prim {
        prim: ExternId,
        context: crate::Context,
        args: Vec<Arg>,
    },
    // Keeps the bindings whose key, the bound columns, `table` lacks.
    Absent {
        table: TableId,
        key: Vec<Option<u32>>,
    },
    // A hoisted premise succeeds only when its computed result exists.
    Ready(u32),
    // The step of a rule with no premise.
    Always,
}

// A join one variant can take, and the atom in the column order it reads.
pub(crate) struct Candidate {
    pub(crate) join: Join,
    atom: usize,
    form: Vec<Var>,
}

// A variant partway through planning.
#[derive(Clone)]
pub(crate) struct State {
    pub(crate) variant: usize,
    pub(crate) regs: Vec<Option<u32>>,
    done: Vec<bool>,
    // Each absent atom whose check is placed.
    absent: Vec<bool>,
    // The number of bound columns of each atom at its last semi-join.
    semi: Vec<usize>,
}

impl State {
    // `regs` holds the register of each constant variable of the rule.
    pub(crate) fn new(variant: usize, rule: &Rule, regs: Vec<Option<u32>>) -> Self {
        Self {
            variant,
            regs,
            done: vec![false; rule.premise.len()],
            absent: vec![false; rule.absent.len()],
            semi: vec![0; rule.premise.len()],
        }
    }

    pub(crate) fn complete(&self) -> bool {
        self.done.iter().chain(&self.absent).all(|&done| done)
    }

    // The joins with the best score: ready primitives first, as they only
    // compute or filter.
    pub(crate) fn candidates(
        &self,
        rule: &Rule,
        slices: &[Slice],
        schema: Schema<'_>,
        syms: &Symmetries,
    ) -> Vec<Candidate> {
        if self.complete() {
            let (atom, form) = (0, Vec::new());
            return vec![Candidate {
                join: Join::Always,
                atom,
                form,
            }];
        }
        let ready: Vec<Candidate> = (rule.premise.iter().enumerate())
            .filter(|(a, _)| !self.done[*a])
            .filter_map(|(a, atom)| {
                let (&out, inputs) = atom.args.split_last()?;
                rule.computed[out as usize]
                    .as_ref()
                    .filter(|(prim, args)| {
                        atom.target == Target::Prim(*prim) && args.as_slice() == inputs
                    })?;
                Some(Candidate {
                    join: Join::Ready(self.regs[out as usize]?),
                    atom: a,
                    form: Vec::new(),
                })
            })
            .collect();
        if !ready.is_empty() {
            return ready;
        }
        // An absent row is checked as soon as the premise binds every
        // variable of it that the premise reads, the others matching any
        // cell; like a primitive it only filters.
        let read = |v: Var| {
            rule.constants[v as usize].is_some()
                || (rule.premise.iter()).any(|atom| atom.args.contains(&v))
        };
        let absent: Vec<Candidate> = (0..rule.absent.len())
            .filter(|&a| !self.absent[a])
            .filter_map(|a| {
                let atom = &rule.absent[a];
                let Target::Table(table) = atom.target else {
                    return None;
                };
                (atom.args.iter())
                    .all(|&v| self.bound(v) || !read(v))
                    .then(|| Candidate {
                        join: Join::Absent {
                            table,
                            key: atom.args.iter().map(|&v| self.regs[v as usize]).collect(),
                        },
                        atom: a,
                        form: atom.args.clone(),
                    })
            })
            .collect();
        if !absent.is_empty() {
            return absent;
        }
        let open = (0..rule.premise.len()).filter(|&a| !self.done[a]);
        let prims: Vec<Candidate> = open
            .clone()
            .filter_map(|a| {
                let atom = &rule.premise[a];
                let Target::Prim(prim) = atom.target else {
                    return None;
                };
                let (_, inputs) = atom.args.split_last()?;
                inputs.iter().all(|&v| self.bound(v)).then(|| Candidate {
                    join: Join::Prim {
                        prim,
                        context: rule.contexts().0,

                        args: self.args(&atom.args),
                    },
                    atom: a,
                    form: atom.args.clone(),
                })
            })
            .collect();
        if !prims.is_empty() {
            return prims;
        }
        let table_atom = |&a: &usize| matches!(rule.premise[a].target, Target::Table(_));
        let remaining = open.clone().filter(table_atom).count();
        // Before any join the batch is one binding of constants, so a join
        // that yields one row at most costs one probe, and goes before New.
        let root = self.done.iter().all(|&done| !done);
        // The variables of constants alone, such as `(u32 0)`. A primitive
        // call follows the calls of its arguments in the premise.
        let mut constant: Vec<bool> = (0..rule.constants.len() as Var)
            .map(|v| rule.constant(v))
            .collect();
        for atom in &rule.premise {
            if let (Target::Prim(_), Some((&out, inputs))) = (atom.target, atom.args.split_last()) {
                constant[out as usize] |= inputs.iter().all(|&v| constant[v as usize]);
            }
        }
        // Among equal scores, a join along an edge of the pattern tree, through
        // a variable that is the output of a premise atom, goes first: one
        // through a shared leaf variable can match far more rows (on fuel3,
        // 6.2M rows for 10k matches). A constant is no edge, however many
        // atoms share it: on the fold rules, a join through `(u32 0)` before
        // the edge matched 600k rows for no match.
        let output = |v: Var| {
            !constant[v as usize]
                && rule.premise.iter().any(|atom| match atom.target {
                    Target::Table(t) => atom.args.get(schema.table(t).key_arity()) == Some(&v),
                    Target::Prim(_) => false,
                })
        };
        let rank = |score, form: &[Var]| {
            let one_row = matches!(
                score,
                Score::SingleElementConnected | Score::AllBound | Score::Global
            );
            let edge = form.iter().any(|&v| self.bound(v) && output(v));
            // The edge decides between the two connected primary joins
            // before Old does.
            let class = if score == Score::OldConnected {
                Score::AllConnected
            } else {
                score
            };
            (root && one_row, class, edge, score)
        };
        let mut best: Vec<((bool, Score, bool, Score), Candidate)> = Vec::new();
        for a in open {
            let atom = &rule.premise[a];
            let Target::Table(table) = atom.target else {
                continue;
            };
            let keys = schema.table(table).key_arity();
            // The order of the atom with the best score, then the fewest and
            // leftmost bound columns, so symmetric orders share an index.
            let forms = syms.forms(atom.target, &atom.args);
            let ranked = forms.into_iter().map(|form| {
                let cols: Vec<usize> = (0..form.len()).filter(|&c| self.bound(form[c])).collect();
                let score = primary(slices[a], keys, form.len(), &cols, remaining);
                (score, cols, form)
            });
            let pick = |(s, cols, _): &(Score, Vec<usize>, Vec<Var>)| {
                (*s, std::cmp::Reverse(cols.clone()))
            };
            let Some((score, cols, form)) = ranked.rev().max_by_key(pick) else {
                continue;
            };
            let connected = matches!(score, Score::AllConnected | Score::OldConnected);
            if connected && cols.len() > self.semi[a] {
                let semi = if slices[a] == Slice::Old {
                    Score::SemiJoinOld
                } else {
                    Score::SemiJoinAll
                };
                let key = form.iter().map(|&v| self.regs[v as usize]).collect();
                let join = Join::Semi { table, key };
                let form = form.clone();
                best.push((
                    rank(semi, &form),
                    Candidate {
                        join,
                        atom: a,
                        form,
                    },
                ));
            }
            let slice = slices[a];
            let args = self.args(&form);
            let join = Join::Primary { table, slice, args };
            best.push((
                rank(score, &form),
                Candidate {
                    join,
                    atom: a,
                    form,
                },
            ));
        }
        let top = best.iter().map(|(score, _)| *score).max();
        best.into_iter()
            .filter(|(score, _)| Some(*score) == top)
            .map(|(_, candidate)| candidate)
            .collect()
    }

    // Takes the join: binds its new variables from register `width` on.
    pub(crate) fn take(&mut self, candidate: &Candidate, width: u32) {
        match candidate.join {
            Join::Always => {}
            Join::Ready(_) => self.done[candidate.atom] = true,
            Join::Absent { .. } => self.absent[candidate.atom] = true,
            Join::Semi { .. } => {
                let bound = candidate.form.iter().filter(|&&v| self.bound(v)).count();
                self.semi[candidate.atom] = bound;
            }
            Join::Primary { .. } | Join::Prim { .. } => {
                self.done[candidate.atom] = true;
                let mut next = width;
                for &v in &candidate.form {
                    if !self.bound(v) {
                        self.regs[v as usize] = Some(next);
                        next += 1;
                    }
                }
            }
        }
    }

    fn bound(&self, v: Var) -> bool {
        self.regs[v as usize].is_some()
    }

    fn args(&self, form: &[Var]) -> Vec<Arg> {
        let mut new: Vec<Var> = Vec::new();
        form.iter()
            .map(|&v| match self.regs[v as usize] {
                Some(reg) => Arg::Reg(reg),
                None => {
                    let k = new.iter().position(|&w| w == v).unwrap_or_else(|| {
                        new.push(v);
                        new.len() - 1
                    });
                    Arg::New(k as u32)
                }
            })
            .collect()
    }
}

fn primary(slice: Slice, keys: usize, len: usize, cols: &[usize], remaining: usize) -> Score {
    if slice == Slice::New {
        Score::New
    } else if keys == 0 {
        Score::Global
    } else if cols.len() == len {
        Score::AllBound
    } else if cols.len() >= keys && cols[..keys].iter().copied().eq(0..keys) {
        Score::SingleElementConnected
    } else if cols.is_empty() {
        Score::Disconnected
    } else if remaining == 1 {
        Score::LastPrimary
    } else if slice == Slice::Old {
        Score::OldConnected
    } else {
        Score::AllConnected
    }
}
