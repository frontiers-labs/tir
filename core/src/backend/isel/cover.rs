//! PBQP cover construction over the saturated e-graph: match bindings, the
//! alternative/compatibility model, and the solved cover.

use std::collections::{HashMap, HashSet};

use tir::{
    ValueId,
    sem::{
        SymKind,
        egraph::{SemEGraph, class_int_binding},
    },
};
use tir_pbqp::{self as pbqp, INF_COST, PbqpMatrix, PbqpProblem};
use tir_relational::ClassId as Id;

use super::RuleMatch;
use super::node::{class_is_pure, class_value_binding, is_low_extract_view};

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

    pub(crate) fn bind(&mut self, symbol: u32, class: Id) -> bool {
        if let Some((_, existing)) = self.entries.iter().find(|(sym, _)| *sym == symbol) {
            *existing == class
        } else {
            self.entries.push((symbol, class));
            true
        }
    }

    pub(crate) fn to_rule_match(
        &self,
        egraph: &SemEGraph,
        class_values: &HashMap<Id, Vec<ValueId>>,
    ) -> RuleMatch {
        // A class can carry both a proven constant and a register value (an
        // assumption proves a condition equal to its truth value); record both so
        // immediate-folding and register-reading emitters each find theirs.
        let mut int_bindings = Vec::new();
        let mut value_bindings = Vec::new();
        for (sym, class) in &self.entries {
            if let Some(v) = class_int_binding(egraph, *class) {
                int_bindings.push((*sym, v));
            }
            if let Some(v) = class_value_binding(egraph, class_values, *class) {
                value_bindings.push((*sym, v));
            }
        }
        RuleMatch::new(int_bindings, value_bindings)
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
}

/// What a boundary binding requires of its class. A register operand needs the
/// value materialized in a register; an immediate (or constant template) is
/// encoded inline, so any constant class satisfies it; a structural boundary (a
/// width variable) demands nothing — the emitter reads it from the match.
/// Ordered by how demanding the requirement is: a structural boundary needs
/// nothing, an immediate needs a constant, a register needs materialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BoundaryDemand {
    Structural,
    Immediate,
    Register,
}

#[derive(Clone, Debug)]
pub(crate) struct FullMatchBindings {
    pub(crate) captures: CaptureBindings,
    pub(crate) pattern_nodes: Vec<PatternNodeBinding>,
}

#[derive(Clone, Debug)]
pub(crate) enum PbqpIselAlternative {
    NotDemanded,
    Tile { match_id: usize },
}

#[derive(Clone, Debug)]
pub(crate) struct PbqpIselMatch {
    pub(crate) pattern_index: usize,
    pub(crate) rule_index: usize,
    pub(crate) root: Id,
    pub(crate) pattern_root: Id,
    pub(crate) bindings: FullMatchBindings,
    pub(crate) cost: u64,
    /// Where the rule's destination class views its storage element. A value
    /// crosses a boundary for free only between equal offsets: no instruction
    /// moves bits across views implicitly.
    pub(crate) result_view_offset: u32,
}
/// A solved cover: the chosen alternative for every PBQP node and the e-class
/// each PBQP node stands for (same index).
pub(crate) struct ClassCover {
    pub(crate) choices: Vec<PbqpIselAlternative>,
    pub(crate) classes: Vec<Id>,
}

pub(crate) struct ClassPolicies<'a> {
    pub(crate) demanded: &'a dyn Fn(Id) -> bool,
    pub(crate) available: &'a dyn Fn(Id) -> bool,
}

