//! PBQP cover construction over the saturated e-graph: match bindings, the
//! alternative/compatibility model, and the solved cover.

use std::collections::{HashMap, HashSet};

use tir::{
    ValueId,
    pbqp::{self, INF_COST, PbqpAlternative, PbqpMatrix, PbqpProblem},
    sem::SymKind,
};
use tir_symbolic::egraph::{ENode, Id};

use super::RuleMatch;
use super::node::{
    SemEGraph, class_int_binding, class_is_pure, class_value_binding, is_low_extract_view,
};
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
    External,
    Root {
        match_id: usize,
    },
    Internal {
        match_id: usize,
        pattern_node: Id,
    },
    /// The class's value is not needed in a register: its only consumer is a
    /// fused conditional branch that recomputes the condition from its own
    /// operands. Unlike `External`, `Dead` never satisfies a boundary's
    /// materialization requirement — the defining op is erased.
    Dead,
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

/// Build and solve the PBQP cover over the supplied `classes` (B's op-root and
/// guard classes closed under the matches' bindings): one PBQP node per class,
/// alternatives drawn from the instruction-pattern `matches`, and root -> bound
/// class compatibility derived from each match's bindings. `op_roots` are B's
/// op-root classes (a class that is neither terminal nor a B op-root falls back
/// to External). `must_materialize` lists classes whose value some consumer can
/// never internalize, so they are never offered a consuming alternative. Returns
/// `None` if the instance is infeasible (a class with no valid alternative).
/// Per-class predicates the cover consults (all derived from the function-wide
/// side tables): `must_materialize` bars consuming alternatives,
/// `force_materialize` drops the free External alternative when a materializer
/// match roots the class, and `externally_bindable` says whether External
/// satisfies a boundary's register requirement (the class carries an IR value
/// or folds to an immediate).
pub(crate) struct ClassPolicies<'a> {
    pub(crate) must_materialize: &'a dyn Fn(Id) -> bool,
    pub(crate) force_materialize: &'a dyn Fn(Id) -> bool,
    pub(crate) externally_bindable: &'a dyn Fn(Id) -> bool,
}

