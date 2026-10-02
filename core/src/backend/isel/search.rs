//! Bounded search over the function's frozen selection problem.
//!
//! The records of [`super::problem`] become one Bool/QF_BV formula: a Boolean
//! per implementation instance and per control realization, the legality of
//! every input against the instances that can produce it, one owner per
//! effect, a strictly increasing rank along every selected register
//! dependency, and the summed cost as a bit-vector. The existing bit-blaster
//! lowers it and the SAT solver answers, under a conflict budget, whether an
//! assignment strictly cheaper than the best known one exists.
//!
//! An answer is a proposal. The caller checks each decoded assignment against
//! the problem's own records before it replaces the incumbent, and a rejected
//! one is excluded from the next query. Running out of budget keeps the
//! incumbent: it says nothing about whether the problem is feasible.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use tir::{
    NodeId, RegionId,
    sem::{SymKind, SymPayload},
};
use tir_adt::{APInt, Dag};
use tir_relational::ClassId as Id;
use tir_symbolic::bitblast::blast;
use tir_symbolic::sat::{Lit, SatResult};

use super::cover::{BoundaryDemand, ChildDemand, PbqpIselAlternative};
use super::problem::{
    Availability, ControlChoice, GuardCandidate, Naming, RegionAssignment, RegionProblem,
};

/// The work one function's search may spend. Budgets and solve order are
/// fixed, so a compilation is reproducible.
pub(crate) struct Budget {
    /// Look for an assignment cheaper than a valid incumbent. Without it the
    /// search only constructs an assignment where the cover found none.
    pub(crate) improve: bool,
    /// Solver conflicts per query.
    pub(crate) conflicts: u64,
    /// Queries per function: each narrows the cost range, excludes a rejected
    /// assignment, or ends the search.
    pub(crate) queries: u32,
}

impl Budget {
    /// The budget this process searches under. Proving that no cheaper
    /// assignment exists costs more than the cover itself, so it is off unless
    /// `TIR_ISEL_SEARCH` asks for it.
    pub(crate) fn current() -> &'static Budget {
        static BUDGET: OnceLock<Budget> = OnceLock::new();
        BUDGET.get_or_init(|| Budget {
            improve: std::env::var_os("TIR_ISEL_SEARCH").is_some_and(|value| value != "0"),
            conflicts: 20_000,
            queries: 16,
        })
    }
}

/// How a search ended. Each speaks for the frozen problem and its additive
/// objective only: instances no rule or axiom produced are not in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// No assignment of the problem is cheaper than the one returned.
    Optimal,
    /// An assignment was constructed where there was none. Nothing says it is
    /// the cheapest.
    Feasible,
    /// The problem has no assignment at all.
    Infeasible,
    /// The budget ended first: a cheaper assignment may exist.
    Exhausted,
    /// The problem is larger than the budget admits; the incumbent stands.
    Skipped,
}

impl Status {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Status::Optimal => "optimal",
            Status::Feasible => "feasible",
            Status::Infeasible => "infeasible",
            Status::Exhausted => "exhausted",
            Status::Skipped => "skipped",
        }
    }
}

pub(crate) struct Outcome {
    /// The cheapest validated assignment the search found, if it found one
    /// cheaper than the incumbent.
    pub(crate) assignment: Option<Vec<RegionAssignment>>,
    pub(crate) status: Status,
}

/// A formula under construction: a lowered Bool/QF_BV graph and the width of
/// every node, which is what the bit-blaster reads.
struct Formula {
    graph: Dag<SymKind, SymPayload<()>>,
    widths: Vec<Option<u32>>,
    symbols: u32,
    truth: NodeId,
    falsity: NodeId,
}

impl Formula {
    fn new() -> Self {
        let mut formula = Self {
            graph: Dag::new(),
            widths: Vec::new(),
            symbols: 0,
            truth: NodeId::from_index(0),
            falsity: NodeId::from_index(0),
        };
        formula.falsity = formula.constant(1, 0);
        formula.truth = formula.constant(1, 1);
        formula
    }

    fn node(&mut self, kind: SymKind, children: &[NodeId], width: u32) -> NodeId {
        let id = self.graph.add_node(kind);
        for &child in children {
            self.graph.add_edge(id, child);
        }
        self.widths.push(Some(width));
        id
    }

