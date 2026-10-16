//! Selecting one function: the rules generated from the automaton's tables,
//! what a function's graph is given before they run, and what a node costs.
//!
//! A rule is written for a number of children, never for a label: what a row
//! is and what state its operands are in are words the rule reads off the
//! graph and joins with the tables.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use smallvec::SmallVec;
use tir::binding::signed_bits;
use tir::sem::node::{WIDEST_RESULT, field};
use tir::sem::{ANY_FORM, ANY_STATE, Kind, Prov, SemNode, SymKind};
use tir::{
    BlockId, ConstantLike, Context, Gamma, IdMap, IdSet, OpHandle, OpId, PassError, RegionId,
    ResourceAccess, ResourceEffects, Theta, TypeArg, TypeId, Use, ValueId,
};
use tir_adt::APInt;
use tir_egraph::{
    Atom, ClassId, ColumnId, Engine, Extraction, Guard, HeadOp, LabelFill, NoExterns, Plan, Query,
    Ref, RowId, Rule, Source, Vocabulary,
};
use tir_relational::{Cmp, Expr, Join, Scalar, Var, Words};

use super::automaton::{
    Accept, Adder, Automaton, Held, Path, Pattern, Seed, accesses, canonical_type, chain, form,
};
use super::edges::Cases;
use super::pool::{Arg, Builder, Tree};
use super::rule::{EmitRequest, RuleMatch, StatePorts};
use crate::rewrite::{self, Rewrite, RuleSet, children, published};

/// How many operand slots and region results read a class's values, all of
/// them together: each value's readers are said once and the column adds.
pub const USES: ColumnId = ColumnId::Client(0);
/// The region a class's value is defined and read in: a class defined or
/// read in two holds none.
pub const REGION: ColumnId = ColumnId::Client(1);
/// How many writes, one after another, a class's value waits for at least:
/// the most of what its rows read, for the state a write leaves one more than
/// the state it observes, and for a result of an operation that changes a
/// state one more than anything the operation reads. A class every row of
/// which reads a class without one holds none.
pub const DEPTH: ColumnId = ColumnId::Client(2);
/// The least [`DEPTH`] of the states the accesses folded into a form
/// observe: what the form reads beside them may not wait for more.
pub const LIMIT: ColumnId = ColumnId::Client(3);
/// How many of a boolean class's readers read its value from a register:
/// every slot [`USES`] counts but a control reading its predicate. A wider
/// predicate has none: its case tests read the register.
pub const DATA: ColumnId = ColumnId::Client(4);

const COLUMNS: [(ColumnId, Join); 5] = [
    (USES, Join::Sum),
    (REGION, Join::Agree),
    (DEPTH, Join::Max),
    (LIMIT, Join::Min),
    (DATA, Join::Sum),
];

/// Declare the columns the rules read in `eg`, a graph with none of a
/// client's yet.
pub fn declare(eg: &mut Engine<SemNode>) {
    for (column, join) in COLUMNS {
        let declared = eg.add_column(join);
        debug_assert_eq!(declared, column);
    }
}

/// The rules of one automaton and the tables their atoms read, by slot.
pub struct Rules {
    // Saturated before the selecting rules: a rule that compares a depth reads
    // the one its round found, which has to be the last.
    pub(super) ordering: Vec<Rule<SemNode>>,
    /// The rules that select.
    pub selecting: Vec<Arc<Rule<SemNode>>>,
    pub tables: Vec<Arc<Words>>,
    // The labels the rows of the tables have, sorted.
    labels: Vec<u32>,
    // The patterns a reader folds an access through, sorted.
    folding: Vec<Pattern>,
}

fn selectable(node: &SemNode) -> bool {
    matches!(node.kind, Kind::Sym(_) | Kind::Ir(_))
}

// The rows an order runs through: an operation over its operands, an access
// over its state, a constant, and a result over what its operation reads.
// The term of a gate or a loop is passed over, the result beside it reading
// all the term reads, and so is the leaf of a port, which `facts` speaks for.
fn ordered(node: &SemNode) -> bool {
    match &node.kind {
        Kind::Ir(_) | Kind::Result { .. } | Kind::Operands => true,
        Kind::Sym(kind) => accesses(*kind) || *kind == SymKind::Constant,
        Kind::State(_) | Kind::Form(_) => false,
    }
}

// The bit of a pattern's guard word, above its holders, that says a reader
// folds the pattern.
const GUARDED: u32 = 8;

fn raise(column: ColumnId, key: Var, value: Expr) -> HeadOp<SemNode> {
    HeadOp::Raise { column, key, value }
}

// `op(c1..cK) in C, DEPTH(ci) = di => DEPTH(C) >= d1 + w .. dK + w`, where
// `w` is one for a result of an operation that changes a state; and for a
// write, a row of `effects` whose observed state is child `state`,
// `effects(L, state, 1), DEPTH(c) = d => DEPTH(C) >= d + 1`.
fn deepening(arity: usize, write: Option<(u32, usize)>) -> Rule<SemNode> {
    let operands: SmallVec<[Var; 4]> = (1..=arity as Var).collect();
    let read = |child: usize| Atom::Fact {
        column: DEPTH,
        key: operands[child],
        value: (1 + child) as Scalar,
    };
    let after = (1 + arity) as Scalar;
    let later = |depth: Expr, by: Expr| Expr::Add(Box::new(depth), Box::new(by));
    let depth = |child: usize| Expr::Scalar((1 + child) as Scalar);
    let mut atoms = vec![Atom::Any {
        accept: ordered,
        args: operands.clone(),
        class: ROOT,
        row: None,
        label: Some(LABEL),
    }];
    let mut guards = Vec::new();
    let (name, head) = match write {
        None => {
            atoms.extend((0..arity).map(read));
            guards.push(Guard::Read {
                term: Source::Label(LABEL),
                field: field::CHANGES,
                out: after,
            });
            let deepest = (0..arity).map(|child| later(depth(child), Expr::Scalar(after)));
            let leaf = (arity == 0).then_some(Expr::Scalar(after));
            let head = deepest.chain(leaf).map(|depth| raise(DEPTH, ROOT, depth));
            (format!("depth-{arity}"), head.collect())
        }
        Some((effects, state)) => {
            let terms = vec![Expr::Scalar(LABEL), Expr::Lit(state as i64), Expr::Lit(1)];
            let written = Atom::Table {
                table: effects,
                terms,
            };
            atoms.extend([written, read(state)]);
            (
                format!("write-{arity}-{state}"),
                vec![raise(DEPTH, ROOT, later(depth(state), Expr::Lit(1)))],
            )
        }
    };
    Rule {
        name,
        plan: Plan::compile(Query {
            vars: 1 + arity as u32,
            scalars: 2 + arity as u32,
            root: ROOT,
            atoms,
            guards,
            nots: Vec::new(),
        }),
        head,
        head_vars: 0,
        post_saturation: false,
    }
}

fn chained(node: &SemNode) -> bool {
    matches!(node.kind, Kind::State(_))
}

// The left side every rule of `arity` children shares:
//   op(c1..cK) in C, ci == bi + di, st(Ti)(vi) in bi,
//   table(L, T1..TK, m1..mK, P, c), di of class mi by imm(L, T1..TK, i, mi, ..)
struct Body {
    arity: usize,
    vars: u32,
    scalars: u32,
    atoms: Vec<Atom<SemNode>>,
    guards: Vec<Guard>,
    // Per child, the variable its chain is over.
    held: SmallVec<[Var; 5]>,
    // The depths the row's form is limited to.
    limits: SmallVec<[Scalar; 2]>,
}

const ROOT: Var = 0;
const LABEL: Scalar = 0;

impl Body {
    fn operand(&self, child: usize) -> Var {
        (1 + child) as Var
    }

    fn base(&self, child: usize) -> Var {
        (1 + self.arity + child) as Var
    }

    fn state(&self, child: usize) -> Scalar {
        (1 + child) as Scalar
    }

    fn offset(&self, child: usize) -> Scalar {
        (1 + self.arity + child) as Scalar
    }

    fn class(&self, child: usize) -> Scalar {
        (1 + 2 * self.arity + child) as Scalar
    }

    fn pattern(&self) -> Scalar {
        (1 + 3 * self.arity) as Scalar
    }

    fn carrier(&self) -> Scalar {
        (2 + 3 * self.arity) as Scalar
    }

    fn var(&mut self) -> Var {
        self.vars += 1;
        self.vars - 1
    }

    fn scalar(&mut self) -> Scalar {
        self.scalars += 1;
        self.scalars - 1
    }

    fn new(arity: usize, step: u32, imm: u32) -> Self {
        let mut body = Body {
            arity,
            vars: (1 + 3 * arity) as u32,
            scalars: (3 + 3 * arity) as u32,
            atoms: Vec::new(),
            guards: Vec::new(),
            held: (0..arity)
                .map(|child| (1 + 2 * arity + child) as Var)
                .collect(),
            limits: SmallVec::new(),
        };
        let children = 0..arity;
        body.atoms.push(Atom::Any {
            accept: selectable,
            args: children.clone().map(|child| body.operand(child)).collect(),
            class: ROOT,
            row: None,
            label: Some(LABEL),
        });
        for child in children.clone() {
            body.atoms.push(Atom::Offset {
                key: body.operand(child),
                base: body.base(child),
                offset: body.offset(child),
            });
            body.atoms.push(Atom::Any {
                accept: chained,
                args: SmallVec::from_slice(&[body.held[child]]),
                class: body.base(child),
                row: None,
                label: Some(body.state(child)),
            });
        }
        let key: Vec<Expr> = std::iter::once(LABEL)
            .chain(children.clone().map(|child| body.state(child)))
            .map(Expr::Scalar)
            .collect();
        let classes = children.clone().map(|child| body.class(child));
        let stepped = classes.chain([body.pattern(), body.carrier()]);
        let stepped = stepped.map(Expr::Scalar);
        body.atoms.push(Atom::Table {
            table: step,
            terms: key.iter().cloned().chain(stepped).collect(),
        });
        for child in children {
            let mut terms = key.clone();
            terms.extend([Expr::Lit(child as i64), Expr::Scalar(body.class(child))]);
            let [width, add, sub, high, sign, bits, align, nonzero] =
                std::array::from_fn(|_| Expr::Scalar(body.scalar()));
            let cells = [&width, &add, &sub, &high, &sign, &bits, &align, &nonzero];
            terms.extend(cells.into_iter().cloned());
            body.atoms.push(Atom::Table { table: imm, terms });
            let offset = || Expr::Scalar(body.offset(child));
            let plus = |a, b| Expr::Add(Box::new(a), Box::new(b));
            let and = |a, b| Expr::And(Box::new(a), Box::new(b));
            let ones = |bits| Expr::Ones(Box::new(bits));
            let moved = Expr::Sub(Box::new(plus(offset(), add)), Box::new(sub));
            let moved = and(plus(plus(moved, ones(high)), sign), ones(width));
            let held = and(moved.clone(), ones(bits));
            let zero = Expr::IsZero(Box::new(offset()));
            body.guards.extend([
                Guard::Cmp(Cmp::Eq, moved, held),
                Guard::Cmp(Cmp::Eq, and(offset(), align), Expr::Lit(0)),
                Guard::Cmp(Cmp::Eq, and(nonzero, zero), Expr::Lit(0)),
            ]);
        }
        body
    }

    // The right side: wi := vi + di, f := form(P)(w1..wK) in the carrier c,
    // C == st(P)(f).
    fn rule(mut self, name: String) -> Rule<SemNode> {
        let query_vars = self.vars;
        let forms: SmallVec<[Var; 4]> = (0..self.arity).map(|_| self.var()).collect();
        let (formed, chained) = (self.var(), self.var());
        let filled = |template, fills: &[(u32, Scalar)]| LabelFill {
            template,
            fills: SmallVec::from_slice(fills),
        };
        let (pattern, carrier) = (self.pattern(), self.carrier());
        let mut head: Vec<HeadOp<SemNode>> = (0..self.arity)
            .map(|child| HeadOp::Offset {
                base: self.held[child],
                offset: Expr::Scalar(self.offset(child)),
                into: forms[child],
            })
            .collect();
        head.extend([
            HeadOp::Insert {
                label: filled(
                    form(ANY_FORM, self.arity),
                    &[(field::FORM, pattern), (field::CARRIER, carrier)],
                ),
                args: forms,
                into: formed,
            },
            HeadOp::Insert {
                label: filled(chain(ANY_STATE), &[(field::STATE, pattern)]),
                args: SmallVec::from_slice(&[formed]),
                into: chained,
            },
            HeadOp::Union(ROOT, chained),
        ]);
        let limits = self.limits.iter();
        head.extend(limits.map(|&limit| raise(LIMIT, formed, Expr::Scalar(limit))));
        Rule {
            name,
            plan: Plan::compile(Query {
                vars: query_vars,
                scalars: self.scalars,
                root: ROOT,
                atoms: self.atoms,
                guards: self.guards,
                nots: Vec::new(),
            }),
            head,
            head_vars: self.vars - query_vars,
            post_saturation: false,
        }
    }