pub(crate) fn build_eclass_cover(
    egraph: &SemEGraph,
    classes: &[Id],
    policies: &ClassPolicies,
    matches: &[PbqpIselMatch],
) -> Option<ClassCover> {
    let classes: Vec<Id> = classes.to_vec();
    let index: HashMap<Id, usize> = classes.iter().enumerate().map(|(i, &c)| (c, i)).collect();
    let class_index = |c: Id| index.get(&egraph.find(c)).copied();

    let mut alternatives_by_node = vec![Vec::<PbqpIselAlternative>::new(); classes.len()];
    for (i, &c) in classes.iter().enumerate() {
        if !(policies.demanded)(c) || (policies.available)(c) {
            alternatives_by_node[i].push(PbqpIselAlternative::NotDemanded);
        }
    }

    for (match_id, m) in matches.iter().enumerate() {
        let Some(root_index) = class_index(m.root) else {
            continue;
        };
        alternatives_by_node[root_index].push(PbqpIselAlternative::Tile { match_id });
    }

    if alternatives_by_node.iter().any(Vec::is_empty) {
        return None;
    }
    if classes
        .iter()
        .all(|&class| !(policies.demanded)(class) || (policies.available)(class))
    {
        return Some(ClassCover {
            choices: vec![PbqpIselAlternative::NotDemanded; classes.len()],
            classes,
        });
    }

    let mut problem = PbqpProblem::new();
    for alternatives in &alternatives_by_node {
        let costs = alternatives
            .iter()
            .map(|alternative| match alternative {
                PbqpIselAlternative::Tile { match_id } => matches[*match_id].cost,
                PbqpIselAlternative::NotDemanded => 0,
            })
            .collect();
        problem.add_node(costs);
    }

    let mut edge_pairs: HashSet<(usize, usize)> = HashSet::new();
    for m in matches {
        let Some(ri) = class_index(m.root) else {
            continue;
        };
        for binding in &m.bindings.pattern_nodes {
            if binding.is_state {
                continue;
            }
            if let Some(ci) = class_index(binding.class)
                && ri != ci
            {
                edge_pairs.insert(ordered_pair(ri, ci));
            }
        }
    }

    // A match's footprint is the effects it *performs* inside its own
    // instruction — the interior classes it recomputes. The chain a memory
    // access reads is not one of them: every access on a chain names it, and two
    // reads of one state are not two effects, so a state binding stays out.
    let effect_footprints: Vec<Vec<Id>> = matches
        .iter()
        .map(|matched| {
            let mut footprint: Vec<Id> = matched
                .bindings
                .pattern_nodes
                .iter()
                .filter(|binding| {
                    !binding.is_boundary
                        && !binding.is_state
                        && binding.pattern_node != matched.pattern_root
                        && !class_is_pure(egraph, binding.class)
                })
                .map(|binding| egraph.find(binding.class))
                .collect();
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
                let compatible =
                    alternatives_compatible(
                        egraph,
                        right_class,
                        left_alt,
                        right_alt,
                        matches,
                        policies.available,
                    ) && alternatives_compatible(
                        egraph,
                        left_class,
                        right_alt,
                        left_alt,
                        matches,
                        policies.available,
                    ) && !effect_tiles_conflict(left_alt, right_alt, &effect_footprints);
                if !compatible {
                    matrix.set(left_idx, right_idx, INF_COST);
                }
            }
        }
        problem.add_edge(
            pbqp::PbqpNodeId::from_index(li),
            pbqp::PbqpNodeId::from_index(ri),
            matrix,
        );
    }

    crate::memstats::pbqp_census(
        "isel-cover",
        problem.node_count(),
        problem.edge_count(),
        problem.matrix_bytes(),
    );

    let solution = pbqp::solve(&problem).ok()?;
    let choices = solution
        .choices
        .iter()
        .copied()
        .enumerate()
        .map(|(node, choice)| alternatives_by_node[node][choice].clone())
        .collect();
    Some(ClassCover { choices, classes })
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
/// more specific, and no less demanding of its boundaries. Specificity (the
/// number of type-constrained pattern nodes) breaks ties between otherwise
/// identical matches without ever touching the PBQP objective — an i32 `addw`
/// beats the untyped `add` at equal cost — and at equal cost/specificity a
/// match folding a class as an immediate beats one demanding it in a register
/// (which may force a whole materializer chain), while a genuinely cheaper
/// instruction still wins on cost alone.
pub(crate) fn prune_dominated_matches(specificity: &[usize], matches: &mut Vec<PbqpIselMatch>) {
    let footprint = |m: &PbqpIselMatch| {
        let mut boundaries = Vec::new();
        let mut internals = Vec::new();
        for binding in &m.bindings.pattern_nodes {
            if binding.is_boundary {
                boundaries.push((binding.class, binding.view_offset, binding.demand));
            } else if binding.pattern_node != m.pattern_root && !binding.is_state {
                internals.push(binding.class);
            }
        }
        // Sorting by (class, view offset, demand) puts equal class multisets in
        // the same class order, so within a group demands compare positionally
        // over aligned classes.
        boundaries.sort();
        internals.sort();
        let (classes, demands): (Vec<(Id, u32)>, Vec<BoundaryDemand>) = boundaries
            .into_iter()
            .map(|(class, offset, demand)| ((class, offset), demand))
            .unzip();
        (m.root, m.result_view_offset, classes, demands, internals)
    };
    let footprints: Vec<_> = matches.iter().map(footprint).collect();

    // Matches reading or writing a different register view are not
    // interchangeable — a value at one bit offset is not the value at another —
    // so the view offsets join the grouping key rather than the comparison.
    let mut groups: HashMap<_, Vec<usize>> = HashMap::new();
    for (index, (root, result_offset, classes, _, internals)) in footprints.iter().enumerate() {
        groups
            .entry((*root, *result_offset, classes, internals))
            .or_default()
            .push(index);
    }

    let comparison_key = |index: usize| {
        (
            matches[index].cost,
            specificity[matches[index].pattern_index],
            &footprints[index].3,
        )
    };
    let dominates = |a: usize, b: usize| {
        let (cost_a, spec_a, demands_a) = comparison_key(a);
        let (cost_b, spec_b, demands_b) = comparison_key(b);
        let demands_le = demands_a.iter().zip(demands_b).all(|(da, db)| da <= db);
        cost_a <= cost_b
            && spec_a >= spec_b
            && demands_le
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
    // costs no less is dominated by it outright, whatever its boundaries — this
    // is what keeps a constant class (into which assumptions merge every proven
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
    let mut best_free: HashMap<(Id, u32), usize> = HashMap::new();
    for (index, m) in matches.iter().enumerate() {
        if !keep[index] || !is_free(m) {
            continue;
        }
        best_free
            .entry((m.root, m.result_view_offset))
            .and_modify(|best| {
                if free_key(index) < free_key(*best) {
                    *best = index;
                }
            })
            .or_insert(index);
    }
    if !best_free.is_empty() {
        for (index, m) in matches.iter().enumerate() {
            let Some(&free) = best_free.get(&(m.root, m.result_view_offset)) else {
                continue;
            };
            if free == index || !keep[index] {
                continue;
            }
            let (free_cost, std::cmp::Reverse(free_spec)) = free_key(free);
            let cost = m.cost;
            let spec = specificity[m.pattern_index];
            if cost > free_cost || (cost == free_cost && spec <= free_spec) {
                keep[index] = false;
            }
        }
    }

    let mut kept = keep.iter();
    matches.retain(|_| *kept.next().unwrap());
}

pub(crate) fn completeness_error(
    egraph: &SemEGraph,
    demanded: &HashSet<Id>,
    matches: &[PbqpIselMatch],
    available: &dyn Fn(Id) -> bool,
) -> Option<String> {
    let rooted: HashSet<Id> = matches
        .iter()
        .map(|matched| egraph.find(matched.root))
        .collect();

    let mut missing: Vec<SymKind> = Vec::new();
    for &class in demanded {
        let class = egraph.find(class);
        if rooted.contains(&class) || available(class) || is_low_extract_view(egraph, class) {
            continue;
        }
        let nodes = egraph.nodes(class);
        if let Some(kind) = nodes
            .clone()
            .filter_map(|n| n.sym())
            .find(|kind| *kind != SymKind::If)
            .or_else(|| nodes.clone().next().and_then(|n| n.sym()))
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
/// Where the value an alternative produces sits in its storage element: a tile's
/// destination class carries the rule's view offset, while an already-available
/// value lives in an ordinary offset-0 register.
fn produced_view_offset(alternative: &PbqpIselAlternative, matches: &[PbqpIselMatch]) -> u32 {
    match alternative {
        PbqpIselAlternative::Tile { match_id } => matches[*match_id].result_view_offset,
        PbqpIselAlternative::NotDemanded => 0,
    }
}

pub(crate) fn alternatives_compatible(
    egraph: &SemEGraph,
    child: Id,
    parent_alt: &PbqpIselAlternative,
    child_alt: &PbqpIselAlternative,
    matches: &[PbqpIselMatch],
    available: &dyn Fn(Id) -> bool,
) -> bool {
    let PbqpIselAlternative::Tile { match_id } = parent_alt else {
        return true;
    };
    let matched = &matches[*match_id];
    let mut register = false;
    let mut immediate = false;
    let mut owned_effect = false;
    let mut demanded_offsets: Vec<u32> = Vec::new();
    for binding in &matched.bindings.pattern_nodes {
        if binding.class != child
            || (binding.pattern_node == matched.pattern_root && binding.class == matched.root)
        {
            continue;
        }
        if binding.is_boundary {
            register |= binding.demand == BoundaryDemand::Register;
            immediate |= binding.demand == BoundaryDemand::Immediate;
            if binding.demand == BoundaryDemand::Register
                && !demanded_offsets.contains(&binding.view_offset)
            {
                demanded_offsets.push(binding.view_offset);
            }
        } else if binding.pattern_node != matched.pattern_root
            && !binding.is_state
            && !class_is_pure(egraph, child)
        {
            owned_effect = true;
        }
    }
    if register {
        if demanded_offsets != [produced_view_offset(child_alt, matches)] {
            return false;
        }
        match child_alt {
            PbqpIselAlternative::Tile { .. } => true,
            PbqpIselAlternative::NotDemanded => available(child),
        }
    } else if immediate {
        class_int_binding(egraph, child).is_some()
    } else if owned_effect {
        matches!(child_alt, PbqpIselAlternative::NotDemanded)
    } else {
        true
    }
}