    fn width(&self, node: NodeId) -> u32 {
        self.widths[node.index()].expect("every formula node has a width")
    }

    fn constant(&mut self, width: u32, value: u64) -> NodeId {
        let id = self.node(SymKind::Constant, &[], width);
        self.graph
            .set_leaf_data(id, SymPayload::Int(APInt::new(width, value)));
        id
    }

    fn truth_of(&self, holds: bool) -> NodeId {
        if holds { self.truth } else { self.falsity }
    }

    /// A fresh variable of `width` bits and the symbol its model is read by.
    fn variable(&mut self, width: u32) -> (NodeId, u32) {
        let symbol = self.symbols;
        self.symbols += 1;
        let id = self.node(SymKind::Symbol, &[], width);
        self.graph.set_leaf_data(id, SymPayload::SymbolId(symbol));
        (id, symbol)
    }

    fn not(&mut self, a: NodeId) -> NodeId {
        if a == self.truth {
            self.falsity
        } else if a == self.falsity {
            self.truth
        } else {
            self.node(SymKind::Not, &[a], 1)
        }
    }

    fn and(&mut self, a: NodeId, b: NodeId) -> NodeId {
        if a == self.falsity || b == self.falsity {
            self.falsity
        } else if a == self.truth {
            b
        } else if b == self.truth || a == b {
            a
        } else {
            self.node(SymKind::And, &[a, b], 1)
        }
    }

    fn or(&mut self, a: NodeId, b: NodeId) -> NodeId {
        if a == self.truth || b == self.truth {
            self.truth
        } else if a == self.falsity {
            b
        } else if b == self.falsity || a == b {
            a
        } else {
            self.node(SymKind::Or, &[a, b], 1)
        }
    }

    fn any(&mut self, items: impl IntoIterator<Item = NodeId>) -> NodeId {
        let mut result = self.falsity;
        for item in items {
            result = self.or(result, item);
        }
        result
    }

    fn all(&mut self, items: impl IntoIterator<Item = NodeId>) -> NodeId {
        let mut result = self.truth;
        for item in items {
            result = self.and(result, item);
        }
        result
    }

    fn implies(&mut self, a: NodeId, b: NodeId) -> NodeId {
        let not_a = self.not(a);
        self.or(not_a, b)
    }

    /// `a + b` at a width neither operand can overflow.
    fn sum(&mut self, a: NodeId, b: NodeId) -> NodeId {
        let width = self.width(a).max(self.width(b)) + 1;
        let a = self.node(SymKind::ZExt, &[a], width);
        let b = self.node(SymKind::ZExt, &[b], width);
        self.node(SymKind::Add, &[a, b], width)
    }

    /// The value of the one chosen `(choice, cost)` pair, or zero when none is
    /// chosen. At most one choice holds.
    fn chosen_cost(&mut self, choices: &[(NodeId, u64)]) -> Option<NodeId> {
        let highest = choices.iter().map(|(_, cost)| *cost).max()?;
        let width = (u64::BITS - highest.leading_zeros()).max(1);
        let mut value = self.constant(width, 0);
        for &(choice, cost) in choices {
            let cost = self.constant(width, cost);
            value = self.node(SymKind::If, &[choice, cost, value], width);
        }
        Some(value)
    }
}

/// The variables standing for one region's choices.
struct RegionVars {
    /// One `(node, symbol)` per instance.
    tiles: Vec<(NodeId, u32)>,
    /// Per control outcome, each realization with its variable. Empty for an
    /// outcome the region's facts decide.
    controls: Vec<Vec<(ControlChoice, NodeId, u32)>>,
    /// Whether an instance leaves the register named by a base member.
    registers: HashMap<Id, NodeId>,
}

struct Encoding {
    formula: Formula,
    regions: Vec<RegionVars>,
    /// The summed cost of every chosen instance and control realization.
    cost: Option<NodeId>,
}