    // The row, an access or a row with one folded in it, folds into its
    // reader only where the two facts of its class hold: one reader, in the
    // region the value is defined in.
    fn guarded(mut self) -> Self {
        let (uses, region) = (self.scalar(), self.scalar());
        let fact = |column, value| Atom::Fact {
            column,
            key: ROOT,
            value,
        };
        self.atoms.extend([fact(USES, uses), fact(REGION, region)]);
        let one = Guard::Cmp(Cmp::Eq, Expr::Scalar(uses), Expr::Lit(1));
        self.guards.push(one);
        self
    }

    // The form of an access is limited to the depth of the state it
    // observes, child `state` by the row of `effects` its label has.
    fn observing(mut self, effects: u32, state: usize) -> Self {
        let (writes, limit) = (self.scalar(), self.scalar());
        let terms = [Expr::Scalar(LABEL), Expr::Lit(state as i64)];
        self.atoms.push(Atom::Table {
            table: effects,
            terms: terms.into_iter().chain([Expr::Scalar(writes)]).collect(),
        });
        self.atoms.push(Atom::Fact {
            column: DEPTH,
            key: self.operand(state),
            value: limit,
        });
        self.limits.push(limit);
        self
    }

    // The children of `folded` hold folded accesses: what each is limited
    // to bounds the depth of every other child, and limits the row's form.
    // A child's depth is its value's, which no leaf of a row folded there
    // is deeper than.
    fn over(mut self, folded: u8) -> Self {
        let fact = |column, key, value| Atom::Fact { column, key, value };
        let depths: SmallVec<[Scalar; 5]> = (0..self.arity).map(|_| self.scalar()).collect();
        for (child, &depth) in depths.iter().enumerate() {
            self.atoms.push(fact(DEPTH, self.base(child), depth));
        }
        for holder in (0..self.arity).filter(|&child| folded & (1 << child) != 0) {
            let limit = self.scalar();
            self.atoms.push(fact(LIMIT, self.held[holder], limit));
            let others = depths
                .iter()
                .enumerate()
                .filter(|&(child, _)| child != holder);
            self.guards.extend(
                others.map(|(_, &depth)| {
                    Guard::Cmp(Cmp::Le, Expr::Scalar(depth), Expr::Scalar(limit))
                }),
            );
            self.limits.push(limit);
        }
        self
    }

    fn rename(&mut self, from: Var, to: Var) {
        let renamed = |var: &mut Var| {
            if *var == from {
                *var = to;
            }
        };
        for atom in &mut self.atoms {
            match atom {
                Atom::Any { args, class, .. } | Atom::Node { args, class, .. } => {
                    args.iter_mut().chain([class]).for_each(renamed);
                }
                Atom::Offset { key, base, .. } => [key, base].into_iter().for_each(renamed),
                _ => {}
            }
        }
        self.held.iter_mut().for_each(renamed);
    }
}

// The forms a tied pattern is over, as far as its ties read into them: what
// the references at two paths of the root's form are, a base and an offset
// each, so one value read at both is one variable.
struct Reach<'a> {
    body: Body,
    // The number of children the form at a path has.
    arities: &'a [(Path, usize)],
    forms: Vec<(Path, SmallVec<[Var; 4]>)>,
    reached: Vec<(Path, Var, Scalar)>,
}

impl Reach<'_> {
    fn at(&mut self, path: &[u8]) -> (Var, Scalar) {
        let (&last, above) = path.split_last().expect("a path names a child");
        let child = usize::from(last);
        if above.is_empty() {
            return (self.body.held[child], self.body.offset(child));
        }
        let known = self.reached.iter().find(|(known, ..)| known[..] == *path);
        if let Some(&(_, base, offset)) = known {
            return (base, offset);
        }
        let (holder, _) = self.at(above);
        let known = self.forms.iter().find(|(known, _)| known[..] == *above);
        let children = match known {
            Some((_, children)) => children.clone(),
            None => {
                let arity = self.arities.iter().find(|(known, _)| known[..] == *above);
                let arity = arity.expect("a tie reads into a form of the pattern").1;
                let children: SmallVec<[Var; 4]> = (0..arity).map(|_| self.body.var()).collect();
                self.body.atoms.push(Atom::Node {
                    template: form(ANY_FORM, arity),
                    args: children.clone(),
                    class: holder,
                    row: None,
                });
                self.forms.push((above.into(), children.clone()));
                children
            }
        };
        let (base, offset) = (self.body.var(), self.body.scalar());
        self.body.atoms.push(Atom::Offset {
            key: children[child],
            base,
            offset,
        });
        self.reached.push((path.into(), base, offset));
        (base, offset)
    }

    fn tie(&mut self, first: &[u8], second: &[u8]) {
        let (kept, at) = self.at(first);
        let (other, also) = self.at(second);
        self.body.rename(other, kept);
        for (_, base, _) in &mut self.reached {
            if *base == other {
                *base = kept;
            }
        }
        let same = Guard::Cmp(Cmp::Eq, Expr::Scalar(at), Expr::Scalar(also));
        self.body.guards.push(same);
    }
}

// What the ties of a pattern read: the paths that hold one value, and the
// number of children of every form on the way to them.
#[derive(Clone, PartialEq, Eq)]
struct Tied {
    arity: usize,
    ties: SmallVec<[(Path, Path); 1]>,
    arities: Vec<(Path, usize)>,
    // The children that hold a folded access.
    carried: u8,
}

// The states of the children of each pattern, read off the rows that step
// to it.
fn shapes(automaton: &Automaton) -> HashMap<Pattern, SmallVec<[Pattern; 5]>> {
    let states: HashMap<u32, Pattern> = automaton.chains.iter().copied().zip(0..).collect();
    let mut shapes = HashMap::new();
    for (arity, table) in stepping(automaton) {
        for row in 0..table.len() {
            let children = (1..=arity).map(|child| states[&table.column(child)[row]]);
            let pattern = table.column(2 * arity + 1)[row];
            shapes.entry(pattern).or_insert_with(|| children.collect());
        }
    }
    shapes
}

// The four step tables of every number of children.
fn stepping(automaton: &Automaton) -> impl Iterator<Item = (usize, &Arc<Words>)> {
    let families = [
        &automaton.step,
        &automaton.step_effect,
        &automaton.step_over,
        &automaton.step_tied,
    ];
    families
        .into_iter()
        .flat_map(|family| family.iter().enumerate())
}

// The ways the tied patterns read one value twice, and the relation
// `(pattern, way)` a tied rule reads which patterns are its own from.
fn ties(automaton: &Automaton) -> (Vec<Tied>, Words) {
    let shapes = shapes(automaton);
    let arity_at = |pattern: Pattern, path: &[u8]| {
        let form = path.iter().try_fold(pattern, |at, &child| {
            shapes.get(&at)?.get(usize::from(child)).copied()
        });
        let form = form.and_then(|form| shapes.get(&form));
        form.expect("a tie reads into a folded row").len()
    };
    let mut kinds: Vec<Tied> = Vec::new();
    let mut tied = Words::new(2);
    for (arity, table) in automaton.step_tied.iter().enumerate() {
        for &pattern in table.column(2 * arity + 1) {
            let ties = automaton.ties[pattern as usize].clone();
            let mut arities: Vec<(Path, usize)> = Vec::new();
            for path in ties.iter().flat_map(|(first, second)| [first, second]) {
                for depth in 1..path.len() {
                    let above: Path = path[..depth].into();
                    let entry = (above.clone(), arity_at(pattern, &above));
                    if !arities.contains(&entry) {
                        arities.push(entry);
                    }
                }
            }
            let kind = Tied {
                arity,
                ties,
                arities,
                carried: automaton.carried[pattern as usize],
            };
            let known = kinds.iter().position(|known| *known == kind);
            let at = known.unwrap_or_else(|| {
                kinds.push(kind);
                kinds.len() - 1
            });
            tied.push(&[pattern, at as u32]);
        }
    }
    (kinds, tied)
}

// The numbers column `column` of `table` holds, each once, ascending.
fn distinct(table: &Words, column: usize) -> Vec<u32> {
    let mut held = table.column(column).to_vec();
    held.sort_unstable();
    held.dedup();
    held
}

impl Rules {
    /// The rules of `automaton`. First the ones that say [`DEPTH`], per
    /// number of children `K` a row of the tables or a [`Kind::Result`] has:
    /// one for a row over its children, and one per place a write of
    /// `effects[K]` observes its state at. Then, per `K` some row of its tables has: the step over
    /// `step[K]`; the step over `step_effect[K]` under the facts of a folded
    /// access, per place its state is at; the step over `step_over[K]` per
    /// set of children that hold a folded access; and one step over
    /// `step_tied[K]` for each way its patterns read one value twice.
    pub fn of(automaton: &Automaton) -> Self {
        let mut tables: Vec<Arc<Words>> = Vec::new();
        let mut slot = |table: &Arc<Words>| {
            tables.push(table.clone());
            tables.len() as u32 - 1
        };
        let (kinds, tied) = ties(automaton);
        let kind = slot(&Arc::new(tied));
        // What a pattern's rule guards it by: the children of it that hold
        // a folded access, and above them whether a reader folds it.
        let guard = |pattern: u32| {
            let folded = u32::from(automaton.guarded[pattern as usize]) << GUARDED;
            folded | u32::from(automaton.carried[pattern as usize])
        };
        let mut carried = Words::new(2);
        let folds = automaton.step_effect.iter().chain(&automaton.step_over);
        for table in folds.filter(|table| !table.is_empty()) {
            for &pattern in table.column(table.width() - 2) {
                carried.push(&[pattern, guard(pattern)]);
            }
        }
        let carried = slot(&Arc::new(carried));
        let masks = |table: &Words| -> Vec<u32> {
            let patterns = distinct(table, table.width() - 2);
            let mut masks: Vec<u32> = patterns.iter().map(|&pattern| guard(pattern)).collect();
            masks.sort_unstable();
            masks.dedup();
            masks
        };
        let arities = 0..automaton.step.len();
        let effects: Vec<u32> = automaton.effects.iter().map(&mut slot).collect();
        let mut rules: Vec<Rule<SemNode>> = (0..arities.end.max(WIDEST_RESULT + 1))
            .map(|arity| deepening(arity, None))
            .collect();
        for arity in arities.clone() {
            let table = &automaton.effects[arity];
            let mut states: Vec<u32> = (0..table.len())
                .filter(|&row| table.column(2)[row] == 1)
                .map(|row| table.column(1)[row])
                .collect();
            states.sort_unstable();
            states.dedup();
            let write = |state: u32| deepening(arity, Some((effects[arity], state as usize)));
            rules.extend(states.into_iter().map(write));
        }
        let ordering = rules.len();
        for arity in arities {
            let filled = |tables: &[Arc<Words>]| !tables[arity].is_empty();
            let (plain, effect) = (&automaton.step, &automaton.step_effect);
            let (over, tying) = (&automaton.step_over, &automaton.step_tied);
            if !(filled(plain) || filled(effect) || filled(over) || filled(tying)) {
                continue;
            }
            let imm = slot(&automaton.imm[arity]);
            if filled(plain) {
                let step = slot(&plain[arity]);
                rules.push(Body::new(arity, step, imm).rule(format!("step-{arity}")));
            }
            // The rules of one table over the patterns of one set of
            // holders each: `carried` tells them apart.
            let held = |table: u32, masks: &[u32], mask: u32| {
                let mut body = Body::new(arity, table, imm);
                if masks.len() > 1 {
                    let pattern = Expr::Scalar(body.pattern());
                    body.atoms.push(Atom::Table {
                        table: carried,
                        terms: vec![pattern, Expr::Lit(i64::from(mask))],
                    });
                }
                let body = match mask as u8 {
                    0 => body,
                    holders => body.over(holders),
                };
                match mask >> GUARDED {
                    0 => body,
                    _ => body.guarded(),
                }
            };
            if filled(effect) {
                let step = slot(&effect[arity]);
                let masks = masks(&effect[arity]);
                for state in distinct(&automaton.effects[arity], 1) {
                    for &mask in &masks {
                        let body = held(step, &masks, mask);
                        let body = body.observing(effects[arity], state as usize);
                        rules.push(body.rule(format!("fold-{arity}-{state}-{mask}")));
                    }
                }
            }
            if filled(over) {
                let step = slot(&over[arity]);
                let masks = masks(&over[arity]);
                for &mask in &masks {
                    let body = held(step, &masks, mask);
                    rules.push(body.rule(format!("over-{arity}-{mask}")));
                }
            }
            if !filled(tying) {
                continue;
            }
            let step = slot(&tying[arity]);
            let own = |(_, tied): &(usize, &Tied)| tied.arity == arity;
            for (at, tied) in kinds.iter().enumerate().filter(own) {
                let mut reach = Reach {
                    body: Body::new(arity, step, imm),
                    arities: &tied.arities,
                    forms: Vec::new(),
                    reached: Vec::new(),
                };
                let pattern = Expr::Scalar(reach.body.pattern());
                reach.body.atoms.push(Atom::Table {
                    table: kind,
                    terms: vec![pattern, Expr::Lit(at as i64)],
                });
                for (first, second) in &tied.ties {
                    reach.tie(first, second);
                }
                let body = match tied.carried {
                    0 => reach.body,
                    folded => reach.body.over(folded),
                };
                rules.push(body.rule(format!("tie-{arity}-{at}")));
            }
        }
        let mut labels: Vec<u32> = stepping(automaton)
            .flat_map(|(_, table)| table.column(0).to_vec())
            .collect();
        labels.sort_unstable();
        labels.dedup();
        let folding = automaton.step_effect.iter();
        let mut folding: Vec<Pattern> = folding
            .flat_map(|table| table.column(table.width() - 2).to_vec())
            .collect();
        folding.sort_unstable();
        let selecting = rules.split_off(ordering);
        Rules {
            ordering: rules,
            selecting: selecting.into_iter().map(Arc::new).collect(),
            tables,
            labels,
            folding,
        }
    }
}

