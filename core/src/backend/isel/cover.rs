//! Match bindings, their pruning, and the PBQP cover that constructs the
//! incumbent assignment of a region's frozen problem.

use std::collections::{HashMap, HashSet};

use tir::sem::SymKind;
use tir_pbqp::{self as pbqp, INF_COST, PbqpMatrix, PbqpProblem};
use tir_relational::ClassId as Id;

use super::problem::{Policy, RegionProblem};

#[derive(Clone, Debug)]
pub(crate) struct CaptureBindings {
    pub(crate) entries: Vec<(u32, Id)>,
}

impl CaptureBindings {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Bind `symbol` to the class the match found for it. A pattern holds one
    /// capture per operand, so a symbol is bound once and there is no second
    /// reading to disagree with.
    pub(crate) fn bind(&mut self, symbol: u32, class: Id) {
        self.entries.push((symbol, class));
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PatternNodeBinding {
    pub(crate) pattern_node: Id,
    pub(crate) class: Id,
    pub(crate) is_boundary: bool,
    /// The chain the matched access reads ([`super::pattern::PatternNodeMeta`]).
    /// It names the access; the match neither computes it nor consumes it, so
    /// the effect model reads it and demands nothing for it.
    pub(crate) is_state: bool,
    pub(crate) demand: BoundaryDemand,
    /// Where this operand's register class views its storage element (see
    /// [`super::RegisterRequirement::view_offset`]).
    pub(crate) view_offset: u32,
    /// The width this operand reads its register at, when it reads it whole
    /// (see [`super::RegisterRequirement::whole_width`]).
    pub(crate) whole_width: Option<u32>,
    pub(crate) low_extract: bool,
}

/// What a boundary binding requires of its class. A register operand needs the
/// value materialized in a register; an immediate (or constant template) is
/// encoded inline, so any constant class satisfies it; a structural boundary (a
/// width variable) demands nothing — the emitter reads it from the match.
/// Ordered by how demanding the requirement is: a structural boundary needs
/// nothing, an immediate needs a constant, a register needs materialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BoundaryDemand {
    #[default]
    Structural,
    Immediate,
    Register,
}

#[derive(Clone, Debug)]
pub(crate) struct FullMatchBindings {
    pub(crate) captures: CaptureBindings,
    pub(crate) pattern_nodes: Vec<PatternNodeBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PbqpIselAlternative {
    NotDemanded,
    /// The effect executes inside this selected match, without a separate value.
    CoveredBy {
        match_id: usize,
    },
    Tile {
        match_id: usize,
    },
    /// A rewrite-introduced source left uncomputed: each low-extract view of it
    /// is tiled in its own right instead.
    Deferred,
}

/// What a match asks of one class it binds besides its root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChildDemand {
    /// A register read at these view offsets, whole at these widths (paired
    /// with whether the read goes through a low-extract view).
    Register {
        offsets: Vec<u32>,
        widths: Vec<(u32, bool)>,
    },
    /// Encoded inline: the class must be a constant.
    Immediate,
    /// An effect the match performs inside its own instruction.
    Effect,
    None,
}

#[derive(Clone, Debug)]
pub(crate) struct PbqpIselMatch {
    pub(crate) pattern_index: usize,
    pub(crate) rule_index: usize,
    /// Which of the rule's results the match roots.
    pub(crate) port: usize,
    /// The instance the match is a result of: the first match of the region
    /// binding the same rule to the same operands. Its cost is paid once.
    pub(crate) instance: usize,
    pub(crate) root: Id,
    pub(crate) pattern_root: Id,
    pub(crate) bindings: FullMatchBindings,
    pub(crate) cost: u64,
    /// Where the rule's destination class views its storage element. A value
    /// crosses a boundary for free only between equal offsets: no instruction
    /// moves bits across views implicitly.
    pub(crate) result_view_offset: u32,
    /// The width of the register the rule defines. A reader that reads its
    /// operand whole answers only at this width.
    pub(crate) result_width: Option<u32>,
    /// What the match demands of each class it binds besides its root, in the
    /// order it binds them.
    pub(crate) uses: Vec<(Id, ChildDemand)>,
    /// The effect classes the match can stand for instead of an instance of
    /// their own.
    pub(crate) covers: Vec<Id>,
    /// The effects the match performs inside its own instruction — the interior
    /// classes it recomputes, in binding order. The chain a memory access reads
    /// is not one of them: every access on a chain names it, and two reads of
    /// one state are not two effects, so a state binding stays out.
    pub(crate) effects: Vec<Id>,
}

impl PbqpIselMatch {
    /// Derive what the match's bindings demand of the classes they name.
    /// `pure` says whether a class holds only value expressions.
    pub(crate) fn with_demands(mut self, pure: impl Fn(Id) -> bool) -> Self {
        let nodes = &self.bindings.pattern_nodes;
        for binding in nodes {
            let child = binding.class;
            if binding.is_state || child == self.root {
                continue;
            }
            if !self.uses.iter().any(|(class, _)| *class == child) {
                let demand = self.child_demand(child, &pure);
                self.uses.push((child, demand));
            }
            if !binding.is_boundary && !pure(child) {
                self.covers.push(child);
            }
        }
        self.effects = nodes
            .iter()
            .filter(|binding| {
                !binding.is_boundary
                    && !binding.is_state
                    && binding.pattern_node != self.pattern_root
                    && !pure(binding.class)
            })
            .map(|binding| binding.class)
            .collect();
        self
    }