fn encode(problems: &[RegionProblem], fixed: &HashSet<(Id, RegionId)>) -> Encoding {
    let mut formula = Formula::new();
    let mut constraints = Vec::new();
    let mut costs = Vec::new();
    let mut regions: Vec<RegionVars> = Vec::new();
    let mut index: HashMap<RegionId, usize> = HashMap::new();
    for problem in problems {
        // The register an enclosing region leaves for a base member: one its
        // demand policy fixes, one an instance of it defines, or none.
        let held = |member: Id, region: RegionId| {
            if fixed.contains(&(member, region)) {
                return Ok(true);
            }
            index
                .get(&region)
                .and_then(|&at| regions[at].registers.get(&member))
                .map_or(Ok(false), |&node| Err(node))
        };
        let vars =
            RegionEncoder::new(&mut formula, &mut constraints, &mut costs, problem, &held).encode();
        index.insert(problem.region, regions.len());
        regions.push(vars);
    }
    // A balanced sum keeps the adders narrow.
    while costs.len() > 1 {
        costs = costs
            .chunks(2)
            .map(|pair| match pair {
                [a, b] => formula.sum(*a, *b),
                [a] => *a,
                _ => unreachable!("chunks of two"),
            })
            .collect();
    }
    let cost = costs.pop();
    // The conjunction is the graph's last node, which is the one blasted.
    let root = formula.all(constraints);
    let _ = formula.node(SymKind::And, &[root, formula.truth], 1);
    Encoding {
        formula,
        regions,
        cost,
    }
}

/// One region's part of the formula.
struct RegionEncoder<'a> {
    f: &'a mut Formula,
    constraints: &'a mut Vec<NodeId>,
    costs: &'a mut Vec<NodeId>,
    problem: &'a RegionProblem,
    /// Whether an enclosing region leaves a base member in a register: a
    /// known answer, or the node deciding it.
    held: &'a dyn Fn(Id, RegionId) -> Result<bool, NodeId>,
    tiles: Vec<(NodeId, u32)>,
    /// The instances rooted at each class.
    at: HashMap<Id, Vec<usize>>,
    /// Whether an instance is selected at each class.
    selected: HashMap<Id, NodeId>,
    /// The control realizations reading each class as a register.
    overlay: HashMap<Id, Vec<NodeId>>,
    /// The control realizations leaving each class without a value instance.
    waived: HashMap<Id, Vec<NodeId>>,
    demanded: HashMap<Id, NodeId>,
    materialized: HashMap<Id, NodeId>,
    available: HashMap<Id, NodeId>,
}

impl<'a> RegionEncoder<'a> {
    fn new(
        f: &'a mut Formula,
        constraints: &'a mut Vec<NodeId>,
        costs: &'a mut Vec<NodeId>,
        problem: &'a RegionProblem,
        held: &'a dyn Fn(Id, RegionId) -> Result<bool, NodeId>,
    ) -> Self {
        let tiles: Vec<(NodeId, u32)> = problem.matches.iter().map(|_| f.variable(1)).collect();
        let mut at: HashMap<Id, Vec<usize>> = HashMap::new();
        for (match_id, matched) in problem.matches.iter().enumerate() {
            at.entry(matched.root).or_default().push(match_id);
        }
        let selected = problem
            .classes
            .iter()
            .map(|&class| {
                let rooted = at.get(&class).into_iter().flatten();
                (class, f.any(rooted.map(|&m| tiles[m].0)))
            })
            .collect();
        Self {
            f,
            constraints,
            costs,
            problem,
            held,
            tiles,
            at,
            selected,
            overlay: HashMap::new(),
            waived: HashMap::new(),
            demanded: HashMap::new(),
            materialized: HashMap::new(),
            available: HashMap::new(),
        }
    }

    /// Classes are visited in the problem's order, never a map's, so the
    /// formula and the model it yields are the same in every process.
    fn encode(mut self) -> RegionVars {
        let controls = self.controls();
        self.demands();
        self.one_instance_per_class();
        self.classes_resolve();
        self.inputs_meet_producers();
        self.one_performer_per_effect();
        self.ranks();
        self.instance_costs();
        let registers = self
            .problem
            .register_names
            .iter()
            .map(|(class, &member)| (member, self.selected[class]))
            .collect();
        RegionVars {
            tiles: self.tiles,
            controls,
            registers,
        }
    }

    fn rooted(&self, class: Id) -> &[usize] {
        self.at.get(&class).map_or(&[], Vec::as_slice)
    }

    fn at_most_one(&mut self, choices: &[NodeId]) {
        for (position, &lhs) in choices.iter().enumerate() {
            for &rhs in &choices[position + 1..] {
                let both = self.f.and(lhs, rhs);
                self.constraints.push(self.f.not(both));
            }
        }
    }