impl Rules {
    pub(super) fn set(&self) -> RuleSet {
        RuleSet {
            rules: self.selecting.clone(),
            tables: self.tables.clone(),
            externs: Box::new(NoExterns),
        }
    }

    /// Say [`DEPTH`] of every class of `eg` the rules can: the rules that
    /// say it, run until none rises. A depth is at most the number of
    /// writes, so `rows`, the number of rows, bounds the rounds.
    pub fn order(&self, eg: &mut Engine<SemNode>, rows: usize) {
        eg.saturate_rules(&self.ordering, &NoExterns, rows, usize::MAX);
        // The log that saturation drained names no match of the other rules.
        eg.mark_all_changed();
    }

    /// Build what a first function's selection would of the tables and the
    /// plans, which `vocabulary` keeps.
    pub fn warm(&self, vocabulary: Vocabulary<SemNode>) -> Vocabulary<SemNode> {
        let mut eg = Engine::over(vocabulary);
        for table in &self.tables {
            eg.add_words(table.clone());
        }
        declare(&mut eg);
        eg.prepare_rules(&self.ordering);
        eg.prepare_rules(&self.selecting);
        eg.release()
    }
}

/// What selection adds to the driver: the generic rules, the automaton's
/// tables and the price of a node.
pub struct Select<'a> {
    pub automaton: &'a Automaton,
    pub rules: &'a Rules,
    pub cases: &'a Cases,
}

/// The selection of one function as the driver commits it: every chain the
/// extraction chose spelled as the instructions it accepts.
pub struct Commit<'a> {
    pub select: Select<'a>,
    pub(super) set: RuleSet,
    /// The target's rules, which the forms of the automaton index.
    pub target: &'a [super::Rule],
    /// The name ids of the operations the rules were synthesized for, sorted.
    pub kinds: &'a [u32],
    pub emitted: RefCell<Emitted>,
}

impl Select<'_> {
    // The seed of what `class` is to a reader: the zero of a carrier, or a
    // value of the type its terms carry.
    fn seed(&self, context: &Context, eg: &Engine<SemNode>, class: ClassId) -> Option<Pattern> {
        let seed = match (eg.int_const(class), eg.width(class)) {
            (Some(0), Some(width)) => Seed::Constant(width),
            _ => {
                let ty = TypeId::from_number(eg.fact(ColumnId::Type, class)? as u32);
                Seed::Register(canonical_type(context, ty))
            }
        };
        let seeds = self.automaton.seeds.iter();
        seeds
            .zip(0..)
            .find_map(|(known, at)| (*known == seed).then_some(at))
    }

    /// Give every class some row the rules may select reads the chain that
    /// says what it is, `c == st(seed)(c)`, and answer how many chains that
    /// is. The classes are the engine's own: the children of its rows, at
    /// offset zero.
    pub fn seed_chains(&self, context: &Context, eg: &mut Engine<SemNode>) -> usize {
        let classes: Vec<ClassId> = eg.class_ids().collect();
        // A class is read by many rows and gets its chain at the first.
        let mut met = vec![false; eg.class_count()];
        let mut seeded = 0;
        for class in classes {
            let rows = eg.rows(class).filter(|&row| selectable(eg.node(row)));
            let rows: SmallVec<[_; 4]> = rows.collect();
            for row in rows {
                for operand in eg.children(row) {
                    if std::mem::replace(&mut met[operand.class.index()], true) {
                        continue;
                    }
                    let base = Ref::from(operand.class);
                    let Some(seed) = self.seed(context, eg, operand.class) else {
                        continue;
                    };
                    let node = chain(seed);
                    if eg.lookup_at(&node, &[base]).is_some() {
                        continue;
                    }
                    let chain = eg.insert(node, &[base]);
                    let _ = eg.union(chain, base);
                    seeded += 1;
                }
            }
        }
        eg.rebuild();
        seeded
    }

    /// Say [`USES`] and [`REGION`] of every class an IR value is, from the
    /// use lists: how many slots read the value from outside its class,
    /// added to what the class's other values are read by, and the region it is defined
    /// in, which each region reading it or naming it as a result is said of
    /// it as well. A port that carries a value in waits for no write: its
    /// [`DEPTH`] is zero. Every other depth is the rules', a carried state's
    /// too, which waits for what its loop is entered on. [`DATA`] is said
    /// of a boolean beside [`USES`].
    pub fn facts(&self, context: &Context, eg: &mut Engine<SemNode>, values: &IdMap<ValueId, Ref>) {
        let bit = tir::builtin::IntegerType::new(context, 1);
        for (&value, &reference) in values.iter() {
            let class = reference.class;
            let held = context.get_value(value);
            if held.defining_op().is_none() && !context.is_state_type(held.ty()) {
                eg.raise_fact(DEPTH, class, 0);
            }
            let (read, published) = (context.uses_of(value), context.result_uses(value));
            // An operation the graph holds as an offset of the class, an
            // add of a constant, is the class and no reader of it: what
            // reads its result is counted there. A read is a reader of the
            // state it observes, though it publishes that state's class.
            let state = context.is_state_type(held.ty());
            let offsets = |read: &&tir::Use| {
                let same = |result: ValueId| {
                    let held = values.get(&result);
                    held.is_some_and(|held| eg.root(held.class) == eg.root(class))
                };
                !state
                    && context.with_rows(|rows| {
                        let reader = rows.op_cells(read.op);
                        !reader.owns_regions() && reader.results().any(same)
                    })
            };
            let readers = read.len() - read.iter().filter(offsets).count();
            eg.raise_fact(USES, class, (readers + published.len()) as u64);
            if held.ty() == bit {
                let data = data(context, &read, &published);
                eg.raise_fact(DATA, class, data as u64);
            }
            let defined = crate::region::defining_region(context, value);
            let reading = read.iter().filter_map(|read| context.region_of_op(read.op));
            let naming = published.iter().map(|&(region, _)| region);
            for region in defined.into_iter().chain(reading).chain(naming) {
                eg.raise_fact(REGION, class, u64::from(region.raw()));
            }
        }
    }

    fn form_cost(&self, form: u32) -> u64 {
        u64::from(self.automaton.forms[form as usize].cost)
    }

    // The builders that put the bits of `value` in a register: the one plan
    // the price and the commit both read.
    fn plan(&self, value: &APInt) -> Option<super::pool::Built> {
        let width = u32::try_from(value.width()).ok()?;
        let usable = |builder: &Builder| builder.builds(width);
        super::pool::plan(&self.automaton.builders, value, usable)
    }

    fn built(&self, value: &APInt) -> u64 {
        self.plan(value).map_or(u64::MAX, |built| {
            let steps = built.steps.iter();
            steps.map(|(builder, _)| self.form_cost(builder.form)).sum()
        })
    }

    // What the constant `node` costs. A carrier's zero is free: a reader
    // pays for the constant it reads at an offset of it, where no field
    // holds that. One no carrier holds is its bits built and moved into a
    // register of its own kind.
    fn constant(&self, eg: &Engine<SemNode>, class: ClassId, node: &SemNode) -> u64 {
        if eg.int_const(class).is_some() {
            return 0;
        }
        let Some(value) = node.bits() else {
            return u64::MAX;
        };
        let mut moves = self.automaton.floats.iter();
        let moved = moves.find(|&&(_, _, width)| u64::from(width) == value.width());
        let moved = moved.map_or(u64::MAX, |&(form, ..)| self.form_cost(form));
        self.built(&value).saturating_add(moved)
    }
}

// The adders that move a register by an offset, in the order they run, each
// with the part of the offset it adds.
type Sum<'a> = SmallVec<[(&'a Adder, u64); 2]>;

impl Select<'_> {
    // The cost a form pays for an offset and what the commit emits for it must
    // come from this one answer.
    pub(super) fn sum(&self, base: Pattern, delta: u64, width: u32) -> Option<(Sum<'_>, u64)> {
        let adders = || self.automaton.adders.iter();
        let adders = || adders().filter(|adder| adder.base == base);
        let bare = |adder: &Adder| u64::from(adder.accept.cost);
        // The builders are asked only where the adder that needs them may
        // still win.
        let whole = |delta: u64| {
            let admitting = || adders().filter(|adder| fits(&adder.class, delta));
            let held = admitting().filter(|adder| !adder.built);
            let held = held
                .map(|adder| (adder, bare(adder)))
                .min_by_key(|&(_, cost)| cost);
            let built = admitting()
                .filter(|adder| adder.built && held.is_none_or(|(_, cost)| bare(adder) < cost));
            let built = built.map(|adder| {
                let offset = APInt::new_signed(u64::from(width), signed_bits(delta, width));
                (adder, bare(adder).saturating_add(self.built(&offset)))
            });
            held.into_iter().chain(built).min_by_key(|&(_, cost)| cost)
        };
        let direct = whole(delta);
        let least = adders().map(bare).min()?;
        // Two adders cost the first and the cheapest at least.
        let cheaper = |first: &&Adder| {
            let two = bare(first).saturating_add(least);
            !first.built && direct.is_none_or(|(_, cost)| two < cost)
        };
        let split = adders().filter(cheaper).flat_map(|first| {
            let parts = [false, true].map(|nearest| part(&first.class, delta, width, nearest));
            let parts = parts.into_iter();
            let parts = parts.filter(|&part| part != 0 && fits(&first.class, part));
            parts.filter_map(move |part| {
                let rest = delta.wrapping_sub(part) & ones(width);
                let (second, cost) = whole(rest)?;
                let cost = cost.saturating_add(bare(first));
                Some((smallvec::smallvec![(first, part), (second, rest)], cost))
            })
        });
        let split: Option<(Sum<'_>, u64)> = split.min_by_key(|&(_, cost)| cost);
        let direct = direct.map(|(adder, cost)| (smallvec::smallvec![(adder, delta)], cost));
        match (direct, split) {
            (Some(direct), Some(split)) if split.1 < direct.1 => Some(split),
            (None, split) => split,
            (direct, _) => direct,
        }
    }
}