    fn child_demand(&self, child: Id, pure: &impl Fn(Id) -> bool) -> ChildDemand {
        let mut register = false;
        let mut immediate = false;
        let mut effect = false;
        let mut offsets: Vec<u32> = Vec::new();
        let mut widths: Vec<(u32, bool)> = Vec::new();
        for binding in &self.bindings.pattern_nodes {
            if binding.class != child {
                continue;
            }
            if binding.is_boundary {
                register |= binding.demand == BoundaryDemand::Register;
                immediate |= binding.demand == BoundaryDemand::Immediate;
                if binding.demand == BoundaryDemand::Register {
                    if !offsets.contains(&binding.view_offset) {
                        offsets.push(binding.view_offset);
                    }
                    if let Some(width) = binding.whole_width
                        && !widths.contains(&(width, binding.low_extract))
                    {
                        widths.push((width, binding.low_extract));
                    }
                }
            } else if binding.pattern_node != self.pattern_root && !binding.is_state && !pure(child)
            {
                effect = true;
            }
        }
        if register {
            ChildDemand::Register { offsets, widths }
        } else if immediate {
            ChildDemand::Immediate
        } else if effect {
            ChildDemand::Effect
        } else {
            ChildDemand::None
        }
    }

    fn demand_of(&self, child: Id) -> Option<&ChildDemand> {
        self.uses
            .iter()
            .find(|(class, _)| *class == child)
            .map(|(_, demand)| demand)
    }
}

/// Why the PBQP cover produced no assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoverError {
    Infeasible,
    /// The heuristic search gave up; the problem may still have a cover.
    Exhausted,
}

/// The failed heuristic choices a region's cover may back out of before the
/// search gives up, per class it decides.
const BACKTRACKS_PER_CLASS: u64 = 64;