    fn register(&mut self, member: Id, region: RegionId) -> NodeId {
        match (self.held)(member, region) {
            Ok(holds) => self.f.truth_of(holds),
            Err(node) => node,
        }
    }

    /// Whether a register holds the class without an instance of this region.
    fn external(&mut self, members: &[Id], availability: &Availability) -> NodeId {
        if availability.always {
            return self.f.truth;
        }
        let mut result = self.f.falsity;
        for &region in &availability.ancestors {
            for &member in members {
                let holds = self.register(member, region);
                result = self.f.or(result, holds);
            }
        }
        result
    }

    fn named(&mut self, naming: &Naming) -> NodeId {
        if naming.always {
            return self.f.truth;
        }
        let mut result = self.f.falsity;
        for &(member, region) in &naming.registers {
            let holds = self.register(member, region);
            result = self.f.or(result, holds);
        }
        result
    }

    /// Control realizations: exactly one per undecided outcome, each reading
    /// operands it can name.
    fn controls(&mut self) -> Vec<Vec<(ControlChoice, NodeId, u32)>> {
        let problem = self.problem;
        let mut controls = Vec::new();
        for control in &problem.controls {
            let mut alternatives = Vec::new();
            if control.decided.is_none() {
                for index in 0..control.fused.len() {
                    let (node, symbol) = self.f.variable(1);
                    alternatives.push((ControlChoice::Fused(index), node, symbol));
                }
                let (node, symbol) = self.f.variable(1);
                alternatives.push((ControlChoice::Nonzero, node, symbol));
                let nodes: Vec<NodeId> = alternatives.iter().map(|(_, node, _)| *node).collect();
                self.constraints.push(self.f.any(nodes.iter().copied()));
                self.at_most_one(&nodes);
            }
            let mut priced = Vec::new();
            for &(choice, node, _) in &alternatives {
                let (readable, cost) = match choice {
                    ControlChoice::Fused(index) => {
                        let guard = &control.fused[index];
                        for class in problem.guard_demands(guard) {
                            self.overlay.entry(class).or_default().push(node);
                        }
                        for &class in &control.waives {
                            self.waived.entry(class).or_default().push(node);
                        }
                        (self.guard_readable(guard), guard.cost)
                    }
                    ControlChoice::Nonzero => {
                        let source = problem.facts(control.condition).source;
                        self.overlay.entry(source).or_default().push(node);
                        let nameable = self.named(&control.nonzero_naming);
                        let tiled = self.selected.get(&source).copied();
                        (
                            self.f.or(tiled.unwrap_or(self.f.falsity), nameable),
                            problem.nonzero_cost,
                        )
                    }
                    ControlChoice::Decided(_) => continue,
                };
                self.constraints.push(self.f.implies(node, readable));
                priced.push((node, cost));
            }
            self.costs.extend(self.f.chosen_cost(&priced));
            controls.push(alternatives);
        }
        controls
    }

    /// Whether a fused branch can name every operand it reads.
    fn guard_readable(&mut self, guard: &GuardCandidate) -> NodeId {
        let mut readable = self.f.truth;
        for operand in &guard.captures {
            let facts = self.problem.facts(operand.class);
            let requirement = match (&facts.int, operand.register) {
                (Some(_), false) => continue,
                // A register operand reads a constant only through a register
                // some other use already forces.
                (Some(_), true) => {
                    let held = self.external(&facts.members, &facts.availability);
                    let forced = self.f.or(held, self.f.truth_of(facts.placed));
                    let nameable = self.named(&operand.naming);
                    self.f.and(nameable, forced)
                }
                (None, _) => self.named(&operand.naming),
            };
            readable = self.f.and(readable, requirement);
        }
        readable
    }

