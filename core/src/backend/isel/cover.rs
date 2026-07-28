//! PBQP cover construction over the saturated e-graph: match bindings, the
//! alternative/compatibility model, and the solved cover.

use std::collections::{HashMap, HashSet};

use tir::{
    ValueId,
    pbqp::{self, INF_COST, PbqpMatrix, PbqpProblem},
    sem::SymKind,
};
use tir_symbolic::egraph::Id;

use super::RuleMatch;
use super::node::{
    SemEGraph, class_int_binding, class_is_pure, class_value_binding, is_low_extract_view,
};
use super::pattern::CompiledIselPattern;

/// The cost charged for keeping a gate as control flow. A conservative fixed
/// estimate of the edge assignments the existing terminators already perform,
/// tuned so a cheaper `If`-rooted value rule wins where the target has one.
const REIFY_COST: u64 = 3;

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
        // assumption merges a condition with its truth value); record both so
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
    pub(crate) demand: BoundaryDemand,
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
    /// Preserve a gated SSA value as control-flow edge assignments. Terminator
    /// emission already performs those assignments; this alternative accounts
    /// for their cost against an `If`/`Theta`-rooted value rule.
    Reify,
    Tile {
        match_id: usize,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct PbqpIselMatch {
    pub(crate) pattern_index: usize,
    pub(crate) rule_index: usize,
    pub(crate) root: Id,
    pub(crate) pattern_root: Id,
    pub(crate) bindings: FullMatchBindings,
    pub(crate) cost: u64,
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
    pub(crate) reifiable_gate: &'a dyn Fn(Id) -> bool,
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
        if (policies.reifiable_gate)(c) {
            alternatives_by_node[i].push(PbqpIselAlternative::Reify);
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

    let mut problem = PbqpProblem::new();
    for alternatives in &alternatives_by_node {
        let costs = alternatives
            .iter()
            .map(|alternative| match alternative {
                PbqpIselAlternative::Tile { match_id } => matches[*match_id].cost,
                PbqpIselAlternative::Reify => REIFY_COST,
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
            if let Some(ci) = class_index(binding.class)
                && ri != ci
            {
                edge_pairs.insert(ordered_pair(ri, ci));
            }
        }
    }

    let effect_footprints: Vec<HashSet<Id>> = matches
        .iter()
        .map(|matched| {
            matched
                .bindings
                .pattern_nodes
                .iter()
                .filter(|binding| {
                    !binding.is_boundary
                        && binding.pattern_node != matched.pattern_root
                        && !class_is_pure(egraph, binding.class)
                })
                .map(|binding| egraph.find(binding.class))
                .collect()
        })
        .collect();
    for (lhs, left) in matches.iter().enumerate() {
        let Some(li) = class_index(left.root) else {
            continue;
        };
        for (rhs, right) in matches.iter().enumerate().skip(lhs + 1) {
            if effect_footprints[lhs].is_disjoint(&effect_footprints[rhs]) {
                continue;
            }
            let Some(ri) = class_index(right.root) else {
                continue;
            };
            if li != ri {
                edge_pairs.insert(ordered_pair(li, ri));
            }
        }
    }

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
    footprints: &[HashSet<Id>],
) -> bool {
    let (PbqpIselAlternative::Tile { match_id: lhs }, PbqpIselAlternative::Tile { match_id: rhs }) =
        (lhs, rhs)
    else {
        return false;
    };
    !footprints[*lhs].is_disjoint(&footprints[*rhs])
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
pub(crate) fn prune_dominated_matches(
    patterns: &[CompiledIselPattern],
    matches: &mut Vec<PbqpIselMatch>,
) {
    let footprint = |m: &PbqpIselMatch| {
        let mut boundaries = Vec::new();
        let mut internals = Vec::new();
        for binding in &m.bindings.pattern_nodes {
            if binding.is_boundary {
                boundaries.push((binding.class, binding.demand));
            } else if binding.pattern_node != m.pattern_root {
                internals.push(binding.class);
            }
        }
        // Sorting by (class, demand) puts equal class multisets in the same
        // class order, so within a group demands compare positionally over
        // aligned classes.
        boundaries.sort();
        internals.sort();
        let (classes, demands): (Vec<Id>, Vec<BoundaryDemand>) = boundaries.into_iter().unzip();
        (m.root, classes, demands, internals)
    };
    let footprints: Vec<_> = matches.iter().map(footprint).collect();

    let mut groups: HashMap<_, Vec<usize>> = HashMap::new();
    for (index, (root, classes, _, internals)) in footprints.iter().enumerate() {
        groups
            .entry((*root, classes, internals))
            .or_default()
            .push(index);
    }

    let mut keep = vec![true; matches.len()];
    for group in groups.values() {
        for &a in group {
            for &b in group {
                if a == b || !keep[a] || !keep[b] {
                    continue;
                }
                let (cost_a, spec_a) = (
                    matches[a].cost,
                    patterns[matches[a].pattern_index].specificity,
                );
                let (cost_b, spec_b) = (
                    matches[b].cost,
                    patterns[matches[b].pattern_index].specificity,
                );
                let (demands_a, demands_b) = (&footprints[a].2, &footprints[b].2);
                let demands_le = demands_a.iter().zip(demands_b).all(|(da, db)| da <= db);
                if cost_a <= cost_b
                    && spec_a >= spec_b
                    && demands_le
                    && (cost_a < cost_b || spec_a > spec_b || demands_a != demands_b)
                {
                    keep[b] = false;
                }
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
    reifiable: &dyn Fn(Id) -> bool,
) -> Option<String> {
    let rooted: HashSet<Id> = matches
        .iter()
        .map(|matched| egraph.find(matched.root))
        .collect();

    let mut missing: Vec<SymKind> = Vec::new();
    for &class in demanded {
        let class = egraph.find(class);
        if rooted.contains(&class)
            || available(class)
            || reifiable(class)
            || is_low_extract_view(egraph, class)
        {
            continue;
        }
        let nodes = egraph.nodes(class);
        if let Some(kind) = nodes
            .iter()
            .map(|n| n.kind)
            .find(|kind| *kind != SymKind::If)
            .or_else(|| nodes.first().map(|n| n.kind))
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
    for binding in &matched.bindings.pattern_nodes {
        if binding.class != child
            || (binding.pattern_node == matched.pattern_root && binding.class == matched.root)
        {
            continue;
        }
        if binding.is_boundary {
            register |= binding.demand == BoundaryDemand::Register;
            immediate |= binding.demand == BoundaryDemand::Immediate;
        } else if binding.pattern_node != matched.pattern_root && !class_is_pure(egraph, child) {
            owned_effect = true;
        }
    }
    if register {
        match child_alt {
            PbqpIselAlternative::Tile { .. } | PbqpIselAlternative::Reify => true,
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