/// The cover the PBQP heuristic finds for a region under `policy`: the
/// alternative chosen for every class of the problem, in class order.
pub(crate) fn solve_cover(
    problem: &RegionProblem,
    policy: &Policy,
) -> Result<Vec<PbqpIselAlternative>, CoverError> {
    let classes = &problem.classes;
    let matches = &problem.matches;
    let index: HashMap<Id, usize> = classes.iter().enumerate().map(|(i, &c)| (c, i)).collect();
    let class_index = |c: Id| index.get(&c).copied();

    let mut alternatives_by_node = vec![Vec::<PbqpIselAlternative>::new(); classes.len()];
    for (i, &c) in classes.iter().enumerate() {
        if !policy.demanded(c) || policy.available(c) {
            alternatives_by_node[i].push(PbqpIselAlternative::NotDemanded);
        }
        alternatives_by_node[i].extend(
            policy
                .views
                .contains_key(&c)
                .then_some(PbqpIselAlternative::Deferred),
        );
    }

    for (match_id, m) in matches.iter().enumerate() {
        let Some(root_index) = class_index(m.root) else {
            continue;
        };
        alternatives_by_node[root_index].push(PbqpIselAlternative::Tile { match_id });
        for &class in &m.covers {
            if problem.materialized(policy, class) {
                continue;
            }
            if let Some(index) = class_index(class) {
                let alternatives = &mut alternatives_by_node[index];
                let covered = PbqpIselAlternative::CoveredBy { match_id };
                if !alternatives.contains(&covered) {
                    alternatives.push(covered);
                }
            }
        }
    }

    if alternatives_by_node.iter().any(Vec::is_empty) {
        return Err(CoverError::Infeasible);
    }
    if classes
        .iter()
        .all(|&class| !policy.demanded(class) || policy.available(class))
    {
        return Ok(vec![PbqpIselAlternative::NotDemanded; classes.len()]);
    }

    let mut pbqp = PbqpProblem::new();
    for alternatives in &alternatives_by_node {
        let costs = alternatives
            .iter()
            .map(|alternative| match alternative {
                PbqpIselAlternative::Tile { match_id } => matches[*match_id].cost,
                PbqpIselAlternative::NotDemanded
                | PbqpIselAlternative::CoveredBy { .. }
                | PbqpIselAlternative::Deferred => 0,
            })
            .collect();
        pbqp.add_node(costs);
    }

    let mut edge_pairs = deferral_edges(policy, &class_index);
    for m in matches {
        let Some(ri) = class_index(m.root) else {
            continue;
        };
        for (class, _) in &m.uses {
            if let Some(ci) = class_index(*class) {
                edge_pairs.insert(ordered_pair(ri, ci));
            }
        }
    }

    let effect_footprints: Vec<Vec<Id>> = matches
        .iter()
        .map(|matched| {
            let mut footprint = matched.effects.clone();
            footprint.sort();
            footprint.dedup();
            footprint
        })
        .collect();
    // Only matches sharing an effect class can conflict, so index by class
    // instead of comparing every pair of matches.
    let mut matches_by_effect: HashMap<Id, Vec<usize>> = HashMap::new();
    for (index, footprint) in effect_footprints.iter().enumerate() {
        for &class in footprint {
            matches_by_effect.entry(class).or_default().push(index);
        }
    }
    for sharing in matches_by_effect.values() {
        for (position, &lhs) in sharing.iter().enumerate() {
            let Some(li) = class_index(matches[lhs].root) else {
                continue;
            };
            for &rhs in &sharing[position + 1..] {
                let Some(ri) = class_index(matches[rhs].root) else {
                    continue;
                };
                if li != ri {
                    edge_pairs.insert(ordered_pair(li, ri));
                }
            }
        }
    }

    let compatible = |child: Id, parent_alt, child_alt| {
        alternatives_compatible(
            child,
            parent_alt,
            child_alt,
            matches,
            problem.facts(child).int.is_some(),
            policy.available(child),
        )
    };
    let mut edge_pairs: Vec<(usize, usize)> = edge_pairs.into_iter().collect();
    edge_pairs.sort_unstable();
    for (li, ri) in edge_pairs {
        let left_class = classes[li];
        let right_class = classes[ri];
        let left_alts = &alternatives_by_node[li];
        let right_alts = &alternatives_by_node[ri];
        let mut matrix = PbqpMatrix::zero(left_alts.len(), right_alts.len());

        for (left_idx, left_alt) in left_alts.iter().enumerate() {
            for (right_idx, right_alt) in right_alts.iter().enumerate() {
                let compatible = compatible(right_class, left_alt, right_alt)
                    && compatible(left_class, right_alt, left_alt)
                    && !effect_tiles_conflict(left_alt, right_alt, &effect_footprints)
                    && deferral_compatible(left_class, left_alt, right_class, right_alt, policy)
                    && deferral_compatible(right_class, right_alt, left_class, left_alt, policy);
                if !compatible {
                    matrix.set(left_idx, right_idx, INF_COST);
                }
            }
        }
        pbqp.add_edge(
            pbqp::PbqpNodeId::from_index(li),
            pbqp::PbqpNodeId::from_index(ri),
            matrix,
        );
    }

    crate::memstats::pbqp_census(
        "isel-cover",
        pbqp.node_count(),
        pbqp.edge_count(),
        pbqp.matrix_bytes(),
    );

    crate::backend::pbqp_dump::dump(crate::backend::pbqp_dump::PbqpTaskKind::Isel, |w, kind| {
        pbqp.write_json(w, kind)
    });
    let budget = BACKTRACKS_PER_CLASS.saturating_mul(classes.len() as u64);
    let solution = pbqp::solve_within(&pbqp, budget).map_err(|error| match error {
        pbqp::PbqpSolveError::Exhausted => CoverError::Exhausted,
        _ => CoverError::Infeasible,
    })?;
    Ok(solution
        .choices
        .iter()
        .copied()
        .enumerate()
        .map(|(node, choice)| alternatives_by_node[node][choice].clone())
        .collect())
}