    /// What the chosen realizations demand of each class, and the registers
    /// that exist without an instance of this region.
    fn demands(&mut self) {
        let problem = self.problem;
        for &class in &problem.classes {
            let facts = problem.facts(class);
            let overlaid = self
                .f
                .any(self.overlay.get(&class).into_iter().flatten().copied());
            let waiver = self
                .f
                .any(self.waived.get(&class).into_iter().flatten().copied());
            let kept = self.f.not(waiver);
            let placed = self.f.and(self.f.truth_of(facts.placed), kept);
            self.demanded.insert(class, self.f.or(overlaid, placed));
            let fixed = self.f.truth_of(facts.register_demand);
            self.materialized.insert(class, self.f.or(overlaid, fixed));
        }
        for &class in &problem.classes {
            let source = problem.facts(class).source;
            let facts = problem.facts(source);
            let held = self.external(&facts.members, &facts.availability);
            let held = self.f.or(held, self.f.truth_of(facts.hook_constant));
            // A low-bit view re-reads its source's register, so it is there
            // once the source is produced.
            let viewed = match self.demanded.get(&source) {
                Some(&demanded) if source != class => demanded,
                _ => self.f.falsity,
            };
            self.available.insert(class, self.f.or(held, viewed));
        }
    }

    fn one_instance_per_class(&mut self) {
        for &class in &self.problem.classes {
            let rooted: Vec<NodeId> = self
                .rooted(class)
                .iter()
                .map(|&m| self.tiles[m].0)
                .collect();
            self.at_most_one(&rooted);
        }
    }

    /// Every class resolves: to an instance, to an existing register, to the
    /// instance performing it, or to its views.
    fn classes_resolve(&mut self) {
        let problem = self.problem;
        let mut coverers: HashMap<Id, Vec<usize>> = HashMap::new();
        for (match_id, matched) in problem.matches.iter().enumerate() {
            for &class in &matched.covers {
                coverers.entry(class).or_default().push(match_id);
            }
        }
        let mut views: HashMap<Id, Vec<Id>> = HashMap::new();
        for &class in &problem.classes {
            let source = problem.facts(class).source;
            if source != class && !problem.facts(source).has_values {
                views.entry(source).or_default().push(class);
            }
        }
        for &class in &problem.classes {
            let covering = coverers.get(&class).into_iter().flatten();
            let covered = self.f.any(covering.map(|&m| self.tiles[m].0));
            let unmaterialized = self.f.not(self.materialized[&class]);
            let covered = self.f.and(covered, unmaterialized);
            let deferred = match views.get(&class) {
                Some(views) => self.f.all(views.iter().map(|view| self.selected[view])),
                None => self.f.falsity,
            };
            let undemanded = self.f.not(self.demanded[&class]);
            let resolved = self.f.any([
                self.selected[&class],
                undemanded,
                self.available[&class],
                covered,
                deferred,
            ]);
            self.constraints.push(resolved);
        }
    }

    /// Each input of a selected instance meets a compatible producer.
    fn inputs_meet_producers(&mut self) {
        let problem = self.problem;
        for (match_id, matched) in problem.matches.iter().enumerate() {
            for (class, demand) in &matched.uses {
                let Some(&tiled) = self.selected.get(class) else {
                    continue;
                };
                let accepts = |state: &PbqpIselAlternative, available: bool| {
                    let constant = problem.facts(*class).int.is_some();
                    demand.accepts(match_id, state, &problem.matches, constant, available)
                };
                let untiled = self.f.not(tiled);
                let requirement = match demand {
                    ChildDemand::Register { .. } => {
                        let produced: Vec<NodeId> = self
                            .rooted(*class)
                            .iter()
                            .filter(|&&match_id| {
                                accepts(&PbqpIselAlternative::Tile { match_id }, false)
                            })
                            .map(|&producer| self.tiles[producer].0)
                            .collect();
                        let produced = self.f.any(produced);
                        let held = if accepts(&PbqpIselAlternative::NotDemanded, true) {
                            self.f.and(untiled, self.available[class])
                        } else {
                            self.f.falsity
                        };
                        self.f.or(produced, held)
                    }
                    ChildDemand::Immediate => self
                        .f
                        .truth_of(accepts(&PbqpIselAlternative::NotDemanded, true)),
                    // The effect stays out of a register of its own: nothing
                    // demands it, or this instance stands for it.
                    ChildDemand::Effect => {
                        let undemanded = self.f.not(self.demanded[class]);
                        let idle = self.f.or(undemanded, self.available[class]);
                        let unmaterialized = self.f.not(self.materialized[class]);
                        let owned = self.f.or(idle, unmaterialized);
                        self.f.and(untiled, owned)
                    }
                    ChildDemand::None => continue,
                };
                let chosen = self.tiles[match_id].0;
                self.constraints.push(self.f.implies(chosen, requirement));
            }
        }
    }

