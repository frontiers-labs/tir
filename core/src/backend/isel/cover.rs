//! PBQP cover construction over the saturated e-graph: match bindings, the
//! alternative/compatibility model, and the solved cover.

use std::collections::{HashMap, HashSet};

use tir::{
    OpId, ValueId,
    graph::NodeId,
    pbqp::{self, INF_COST, PbqpAlternative, PbqpMatrix, PbqpProblem},
    sem::SymKind,
};
use tir_symbolic::egraph::{ENode, Id};

use super::RuleMatch;
use super::node::{Binding, SemEGraph, class_binding, class_is_pure};
use super::pattern::CompiledIselPattern;

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
        class_value: &HashMap<Id, ValueId>,
    ) -> RuleMatch {
        let mut int_bindings = Vec::new();
        let mut value_bindings = Vec::new();
        for (sym, class) in &self.entries {
            match class_binding(egraph, class_value, *class) {
                Some(Binding::Int(v)) => int_bindings.push((*sym, v)),
                Some(Binding::Value(v)) => value_bindings.push((*sym, v)),
                None => {}
            }
        }
        RuleMatch::new(int_bindings, value_bindings)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PatternNodeBinding {
    pub(crate) pattern_node: NodeId,
    pub(crate) class: Id,
    pub(crate) is_boundary: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct FullMatchBindings {
    pub(crate) captures: CaptureBindings,
    pub(crate) pattern_nodes: Vec<PatternNodeBinding>,
}

#[derive(Clone, Debug)]
pub(crate) enum PbqpIselAlternative {
    External,
    Root {
        match_id: usize,
    },
    Internal {
        match_id: usize,
        pattern_node: NodeId,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct PbqpIselMatch {
    pub(crate) pattern_index: usize,
    pub(crate) rule_index: usize,
    pub(crate) root: Id,
    pub(crate) pattern_root: NodeId,
    pub(crate) bindings: FullMatchBindings,
    pub(crate) cost: u64,
}
/// A solved cover: the chosen alternative for every PBQP node and the e-class
/// each PBQP node stands for (same index).
pub(crate) struct ClassCover {
    pub(crate) choices: Vec<PbqpIselAlternative>,
    pub(crate) classes: Vec<Id>,
}

/// Build and solve the PBQP cover over the e-graph: one PBQP node per e-class,
/// alternatives drawn from the instruction-pattern `matches`, and root -> bound
/// class compatibility derived from each match's bindings. `must_materialize`
/// lists classes whose value some consumer can never internalize (a use outside
/// any match's reach), so they are never offered a consuming alternative.
/// Returns `None` if the instance is infeasible (a class with no valid
/// alternative).
pub(crate) fn build_eclass_cover(
    egraph: &SemEGraph,
    op_by_root: &HashMap<Id, OpId>,
    must_materialize: &HashSet<Id>,
    matches: &[PbqpIselMatch],
) -> Option<ClassCover> {
    let classes: Vec<Id> = egraph.classes().map(|c| egraph.find(c.id())).collect();
    let index: HashMap<Id, usize> = classes.iter().enumerate().map(|(i, &c)| (c, i)).collect();
    let class_index = |c: Id| index[&egraph.find(c)];

    let is_terminal = |c: Id| egraph.nodes(c).iter().any(|n| n.children().is_empty());

    let mut alternatives_by_node = vec![Vec::<PbqpIselAlternative>::new(); classes.len()];
    for (i, &c) in classes.iter().enumerate() {
        if is_terminal(c) {
            alternatives_by_node[i].push(PbqpIselAlternative::External);
        }
    }

    for (match_id, m) in matches.iter().enumerate() {
        alternatives_by_node[class_index(m.root)].push(PbqpIselAlternative::Root { match_id });
        for binding in &m.bindings.pattern_nodes {
            if binding.is_boundary
                || binding.pattern_node == m.pattern_root
                || must_materialize.contains(&egraph.find(binding.class))
            {
                continue;
            }
            alternatives_by_node[class_index(binding.class)].push(PbqpIselAlternative::Internal {
                match_id,
                pattern_node: binding.pattern_node,
            });
        }
    }

    for (i, &c) in classes.iter().enumerate() {
        if alternatives_by_node[i].is_empty() && (is_terminal(c) || !op_by_root.contains_key(&c)) {
            alternatives_by_node[i].push(PbqpIselAlternative::External);
        }
    }

    if alternatives_by_node.iter().any(Vec::is_empty) {
        return None;
    }

    let mut problem = PbqpProblem::new();
    for alternatives in &alternatives_by_node {
        let costs = alternatives
            .iter()
            .map(|alternative| match alternative {
                PbqpIselAlternative::Root { match_id } => matches[*match_id].cost,
                PbqpIselAlternative::External | PbqpIselAlternative::Internal { .. } => 0,
            })
            .collect();
        problem.add_node(costs);
    }

    for (match_id, m) in matches.iter().enumerate() {
        let mut coherent = Vec::new();
        for (node, alternatives) in alternatives_by_node.iter().enumerate() {
            for (alternative, pbqp_alt) in alternatives.iter().enumerate() {
                // A pure internal class is *not* coherence-tied to the match: the
                // instruction recomputes it (duplication), so the match stays
                // selectable even when the class is claimed by another match or
                // materialized in its own right. Only the root and memory-effect
                // internals stand and fall with the match.
                let belongs_to_match = match pbqp_alt {
                    PbqpIselAlternative::Root {
                        match_id: alt_match,
                    } => *alt_match == match_id,
                    PbqpIselAlternative::Internal {
                        match_id: alt_match,
                        ..
                    } => *alt_match == match_id && !class_is_pure(egraph, classes[node]),
                    PbqpIselAlternative::External => false,
                };
                if belongs_to_match {
                    coherent.push(PbqpAlternative {
                        node: pbqp::PbqpNodeId::from_index(node),
                        alternative,
                    });
                }
            }
        }
        if m.bindings.pattern_nodes.len() > 1 {
            problem.add_coherence_set(coherent);
        }
    }

    // Edges connect each match's root class to every class the match binds: the
    // root alternative imposes the match's requirements (materialized boundary
    // operands, same-match memory internals) directly, so they don't depend on
    // the choices of intermediate pattern nodes. Deduplicated so each ordered
    // class pair gets one compatibility matrix.
    let mut edge_pairs: HashSet<(usize, usize)> = HashSet::new();
    for m in matches {
        let ri = class_index(m.root);
        for binding in &m.bindings.pattern_nodes {
            let ci = class_index(binding.class);
            if ri != ci {
                edge_pairs.insert((ri, ci));
            }
        }
    }

    for (pi, ci) in edge_pairs {
        let child_class = classes[ci];
        let parent_alts = &alternatives_by_node[pi];
        let child_alts = &alternatives_by_node[ci];
        let mut matrix = PbqpMatrix::zero(parent_alts.len(), child_alts.len());

        for (parent_alt_idx, parent_alt) in parent_alts.iter().enumerate() {
            for (child_alt_idx, child_alt) in child_alts.iter().enumerate() {
                if !alternatives_compatible(egraph, child_class, parent_alt, child_alt, matches) {
                    matrix.set(parent_alt_idx, child_alt_idx, INF_COST);
                }
            }
        }

        problem.add_edge(
            pbqp::PbqpNodeId::from_index(pi),
            pbqp::PbqpNodeId::from_index(ci),
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

/// Drop matches dominated by an interchangeable alternative: same root class,
/// same internal-class coverage, same boundary operands, but no cheaper and no
/// more specific. Specificity (the number of type-constrained pattern nodes)
/// thus breaks ties between otherwise identical matches without ever touching
/// the PBQP objective — an i32 `addw` beats the untyped `add` at equal cost,
/// while a genuinely cheaper instruction still wins on cost alone.
pub(crate) fn prune_dominated_matches(
    patterns: &[CompiledIselPattern],
    matches: &mut Vec<PbqpIselMatch>,
) {
    let footprint = |m: &PbqpIselMatch| {
        let mut boundaries = Vec::new();
        let mut internals = Vec::new();
        for binding in &m.bindings.pattern_nodes {
            if binding.is_boundary {
                boundaries.push(binding.class);
            } else if binding.pattern_node != m.pattern_root {
                internals.push(binding.class);
            }
        }
        boundaries.sort();
        internals.sort();
        (m.root, boundaries, internals)
    };

    let mut groups: HashMap<_, Vec<usize>> = HashMap::new();
    for (index, m) in matches.iter().enumerate() {
        groups.entry(footprint(m)).or_default().push(index);
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
                if cost_a <= cost_b && spec_a >= spec_b && (cost_a < cost_b || spec_a > spec_b) {
                    keep[b] = false;
                }
            }
        }
    }

    let mut kept = keep.iter();
    matches.retain(|_| *kept.next().unwrap());
}

/// Coverage completeness: every op-root e-class must be emittable as an instruction
/// (it roots some match) or consumable by a parent match (it is an interior node of
/// some match). A non-terminal op-root that is neither cannot be selected by this
/// rule set — even after saturation — so selection fails with a diagnostic.
pub(crate) fn completeness_error(
    egraph: &SemEGraph,
    op_by_root: &HashMap<Id, OpId>,
    matches: &[PbqpIselMatch],
) -> Option<String> {
    let mut has_root: HashSet<Id> = HashSet::new();
    let mut has_internal: HashSet<Id> = HashSet::new();
    for m in matches {
        has_root.insert(egraph.find(m.root));
        for binding in &m.bindings.pattern_nodes {
            if !binding.is_boundary && binding.pattern_node != m.pattern_root {
                has_internal.insert(egraph.find(binding.class));
            }
        }
    }

    let mut missing: Vec<SymKind> = Vec::new();
    for &class in op_by_root.keys() {
        let class = egraph.find(class);
        if egraph.nodes(class).iter().any(|n| n.children().is_empty()) {
            continue;
        }
        if has_root.contains(&class) || has_internal.contains(&class) {
            continue;
        }
        if let Some(kind) = egraph.nodes(class).first().map(|n| n.kind)
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
) -> bool {
    match child_requirement(egraph, child, parent_alt, matches) {
        Some(ChildRequirement::Materialized) => matches!(
            child_alt,
            PbqpIselAlternative::Root { .. } | PbqpIselAlternative::External
        ),
        Some(ChildRequirement::SameMatch {
            match_id,
            pattern_node,
        }) => matches!(
            child_alt,
            PbqpIselAlternative::Internal {
                match_id: child_match,
                pattern_node: child_pattern_node,
            } if *child_match == match_id && *child_pattern_node == pattern_node
        ),
        None => true,
    }
}

/// What the parent alternative's match demands of a class it binds. A boundary
/// binding needs the value in a register (even if the match also recomputes it
/// at another node). An internal binding of a *pure* class demands nothing: the
/// instruction recomputes the value, so the class is free to be internal to
/// another match, materialized by its own instruction, or both (duplication).
/// Only a memory-effect internal must belong to exactly this match.
pub(crate) fn child_requirement(
    egraph: &SemEGraph,
    child: Id,
    parent_alt: &PbqpIselAlternative,
    matches: &[PbqpIselMatch],
) -> Option<ChildRequirement> {
    let match_id = match parent_alt {
        PbqpIselAlternative::Root { match_id } | PbqpIselAlternative::Internal { match_id, .. } => {
            *match_id
        }
        PbqpIselAlternative::External => return None,
    };

    let m = &matches[match_id];
    let mut internal_node = None;
    let mut boundary = false;
    for binding in &m.bindings.pattern_nodes {
        if binding.class != child || binding.pattern_node == m.pattern_root {
            continue;
        }
        if binding.is_boundary {
            boundary = true;
        } else if internal_node.is_none() {
            internal_node = Some(binding.pattern_node);
        }
    }

    if boundary {
        return Some(ChildRequirement::Materialized);
    }
    match internal_node {
        Some(_) if class_is_pure(egraph, child) => None,
        Some(pattern_node) => Some(ChildRequirement::SameMatch {
            match_id,
            pattern_node,
        }),
        None => None,
    }
}

pub(crate) enum ChildRequirement {
    Materialized,
    SameMatch {
        match_id: usize,
        pattern_node: NodeId,
    },
}