/// The PBQP edges joining each deferrable source to its views.
fn deferral_edges(
    policy: &Policy,
    class_index: &dyn Fn(Id) -> Option<usize>,
) -> HashSet<(usize, usize)> {
    policy
        .views
        .iter()
        .flat_map(|(&source, views)| views.iter().map(move |&view| (source, view)))
        .filter_map(|(source, view)| Some(ordered_pair(class_index(source)?, class_index(view)?)))
        .collect()
}

/// A deferred source leaves each of its views to a tile of its own.
fn deferral_compatible(
    source: Id,
    source_alt: &PbqpIselAlternative,
    view: Id,
    view_alt: &PbqpIselAlternative,
    policy: &Policy,
) -> bool {
    !matches!(source_alt, PbqpIselAlternative::Deferred)
        || !policy
            .views
            .get(&source)
            .is_some_and(|views| views.contains(&view))
        || matches!(view_alt, PbqpIselAlternative::Tile { .. })
}

fn ordered_pair(lhs: usize, rhs: usize) -> (usize, usize) {
    if lhs < rhs { (lhs, rhs) } else { (rhs, lhs) }
}

fn effect_tiles_conflict(
    lhs: &PbqpIselAlternative,
    rhs: &PbqpIselAlternative,
    footprints: &[Vec<Id>],
) -> bool {
    let (PbqpIselAlternative::Tile { match_id: lhs }, PbqpIselAlternative::Tile { match_id: rhs }) =
        (lhs, rhs)
    else {
        return false;
    };
    let (lhs, rhs) = (&footprints[*lhs], &footprints[*rhs]);
    lhs.iter().any(|class| rhs.binary_search(class).is_ok())
}

