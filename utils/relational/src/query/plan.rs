//! Evaluation orders: a query's atoms and guards placed once, from the root
//! and from each atom a semi-naive round can start at.

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use smallvec::SmallVec;

use super::ghd::{Bag, decompose};
use super::ir::{
    Atom, Cmp, Expr, Guard, Lattice, Param, Place, Query, Read, Rows, Scalar, Select, Tuples, Var,
};
use crate::db::LatticeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Atom(usize),
    /// A negated sub-conjunction, by index into the plan's nested plans.
    Not(usize),
    /// A sideways row atom: its value is not bound, but its argument `slot`
    /// is, so it is reached through the rows holding that id rather than
    /// through its value.
    Parents {
        atom: usize,
        slot: u8,
    },
    Guard(usize),
    /// A client table atom. `keys` masks the columns whose term is bound when
    /// the step is reached: its rows are looked up under those and bind the
    /// rest.
    Table {
        atom: usize,
        keys: u32,
    },
    /// The atoms of the run of steps that follows, joined at once: by
    /// index into the order's joins. The run's steps stay in the order, for
    /// when the loop nest is the cheaper way to run them.
    Join(usize),
}

/// An evaluation order: the steps, the row levels below the root, and the
/// plans of the negated conjunctions.
type Ordered = (Vec<Step>, usize, Vec<Plan>);

/// A query with its atoms ordered for evaluation: each step's key variable is
/// bound by the root or by an earlier step, so evaluation is a loop nest with
/// no search over orders. Guards run as soon as their inputs are bound.
///
/// A rule set asks for the same query many times over, under different
/// parameters, so what planning works out is shared by every plan of one
/// query.
#[derive(Clone, Debug)]
pub struct Plan {
    planned: Arc<Planned>,
    /// Names the plan to a client's caches. Zero for a plan that is part of
    /// another.
    id: u64,
}

/// What planning works out of a query.
#[derive(Debug)]
pub struct Planned {
    pub(super) query: Query,
    pub(super) steps: Vec<Step>,
    /// The steps as evaluated: [`Self::steps`] with each run worth joining
    /// marked, and the joins the marks name.
    pub(super) run: Vec<Step>,
    pub(super) joins: Vec<JoinPlan>,
    // From there on only client tables, places and comparisons are read, which
    // no epoch changes.
    pub(super) settled: usize,
    height: usize,
    /// One compiled plan per negated sub-conjunction, in the query's order.
    pub(super) nots: Vec<Plan>,
    /// Per atom, what the evaluator reads of it.
    pub(super) shapes: Vec<Shape>,
    /// One evaluation order per row atom, starting at that atom rather than at
    /// the root: what a round uses to search from the rows the round before
    /// wrote. Empty for a plan that cannot be searched that way. Worked out on
    /// first use: a plan compiled and never saturated with does not pay for it.
    anchors: OnceLock<Vec<Anchor>>,
    /// One evaluation order per lattice atom, starting at its key: what a
    /// round uses to search from the values the round before raised. Empty for
    /// a plan that cannot be searched that way.
    fact_anchors: OnceLock<Vec<FactAnchor>>,
    reductions: OnceLock<Vec<Reduction>>,
}

impl Deref for Plan {
    type Target = Planned;

    fn deref(&self) -> &Planned {
        &self.planned
    }
}

/// An atom as the evaluator reads it, worked out once rather than per row: a
/// row atom's selection, tag, swap set, key and arguments, each argument with
/// the column it sits in as written and as read swapped.
#[derive(Clone, Debug)]
pub(super) enum Shape {
    Rows(RowShape),
    Lattice(Lattice),
    Place(Place),
    Words(Tuples),
}