impl Rewrite for Commit<'_> {
    fn rules(&self) -> &RuleSet {
        &self.set
    }

    fn cost(
        &self,
        eg: &Engine<SemNode>,
        class: ClassId,
        node: &SemNode,
        operands: &[Ref],
        _offset: u64,
    ) -> u64 {
        self.select.price(eg, class, node, operands)
    }

    fn spell(
        &self,
        out: &mut rewrite::Commit<'_>,
        row: RowId,
        region: RegionId,
    ) -> Option<ValueId> {
        let (context, eg) = (out.context, out.eg);
        let (reference, ty) = out.wanted();
        let float = |out: &rewrite::Commit<'_>, bits: &APInt| {
            let mut place = self.place(out, region);
            let built = self.float(context, bits, ty, &mut place);
            built.unwrap_or_else(|error| self.fail(error))
        };
        // A constant a carrier holds is built, and a register at an offset
        // moved there by the adders.
        if !out.sits(row) {
            let written = match eg.int_const(reference) {
                Some(bits) => {
                    let value =
                        APInt::new_signed(u64::from(eg.width(reference.class)?), bits as i64);
                    let mut place = self.place(out, region);
                    let built = self.built(context, &value, ty, &mut place);
                    built.unwrap_or_else(|error| self.fail(error))
                }
                None => {
                    let at = eg.value(row).offset;
                    let base = out.materialize(Ref::new(reference.class, at), ty, region)?;
                    let delta = reference.offset.wrapping_sub(at);
                    self.add_offset(out, base, delta, eg.width(reference.class)?, ty, region)
                }
            };
            return written.or_else(|| out.builtin(row, region));
        }
        let node = eg.node(row);
        let written = match node.kind {
            Kind::State(state) => {
                let spelled = self.chain(out, row, state, region);
                return spelled.unwrap_or_else(|error| self.fail(error));
            }
            Kind::Form(_) => return None,
            // A constant no carrier holds: its bits, moved where a value
            // of its type lives.
            Kind::Sym(SymKind::Constant) => match node.bits() {
                Some(bits) => return float(out, &bits),
                None => None,
            },
            // A row that stays is several operations where they are alike,
            // as twin gates are: the reader reads the one it names.
            _ => match self.stays(out, row, region) {
                Some(Ok(named)) => Some(named),
                Some(Err(bits)) => return float(out, &bits),
                None => None,
            },
        };
        written.or_else(|| out.builtin(row, region))
    }

    fn arm_facts(&self) -> bool {
        false
    }

    // A control that branches on a value by its chain keeps reading the value:
    // the operation stays as what the branch reads
    // until recovery emits it. Only what reads the value from a register is
    // handed a spelling, whose instruction follows the
    // operation, so a control recovery moves to that instruction finds the
    // branch.
    fn rewire_op(
        &self,
        out: &mut rewrite::Commit<'_>,
        instance: &OpHandle,
        region: RegionId,
    ) -> bool {
        let context = out.context;
        if !self.replaces(context, instance) {
            return false;
        }
        let stays = |read: &Use| !self.replaces(context, &context.get_op(read.op));
        for value in instance.value_results() {
            let read: Vec<Use> = context.uses_of(value).into_iter().filter(stays).collect();
            let named = context.result_uses(value);
            let (tests, data): (Vec<&Use>, Vec<&Use>) =
                read.iter().partition(|read| decides(context, read));
            let repeat = |&&(region, index): &&(RegionId, usize)| repeats(context, region, index);
            let (repeated, yielded): (Vec<_>, Vec<_>) = named.iter().partition(repeat);
            let controlled = !tests.is_empty() || !repeated.is_empty() || !yielded.is_empty();
            let left = controlled.then(|| self.left(out, value, region)).flatten();
            let rewire = match left {
                Some(_) => !data.is_empty() || !yielded.is_empty(),
                None => !read.is_empty() || published(context, region, value),
            };
            let rewired = rewire.then(|| {
                let rewired = self.spelling(Some(value), || out.rewire_read(value, region));
                rewired.unwrap_or_else(|error| self.fail(error))
            });
            let Some((kept, registers)) = left else {
                continue;
            };
            let defined = |value: ValueId| context.get_value(value).defining_op();
            let computed = rewired.flatten().and_then(defined);
            if let (Some(computed), Some(stays)) = (computed, defined(kept)) {
                let mut emitted = self.emitted.borrow_mut();
                emitted.implicit.entry(computed).or_default().push(stays);
            }
            let moved = rewire || kept != value;
            for test in tests.into_iter().filter(|_| moved) {
                context.set_op_operand(test.op, test.index, kept);
            }
            for &(body, index) in repeated.into_iter().filter(|_| moved) {
                let mut results = context.get_region(body).results();
                results[index] = kept;
                context.set_region_results(body, results);
            }
            if let Some(registers) = registers {
                context.set_op_operands(instance.id, registers);
            }
        }
        let first = instance.results().first().copied();
        if let Some(first) = first.filter(|_| changes(instance))
            && let Some(&reference) = out.value_class.get(&first)
        {
            let ty = context.get_value(first).ty();
            self.spelling(Some(first), || {
                out.materialize_as(first, reference, ty, region)
            });
        }
        true
    }
}

impl Select<'_> {
    /// What `node` over `operands` costs in `class`: the tree an accepted
    /// chain emits, the adds a form's operands at offsets no field holds are
    /// built by, the builders of a constant, nothing for an operation that
    /// stays, and the top price for anything else.
    pub fn price(
        &self,
        eg: &Engine<SemNode>,
        class: ClassId,
        node: &SemNode,
        operands: &[Ref],
    ) -> u64 {
        let automaton = self.automaton;
        match node.kind {
            Kind::State(state) => {
                let Some(accept) = automaton.accept[state as usize].as_ref() else {
                    return u64::MAX;
                };
                // A boolean only controls read is the branch on it: the
                // pattern's own where its form holds no access, or the
                // test of the value its tree computes.
                if eg.fact(DATA, class) != Some(0) {
                    return u64::from(accept.cost);
                }
                let pure = automaton.carried[state as usize] == 0;
                match automaton.branch(state).filter(|_| pure) {
                    Some(branch) => u64::from(branch.cost),
                    None => {
                        let tested = eg.width(class).and_then(|width| self.cases.tested(width));
                        u64::from(accept.cost) + u64::from(tested.unwrap_or(0))
                    }
                }
            }
            Kind::Form(pattern) => {
                let held = automaton.field[pattern as usize];
                let children = &automaton.children[pattern as usize];
                let built = operands.iter().zip(children).enumerate();
                let built = built.map(|(child, (operand, &(state, field)))| {
                    let width = eg.width(operand.class).unwrap_or(64);
                    let rest = match (held & (1 << child) != 0, field) {
                        (true, Some(field)) => rest(&field, operand.offset, width),
                        (true, None) => return 0,
                        (false, _) => match eg.int_const(*operand) {
                            Some(value) => {
                                return self.built(&tir_adt::APInt::new(u64::from(width), value));
                            }
                            None => operand.offset,
                        },
                    };
                    match rest {
                        0 => 0,
                        rest => self.sum(state, rest, width).map_or(u64::MAX, |sum| sum.1),
                    }
                });
                built.fold(0, u64::saturating_add)
            }
            Kind::Sym(SymKind::Constant) => self.constant(eg, class, node),
            _ if self.kept(node) => 0,
            _ => u64::MAX,
        }
    }

    /// Whether `node` stays the operation it is: the rules were
    /// synthesized from no instance of it, so it has nothing to select.
    pub fn kept(&self, node: &SemNode) -> bool {
        let automaton = self.automaton;
        match &node.kind {
            Kind::Ir(op) => {
                let instantiated = &automaton.instantiated;
                instantiated.binary_search(&(op.dialect, op.name)).is_err()
            }
            Kind::Sym(kind) => !automaton.accessed.contains(kind),
            Kind::Result { .. } | Kind::Operands => true,
            Kind::State(_) | Kind::Form(_) => false,
        }
    }
}

impl Select<'_> {
    /// Fail on the first operation the rules have to cover and no accepted
    /// chain reached, naming it as it is written.
    pub fn covered(
        &self,
        context: &Context,
        eg: &Engine<SemNode>,
        extraction: &Extraction<'_, SemNode>,
    ) -> Result<(), tir::PassError> {
        for class in eg.class_ids() {
            let Some(row) = extraction.row(class) else {
                continue;
            };
            if self.price(eg, class, eg.node(row), &eg.children(row)) != u64::MAX {
                continue;
            }
            let written = eg.nodes(class).find_map(|node| match node.prov {
                Prov::Op(op) => Some(op),
                Prov::Value(value) => context.get_value(value).defining_op(),
                _ => None,
            });
            let Some(op) = written else {
                continue;
            };
            let instance = context.get_op(op);
            let name = format!("{}.{}", instance.dialect(), instance.name());
            let mut spelled = String::new();
            let mut fmt = tir::IRFormatter::new(&mut spelled);
            let _ = crate::region_format::print_generic(&mut fmt, &instance, &name);
            let spelled = spelled.trim();
            return Err(tir::PassError::InvalidRuleSet(format!(
                "{name} has no instruction: {spelled}"
            )));
        }
        Ok(())
    }
}

/// What one function's selection came to, for `TIR_ISEL_TRACE`.
#[derive(Default)]
pub struct Trace {
    pub rows: usize,
    pub seeds: usize,
    pub seed_us: u128,
    pub chains_us: u128,
    pub facts_us: u128,
    pub order_us: u128,
    pub saturate_us: u128,
    pub extract_us: u128,
    /// The class each IR value is.
    pub values: Vec<(ValueId, Ref)>,
}