/// Drop matches dominated by an interchangeable alternative: same root class,
/// same internal-class coverage, same boundary operands, but no cheaper, no
/// more specific, no less demanding of its boundaries, and no better at
/// answering the readers of its root. Specificity (the
/// number of type-constrained pattern nodes) breaks ties between otherwise
/// identical matches without ever touching the PBQP objective — an i32 `addw`
/// beats the untyped `add` at equal cost — and at equal cost/specificity a
/// match folding a class as an immediate beats one demanding it in a register
/// (which may force a whole materializer chain), while a genuinely cheaper
/// instruction still wins on cost alone.
///
/// `width` is the width the matches' root class is known to have. A reader
/// read whole reads a class at that width, so a match defining exactly it
/// answers every reader; one defining another width answers only the readers
/// that width admits, and stands in for nothing but its like.
pub(crate) fn prune_dominated_matches(
    specificity: &[usize],
    width: Option<u32>,
    matches: &mut Vec<PbqpIselMatch>,
) {
    // The width a match defines where that restricts who reads it.
    let defined = |m: &PbqpIselMatch| m.result_width.filter(|defined| Some(*defined) != width);
    let footprint = |m: &PbqpIselMatch| {
        let mut boundaries = Vec::new();
        let mut internals = Vec::new();
        for binding in &m.bindings.pattern_nodes {
            if binding.is_boundary {
                // The width a register is read at is part of what the operand
                // demands; an immediate or structural operand reads none.
                let read = match binding.demand {
                    BoundaryDemand::Register => (binding.whole_width, binding.low_extract),
                    BoundaryDemand::Immediate | BoundaryDemand::Structural => (None, false),
                };
                boundaries.push(((binding.class, binding.view_offset), (binding.demand, read)));
            } else if binding.pattern_node != m.pattern_root && !binding.is_state {
                internals.push(binding.class);
            }
        }
        // Sorting by (class, view offset, demand) puts equal class multisets in
        // the same class order, so within a group demands compare positionally
        // over aligned classes.
        boundaries.sort();
        internals.sort();
        let (classes, demands): (Vec<(Id, u32)>, Vec<_>) = boundaries.into_iter().unzip();
        ((m.root, m.result_view_offset), classes, demands, internals)
    };
    let footprints: Vec<_> = matches.iter().map(footprint).collect();

    // Matches reading or writing a different register view are not
    // interchangeable — a value at one bit offset is not the value at another —
    // so the view offsets join the grouping key rather than the comparison.
    let mut groups: HashMap<_, Vec<usize>> = HashMap::new();
    for (index, (result, classes, _, internals)) in footprints.iter().enumerate() {
        groups
            .entry((result, classes, internals))
            .or_default()
            .push(index);
    }

    let comparison_key = |index: usize| {
        (
            matches[index].cost,
            specificity[matches[index].pattern_index],
            &footprints[index].2,
            defined(&matches[index]),
        )
    };
    // An operand demands no more than another when it asks for no more of the
    // class and admits every producer the other admits: it reads no whole
    // width, or the same one, and reads through a view wherever the other does.
    let demands_no_more = |a: &(BoundaryDemand, (Option<u32>, bool)),
                           b: &(BoundaryDemand, (Option<u32>, bool))| {
        let ((demand_a, (whole_a, view_a)), (demand_b, (whole_b, view_b))) = (a, b);
        demand_a <= demand_b && (whole_a.is_none() || (whole_a == whole_b && (*view_a || !view_b)))
    };
    let dominates = |a: usize, b: usize| {
        let (cost_a, spec_a, demands_a, defined_a) = comparison_key(a);
        let (cost_b, spec_b, demands_b, defined_b) = comparison_key(b);
        let demands_le = demands_a
            .iter()
            .zip(demands_b)
            .all(|(da, db)| demands_no_more(da, db));
        cost_a <= cost_b
            && spec_a >= spec_b
            && demands_le
            && (defined_a.is_none() || defined_a == defined_b)
            && (cost_a < cost_b || spec_a > spec_b || demands_a != demands_b)
    };

    let mut keep = vec![true; matches.len()];
    for group in groups.values() {
        // Domination depends only on the comparison key, and it is a strict
        // partial order over distinct keys, so one comparison per pair of
        // distinct keys decides every member of the group.
        let mut representatives: Vec<usize> = Vec::new();
        let mut representative_of: Vec<usize> = Vec::with_capacity(group.len());
        for &index in group {
            let existing = representatives
                .iter()
                .position(|&other| comparison_key(other) == comparison_key(index));
            representative_of.push(existing.unwrap_or(representatives.len()));
            if existing.is_none() {
                representatives.push(index);
            }
        }

        let dominated: Vec<bool> = representatives
            .iter()
            .map(|&b| representatives.iter().any(|&a| a != b && dominates(a, b)))
            .collect();
        // Equal-key members are fully interchangeable — identical cost,
        // constraints and conflicts — so only the representative survives.
        for (&index, &representative) in group.iter().zip(&representative_of) {
            keep[index] = !dominated[representative] && representatives[representative] == index;
        }
    }

    // A free tile constrains nothing: every binding is state, the root itself,
    // or a structural boundary, so its compatibility rows are all-true and its
    // effect footprint empty. Any tile at the same root and view offset that
    // costs no less and answers no reader it does not is dominated by it
    // outright, whatever its boundaries — this is what keeps a constant class (into which assumptions merge every proven
    // condition) from carrying thousands of comparison-shaped alternatives.
    let is_free = |m: &PbqpIselMatch| {
        m.bindings.pattern_nodes.iter().all(|binding| {
            binding.is_state
                || (binding.pattern_node == m.pattern_root && binding.class == m.root)
                || (binding.is_boundary && binding.demand == BoundaryDemand::Structural)
        })
    };
    let free_key = |index: usize| {
        (
            matches[index].cost,
            std::cmp::Reverse(specificity[matches[index].pattern_index]),
        )
    };
    let result = |m: &PbqpIselMatch, defined| (m.root, m.result_view_offset, defined);
    let mut best_free: HashMap<_, usize> = HashMap::new();
    for (index, m) in matches.iter().enumerate() {
        if !keep[index] || !is_free(m) {
            continue;
        }
        best_free
            .entry(result(m, defined(m)))
            .and_modify(|best| {
                if free_key(index) < free_key(*best) {
                    *best = index;
                }
            })
            .or_insert(index);
    }
    if !best_free.is_empty() {
        for (index, m) in matches.iter().enumerate() {
            // A free tile of the class's own width answers every reader; one
            // of another width only the readers `m` at that width answers. A
            // tie stands only between tiles defining the same width.
            for like in [false, true] {
                let width = if like { defined(m) } else { None };
                let Some(&free) = best_free.get(&result(m, width)) else {
                    continue;
                };
                if free == index || !keep[index] {
                    continue;
                }
                let (free_cost, std::cmp::Reverse(free_spec)) = free_key(free);
                let cost = m.cost;
                let spec = specificity[m.pattern_index];
                let tied = defined(m) == defined(&matches[free]);
                if cost > free_cost
                    || (cost == free_cost && (spec < free_spec || (tied && spec == free_spec)))
                {
                    keep[index] = false;
                }
            }
        }
    }

    let mut kept = keep.iter();
    matches.retain(|_| *kept.next().unwrap());
    // Survivors with different width contracts no longer meet in a group. The
    // cover takes the first of equal-cost alternatives, so the more specific
    // match goes first.
    matches.sort_by_key(|matched| std::cmp::Reverse(specificity[matched.pattern_index]));
}