#[derive(Clone, Debug)]
pub(super) struct RowShape {
    pub(super) select: Select,
    pub(super) tag: Option<Scalar>,
    pub(super) cell: Option<Scalar>,
    /// The parameter naming the cells whose rows also read swapped.
    pub(super) when: Option<Param>,
    pub(super) key: Var,
    pub(super) value: u8,
    pub(super) args: SmallVec<[Arg; 4]>,
    pub(super) within: Option<(Expr, Expr)>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Arg {
    pub(super) column: u8,
    pub(super) swapped: u8,
    pub(super) var: Var,
}

impl Shape {
    fn of(atom: &Atom) -> Self {
        match atom {
            Atom::Rows(rows) => Shape::Rows(RowShape {
                select: rows.select,
                tag: rows.tag,
                cell: rows.cell,
                when: rows.swap.map(|swap| swap.when),
                key: rows.value(),
                value: (rows.terms.len() - 1) as u8,
                args: rows
                    .args()
                    .map(|(column, var)| Arg {
                        column: column as u8,
                        swapped: rows.swapped(column) as u8,
                        var,
                    })
                    .collect(),
                within: rows.tag_within.clone(),
            }),
            Atom::Lattice(lattice) => Shape::Lattice(lattice.clone()),
            Atom::Place(place) => Shape::Place(*place),
            Atom::Words(tuples) => {
                assert!(tuples.terms.len() <= 32, "a mask of columns");
                Shape::Words(tuples.clone())
            }
        }
    }
}

/// An evaluation order that starts at one row atom. Its first step is that
/// atom, which the search scans; the rest reach every other atom from what it
/// bound.
#[derive(Clone, Debug)]
pub(super) struct Anchor {
    pub(super) atom: usize,
    pub(super) steps: Vec<Step>,
    pub(super) joins: Vec<JoinPlan>,
    pub(super) below: Vec<Below>,
}

// A row of the other takes part in a match only where some row of this one
// has, for its value, the id the other holds in that argument's column.
#[derive(Clone, Debug)]
pub(super) struct Below {
    pub(super) atom: usize,
    pub(super) column: u8,
    pub(super) swapped: u8,
    pub(super) below: Vec<Below>,
}

fn below(shapes: &[Shape], base: &[Var], atom: usize, taken: &mut [bool]) -> Vec<Below> {
    let Shape::Rows(above) = &shapes[atom] else {
        return Vec::new();
    };
    let mut found: Vec<Below> = Vec::new();
    for (index, shape) in shapes.iter().enumerate() {
        let Shape::Rows(rows) = shape else {
            continue;
        };
        let held = |arg: &&Arg| arg.var == base[rows.key as usize];
        if let (false, Some(arg)) = (taken[index], above.args.iter().find(held)) {
            taken[index] = true;
            found.push(Below {
                atom: index,
                column: arg.column,
                swapped: arg.swapped,
                below: Vec::new(),
            });
        }
    }
    for under in &mut found {
        under.below = below(shapes, base, under.atom, taken);
    }
    found
}

/// An evaluation order that starts at the key a lattice atom reads: its first
/// step is that atom.
#[derive(Clone, Debug)]
pub(super) struct FactAnchor {
    pub(super) column: LatticeId,
    pub(super) key: Var,
    pub(super) steps: Vec<Step>,
    pub(super) joins: Vec<JoinPlan>,
    /// The lattice atoms that come before this one, as a mask over atom
    /// indices. A match with several new values is found from the first of
    /// them, so from this anchor those must be old.
    pub(super) earlier: u64,
    /// Whether no earlier lattice atom reads the same key. An id minted with
    /// its values has all of them new at once, so only such an anchor can find
    /// a match there.
    pub(super) first_on_key: bool,
}

// A row whose cell no row of the table holds there is in no match, so the
// selection need not list the cell.
#[derive(Clone, Copy, Debug)]
pub(super) struct Reduction {
    pub(super) rows: Param,
    pub(super) atom: usize,
    pub(super) column: u8,
}

impl Plan {
    /// Order `query`'s atoms — the root's atom first, then whatever it bound,
    /// in the order the query was written — with each guard placed at the
    /// first point its inputs are all bound.
    ///
    /// Panics if an atom's key is unreachable from the root, or a guard reads
    /// a scalar nothing binds.
    pub fn compile(query: Query) -> Self {
        static PLANNED: Mutex<Option<crate::FxHashMap<Query, Arc<Planned>>>> = Mutex::new(None);
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let known = |query: &Query| {
            let planned = PLANNED.lock().unwrap();
            planned
                .as_ref()
                .and_then(|planned| planned.get(query).cloned())
        };
        if let Some(planned) = known(&query) {
            return Self { planned, id };
        }
        let mut bound = vec![false; query.vars as usize];
        bound[query.root as usize] = true;
        let known = vec![false; query.scalars as usize];
        let mut planned = Self::plan(query.clone(), bound.clone(), known.clone(), None)
            .expect("every atom is reached and every guard read");
        (planned.run, planned.joins) = joined(&planned, &planned.steps, bound, known, 0);
        let settles = |step: &Step| match *step {
            Step::Table { .. } => true,
            Step::Guard(guard) => {
                !matches!(planned.query.guards[guard], super::Guard::Extern { .. })
            }
            Step::Atom(atom) => matches!(planned.shapes[atom], Shape::Place(_)),
            Step::Parents { .. } | Step::Join(_) | Step::Not(_) => false,
        };
        let last = planned.run.iter().rposition(|step| !settles(step));
        planned.settled = last.map_or(0, |last| last + 1);
        let planned = Arc::new(planned);
        let mut all = PLANNED.lock().unwrap();
        all.get_or_insert_default().insert(query, planned.clone());
        Self { planned, id }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn anchors(&self) -> &[Anchor] {
        self.anchors.get_or_init(|| self.anchor())
    }

    /// Whether a round can search this plan from the rows the round before
    /// wrote, instead of from every id near a change.
    pub fn anchorable(&self) -> bool {
        !self.anchors().is_empty()
    }

    fn anchor(&self) -> Vec<Anchor> {
        let query = &self.query;
        let rooted = query
            .atoms
            .iter()
            .any(|atom| matches!(atom, Atom::Rows(rows) if rows.value() == query.root));
        if !rooted || !self.nots.is_empty() {
            return Vec::new();
        }
        // A variable an offset atom reads its base into names the id of the
        // variable it is keyed by.
        let mut base: Vec<Var> = (0..query.vars).collect();
        for atom in &query.atoms {
            if let Atom::Place(Place {
                key,
                read: Read::Offset { base: of, .. },
            }) = atom
            {
                base[*of as usize] = *key;
            }
        }
        for var in 0..base.len() {
            let mut of = base[var];
            for _ in 0..base.len() {
                of = base[of as usize];
            }
            base[var] = of;
        }
        let mut anchors = Vec::new();
        for (index, atom) in query.atoms.iter().enumerate() {
            let Atom::Rows(rows) = atom else {
                continue;
            };
            let mut bound = vec![false; query.vars as usize];
            bound[rows.value() as usize] = true;
            for (_, arg) in rows.args() {
                bound[arg as usize] = true;
            }
            let mut known = vec![false; query.scalars as usize];
            for slot in atom.writes() {
                known[slot as usize] = true;
            }
            let Some((rest, _, _)) = Self::order(query, bound.clone(), known.clone(), Some(index))
            else {
                return Vec::new();
            };
            let mut steps = vec![Step::Atom(index)];
            steps.extend(rest);
            // The search runs the anchor's own step, so no run starts there.
            let (steps, joins) = joined(self, &steps, bound, known, 1);
            let mut taken = vec![false; query.atoms.len()];
            taken[index] = true;
            anchors.push(Anchor {
                atom: index,
                steps,
                joins,
                below: below(&self.shapes, &base, index, &mut taken),
            });
        }
        anchors
    }

    /// A row atom's selection the host can tell, without resolving anything,
    /// reads no table, so the plan has no match.
    pub fn blocked<H: super::Host + ?Sized>(&self, host: &H) -> Option<super::Param> {
        self.query.atoms.iter().find_map(|atom| match atom {
            Atom::Rows(rows) if host.empty(rows.select.param) => Some(rows.select.param),
            _ => None,
        })
    }

    /// Whether the plan reads no row at all.
    pub fn rowless(&self) -> bool {
        !self
            .query
            .atoms
            .iter()
            .any(|atom| matches!(atom, Atom::Rows(_)))
    }

    /// Whether a round can search this plan from the values the round before
    /// raised.
    pub fn fact_anchorable(&self) -> bool {
        !self.fact_anchors().is_empty()
    }

    pub(super) fn fact_anchors(&self) -> &[FactAnchor] {
        self.fact_anchors.get_or_init(|| self.fact_anchor())
    }

    fn fact_anchor(&self) -> Vec<FactAnchor> {
        let query = &self.query;
        if !self.nots.is_empty() {
            return Vec::new();
        }
        let mut facts: Vec<(usize, LatticeId, Var)> = query
            .atoms
            .iter()
            .enumerate()
            .filter_map(|(index, atom)| match atom {
                Atom::Lattice(lattice) => Some((index, lattice.column, lattice.key)),
                Atom::Rows(_) | Atom::Place(_) | Atom::Words(_) => None,
            })
            .collect();
        if facts.iter().any(|&(index, _, _)| index >= 64) {
            return Vec::new();
        }
        // Columns are tried in the order they were declared, which a client
        // makes the order of how rarely they rise.
        facts.sort_by_key(|&(index, column, _)| (column.index(), index));
        let mut anchors = Vec::new();
        let mut earlier = 0u64;
        for (position, &(index, column, key)) in facts.iter().enumerate() {
            let mut bound = vec![false; query.vars as usize];
            bound[key as usize] = true;
            let known = vec![false; query.scalars as usize];
            // No atom is skipped, but the order is not from the root.
            let Some((steps, _, _)) =
                Self::order(query, bound.clone(), known.clone(), Some(usize::MAX))
            else {
                return Vec::new();
            };
            let (steps, joins) = joined(self, &steps, bound, known, 0);
            let first_on_key = !facts[..position].iter().any(|&(_, _, other)| other == key);
            anchors.push(FactAnchor {
                column,
                key,
                steps,
                joins,
                earlier,
                first_on_key,
            });
            earlier |= 1 << index;
        }
        anchors
    }

    pub(super) fn reductions(&self) -> &[Reduction] {
        self.reductions.get_or_init(|| self.reduce())
    }

    fn reduce(&self) -> Vec<Reduction> {
        let atoms = &self.query.atoms;
        let tables = || {
            atoms
                .iter()
                .enumerate()
                .filter_map(|(at, atom)| match atom {
                    Atom::Words(tuples) => Some((at, tuples)),
                    _ => None,
                })
        };
        if tables().next().is_none() {
            return Vec::new();
        }
        let orders = std::iter::once(&self.run)
            .chain(self.anchors().iter().map(|anchor| &anchor.steps))
            .chain(self.fact_anchors().iter().map(|anchor| &anchor.steps));
        let orders: Vec<&Vec<Step>> = orders.collect();
        let follows = |table: usize, rows: usize| {
            orders.iter().all(|steps| {
                let at = |atom: usize| {
                    steps.iter().position(|step| match *step {
                        Step::Atom(known)
                        | Step::Parents { atom: known, .. }
                        | Step::Table { atom: known, .. } => known == atom,
                        _ => false,
                    })
                };
                matches!((at(rows), at(table)), (Some(rows), Some(table)) if rows < table)
            })
        };
        // The selections one atom alone reads, a negated one counted.
        let nested = self.query.nots.iter().flat_map(|not| &not.atoms);
        let selected = |param: Param| {
            let reads = |atom: &&Atom| match atom {
                Atom::Rows(rows) => {
                    rows.select.param == param || rows.swap.is_some_and(|swap| swap.when == param)
                }
                Atom::Lattice(lattice) => {
                    matches!(lattice.out, super::Out::In(read) if read == param)
                }
                Atom::Place(_) | Atom::Words(_) => false,
            };
            atoms.iter().chain(nested.clone()).filter(reads).count() == 1
        };
        let mut reductions = Vec::new();
        for (at, atom) in atoms.iter().enumerate() {
            let Atom::Rows(Rows {
                select,
                cell: Some(cell),
                ..
            }) = atom
            else {
                continue;
            };
            // The scalar is the cell's wherever a table atom reads it: no
            // other row, value or guard writes it, a negated one included.
            let others = atoms.iter().enumerate().filter(|&(other, _)| other != at);
            let others = others.map(|(_, other)| other).chain(nested.clone());
            let guards = self.query.nots.iter().flat_map(|not| &not.guards);
            let mut guards = self.query.guards.iter().chain(guards);
            let rewritten = others
                .filter(|other| !matches!(other, Atom::Words(_)))
                .any(|other| other.writes().contains(cell))
                || guards.any(|guard| guard.writes().contains(cell));
            if rewritten || !selected(select.param) {
                continue;
            }
            for (table, tuples) in tables().filter(|&(table, _)| follows(table, at)) {
                let held = |term: &Expr| term.lone() == Some(*cell);
                reductions.extend(tuples.terms.iter().position(held).map(|column| Reduction {
                    rows: select.param,
                    atom: table,
                    column: column as u8,
                }));
            }
        }
        reductions
    }

    /// The lattice columns this plan reads, as a mask over their ids. A match
    /// can be new without any of its rows being new when a value rises under
    /// rows that stand still.
    pub fn fact_columns(&self) -> u64 {
        self.query.atoms.iter().fold(0, |mask, atom| match atom {
            Atom::Lattice(lattice) => mask | 1 << lattice.column.index(),
            Atom::Rows(_) | Atom::Place(_) | Atom::Words(_) => mask,
        })
    }

    fn compile_from(
        query: Query,
        bound: Vec<bool>,
        known: Vec<bool>,
        skip: Option<usize>,
    ) -> Option<Self> {
        let planned = Arc::new(Self::plan(query, bound, known, skip)?);
        Some(Self { planned, id: 0 })
    }

    fn plan(
        mut query: Query,
        bound: Vec<bool>,
        known: Vec<bool>,
        skip: Option<usize>,
    ) -> Option<Planned> {
        // An order that reaches an interval's atom some other way than
        // through its value reads every row, so the interval is also a filter.
        for atom in &query.atoms {
            if let Atom::Rows(Rows {
                tag,
                tag_within: Some((lo, hi)),
                ..
            }) = atom
            {
                let tag = Expr::Scalar(tag.expect("an interval holds the tag the atom binds"));
                query
                    .guards
                    .push(Guard::Cmp(Cmp::Le, lo.clone(), tag.clone()));
                query.guards.push(Guard::Cmp(Cmp::Lt, tag, hi.clone()));
            }
        }
        let (steps, height, nots) = Self::order(&query, bound, known, skip)?;
        let shapes = query.atoms.iter().map(Shape::of).collect();
        Some(Planned {
            query,
            run: steps.clone(),
            joins: Vec::new(),
            settled: usize::MAX,
            steps,
            height,
            nots,
            shapes,
            anchors: OnceLock::new(),
            fact_anchors: OnceLock::new(),
            reductions: OnceLock::new(),
        })
    }

    fn order(
        query: &Query,
        mut bound: Vec<bool>,
        mut known: Vec<bool>,
        skip: Option<usize>,
    ) -> Option<Ordered> {
        let mut steps = Vec::with_capacity(query.atoms.len() + query.guards.len());
        let mut taken = vec![false; query.atoms.len()];
        if let Some(taken) = skip.and_then(|atom| taken.get_mut(atom)) {
            *taken = true;
        }
        let mut checked = vec![false; query.guards.len()];
        let mut nots: Vec<Plan> = Vec::new();
        let mut placed = vec![false; query.nots.len()];
        let mut depth = vec![0usize; query.vars as usize];
        let mut height = 0usize;
        let writes: Vec<SmallVec<[Scalar; 4]>> = query.atoms.iter().map(Atom::writes).collect();
        // The variables each negated conjunction shares with the query: it
        // asks about what they are bound to, so it waits for all of them.
        let outer: Vec<Var> = query.atoms.iter().flat_map(atom_vars).collect();
        let named: Vec<Vec<Var>> = query
            .nots
            .iter()
            .map(|not| {
                let mut vars: Vec<Var> = not.atoms.iter().flat_map(atom_vars).collect();
                vars.retain(|var| *var == query.root || outer.contains(var));
                vars
            })
            .collect();
        loop {
            // A host call that binds nothing may be an assertion about a whole
            // match rather than a filter, and a rule's author sees to it that
            // every filter runs first when the match is built from the root. An
            // order that starts elsewhere keeps that promise by running such a
            // call only once every atom is matched.
            let all_taken = taken.iter().all(|&t| t);
            let guard = (0..query.guards.len()).find(|&i| {
                let held_back = skip.is_some()
                    && !all_taken
                    && matches!(&query.guards[i], super::Guard::Extern { out, .. } if out.is_empty());
                !checked[i] && !held_back && query.guards[i].ready(&known, &bound)
            });
            if let Some(guard) = guard {
                checked[guard] = true;
                for &out in query.guards[guard].writes() {
                    known[out as usize] = true;
                }
                steps.push(Step::Guard(guard));
                continue;
            }
            // A negated conjunction reads and never writes, so it goes as soon
            // as everything it names is bound — the earliest it can prune.
            let not = (0..query.nots.len()).find_map(|i| {
                if placed[i] || !named[i].iter().all(|&var| bound[var as usize]) {
                    return None;
                }
                let nested = Query {
                    space: query.space,
                    vars: query.vars,
                    scalars: query.scalars,
                    params: query.params,
                    root: query.root,
                    atoms: query.nots[i].atoms.clone(),
                    guards: query.nots[i].guards.clone(),
                    nots: Vec::new(),
                };
                Plan::compile_from(nested, bound.clone(), known.clone(), None).map(|plan| (i, plan))
            });
            if let Some((index, plan)) = not {
                placed[index] = true;
                steps.push(Step::Not(nots.len()));
                nots.push(plan);
                continue;
            }
            if taken.iter().all(|&t| t) {
                break;
            }
            // A client table atom goes once nothing else is left to bind the
            // scalars it reads, so its rows are looked up under every column
            // the match can give. `settled` unset takes, of those that wait
            // for each other or for a guard reading what they bind, the one
            // with most columns bound.
            let awaited = |by: usize, slot: Scalar| {
                let atom = |i: usize| i != by && !taken[i] && writes[i].contains(&slot);
                let guard = |i: usize| !checked[i] && query.guards[i].writes().contains(&slot);
                (0..query.atoms.len()).any(atom) || (0..query.guards.len()).any(guard)
            };
            let table = |settled: bool| {
                let placed = |&i: &usize| match &query.atoms[i] {
                    Atom::Words(tuples) if !taken[i] => tuples.terms.iter().all(|term| {
                        term.ready(&known)
                            || term
                                .lone()
                                .is_some_and(|slot| !(settled && awaited(i, slot)))
                    }),
                    _ => false,
                };
                let bound = |&i: &usize| match &query.atoms[i] {
                    Atom::Words(tuples) => tuples.keys(&known).count_ones(),
                    _ => 0,
                };
                // The first of those with most.
                (0..query.atoms.len())
                    .filter(placed)
                    .rev()
                    .max_by_key(bound)
            };
            let interval = |i: usize| match &query.atoms[i] {
                Atom::Rows(Rows {
                    tag_within: Some((lo, hi)),
                    ..
                }) => lo.ready(&known) && hi.ready(&known),
                _ => true,
            };
            let downward = || {
                (0..query.atoms.len()).find(|&i| {
                    !taken[i]
                        && query.atoms[i].key().is_some_and(|key| bound[key as usize])
                        && interval(i)
                })
            };
            // Nothing left to reach through a bound key: reach a row atom
            // whose value is still free through an argument that is not,
            // which is the one shape a root-first plan cannot cover.
            let sideways = || {
                (0..query.atoms.len())
                    .filter(|&i| !taken[i])
                    .find_map(|i| match &query.atoms[i] {
                        Atom::Rows(rows) => rows
                            .args()
                            .position(|(_, arg)| bound[arg as usize])
                            .map(|slot| {
                                (
                                    i,
                                    Step::Parents {
                                        atom: i,
                                        slot: slot as u8,
                                    },
                                )
                            }),
                        Atom::Lattice(_) | Atom::Place(_) | Atom::Words(_) => None,
                    })
            };
            let reached = downward()
                .map(|next| (next, Step::Atom(next)))
                .or_else(sideways);
            let unreached = || reached.is_none().then(|| table(false)).flatten();
            if let Some(atom) = table(true).or_else(unreached) {
                let Atom::Words(tuples) = &query.atoms[atom] else {
                    unreachable!("a client table atom")
                };
                let keys = tuples.keys(&known);
                taken[atom] = true;
                steps.push(Step::Table { atom, keys });
                for &slot in &writes[atom] {
                    known[slot as usize] = true;
                }
                continue;
            }
            let (next, step) = reached?;
            taken[next] = true;
            steps.push(step);
            let key = query.atoms[next].key()? as usize;
            bound[key] = true;
            let below = depth[key] + 1;
            match &query.atoms[next] {
                Atom::Rows(rows) => {
                    height = height.max(below);
                    for (_, arg) in rows.args() {
                        if !bound[arg as usize] {
                            bound[arg as usize] = true;
                            depth[arg as usize] = below;
                        }
                    }
                }
                Atom::Place(Place {
                    read: Read::Offset { base, .. },
                    ..
                }) => bound[*base as usize] = true,
                Atom::Lattice(_) | Atom::Place(_) | Atom::Words(_) => {}
            }
            for &slot in &writes[next] {
                known[slot as usize] = true;
            }
        }
        (checked.iter().all(|&c| c) && placed.iter().all(|&p| p)).then_some((steps, height, nots))
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The atoms and guards in evaluation order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Row levels below the root this plan binds. An id outside the frontier
    /// at this depth has an unchanged cone down to it, so its matches are the
    /// previous round's.
    ///
    /// Only meaningful for a plan that walks down: see [`Self::unbounded`].
    pub fn height(&self) -> usize {
        self.height
    }

    /// Whether the match depends on rows the root's downward cone does not
    /// contain, so neither narrowing a round's roots to the change frontier nor
    /// skipping a match with no new row of its own is licensed.
    ///
    /// Two shapes do this. A sideways atom sits under a sibling sharing an
    /// argument, which no upward closure of the change log reaches from the
    /// root. And a negated conjunction is read against the whole database: what
    /// satisfies it can change (an id placed consistently in one epoch is
    /// conflicted in the next) without anything in the match moving at all.
    pub fn unbounded(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, Step::Parents { .. } | Step::Not(_)))
            || self.nots.iter().any(Plan::unbounded)
    }
}

/// A run of row and client table steps two of whose atoms read a variable or
/// a scalar unbound when the run starts, neither reaching its rows through
/// it: joined by intersecting the cells each atom allows for it, rather than
/// by probing one atom per row of the other.
#[derive(Clone, Debug)]
pub(super) struct JoinPlan {
    /// How many steps after the join's own make up the run.
    pub(super) len: usize,
    /// What the run binds, in the order the loop nest first binds it: the
    /// attribute order.
    pub(super) vars: SmallVec<[Attr; 8]>,
    /// The run's atoms, in step order.
    pub(super) atoms: SmallVec<[JoinAtom; 4]>,
    /// The run's decomposition: one bag of every atom when they are best
    /// joined at once.
    pub(super) bags: Vec<Bag>,
}

/// What a join binds by intersection: a variable to an id, or a scalar to a
/// cell of a plain column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Attr {
    Var(Var),
    Scalar(Scalar),
}