    fn one_performer_per_effect(&mut self) {
        let problem = self.problem;
        let mut performers: HashMap<Id, Vec<NodeId>> = HashMap::new();
        for (match_id, matched) in problem.matches.iter().enumerate() {
            for &class in &matched.effects {
                let performing = performers.entry(class).or_default();
                if !performing.contains(&self.tiles[match_id].0) {
                    performing.push(self.tiles[match_id].0);
                }
            }
        }
        for class in &problem.classes {
            if let Some(performing) = performers.get(class) {
                self.at_most_one(performing);
            }
        }
    }

    /// Selected register dependencies admit a strict order. Only classes on a
    /// potential cycle carry a rank: peeling classes nothing reads or that
    /// read nothing leaves exactly the ones a cycle can pass through.
    fn ranks(&mut self) {
        // `(reader instance, reader class, read class)`.
        let mut edges: Vec<(usize, Id, Id)> = Vec::new();
        for (match_id, matched) in self.problem.matches.iter().enumerate() {
            for binding in &matched.bindings.pattern_nodes {
                if binding.class != matched.root
                    && binding.is_boundary
                    && binding.demand == BoundaryDemand::Register
                    && self.at.contains_key(&binding.class)
                    && !edges.contains(&(match_id, matched.root, binding.class))
                {
                    edges.push((match_id, matched.root, binding.class));
                }
            }
        }
        loop {
            let readers: HashSet<Id> = edges.iter().map(|&(_, reader, _)| reader).collect();
            let read: HashSet<Id> = edges.iter().map(|&(_, _, read)| read).collect();
            let before = edges.len();
            edges.retain(|(_, reader, class)| read.contains(reader) && readers.contains(class));
            if edges.len() == before {
                break;
            }
        }
        let mut cyclic: Vec<Id> = edges.iter().map(|&(_, reader, _)| reader).collect();
        cyclic.sort();
        cyclic.dedup();
        let width = (usize::BITS - cyclic.len().leading_zeros()).max(1);
        let rank: HashMap<Id, NodeId> = cyclic
            .iter()
            .map(|&class| (class, self.f.variable(width).0))
            .collect();
        for (match_id, reader, class) in edges {
            let ordered = self.f.node(SymKind::ULt, &[rank[&class], rank[&reader]], 1);
            let dependent = self.f.and(self.tiles[match_id].0, self.selected[&class]);
            self.constraints.push(self.f.implies(dependent, ordered));
        }
    }

    /// The results of one instance are paid for once, however many are chosen.
    fn instance_costs(&mut self) {
        let matches = &self.problem.matches;
        let mut results: Vec<Vec<usize>> = vec![Vec::new(); matches.len()];
        for (match_id, matched) in matches.iter().enumerate() {
            results[matched.instance].push(match_id);
        }
        for &class in &self.problem.classes {
            let priced: Vec<(NodeId, u64)> = self
                .rooted(class)
                .iter()
                .filter(|&&m| results[matches[m].instance].len() == 1)
                .map(|&m| (self.tiles[m].0, matches[m].cost))
                .collect();
            self.costs.extend(self.f.chosen_cost(&priced));
        }
        for (instance, results) in results.iter().enumerate() {
            if results.len() > 1 {
                let chosen = self.f.any(results.iter().map(|&m| self.tiles[m].0));
                let cost = matches[instance].cost;
                self.costs.extend(self.f.chosen_cost(&[(chosen, cost)]));
            }
        }
    }
}

/// Clauses forcing a little-endian bit-vector to at most `bound`.
fn at_most(bits: &[Lit], bound: u64) -> Vec<Vec<Lit>> {
    if bits.len() < 64 && bound >> bits.len() != 0 {
        return Vec::new();
    }
    let one = |position: usize| position < 64 && (bound >> position) & 1 == 1;
    (0..bits.len())
        .filter(|&position| !one(position))
        .map(|position| {
            let mut clause = vec![bits[position].negate()];
            clause.extend(
                (position + 1..bits.len())
                    .filter(|&higher| one(higher))
                    .map(|higher| bits[higher].negate()),
            );
            clause
        })
        .collect()
}