/// The demanded classes no instance can root, as the diagnostic naming the
/// semantic kinds a target rule is missing for.
pub(crate) fn completeness_error(problem: &RegionProblem, policy: &Policy) -> Option<String> {
    let rooted: HashSet<Id> = problem.matches.iter().map(|matched| matched.root).collect();

    let mut missing: Vec<SymKind> = Vec::new();
    for &class in &policy.demanded {
        let deferred = policy
            .views
            .get(&class)
            .is_some_and(|views| views.iter().all(|view| rooted.contains(view)));
        let facts = problem.facts(class);
        if rooted.contains(&class) || policy.available(class) || deferred || facts.source != class {
            continue;
        }
        if let Some(kind) = facts.kind
            && !missing.contains(&kind)
        {
            missing.push(kind);
        }
    }

    if missing.is_empty() {
        return None;
    }
    missing.sort();
    Some(
        missing
            .iter()
            .map(|kind| format!("missing atomic materializer rule for semantic kind {kind:?}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn alternatives_compatible(
    child: Id,
    parent_alt: &PbqpIselAlternative,
    child_alt: &PbqpIselAlternative,
    matches: &[PbqpIselMatch],
    constant: bool,
    available: bool,
) -> bool {
    match parent_alt {
        PbqpIselAlternative::CoveredBy { match_id } => {
            matches[*match_id].root != child
                || matches!(child_alt, PbqpIselAlternative::Tile { match_id: owner } if owner == match_id)
        }
        PbqpIselAlternative::Tile { match_id } => {
            matches[*match_id].demand_of(child).is_none_or(|demand| {
                demand.accepts(*match_id, child_alt, matches, constant, available)
            })
        }
        PbqpIselAlternative::NotDemanded | PbqpIselAlternative::Deferred => true,
    }
}