#[derive(Clone, Debug)]
pub(super) struct JoinAtom {
    /// The step that reaches the atom in the run.
    pub(super) step: Step,
    /// The trie column orders the atom reads, as written and swapped: a row
    /// atom's selected column and then one per variable, a client table
    /// atom's every column. The second is empty for an atom that does not
    /// swap.
    pub(super) columns: [SmallVec<[u8; 6]>; 2],
    /// Per trie level, what it reads.
    pub(super) levels: SmallVec<[Level; 6]>,
    pub(super) cell: Option<u32>,
}

impl JoinAtom {
    pub(super) fn atom(&self) -> usize {
        match self.step {
            Step::Atom(atom) | Step::Parents { atom, .. } | Step::Table { atom, .. } => atom,
            _ => unreachable!("a run holds row and client table atoms"),
        }
    }

    // The variable a row atom's rows are reached through.
    fn lookup(&self, shapes: &[Shape]) -> Option<Var> {
        match (self.step, &shapes[self.atom()]) {
            (Step::Parents { slot, .. }, Shape::Rows(rows)) => Some(rows.args[slot as usize].var),
            (_, Shape::Rows(rows)) => Some(rows.key),
            _ => None,
        }
    }
}

/// What a trie level reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    Select,
    /// A variable bound before the run: looked up.
    Known(Var),
    Key(u8),
    /// The run's attribute at this index of the attribute order: intersected.
    Attr(u32),
}