pub(crate) fn build_eclass_cover(
    egraph: &SemEGraph,
    op_roots: &HashSet<Id>,
    classes: &[Id],
    policies: &ClassPolicies,
    dead_allowed: &HashSet<Id>,
    matches: &[PbqpIselMatch],
) -> Option<ClassCover> {
    let classes: Vec<Id> = classes.to_vec();
    let index: HashMap<Id, usize> = classes.iter().enumerate().map(|(i, &c)| (c, i)).collect();
    let class_index = |c: Id| index.get(&egraph.find(c)).copied();

    let is_terminal = |c: Id| egraph.nodes(c).iter().any(|n| n.children().is_empty());

    // A forced class (a constant that must reach an unselected consumer in a
    // register) with a materializer match rooted on it loses the zero-cost
    // External alternative, so the cover emits the materializer instead of
    // leaving the constant to a later target hook. Without a rooted match,
    // External stays so that hook can diagnose or lower the unsupported case.
    let rooted: HashSet<Id> = matches.iter().map(|m| egraph.find(m.root)).collect();

    let mut alternatives_by_node = vec![Vec::<PbqpIselAlternative>::new(); classes.len()];
    for (i, &c) in classes.iter().enumerate() {
        if is_terminal(c) && !((policies.force_materialize)(c) && rooted.contains(&c)) {
            alternatives_by_node[i].push(PbqpIselAlternative::External);
        }
    }

    for (match_id, m) in matches.iter().enumerate() {
        let Some(root_index) = class_index(m.root) else {
            continue;
        };
        alternatives_by_node[root_index].push(PbqpIselAlternative::Root { match_id });
        for binding in &m.bindings.pattern_nodes {
            if binding.is_boundary
                || binding.pattern_node == m.pattern_root
                || (policies.must_materialize)(egraph.find(binding.class))
            {
                continue;
            }
            let Some(child_index) = class_index(binding.class) else {
                continue;
            };
            alternatives_by_node[child_index].push(PbqpIselAlternative::Internal {
                match_id,
                pattern_node: binding.pattern_node,
            });
        }
    }

    for &c in dead_allowed {
        if let Some(i) = class_index(c) {
            alternatives_by_node[i].push(PbqpIselAlternative::Dead);
        }
    }

    for (i, &c) in classes.iter().enumerate() {
        if alternatives_by_node[i].is_empty()
            && (is_terminal(c) || !op_roots.contains(&c) || is_low_extract_view(egraph, c))
        {
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
                PbqpIselAlternative::External
                | PbqpIselAlternative::Internal { .. }
                | PbqpIselAlternative::Dead => 0,
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
                    PbqpIselAlternative::External | PbqpIselAlternative::Dead => false,
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
        let Some(ri) = class_index(m.root) else {
            continue;
        };
        for binding in &m.bindings.pattern_nodes {
            if let Some(ci) = class_index(binding.class)
                && ri != ci
            {
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
                if !alternatives_compatible(
                    egraph,
                    child_class,
                    parent_alt,
                    child_alt,
                    matches,
                    policies.externally_bindable,
                ) {
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

/// Coverage completeness: every op-root e-class must be emittable as an instruction
/// (it roots some match) or consumable by a parent match (it is an interior node of
/// some match). A non-terminal op-root that is neither cannot be selected by this
/// rule set — even after saturation — so selection fails with a diagnostic.
/// Classes in `exempt` (guard conditions covered by a fused branch, with no other
/// consumer needing the value) are skipped.
pub(crate) fn completeness_error(
    egraph: &SemEGraph,
    op_roots: &HashSet<Id>,
    matches: &[PbqpIselMatch],
    exempt: &HashSet<Id>,
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
    for &class in op_roots {
        let class = egraph.find(class);
        if egraph.nodes(class).iter().any(|n| n.children().is_empty()) {
            continue;
        }
        // A low-bit truncation is a re-view of its operand's register, covered
        // as External with zero instructions (see `is_low_extract_view`).
        if is_low_extract_view(egraph, class) {
            continue;
        }
        if has_root.contains(&class) || has_internal.contains(&class) || exempt.contains(&class) {
            continue;
        }
        // Prefer a member that isn't a rewrite-introduced `If` bridge, so the
        // diagnostic names the original semantic kind (e.g. the comparison).
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
    externally_bindable: &dyn Fn(Id) -> bool,
) -> bool {
    match child_requirement(egraph, child, parent_alt, matches) {
        // A boundary requirement is satisfied by the class rooting its own
        // instruction, or by External when the class reaches the operand
        // externally: for a register, it carries an IR value or folds to an
        // immediate — a valueless synthetic class (the injected `zext(0b0, W)`
        // zero shape) can only satisfy it by rooting a match; for an immediate,
        // any constant class (the value is folded into the encoding).
        Some(req @ (ChildRequirement::Materialized | ChildRequirement::Immediate)) => {
            match child_alt {
                PbqpIselAlternative::Root { .. } => true,
                PbqpIselAlternative::External => match req {
                    ChildRequirement::Materialized => externally_bindable(child),
                    _ => class_int_binding(egraph, child).is_some(),
                },
                PbqpIselAlternative::Internal { .. } | PbqpIselAlternative::Dead => false,
            }
        }
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
        PbqpIselAlternative::External | PbqpIselAlternative::Dead => return None,
    };

    let m = &matches[match_id];
    let mut internal_node = None;
    let mut register_boundary = false;
    let mut immediate_boundary = false;
    for binding in &m.bindings.pattern_nodes {
        if binding.class != child || binding.pattern_node == m.pattern_root {
            continue;
        }
        if binding.is_boundary {
            match binding.demand {
                BoundaryDemand::Register => register_boundary = true,
                BoundaryDemand::Immediate => immediate_boundary = true,
                BoundaryDemand::Structural => {}
            }
        } else if internal_node.is_none() {
            internal_node = Some(binding.pattern_node);
        }
    }

    if register_boundary {
        return Some(ChildRequirement::Materialized);
    }
    if immediate_boundary {
        return Some(ChildRequirement::Immediate);
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
    Immediate,
    SameMatch { match_id: usize, pattern_node: Id },
}