impl Select<'_> {
    /// Print what the graph holds after extraction: the rows the rules
    /// inserted, what each class was extracted as, and the operations no
    /// accepted chain reached, each with one row of it and the chains of its
    /// operands.
    pub fn report(
        &self,
        context: &Context,
        eg: &Engine<SemNode>,
        extraction: &Extraction<'_, SemNode>,
        function: &str,
        trace: &Trace,
    ) {
        let named = |node: &SemNode| {
            let ty = node.ty.map(|ty| context.type_to_string(ty));
            let ty = ty.unwrap_or_default();
            match &node.kind {
                Kind::Ir(op) => format!("{}.{}:{ty}", op.dialect, op.name),
                kind => format!("{kind:?}:{ty}"),
            }
        };
        let states = |class: ClassId| -> Vec<u32> {
            let chains = eg.nodes(class).filter_map(|node| match node.kind {
                Kind::State(state) => Some(state),
                _ => None,
            });
            chains.collect()
        };
        let (mut chains, mut forms) = (0, 0);
        let [
            mut accepted,
            mut kept,
            mut constant,
            mut formed,
            mut starved,
            mut bare,
        ] = [0usize; 6];
        let mut uncovered = std::collections::BTreeMap::new();
        let mut keeping = std::collections::BTreeMap::new();
        let mut folds = 0;
        for class in eg.class_ids() {
            for node in eg.nodes(class) {
                match node.kind {
                    Kind::State(state) if (state as usize) >= self.automaton.seeds.len() => {
                        chains += 1
                    }
                    Kind::Form(_) => forms += 1,
                    _ => {}
                }
            }
            let Some(row) = extraction.row(class) else {
                bare += 1;
                continue;
            };
            let node = eg.node(row);
            let own = self.price(eg, class, node, &eg.children(row));
            if own == u64::MAX {
                let shown = eg.rows(class).find(|&row| selectable(eg.node(row)));
                let shown = shown.unwrap_or(row);
                let operands = eg.children(shown);
                let operands: Vec<_> = operands
                    .iter()
                    .map(|operand| (operand.offset, states(operand.class)))
                    .collect();
                let why = self.unreached(eg, shown);
                let example = format!("{why}: label {} over {operands:?}", eg.label(shown).0);
                let entry = uncovered
                    .entry(named(eg.node(shown)))
                    .or_insert((0usize, example));
                entry.0 += 1;
                continue;
            }
            if extraction.cost(class) == Some(u64::MAX) {
                starved += 1;
            }
            match node.kind {
                Kind::State(_) => {
                    accepted += 1;
                    folds += self.folds(eg, eg.children(row)[0].class);
                }
                Kind::Form(_) => formed += 1,
                Kind::Sym(SymKind::Constant) => constant += 1,
                _ => {
                    kept += 1;
                    *keeping.entry(named(node)).or_insert(0usize) += 1;
                }
            }
        }
        let missed: usize = uncovered.values().map(|&(count, _)| count).sum();
        eprintln!(
            "isel-select: {function} rows {} seeds {} chains {chains} forms {forms}; classes accepted {accepted} kept {kept} constant {constant} form {formed} uncovered {missed} unextracted {bare}, over an uncovered operand {starved}; us seed {} chains {} facts {} saturate {} extract {} order {}",
            trace.rows,
            trace.seeds,
            trace.seed_us,
            trace.chains_us,
            trace.facts_us,
            trace.saturate_us,
            trace.extract_us,
            trace.order_us,
        );
        for (op, (count, example)) in &uncovered {
            eprintln!("isel-uncovered: {function} {op} x{count}: {example}");
        }
        for (op, count) in &keeping {
            eprintln!("isel-kept: {function} {op} x{count}");
        }
        self.report_folds(context, eg, function, &trace.values, folds);
    }

    // How many accesses are folded into the form in `class`.
    fn folds(&self, eg: &Engine<SemNode>, class: ClassId) -> usize {
        let Some(row) = eg.rows(class).next() else {
            return 0;
        };
        let Kind::Form(pattern) = eg.node(row).kind else {
            return 0;
        };
        let children = eg.children(row);
        let below: usize = children.iter().map(|held| self.folds(eg, held.class)).sum();
        below + usize::from(self.rules.folding.binary_search(&pattern).is_ok())
    }

    // Why no accepted chain reached `row`, a row of an operator the tables
    // have.
    fn unreached(&self, eg: &Engine<SemNode>, row: tir_egraph::RowId) -> &'static str {
        let automaton = self.automaton;
        let label = eg.label(row).0;
        if self.rules.labels.binary_search(&label).is_err() {
            return "its label is in no table";
        }
        let states = |class: ClassId| {
            eg.nodes(class).filter_map(|node| match node.kind {
                Kind::State(state) => Some(state as usize),
                _ => None,
            })
        };
        let own = || states(eg.value(row).class);
        if own().any(|state| automaton.accept[state].is_some()) {
            return "an operand of its tree is uncovered";
        }
        if own().any(|state| state >= automaton.seeds.len()) {
            return "its patterns accept nothing";
        }
        let chains = |class: ClassId| states(class).map(|state| automaton.chains[state]);
        let operands = eg.children(row);
        let families = [
            &automaton.step,
            &automaton.step_effect,
            &automaton.step_over,
            &automaton.step_tied,
        ];
        let tables = families
            .iter()
            .filter_map(|family| family.get(operands.len()));
        let keyed = tables.into_iter().any(|table| {
            (0..table.len()).any(|at| {
                let held = |(child, operand): (usize, &Ref)| {
                    chains(operand.class).any(|chain| chain == table.column(child + 1)[at])
                };
                table.column(0)[at] == label && operands.iter().enumerate().all(held)
            })
        });
        match keyed {
            true => "an offset fits no class of its key",
            false => "no step is keyed by its operands' states",
        }
    }

    // How the two facts settle each class a reader may fold an access of:
    // foldable, read more than once, read in another region, or several
    // accesses in several regions that are one class. The last are refused
    // although each has one reader where it sits; `alike` counts those whose
    // readers are one class too. `ordered` of the foldable ones observe a
    // state with a depth, and `extracted` accesses are folded into the trees
    // the extraction chose.
    fn report_folds(
        &self,
        context: &Context,
        eg: &Engine<SemNode>,
        function: &str,
        values: &[(ValueId, Ref)],
        extracted: usize,
    ) {
        let foldable = self.automaton.step_effect.iter();
        let foldable: Vec<u32> = foldable
            .flat_map(|table| table.column(0).to_vec())
            .collect();
        let classes: HashMap<ValueId, ClassId> = values
            .iter()
            .map(|&(value, reference)| (value, eg.root(reference.class)))
            .collect();
        let mut held: HashMap<ClassId, Vec<ValueId>> = HashMap::new();
        for (&value, &class) in &classes {
            if context.get_value(value).defining_op().is_some() {
                held.entry(class).or_default().push(value);
            }
        }
        let region = |value: ValueId| crate::region::defining_region(context, value);
        let once = |&value: &ValueId| {
            let read = context.uses_of(value);
            let local = |read: &tir::Use| context.region_of_op(read.op) == region(value);
            read.len() == 1 && context.result_uses(value).is_empty() && local(&read[0])
        };
        let reader = |&value: &ValueId| {
            let defined = context.get_op(context.uses_of(value)[0].op).results();
            defined.first().and_then(|defined| classes.get(defined))
        };
        let [mut folded, mut shared, mut elsewhere, mut twins, mut alike] = [0usize; 5];
        let mut ordered = 0;
        for class in eg.class_ids() {
            if !eg
                .rows(class)
                .any(|row| foldable.contains(&eg.label(row).0))
            {
                continue;
            }
            if eg.fact(USES, class) == Some(1) && eg.fact(REGION, class).is_some() {
                folded += 1;
                ordered += usize::from(eg.fact(DEPTH, class).is_some());
                continue;
            }
            let values = held.get(&class).map_or(&[][..], Vec::as_slice);
            let regions: std::collections::BTreeSet<_> =
                values.iter().map(|&value| region(value)).collect();
            if !values.iter().all(once) {
                let several = |&value: &ValueId| {
                    context.uses_of(value).len() + context.result_uses(value).len() != 1
                };
                match values.iter().any(several) {
                    true => shared += 1,
                    false => elsewhere += 1,
                }
            } else if regions.len() > 1 {
                twins += 1;
                let readers: std::collections::BTreeSet<_> = values.iter().map(reader).collect();
                alike += usize::from(readers.len() == 1 && !readers.contains(&None));
            } else {
                elsewhere += 1;
            }
        }
        eprintln!(
            "isel-folds: {function} accesses foldable {folded} of which ordered {ordered} read more than once {shared} read in another region {elsewhere} twins {twins} with one reader class {alike}; folded in the extracted trees {extracted}"
        );
    }
}

/// What an operand of a tree is bound to.
#[derive(Clone, Debug)]
pub enum Bound {
    Register(ValueId),
    Immediate(APInt),
}

/// What a tree's operands are bound to, by the number its rule gives each.
pub type Operands = SmallVec<[Option<Bound>; 4]>;

/// The state ports one instruction takes over.
pub type Ports = SmallVec<[StatePorts; 1]>;

/// The values the last instruction of a tree defines, by result.
pub type Defined = SmallVec<[(usize, ValueId); 2]>;

// How an instruction reads an operand of its rule: the constant it encodes,
// or the register of a reference.
pub(super) enum Slot {
    Immediate(APInt),
    Register(Ref),
}

/// What the commit emitted, as control recovery reads it.
#[derive(Default)]
pub struct Emitted {
    /// Per instruction, the instructions it runs after besides those
    /// defining its operands: the ones of its rule emitted ahead of it,
    /// which it reads through registers nothing names.
    pub implicit: IdMap<OpId, Vec<OpId>>,
    /// The integer each built literal holds.
    pub literals: IdMap<ValueId, u64>,
    /// The type each emitted value, and each value an instruction reads,
    /// stood for in the source.
    pub types: IdMap<ValueId, TypeId>,
    // The first error of an emitter, which a spelling cannot carry.
    failed: Option<PassError>,
    // The operations whose state ports an instruction took over.
    taken: IdSet<OpId>,
    // The IR value whose spelling is being written.
    spelled: Option<ValueId>,
    // What an instruction defining several values defined, by its rule and
    // operands: every value of it some chain is comes from the one.
    several: HashMap<(usize, SmallVec<[u64; 4]>), Defined>,
}

impl Emitted {
    /// Nothing emitted yet, where the operations `implicit` names already
    /// run after the ones it lists for them.
    pub fn after(implicit: impl IntoIterator<Item = (OpId, Vec<OpId>)>) -> Self {
        Self {
            implicit: implicit.into_iter().collect(),
            ..Self::default()
        }
    }
}

/// Where the instructions of a tree go and what they are given.
pub struct Emit<'s> {
    pub context: &'s Context,
    /// The source type of the value the tree computes.
    pub ty: TypeId,
    /// By instruction of the tree, the state ports it takes over.
    pub ports: &'s [Ports],
    /// The block a branch tree's last instruction branches to.
    pub target: Option<BlockId>,
    /// The lane count the tree's row holds, for [`Arg::Lanes`].
    pub lanes: Option<u64>,
    /// Takes each instruction and the one of its rule emitted before it.
    pub place: &'s mut dyn FnMut(OpId, Option<OpId>),
}

fn ones(bits: u32) -> u64 {
    match bits {
        64.. => u64::MAX,
        bits => (1u64 << bits) - 1,
    }
}

// Whether the offset `delta` is of `class`, the last cells of an `imm` row.
pub(super) fn fits(class: &[u32; 8], delta: u64) -> bool {
    let [width, add, sub, high, sign, bits, align, nonzero] = class.map(u64::from);
    let moved = delta.wrapping_add(add).wrapping_sub(sub);
    let moved = moved.wrapping_add(ones(high as u32)).wrapping_add(sign) & ones(width as u32);
    moved == moved & ones(bits as u32) && delta & align == 0 && !(nonzero != 0 && delta == 0)
}

// Whether `read` is a gate reading its predicate, the one operand it does
// not forward to its arms.
fn decides(context: &Context, read: &Use) -> bool {
    let gate = context.with_rows(|rows| rows.op_cells(read.op).owns_regions());
    let gate = gate.then(|| context.get_op(read.op).as_interface::<dyn Gamma>());
    gate.flatten()
        .is_some_and(|gate| !gate.binding().operands.contains(&read.index))
}

// How many of the slots `read` and `published`, the readers of a boolean,
// read it from a register.
fn data(context: &Context, read: &[Use], published: &[(RegionId, usize)]) -> usize {
    let data = |read: &&Use| !decides(context, read);
    let yielded = |&&(region, index): &&(RegionId, usize)| !repeats(context, region, index);
    read.iter().filter(data).count() + published.iter().filter(yielded).count()
}

// Whether result `index` of `region` is the repeat predicate of the loop
// whose body `region` is.
fn repeats(context: &Context, region: RegionId, index: usize) -> bool {
    let owner = context.get_region(region).parent_op();
    let theta = owner.and_then(|op| context.get_op(op).as_interface::<dyn Theta>());
    theta.is_some_and(|theta| {
        let binding = theta.binding();
        !binding.continue_.contains(&index) && !binding.exit.contains(&index)
    })
}

// The offset nearest `delta`, `width` bits wide, that `class` admits where
// `nearest`, and otherwise the one that agrees with `delta` in the bits the
// class holds, which leaves a multiple of the field's reach.
fn part(class: &[u32; 8], delta: u64, width: u32, nearest: bool) -> u64 {
    let [_, add, sub, high, sign, bits, align, _] = class.map(u64::from);
    let bias = add.wrapping_sub(sub).wrapping_add(ones(high as u32));
    let bias = bias.wrapping_add(sign);
    let moved = match (nearest, signed_bits(delta, width) < 0) {
        (false, _) => delta.wrapping_add(bias) & ones(bits as u32),
        (true, true) => 0,
        (true, false) => ones(bits as u32),
    };
    let part = moved.wrapping_sub(bias);
    part.wrapping_sub(part & align) & ones(width)
}

// What is left of `offset`, `width` bits wide, past the part of it a field
// of `class` holds.
fn rest(class: &[u32; 8], offset: u64, width: u32) -> u64 {
    let held = part(class, offset, width, false);
    match fits(class, held) {
        true => offset.wrapping_sub(held) & ones(width),
        false => offset,
    }
}

fn changes(instance: &OpHandle) -> bool {
    let effects = instance.clone().as_interface::<dyn ResourceEffects>();
    effects.is_some_and(|effects| {
        let mut effects = effects.resource_effects().into_iter();
        effects.any(|effect| effect.access == ResourceAccess::Change)
    })
}

fn unselected(instance: &OpHandle, why: &str) -> PassError {
    let name = format!("{}.{}", instance.dialect(), instance.name());
    let mut written = String::new();
    let mut fmt = tir::IRFormatter::new(&mut written);
    let _ = crate::region_format::print_generic(&mut fmt, instance, &name);
    PassError::InvalidRuleSet(format!("{name} {why}: {}", written.trim()))
}

fn form_of(eg: &Engine<SemNode>, class: ClassId) -> Option<RowId> {
    let row = eg.rows(class).next()?;
    matches!(eg.node(row).kind, Kind::Form(_)).then_some(row)
}