// `steps` with a [`Step::Join`] in front of each run worth joining, and the
// joins. `bound` and `known` are what is bound before the first step; runs
// start no earlier than step `from`.
fn joined(
    planned: &Planned,
    steps: &[Step],
    mut bound: Vec<bool>,
    mut known: Vec<bool>,
    from: usize,
) -> (Vec<Step>, Vec<JoinPlan>) {
    let (query, shapes) = (&planned.query, &planned.shapes);
    let joins_rows = |step: &Step| match *step {
        Step::Atom(atom) => matches!(shapes[atom], Shape::Rows(_)),
        Step::Parents { .. } | Step::Table { .. } => true,
        _ => false,
    };
    let mut out = Vec::with_capacity(steps.len());
    let mut joins = Vec::new();
    let mut at = 0;
    while at < steps.len() {
        let mut end = at;
        while at >= from && end < steps.len() && joins_rows(&steps[end]) {
            end += 1;
        }
        let (len, join) = JoinPlan::of(shapes, &steps[at..end], &bound, &known);
        if let Some(join) = join {
            out.push(Step::Join(joins.len()));
            joins.push(join);
        }
        let end = at + len.max(1);
        for step in &steps[at..end] {
            out.push(*step);
            let atom = match *step {
                Step::Atom(atom) | Step::Parents { atom, .. } | Step::Table { atom, .. } => atom,
                Step::Guard(guard) => {
                    for &slot in query.guards[guard].writes() {
                        known[slot as usize] = true;
                    }
                    continue;
                }
                _ => continue,
            };
            for slot in query.atoms[atom].writes() {
                known[slot as usize] = true;
            }
            match &shapes[atom] {
                Shape::Rows(rows) => {
                    bound[rows.key as usize] = true;
                    for arg in &rows.args {
                        bound[arg.var as usize] = true;
                    }
                }
                Shape::Place(Place {
                    read: Read::Offset { base, .. },
                    ..
                }) => bound[*base as usize] = true,
                Shape::Lattice(_) | Shape::Place(_) | Shape::Words(_) => {}
            }
        }
        at = end;
    }
    (out, joins)
}