/// Look for an assignment of the frozen problem strictly cheaper than
/// `incumbent`, or for any assignment when there is none. `fixed` holds the
/// registers the demand policy places whatever is chosen. `accept` is the
/// independent check a decoded assignment passes before it counts; it returns
/// the assignment without the instances nothing reads.
pub(crate) fn search(
    problems: &[RegionProblem],
    fixed: &HashSet<(Id, RegionId)>,
    incumbent: Option<u64>,
    accept: &dyn Fn(Vec<RegionAssignment>) -> Option<Vec<RegionAssignment>>,
    budget: &Budget,
) -> Outcome {
    if incumbent.is_some() && !budget.improve {
        return Outcome {
            assignment: None,
            status: Status::Skipped,
        };
    }
    let encoding = encode(problems, fixed);
    let formula = &encoding.formula;
    // The cheapest assignment costs at least `floor` and at most `best`.
    let mut floor = 0;
    let mut best = incumbent;
    let mut found = None;
    let mut rejected: Vec<Vec<(u32, bool)>> = Vec::new();
    let mut status = Status::Exhausted;
    for _ in 0..budget.queries {
        // Halve the range the cheapest assignment lies in.
        let bound = match best {
            Some(best) if best <= floor => {
                status = Status::Optimal;
                break;
            }
            Some(best) => Some(floor + (best - 1 - floor) / 2),
            None => None,
        };
        let Ok(mut blasted) = blast(&formula.graph, &formula.widths) else {
            break;
        };
        blasted.solver.add_clause(&[blasted.root_bit]);
        if let (Some(bound), Some(cost)) = (bound, encoding.cost) {
            for clause in at_most(&blasted.node_bits[cost.index()], bound) {
                blasted.solver.add_clause(&clause);
            }
        }
        for assignment in &rejected {
            let clause: Vec<Lit> = assignment
                .iter()
                .map(|&(symbol, value)| {
                    let bit = blasted.sym_bits[&symbol][0];
                    if value { bit.negate() } else { bit }
                })
                .collect();
            blasted.solver.add_clause(&clause);
        }
        let model = match blasted.solver.solve_with_budget(Some(budget.conflicts)) {
            SatResult::Sat(model) => model,
            SatResult::Unsat => match bound {
                Some(bound) => {
                    floor = bound + 1;
                    continue;
                }
                None => {
                    status = Status::Infeasible;
                    break;
                }
            },
            SatResult::Unknown => break,
        };
        let holds = |symbol: u32| {
            let bit = blasted.sym_bits[&symbol][0];
            model[bit.var().index()] ^ bit.is_negated()
        };
        let assignment: Vec<RegionAssignment> = problems
            .iter()
            .zip(&encoding.regions)
            .map(|(problem, vars)| decode(problem, vars, &holds))
            .collect();
        if let Some(assignment) = accept(assignment) {
            best = Some(
                problems
                    .iter()
                    .zip(&assignment)
                    .map(|(problem, assignment)| problem.cost(assignment))
                    .sum(),
            );
            found = Some(assignment);
            if !budget.improve {
                status = Status::Feasible;
                break;
            }
        } else {
            rejected.push(
                encoding
                    .regions
                    .iter()
                    .flat_map(|vars| {
                        vars.tiles
                            .iter()
                            .map(|&(_, symbol)| symbol)
                            .chain(vars.controls.iter().flatten().map(|&(_, _, symbol)| symbol))
                    })
                    .map(|symbol| (symbol, holds(symbol)))
                    .collect(),
            );
        }
    }
    Outcome {
        assignment: found,
        status,
    }
}

fn decode(
    problem: &RegionProblem,
    vars: &RegionVars,
    holds: &dyn Fn(u32) -> bool,
) -> RegionAssignment {
    let tiles = problem
        .matches
        .iter()
        .zip(&vars.tiles)
        .enumerate()
        .filter(|(_, (_, (_, symbol)))| holds(*symbol))
        .map(|(match_id, (matched, _))| (matched.root, match_id))
        .collect();
    let controls = problem
        .controls
        .iter()
        .zip(&vars.controls)
        .map(|(control, alternatives)| match control.decided {
            Some(known) => ControlChoice::Decided(known),
            None => alternatives
                .iter()
                .find(|(_, _, symbol)| holds(*symbol))
                .map_or(ControlChoice::Nonzero, |(choice, _, _)| *choice),
        })
        .collect();
    RegionAssignment { tiles, controls }
}