impl Commit<'_> {
    fn fail<T>(&self, error: PassError) -> Option<T> {
        self.emitted.borrow_mut().failed.get_or_insert(error);
        None
    }

    // Whether selection replaces `instance`: an operation the rules were
    // synthesized for, or a constant.
    pub(super) fn replaces(&self, context: &Context, instance: &OpHandle) -> bool {
        let name = context.op_name_id(instance.id).index() as u32;
        self.kinds.binary_search(&name).is_ok() || instance.has_interface::<dyn ConstantLike>()
    }

    // What a row that stays is to the reader that names it: the value
    // being spelled, where it stays, is the row and `region` can read it;
    // or the bits of the constant the row carries, as a loop's port does,
    // where the value is out of sight. `None` where the commit's own
    // spelling of the row is the answer.
    fn stays(
        &self,
        out: &rewrite::Commit<'_>,
        row: RowId,
        region: RegionId,
    ) -> Option<Result<ValueId, APInt>> {
        let (context, eg) = (out.context, out.eg);
        let named = self.emitted.borrow().spelled?;
        let held = eg.find(*out.value_class.get(&named)?);
        let stays = context
            .get_value(named)
            .defining_op()
            .is_none_or(|op| !self.replaces(context, &context.get_op(op)));
        if stays && held == eg.value(row) && out.readable(named, region) {
            return Some(Ok(named));
        }
        let seen = match eg.node(row).prov {
            Prov::Value(value) => out.readable(value, region),
            _ => true,
        };
        let constants = eg.nodes(eg.value(row).class);
        let mut constants = constants.filter(|node| node.kind == SymKind::Constant);
        let bits = constants.find_map(SemNode::bits).filter(|_| !seen)?;
        Some(Err(bits))
    }

    // `base`, a register of type `ty`, moved by `delta`, an offset `width`
    // bits wide, by the adders; `None` where the target has none for it.
    fn add_offset(
        &self,
        out: &rewrite::Commit<'_>,
        base: ValueId,
        delta: u64,
        width: u32,
        ty: TypeId,
        region: RegionId,
    ) -> Option<ValueId> {
        let context = out.context;
        let seed = Seed::Register(canonical_type(context, ty));
        let seeds = self.select.automaton.seeds.iter();
        let seed = seeds
            .zip(0..)
            .find_map(|(known, at)| (*known == seed).then_some(at))?;
        let (sum, _) = self.select.sum(seed, delta & ones(width), width)?;
        let mut place = self.place(out, region);
        let mut moved = base;
        for (adder, by) in sum {
            let field = |by: u64| APInt::new_signed(u64::from(width), signed_bits(by, width));
            let bound = adder.accept.operands.iter().map(|bind| match bind.held {
                Held::Offset | Held::Constant => Some(Bound::Immediate(field(by))),
                Held::Negated => Some(Bound::Immediate(field(by.wrapping_neg()))),
                Held::Register | Held::Base => Some(Bound::Register(moved)),
            });
            let bound: Operands = bound.collect();
            let emit = Emit {
                context,
                ty,
                ports: &[],
                target: None,
                lanes: None,
                place: &mut place,
            };
            let emitted = self.value(emit, &adder.accept, &bound);
            moved = emitted.unwrap_or_else(|error| self.fail(error))?;
        }
        Some(moved)
    }

    fn place<'c>(
        &'c self,
        out: &'c rewrite::Commit<'_>,
        region: RegionId,
    ) -> impl FnMut(OpId, Option<OpId>) + 'c {
        move |op, before| {
            out.add(region, op);
            if let Some(before) = before {
                let mut emitted = self.emitted.borrow_mut();
                emitted.implicit.entry(op).or_default().push(before);
            }
        }
    }

    pub(super) fn built(
        &self,
        context: &Context,
        value: &APInt,
        ty: TypeId,
        place: &mut dyn FnMut(OpId, Option<OpId>),
    ) -> Result<Option<ValueId>, PassError> {
        if tir::builtin::FloatType::view(context, ty).is_some() {
            return self.float(context, value, ty, place);
        }
        let Some(built) = self.select.plan(value) else {
            return Ok(None);
        };
        let mut made = None;
        for (builder, immediate) in built.steps {
            let values = builder.register.zip(made).into_iter().collect();
            let matched = RuleMatch::new(vec![(builder.symbol, immediate)], values);
            made = Some(self.made(context, builder.form, &matched, ty, place)?);
        }
        if let (Some(made), true) = (made, value.width() <= 64) {
            let mut emitted = self.emitted.borrow_mut();
            emitted.literals.insert(made, value.to_u64());
        }
        Ok(made)
    }

    fn float(
        &self,
        context: &Context,
        bits: &APInt,
        ty: TypeId,
        place: &mut dyn FnMut(OpId, Option<OpId>),
    ) -> Result<Option<ValueId>, PassError> {
        let integer = context.terms().int(bits.width());
        let moves = |&&(form, symbol, width): &&(u32, u32, u32)| {
            let rule = &self.target[self.select.automaton.forms[form as usize].rule];
            let mut registers = rule.operand_registers.iter();
            let read = registers.find(|(known, _)| *known == symbol);
            u64::from(width) == bits.width()
                && read.is_none_or(|(_, register)| {
                    register.accepts(context.terms(), &TypeArg::Type(integer))
                })
        };
        let Some(&(form, symbol, _)) = self.select.automaton.floats.iter().find(moves) else {
            return Ok(None);
        };
        let Some(held) = self.built(context, bits, integer, place)? else {
            return Ok(None);
        };
        let matched = RuleMatch::new(Vec::new(), vec![(symbol, held)]);
        self.made(context, form, &matched, ty, place).map(Some)
    }

    // Emit the rule of `form` as `matched` binds it: the value of `ty` it
    // defines.
    fn made(
        &self,
        context: &Context,
        form: u32,
        matched: &RuleMatch,
        ty: TypeId,
        place: &mut dyn FnMut(OpId, Option<OpId>),
    ) -> Result<ValueId, PassError> {
        let form = &self.select.automaton.forms[form as usize];
        let rule = &self.target[form.rule];
        let mut results = vec![None; (rule.secondary.len() + 1).max(form.port + 1)];
        results[form.port] = Some(context.create_value(ty, None).id());
        let request = EmitRequest {
            op: None,
            results: &results,
            result_ty: Some(ty),
            states: &[],
        };
        let written = Self::emit(context, rule, &request, matched, place)?;
        let made = written.iter().find(|&&(port, _)| port == form.port);
        let made = made.map(|&(_, value)| value);
        made.ok_or_else(|| PassError::InvalidRuleSet(format!("{} defines no value", rule.name)))
    }

    fn emit(
        context: &Context,
        rule: &super::Rule,
        request: &EmitRequest,
        matched: &RuleMatch,
        place: &mut dyn FnMut(OpId, Option<OpId>),
    ) -> Result<Vec<(usize, ValueId)>, PassError> {
        let mut written = Vec::new();
        let mut before = None;
        for emitter in &rule.emit {
            let (op, defines) = emitter.emit(context, request, matched).map_err(|error| {
                PassError::InvalidRuleSet(format!("{} failed to emit: {error:?}", rule.name))
            })?;
            place(op, before.replace(op));
            written = defines;
        }
        Ok(written)
    }

    pub(super) fn steps(
        &self,
        emit: Emit<'_>,
        tree: &Tree,
        bound: &[Option<Bound>],
    ) -> Result<Defined, PassError> {
        let (context, automaton) = (emit.context, self.select.automaton);
        let mut defined: SmallVec<[ValueId; 2]> = SmallVec::new();
        let mut written = Vec::new();
        for (index, step) in tree.iter().enumerate() {
            let form = &automaton.forms[step.form as usize];
            let rule = &self.target[form.rule];
            let missing = |what: &str| {
                PassError::InvalidRuleSet(format!("{} reads {what} nothing spells", rule.name))
            };
            let (mut ints, mut values) = (Vec::new(), Vec::new());
            let roles = form.ranges.iter().zip(&form.registers);
            for ((&symbol, (range, &register)), &arg) in
                form.symbols.iter().zip(roles).zip(&step.args)
            {
                let encoded = range.is_some() || !register;
                let arg = match arg {
                    Arg::Lanes => {
                        let lanes = emit.lanes.ok_or_else(|| missing("a lane count"))?;
                        Arg::Literal(lanes, 32)
                    }
                    arg => arg,
                };
                match arg {
                    Arg::Operand(operand) => match (bound.get(usize::from(operand)), encoded) {
                        (Some(Some(Bound::Immediate(value))), true) => {
                            ints.push((symbol, value.clone()));
                        }
                        (Some(Some(Bound::Register(value))), false) => {
                            values.push((symbol, *value));
                        }
                        // A constant the instruction reads from a register.
                        (Some(Some(Bound::Immediate(value))), false) => {
                            let held = context.terms().int(value.width());
                            let built = self.built(context, value, held, emit.place)?;
                            values.push((symbol, built.ok_or_else(|| missing("a constant"))?));
                        }
                        _ => return Err(missing("an operand")),
                    },
                    Arg::Step(earlier) => values.push((symbol, defined[usize::from(earlier)])),
                    Arg::Literal(bits, width) if (1..=64).contains(&width) => {
                        // A width or a size is read unsigned; any other
                        // literal is two's complement, and one an
                        // instruction reads from a register is built.
                        let literal = match encoded && range.is_none() {
                            true => APInt::new(u64::from(width), bits & (u64::MAX >> (64 - width))),
                            false => APInt::new_signed(u64::from(width), signed_bits(bits, width)),
                        };
                        if !encoded {
                            let held = context.terms().int(u64::from(width));
                            let built = self.built(context, &literal, held, emit.place)?;
                            values.push((symbol, built.ok_or_else(|| missing("a literal"))?));
                        }
                        ints.push((symbol, literal));
                    }
                    Arg::Literal(..) => return Err(missing("a literal wider than a register")),
                    Arg::Lanes => unreachable!("read as the literal above"),
                }
            }
            let last = index + 1 == tree.len();
            let branching = emit.target.filter(|_| last);
            let stateful = context.is_state_type(emit.ty);
            let mut results = Vec::new();
            if branching.is_none() {
                results = vec![None; (rule.secondary.len() + 1).max(form.port + 1)];
                // What an instruction ahead of the last defines is a
                // register's worth of bits, whatever the row is.
                let held = match last {
                    true => (!stateful).then_some(emit.ty),
                    false => Some(context.terms().int(64)),
                };
                let slot = held.map(|ty| context.create_value(ty, None).id());
                match last {
                    true => results.fill(slot),
                    false => results[form.port] = slot,
                }
            }
            let request = EmitRequest {
                op: None,
                results: &results,
                result_ty: (!stateful && branching.is_none()).then_some(emit.ty),
                states: emit.ports.get(index).map_or(&[], |ports| ports),
            };
            let mut matched = RuleMatch::new(ints, values);
            if let (Some(target), Some(symbol)) = (branching, form.branch) {
                matched = matched.with_block_binding(symbol, target);
            }
            written = Self::emit(context, rule, &request, &matched, emit.place)?;
            if !last {
                let value = written.iter().find(|&&(port, _)| port == form.port);
                defined.push(value.ok_or_else(|| missing("a step's value"))?.1);
            }
        }
        Ok(written.into_iter().collect())
    }

    // What a reader of the row `accept` is for reads: the value its tree
    // defines over `bound`, the operand the row is, or the constant it is.
    fn value(
        &self,
        emit: Emit<'_>,
        accept: &Accept,
        bound: &[Option<Bound>],
    ) -> Result<Option<ValueId>, PassError> {
        let (context, ty) = (emit.context, emit.ty);
        match accept.value {
            Arg::Step(_) => {
                let defined = self.steps(emit, &accept.tree, bound)?;
                let port = usize::from(accept.port);
                Ok(defined
                    .iter()
                    .find(|&&(at, _)| at == port)
                    .map(|&(_, value)| value))
            }
            Arg::Operand(operand) => match bound.get(usize::from(operand)) {
                Some(Some(Bound::Register(value))) => Ok(Some(*value)),
                Some(Some(Bound::Immediate(value))) => self.built(context, value, ty, emit.place),
                _ => Ok(None),
            },
            Arg::Literal(bits, width) => {
                let literal = APInt::new_signed(u64::from(width), signed_bits(bits, width));
                self.built(context, &literal, ty, emit.place)
            }
            Arg::Lanes => Ok(None),
        }
    }

    // Run `spell` as the spelling of the IR value `value`.
    fn spelling<T>(&self, value: Option<ValueId>, spell: impl FnOnce() -> T) -> T {
        let spelled = |value| std::mem::replace(&mut self.emitted.borrow_mut().spelled, value);
        let outer = spelled(value);
        let written = spell();
        spelled(outer);
        written
    }

    // The operation whose row `value`, an IR value, is a reference to: the
    // one defining it, or behind the operations between them that are no
    // rows, each an offset of an operand in its own class as the add of a
    // constant is.
    fn source(&self, out: &rewrite::Commit<'_>, mut value: ValueId) -> Option<OpId> {
        let (context, eg) = (out.context, out.eg);
        let class = |value: &ValueId| Some(eg.find(*out.value_class.get(value)?).class);
        let row = class(&value)?;
        loop {
            let op = context.get_value(value).defining_op()?;
            let mut operands = children(context, &context.get_op(op)).into_iter().flatten();
            match operands.find(|operand| class(operand) == Some(row)) {
                Some(base) => value = base,
                None => return Some(op),
            }
        }
    }

    // Whether `child`, a child of a form, is the IR value `operand`: a
    // reference to its class, the value the commit spelled the child as,
    // which an earlier rewire handed the operation, or the form of the
    // row it refers to. A value the commit wrote is no evidence against.
    fn stands(&self, out: &rewrite::Commit<'_>, child: Ref, operand: ValueId) -> bool {
        let eg = out.eg;
        let Some(read) = out.value_class.get(&operand) else {
            return true;
        };
        let Some(form) = form_of(eg, child.class) else {
            let spelled = out.memo.get(&eg.find(child)) == Some(&operand);
            return spelled || eg.find(*read).class == child.class;
        };
        let folded = self.source(out, operand);
        folded.is_some_and(|source| self.operands(out, form, source).is_some())
    }

    // The operand of `source` each child of `form` stands for, where `form`
    // is the form of its row: the seeder's, or for a row whose label
    // commutes, which matches in either order, those exchanged.
    fn operands(
        &self,
        out: &rewrite::Commit<'_>,
        form: RowId,
        source: OpId,
    ) -> Option<SmallVec<[Option<ValueId>; 5]>> {
        let (context, eg) = (out.context, out.eg);
        let held = eg.children(form);
        let mut operands = children(context, &context.get_op(source));
        let fits = |operands: &[Option<ValueId>]| {
            let stands = |(child, operand): (&Ref, &Option<ValueId>)| {
                operand.is_none_or(|operand| self.stands(out, *child, operand))
            };
            operands.len() == held.len() && held.iter().zip(operands).all(stands)
        };
        if fits(&operands) {
            return Some(operands);
        }
        let instance = context.get_op(source);
        let named = (instance.dialect().as_str(), instance.name().as_str());
        let row = instance.results().first().copied();
        let row = row.and_then(|value| out.value_class.get(&value));
        let mut labels = row
            .into_iter()
            .flat_map(|row| eg.nodes(eg.find(*row).class));
        let commutes = labels.any(|node| match &node.kind {
            Kind::Ir(op) => (op.dialect, op.name) == named && op.commutative,
            _ => false,
        });
        if !commutes {
            return None;
        }
        operands.swap(0, 1);
        fits(&operands).then_some(operands)
    }

    // The operations the rows of a tree are, each with the path of its
    // row's form, the root's first: the operation `value` refers to the row
    // of, where `form` is that row's form, and for a row folded at a child
    // of it the one the operand that child stands for refers to. `read`
    // gets the operand every other child stands for, by the child's path.
    fn sources(
        &self,
        out: &rewrite::Commit<'_>,
        value: ValueId,
        (form, path): (RowId, &Path),
        sources: &mut SmallVec<[(Path, OpId); 2]>,
        read: &mut SmallVec<[(Path, ValueId); 4]>,
    ) {
        let eg = out.eg;
        let Some(source) = self.source(out, value) else {
            return;
        };
        let Some(operands) = self.operands(out, form, source) else {
            return;
        };
        sources.push((path.clone(), source));
        for (at, (child, operand)) in eg.children(form).iter().zip(operands).enumerate() {
            let Some(operand) = operand else {
                continue;
            };
            let mut under = path.clone();
            under.push(at as u8);
            match form_of(eg, child.class) {
                Some(below) => self.sources(out, operand, (below, &under), sources, read),
                None => read.push((under, operand)),
            }
        }
    }

    // The state ports an instruction covering `sources` takes over: what
    // each observes and a state of its own for what each publishes, a
    // state one of them publishes and another observes dropped. Beside a
    // write, a read of the same resource is no port: the instruction
    // observes what the write does, which the read's state leads to. The
    // states whose readers are handed another go to `renamed`, as
    // `(old, new)`.
    fn ports(
        context: &Context,
        sources: impl Iterator<Item = OpId>,
        renamed: &mut SmallVec<[(ValueId, ValueId); 2]>,
    ) -> Ports {
        // What is observed and published, of which resource, and whether
        // the source changes it.
        type Port = (ValueId, Option<ValueId>, tir::builtin::StateResource, bool);
        let mut held: SmallVec<[Port; 2]> = SmallVec::new();
        for source in sources {
            let effects = context.get_op(source).as_interface::<dyn ResourceEffects>();
            let effects = effects.map(|effects| effects.resource_effects());
            for effect in effects.unwrap_or_default() {
                if let Some(&observed) = effect.observed.first() {
                    let published = effect.produced.first().copied();
                    let writes = effect.access == ResourceAccess::Change;
                    let port = (observed, published, effect.resource, writes);
                    if !held.contains(&port) {
                        held.push(port);
                    }
                }
            }
        }
        let internal = |held: &[Port]| {
            let observed = |state: &ValueId| held.iter().any(|port| port.0 == *state);
            let published = |at: usize| held[at].1.filter(observed);
            (0..held.len()).find_map(|at| published(at).map(|state| (at, state)))
        };
        while let Some((at, state)) = internal(&held) {
            let (observed, ..) = held.remove(at);
            renamed.push((state, observed));
            for port in &mut held {
                if port.0 == state {
                    port.0 = observed;
                }
            }
        }
        let written = held.iter().filter(|port| port.3);
        let written: SmallVec<[_; 1]> = written.map(|port| port.2).collect();
        held.retain(|&mut (observed, published, resource, writes)| {
            let read = !writes && written.contains(&resource);
            if let (true, Some(published)) = (read, published) {
                renamed.push((published, observed));
            }
            !read
        });
        let mut own = |old: ValueId| {
            let fresh = context.create_value(context.get_value(old).ty(), None).id();
            renamed.push((old, fresh));
            fresh
        };
        let ports = held
            .into_iter()
            .map(|(observed, published, ..)| StatePorts {
                observed,
                published: published.map(&mut own),
            });
        ports.collect()
    }

    // The reference the form `form` holds at `path`, and the offsets the
    // field there admits where it holds a part of any offset.
    fn child(
        &self,
        eg: &Engine<SemNode>,
        form: RowId,
        path: &[u8],
    ) -> Option<(Ref, Option<[u32; 8]>)> {
        let (&last, above) = path.split_last()?;
        let mut row = form;
        for &at in above {
            row = form_of(eg, eg.children(row).get(usize::from(at))?.class)?;
        }
        let Kind::Form(pattern) = eg.node(row).kind else {
            return None;
        };
        let children = &self.select.automaton.children[pattern as usize];
        let field = children.get(usize::from(last)).and_then(|child| child.1);
        Some((*eg.children(row).get(usize::from(last))?, field))
    }

    pub(super) fn slots<'t>(
        &self,
        eg: &Engine<SemNode>,
        form: RowId,
        accept: &Accept,
        trees: impl IntoIterator<Item = &'t Tree>,
        own: bool,
    ) -> Result<SmallVec<[(u8, Slot); 4]>, &'static str> {
        let automaton = self.select.automaton;
        let mut read: SmallVec<[(u8, bool); 4]> = SmallVec::new();
        for step in trees.into_iter().flatten() {
            let form = &automaton.forms[step.form as usize];
            let roles = form.ranges.iter().zip(&form.registers);
            for ((range, &register), arg) in roles.zip(&step.args) {
                if let Arg::Operand(operand) = *arg {
                    read.push((operand, range.is_some() || !register));
                }
            }
        }
        if let (Arg::Operand(operand), true) = (accept.value, own) {
            read.push((operand, false));
        }
        let mut slots: SmallVec<[(u8, Slot); 4]> = SmallVec::new();
        for (operand, encoded) in read {
            if slots.iter().any(|&(known, _)| known == operand) {
                continue;
            }
            let bind = accept.operands.get(usize::from(operand));
            let bind = bind.ok_or("an operand no child binds")?;
            let child = self.child(eg, form, &bind.path);
            let (child, field) = child.ok_or("an operand under no form")?;
            let width = eg.width(child.class).unwrap_or(64);
            let constant =
                |bits: u64| APInt::new_signed(u64::from(width), signed_bits(bits, width));
            // A field holds the offset, or the part of it that it can: the
            // base is then the register at the rest.
            let rest = field.map_or(0, |field| rest(&field, child.offset, width));
            let slot = match (encoded, bind.held) {
                (true, Held::Offset) => Slot::Immediate(constant(child.offset.wrapping_sub(rest))),
                (true, _) => {
                    let bits = eg.int_const(child);
                    Slot::Immediate(constant(bits.ok_or("an encoded operand is no constant")?))
                }
                (false, Held::Base) => Slot::Register(Ref::new(child.class, rest)),
                (false, _) => Slot::Register(child),
            };
            slots.push((operand, slot));
        }
        Ok(slots)
    }

    // The register of `read` where `home` can read it, as the spelling of
    // the IR value `named`; the reference is typed by its class, or as an
    // integer of its width where the class has no type.
    fn register(
        &self,
        out: &mut rewrite::Commit<'_>,
        read: Ref,
        named: Option<ValueId>,
        home: RegionId,
    ) -> Option<ValueId> {
        let eg = out.eg;
        let typed = eg.fact(ColumnId::Type, read.class);
        let typed = typed.map(|ty| TypeId::from_number(ty as u32));
        let width = eg.width(read.class).unwrap_or(64);
        let ty = typed.unwrap_or_else(|| out.context.terms().int(u64::from(width)));
        let value = self.spelling(named, || match named {
            Some(named) => out.materialize_as(named, read, ty, home),
            None => out.materialize(read, ty, home),
        })?;
        if typed.is_some() {
            let mut emitted = self.emitted.borrow_mut();
            emitted.types.entry(value).or_insert(ty);
        }
        Some(value)
    }

    pub(super) fn tested(
        &self,
        (eg, classes): (&Engine<SemNode>, &IdMap<ValueId, Ref>),
        extraction: &Extraction<'_, SemNode>,
        value: ValueId,
    ) -> Option<(RowId, Pattern)> {
        let automaton = self.select.automaton;
        let reference = eg.find(*classes.get(&value)?);
        let row = extraction.row(reference.class)?;
        let Kind::State(pattern) = eg.node(row).kind else {
            return None;
        };
        automaton.branch(pattern)?;
        let [over] = eg.children(row)[..] else {
            return None;
        };
        let held = eg.value(row).offset == reference.offset;
        let pure = automaton.carried[pattern as usize] == 0;
        form_of(eg, over.class)
            .filter(|_| held && pure)
            .map(|form| (form, pattern))
    }

    // The value the controls of `region` that branch on `value` read: the
    // one of an operation already left for such a branch that the region
    // can read, which `memo` holds as the spelling of the form, or `value`
    // itself with the registers its operation is to read. `None` where no
    // control branches on the value by its chain.
    fn left(
        &self,
        out: &mut rewrite::Commit<'_>,
        value: ValueId,
        region: RegionId,
    ) -> Option<(ValueId, Option<Vec<ValueId>>)> {
        let tested = self.tested((out.eg, out.value_class), out.extraction, value)?;
        let form = out.eg.value(tested.0);
        let twin = out.memo.get(&form).copied();
        if let Some(twin) = twin.filter(|&twin| twin != value && out.readable(twin, region)) {
            return Some((twin, None));
        }
        let registers = self.registers(out, value, tested, region)?;
        out.memo.insert(form, value);
        Some((value, Some(registers)))
    }

    // The registers the branch on `value` reads, each where `region` can
    // read it, in the order [`Self::slots`] names them, where the chain
    // extracted for the value has the form `form` and the pattern `pattern`.
    fn registers(
        &self,
        out: &mut rewrite::Commit<'_>,
        value: ValueId,
        (form, pattern): (RowId, Pattern),
        region: RegionId,
    ) -> Option<Vec<ValueId>> {
        let (eg, automaton) = (out.eg, self.select.automaton);
        let accept = automaton.accept[pattern as usize].as_ref()?;
        let branch = automaton.branch(pattern)?;
        let mut sources: SmallVec<[(Path, OpId); 2]> = SmallVec::new();
        let mut operands: SmallVec<[(Path, ValueId); 4]> = SmallVec::new();
        let root = (form, &Path::new());
        self.sources(out, value, root, &mut sources, &mut operands);
        let trees = std::iter::once(&branch.taken).chain(&branch.inverse);
        let slots = self.slots(eg, form, accept, trees, false).ok()?;
        let mut registers = Vec::new();
        for (operand, slot) in slots {
            let Slot::Register(read) = slot else {
                continue;
            };
            let path = &accept.operands[usize::from(operand)].path;
            let named = operands.iter().find(|(known, _)| known == path);
            let named = named.map(|&(_, operand)| operand);
            registers.push(self.register(out, read, named, region)?);
        }
        Some(registers)
    }

    // Spell the chain `row` of pattern `state`: its form's operands bound
    // as the instructions of the accepted tree read them, then the tree.
    fn chain(
        &self,
        out: &mut rewrite::Commit<'_>,
        row: RowId,
        state: Pattern,
        region: RegionId,
    ) -> Result<Option<ValueId>, PassError> {
        let (context, eg, automaton) = (out.context, out.eg, self.select.automaton);
        let (_, expected_ty) = out.wanted();
        let trace = std::env::var_os("TIR_ISEL_TRACE").is_some();
        let refused = |why: &str| {
            if trace {
                eprintln!(
                    "isel-refused: pattern {state} in {:?}: {why}",
                    eg.value(row)
                );
            }
            Ok(None)
        };
        let Some(accept) = automaton.accept[state as usize].as_ref() else {
            return refused("it accepts nothing");
        };
        let [over] = eg.children(row)[..] else {
            return refused("a chain over no form");
        };
        let Some(form) = form_of(eg, over.class) else {
            return refused("a chain over no form");
        };
        let mut sources: SmallVec<[(Path, OpId); 2]> = SmallVec::new();
        let mut operands: SmallVec<[(Path, ValueId); 4]> = SmallVec::new();
        let spelled = self.emitted.borrow().spelled;
        let values = out.value_class;
        let own = |value: &ValueId| {
            let held = values.get(value);
            held.is_some_and(|&held| eg.find(held).class == eg.value(row).class)
        };
        if let Some(value) = spelled.filter(own) {
            let root = (form, &Path::new());
            self.sources(out, value, root, &mut sources, &mut operands);
        }
        let lanes = accept.lanes.as_ref();
        let lanes = lanes.and_then(|path| eg.int_const(self.child(eg, form, path)?.0));
        // An instruction threads the states its arguments past the form's
        // own name: the ports of the row whose form holds each. A state no
        // instruction names passes through the last one.
        let mut renamed = SmallVec::new();
        let mut named: SmallVec<[SmallVec<[usize; 2]>; 2]> = SmallVec::new();
        for step in &accept.tree {
            let symbols = automaton.forms[step.form as usize].symbols.len();
            let states = step.args.iter().skip(symbols).filter_map(|arg| match *arg {
                Arg::Operand(operand) => accept.operands.get(usize::from(operand)),
                _ => None,
            });
            let holders = states.filter_map(|bind| Some(bind.path.split_last()?.1));
            let held = |holder: &[u8]| {
                let mut sources = sources.iter();
                sources.position(|(path, _)| path[..] == *holder)
            };
            let mut threaded: SmallVec<[usize; 2]> = holders.filter_map(held).collect();
            threaded.dedup();
            named.push(threaded);
        }
        let unnamed = |at: &usize| !named.iter().flatten().any(|known| known == at);
        let unnamed: SmallVec<[usize; 2]> = (0..sources.len()).filter(unnamed).collect();
        if let Some(last) = named.last_mut() {
            last.extend(unnamed);
        }
        let effect = |&(_, op): &(Path, OpId)| {
            let effects = context.get_op(op).as_interface::<dyn ResourceEffects>();
            effects.is_some_and(|effects| !effects.resource_effects().is_empty())
        };
        let stateful = sources.iter().any(effect);
        if trace {
            eprintln!(
                "isel-chain: pattern {state} in {:?} for {region:?} spelling {spelled:?} sources {sources:?} by step {named:?}",
                eg.value(row)
            );
        }
        // An instruction taking a state over sits where the operation it
        // covers does, and any other where the IR computed the value if
        // the reader sees that region.
        let visible = |(_, op): &&(Path, OpId)| {
            let defined = context.get_op(*op).results();
            let mut defined = defined.iter();
            defined
                .next()
                .is_some_and(|&value| out.readable(value, region))
        };
        let home = match sources.first().filter(visible) {
            Some(&(_, op)) => context.region_of_op(op).unwrap_or(region),
            None if !stateful => region,
            None => return refused("the access it covers is in a region the reader cannot see"),
        };
        let accesses = |step: &super::pool::Step| {
            let rule = &self.target[automaton.forms[step.form as usize].rule];
            rule.emit.iter().any(super::Emitter::accesses_memory)
        };
        if !stateful && accept.tree.iter().any(accesses) {
            return refused("no access it covers is one the reader can see");
        }
        if stateful {
            let mut emitted = self.emitted.borrow_mut();
            // A row with no effect, an address two accesses share, is
            // each one's to fold.
            let accesses = || sources.iter().filter(|source| effect(source));
            if accesses().any(|(_, op)| emitted.taken.contains(op)) {
                return refused("an instruction took the access's states over already");
            }
            emitted.taken.extend(accesses().map(|&(_, op)| op));
        }
        let slots = match self.slots(eg, form, accept, [&accept.tree], true) {
            Ok(slots) => slots,
            Err(why) => return refused(why),
        };
        let mut bound: Operands = SmallVec::from_elem(None, accept.operands.len());
        for (operand, slot) in slots {
            let held = match slot {
                Slot::Immediate(value) => Bound::Immediate(value),
                Slot::Register(read) => {
                    let path = &accept.operands[usize::from(operand)].path;
                    let named = operands.iter().find(|(known, _)| known == path);
                    let named = named.map(|&(_, operand)| operand);
                    let Some(value) = self.register(out, read, named, home) else {
                        let chosen = out.extraction.row(eg.find(read).class);
                        let chosen = chosen.map(|row| (&eg.node(row).kind, eg.node(row).prov));
                        return refused(&format!(
                            "operand {operand} at {:?} has no spelling in {home:?}: {chosen:?}",
                            read
                        ));
                    };
                    Bound::Register(value)
                }
            };
            bound[usize::from(operand)] = Some(held);
        }
        // The ports are read once the operands are spelled: an operand
        // emitted just now took the readers of the state it published.
        let ports = named.iter().map(|threaded| {
            let threaded = threaded.iter().map(|&at| sources[at].1);
            Self::ports(context, threaded, &mut renamed)
        });
        let ports: SmallVec<[Ports; 2]> = ports.collect();
        // A tree of no instruction leaves every state as it found it.
        if accept.tree.is_empty() {
            let effects = sources.iter().filter_map(|&(_, op)| {
                let effects = context.get_op(op).as_interface::<dyn ResourceEffects>()?;
                Some(effects.resource_effects())
            });
            for effect in effects.flatten() {
                if let (Some(&observed), Some(&published)) =
                    (effect.observed.first(), effect.produced.first())
                {
                    renamed.push((published, observed));
                }
            }
        }
        let several = (accept.results > 1 && !stateful).then(|| {
            let last = accept
                .tree
                .last()
                .map_or(0, |step| automaton.forms[step.form as usize].rule);
            let operands = bound.iter().map(|bound| match bound {
                Some(Bound::Register(value)) => u64::from(value.number()),
                Some(Bound::Immediate(value)) => value.to_u64(),
                None => u64::MAX,
            });
            (last, operands.collect::<SmallVec<[u64; 4]>>())
        });
        let port = usize::from(accept.port);
        let known = several.as_ref().and_then(|key| {
            let emitted = self.emitted.borrow();
            let defined = emitted.several.get(key)?;
            let &(_, value) = defined.iter().find(|&&(at, _)| at == port)?;
            out.readable(value, region).then_some(value)
        });
        let value = match known {
            Some(value) => Some(value),
            None => {
                let mut place = self.place(out, home);
                let emit = Emit {
                    context,
                    ty: expected_ty,
                    ports: &ports,
                    target: None,
                    lanes,
                    place: &mut place,
                };
                match (accept.value, several) {
                    (Arg::Step(_), Some(key)) => {
                        let defined = self.steps(emit, &accept.tree, &bound)?;
                        let value = defined.iter().find(|&&(at, _)| at == port);
                        let value = value.map(|&(_, value)| value);
                        self.emitted.borrow_mut().several.insert(key, defined);
                        value
                    }
                    _ => self.value(emit, accept, &bound)?,
                }
            }
        };
        for &(old, new) in &renamed {
            context.replace_value_uses(old, new);
            context.rename_region_results(home, old, new, &[]);
        }
        // A write's row is the state it publishes.
        let value = match context.is_state_type(expected_ty) {
            true => ports
                .iter()
                .flatten()
                .find_map(|port| port.published)
                .or(value),
            false => value,
        };
        let Some(value) = value else {
            return refused("its tree defines no value");
        };
        let mut emitted = self.emitted.borrow_mut();
        emitted.types.entry(value).or_insert(expected_ty);
        Ok(Some(value))
    }

    // Erased because the branch is emitted and nothing reads their values.
    pub(super) fn branched(&self, context: &Context, body: RegionId) -> Result<(), PassError> {
        for block in context.get_region(body).block_ids() {
            for op in context.get_block(block).op_ids() {
                let instance = context.get_op(op);
                if !self.replaces(context, &instance) {
                    continue;
                }
                let defined = instance.results();
                if defined.iter().any(|&value| context.is_used(value)) {
                    return Err(unselected(&instance, "has no instruction"));
                }
                context.erase_op(&tir::OperationRef::new(instance))?;
            }
        }
        Ok(())
    }

    /// End the commit of `function`: a read no instruction was emitted for
    /// leaves the state it observed to its state's readers, what nothing
    /// demands is swept, and an operation selection replaces that is still
    /// there is the error, unless a control branches on its value: recovery
    /// replaces that one by the branch. Answers what the commit emitted.
    pub fn finish(
        &self,
        context: &Context,
        graph: (&Engine<SemNode>, &IdMap<ValueId, Ref>),
        extraction: &Extraction<'_, SemNode>,
        function: OpId,
    ) -> Result<Emitted, PassError> {
        let mut emitted = self.emitted.replace(Emitted::default());
        if let Some(error) = emitted.failed.take() {
            return Err(error);
        }
        // The replaced operations, in the order the regions hold them.
        let mut replaced = Vec::new();
        for region in crate::passes::regions_under(context, function) {
            for op in context.get_region(region).op_ids() {
                let instance = context.get_op(op);
                if !self.replaces(context, &instance) {
                    continue;
                }
                replaced.push(op);
                if changes(&instance) {
                    continue;
                }
                let effects = instance.as_interface::<dyn ResourceEffects>();
                let effects = effects.map(|effects| effects.resource_effects());
                for effect in effects.unwrap_or_default() {
                    if let (Some(&observed), Some(&produced)) =
                        (effect.observed.first(), effect.produced.first())
                    {
                        context.replace_value_uses(produced, observed);
                        context.rename_region_results(region, produced, observed, &[]);
                    }
                }
            }
        }
        let body = context.get_op(function).regions()[0];
        let implicit = |op: OpId| emitted.implicit.get(&op).map_or(&[][..], Vec::as_slice);
        let demanded = crate::passes::demanded_ops_with(context, &[body], &[], &implicit);
        crate::passes::instcombine::sweep_demanded(context, body, &demanded)?;
        let tested = |op: &OpId| {
            let defined = context.get_op(*op).results();
            let tested = |value: &ValueId| self.tested(graph, extraction, *value).is_some();
            defined.first().is_some_and(tested)
        };
        let left = replaced.into_iter().filter(|&op| context.has_operation(op));
        let mut left = left.filter(|op| !tested(op));
        match left.next() {
            Some(op) => Err(unselected(&context.get_op(op), "has no instruction")),
            None => Ok(emitted),
        }
    }
}