// What a scalar is to a run at one of its steps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    Free,
    // Bound before the run.
    Known,
    // The attribute at this index: every atom reading it agrees on it.
    Attr(u32),
    // Written over in the run, so no later atom of it can be held to it.
    Written,
}

impl JoinPlan {
    // How many steps of `run` can be joined at once, and their join, if two
    // of those atoms read a variable `bound` or a scalar `known` leaves
    // free, neither reaching its rows through it. A client table atom ends
    // the run before it when a term of it is neither bound before the run
    // nor a scalar the run's atoms agree on.
    fn of(shapes: &[Shape], run: &[Step], bound: &[bool], known: &[bool]) -> (usize, Option<Self>) {
        let mut words: Vec<Word> = known
            .iter()
            .map(|&known| if known { Word::Known } else { Word::Free })
            .collect();
        let mut order: SmallVec<[Attr; 8]> = SmallVec::new();
        let mut atoms: SmallVec<[JoinAtom; 4]> = SmallVec::new();
        for step in run {
            let mut attr = |words: &mut [Word], slot: Scalar| {
                order.push(Attr::Scalar(slot));
                words[slot as usize] = Word::Attr(order.len() as u32 - 1);
                order.len() as u32 - 1
            };
            let shape = match *step {
                Step::Atom(atom) | Step::Parents { atom, .. } | Step::Table { atom, .. } => {
                    &shapes[atom]
                }
                _ => unreachable!("a run holds row and client table atoms"),
            };
            let rows = match shape {
                Shape::Rows(rows) => rows,
                Shape::Words(tuples) => {
                    let agreed = |term: &Expr| match term.lone() {
                        Some(slot) => words[slot as usize] != Word::Written,
                        None => term.ready(known),
                    };
                    if !tuples.terms.iter().all(agreed) {
                        break;
                    }
                    let mut cells: SmallVec<[(Level, u8); 6]> = SmallVec::new();
                    for (column, term) in tuples.terms.iter().enumerate() {
                        let level = match term.lone().map(|slot| (slot, words[slot as usize])) {
                            Some((slot, Word::Free)) => Level::Attr(attr(&mut words, slot)),
                            Some((_, Word::Attr(at))) => Level::Attr(at),
                            _ => Level::Key(column as u8),
                        };
                        cells.push((level, column as u8));
                    }
                    // The columns looked up first; the attributes follow in
                    // attribute order, so the trie descends the way the join
                    // binds.
                    cells.sort_by_key(|&(level, column)| match level {
                        Level::Attr(at) => (1, at, column),
                        _ => (0, 0, column),
                    });
                    atoms.push(JoinAtom {
                        step: *step,
                        columns: [cells.iter().map(|cell| cell.1).collect(), SmallVec::new()],
                        levels: cells.iter().map(|cell| cell.0).collect(),
                        cell: None,
                    });
                    continue;
                }
                _ => unreachable!("a run holds row and client table atoms"),
            };
            // A row atom writes its scalars whatever they held: only the
            // first to write its selected cell is held to it.
            if let Some(tag) = rows.tag {
                words[tag as usize] = Word::Written;
            }
            let cell = rows.cell.and_then(|slot| match words[slot as usize] {
                Word::Free => Some(attr(&mut words, slot)),
                _ => {
                    words[slot as usize] = Word::Written;
                    None
                }
            });
            // The loop nest binds a sideways atom's value, then each atom's
            // arguments in column order.
            let (key, sideways) = match *step {
                Step::Parents { slot, .. } => (rows.args[slot as usize].var, Some(rows.key)),
                _ => (rows.key, None),
            };
            for var in sideways
                .into_iter()
                .chain(rows.args.iter().map(|arg| arg.var))
            {
                if !bound[var as usize] && !order.contains(&Attr::Var(var)) {
                    order.push(Attr::Var(var));
                }
            }
            let value = rows.value;
            let mut cells: SmallVec<[(Var, u8, u8); 6]> = rows
                .args
                .iter()
                .map(|arg| (arg.var, arg.column, arg.swapped))
                .collect();
            cells.push((rows.key, value, value));
            // What is bound before the run is looked up first, the key
            // ahead of the rest; the run's variables follow in attribute
            // order, so every trie descends the way the join binds.
            let position = |var: Var| order.iter().position(|&other| other == Attr::Var(var));
            cells.sort_by_key(|&(var, column, _)| match position(var) {
                None => (0, usize::from(var != key), column),
                Some(at) => (1, at, column),
            });
            let select = rows.select.column;
            let written: SmallVec<[u8; 6]> = std::iter::once(select)
                .chain(cells.iter().map(|cell| cell.1))
                .collect();
            let swapped = match rows.when {
                Some(_) => std::iter::once(select)
                    .chain(cells.iter().map(|cell| cell.2))
                    .collect(),
                None => SmallVec::new(),
            };
            let levels = cells.iter().map(|&(var, _, _)| match position(var) {
                Some(at) => Level::Attr(at as u32),
                None => Level::Known(var),
            });
            atoms.push(JoinAtom {
                step: *step,
                columns: [written, swapped],
                levels: std::iter::once(Level::Select).chain(levels).collect(),
                cell,
            });
        }
        let len = atoms.len();
        // An atom reached through a variable reads only the rows holding it,
        // so nothing is intersected there: the loop nest looks it up.
        let shared = order.iter().enumerate().any(|(at, &attr)| {
            let reads = |atom: &&JoinAtom| {
                let reached = attr == Attr::Var(atom.lookup(shapes).unwrap_or(Var::MAX));
                let read =
                    atom.cell == Some(at as u32) || atom.levels.contains(&Level::Attr(at as u32));
                read && !(reached && matches!(attr, Attr::Var(_)))
            };
            atoms.iter().filter(reads).count() >= 2
        });
        // A decomposition's masks hold this many atoms and attributes.
        if !shared || len > 64 || order.len() > 64 {
            return (len, None);
        }
        let edges: SmallVec<[u64; 8]> = atoms
            .iter()
            .map(|atom| {
                let cell = atom.cell.map_or(0, |at| 1 << at);
                atom.levels.iter().fold(cell, |vars, level| match level {
                    Level::Attr(at) => vars | 1 << at,
                    _ => vars,
                })
            })
            .collect();
        let join = Self {
            len,
            vars: order,
            bags: decompose(&edges),
            atoms,
        };
        (len, Some(join))
    }
}

fn atom_vars(atom: &Atom) -> SmallVec<[Var; 6]> {
    match atom {
        Atom::Rows(rows) => rows
            .terms
            .iter()
            .filter_map(|term| match term {
                super::ir::Term::Var(var) => Some(*var),
                super::ir::Term::Any => None,
            })
            .collect(),
        Atom::Lattice(lattice) => SmallVec::from_slice(&[lattice.key]),
        Atom::Place(place) => match place.read {
            Read::Offset { base, .. } => SmallVec::from_slice(&[place.key, base]),
            _ => SmallVec::from_slice(&[place.key]),
        },
        Atom::Words(_) => SmallVec::new(),
    }
}
