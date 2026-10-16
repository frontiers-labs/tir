//! Proofs that a rule's matching pattern is its instruction's guarded
//! behaviour wherever the pattern is defined.

use tir::{
    NodeId, PassError,
    sem::{
        EquivalenceOracle, SemGraph, SmtOracle, SymKind, SymPayload, canonicalize_for_selection,
        definedness_condition, infer_widths,
    },
};

use std::collections::{HashMap, HashSet};

use tir_adt::APInt;

use super::float_refinement;
use super::rule::Rule;
use super::rules::OperandConstraint;

/// Results for guarded rules whose obligations were proved or could not be encoded.
#[derive(Default)]
pub struct GuardedRelaxationProofs {
    pub proven: Vec<String>,
    pub unsupported: Vec<(String, String)>,
}

enum GuardedRelaxationProof {
    Proven,
}

/// Prove guarded rule refinement, reporting unsupported encodings separately.
/// Invalid supported obligations return [`PassError::InvalidRuleSet`].
pub fn prove_guarded_relaxations(rules: &[Rule]) -> Result<GuardedRelaxationProofs, PassError> {
    let mut report = GuardedRelaxationProofs::default();
    for rule in rules {
        let Some(guarded) = &rule.guarded_semantics else {
            continue;
        };
        match prove_relaxation(rule, guarded).map_err(PassError::InvalidRuleSet)? {
            GuardedRelaxationProof::Proven => report.proven.push(rule.name.to_string()),
        }
    }
    Ok(report)
}

fn relaxation_error(rule: &Rule, why: &str) -> String {
    format!("rule `{}`: {why}", rule.name)
}

/// Prove result refinement on the selection pattern's defined domain.
fn prove_relaxation(rule: &Rule, guarded: &SemGraph) -> Result<GuardedRelaxationProof, String> {
    let (pattern, guarded) = peel_relaxation_effect(rule, guarded)?;
    let pattern = &pattern;
    let guarded = &guarded;
    let pattern_root = pattern.root().unwrap();
    let full_root = guarded.root().unwrap();
    let immediate_symbols: HashSet<u32> = rule
        .operand_constraints
        .iter()
        .filter(|(_, c)| matches!(c, OperandConstraint::Immediate))
        .map(|(symbol, _)| *symbol)
        .collect();
    let candidate = relaxation_candidate(rule, guarded, pattern, &immediate_symbols)?;
    let register_width = infer_widths(guarded, |_| None)[full_root.index()].unwrap_or(64);
    let floating = relaxation_uses_float(&candidate);
    if !floating
        && *guarded.get_node(full_root) == SymKind::If
        && let Some(else_arm) = guarded.children(full_root).nth(2)
    {
        let (canonical_else, else_root, _) =
            canonicalize_for_selection(guarded, else_arm, &immediate_symbols);
        if canonical_else.subgraph_eq(else_root, pattern, pattern_root)
            && relaxation_holds(guarded, full_root, else_arm, register_width)
        {
            return Ok(GuardedRelaxationProof::Proven);
        }
    }
    // The proof names no IR, so its types live in a store of its own.
    let terms = tir::Terms::new();
    let symbol_types = relaxation_symbol_types(&terms, rule, guarded, &candidate, register_width)?;
    if let Some(proof) =
        prove_float_relaxation(&terms, guarded, &candidate, &symbol_types, floating)
    {
        return Ok(proof);
    }
    Err(relaxation_error(
        rule,
        &format!(
            "guard relaxation `D(pattern) => guarded == pattern` is not valid at register width {register_width}"
        ),
    ))
}

fn peel_relaxation_effect(rule: &Rule, guarded: &SemGraph) -> Result<(SemGraph, SemGraph), String> {
    let mut value_pattern = SemGraph::new();
    let pattern_root = rule
        .pattern
        .root()
        .ok_or_else(|| relaxation_error(rule, "empty selection pattern"))?;
    let effect = *rule.pattern.get_node(pattern_root) == SymKind::FPEffect;
    let root = if effect {
        rule.pattern.children(pattern_root).next().unwrap()
    } else {
        pattern_root
    };
    tir_symbolic::sem::copy_subgraph(&mut value_pattern, &rule.pattern, root, &mut HashMap::new());
    let pattern = &value_pattern;
    let pattern_root = pattern.root().unwrap();
    let full_root = guarded
        .root()
        .ok_or_else(|| relaxation_error(rule, "empty guarded semantics"))?;
    let mut observed = SemGraph::new();
    let observed_root =
        if effect && matches!(guarded.get_node(full_root), SymKind::SExt | SymKind::ZExt) {
            let child = guarded.children(full_root).next().unwrap();
            let width = infer_widths(guarded, |_| None)[child.index()];
            if width.is_some() && width == infer_widths(pattern, |_| None)[pattern_root.index()] {
                child
            } else {
                full_root
            }
        } else {
            full_root
        };
    tir_symbolic::sem::copy_subgraph(&mut observed, guarded, observed_root, &mut HashMap::new());
    Ok((value_pattern, observed))
}

fn relaxation_candidate(
    rule: &Rule,
    guarded: &SemGraph,
    pattern: &SemGraph,
    immediate_symbols: &HashSet<u32>,
) -> Result<SemGraph, String> {
    let full_root = guarded.root().unwrap();
    let pattern_root = pattern.root().unwrap();
    let matches_pattern = |candidate: &SemGraph| {
        let (canonical, root, _) =
            canonicalize_for_selection(candidate, candidate.root().unwrap(), immediate_symbols);
        canonical.subgraph_eq(root, pattern, pattern_root)
    };
    let candidate = tir_symbolic::lang::selection_fallback_preserving_rounding(guarded, full_root)
        .filter(matches_pattern)
        .or_else(|| {
            tir_symbolic::lang::selection_fallback(guarded, full_root).filter(matches_pattern)
        })
        .or_else(|| {
            if *guarded.get_node(full_root) != SymKind::If {
                return None;
            }
            let else_arm = guarded.children(full_root).nth(2)?;
            let mut candidate = SemGraph::new();
            tir_symbolic::sem::copy_subgraph(
                &mut candidate,
                guarded,
                else_arm,
                &mut HashMap::new(),
            );
            Some(candidate)
        })
        .ok_or_else(|| relaxation_error(rule, "guarded semantics has no selection fallback"))?;
    if !matches_pattern(&candidate) {
        return Err(relaxation_error(
            rule,
            "guarded else arm does not match the selection pattern",
        ));
    }
    Ok(candidate)
}

fn relaxation_uses_float(candidate: &SemGraph) -> bool {
    candidate.postorder(candidate.root().unwrap()).any(|node| {
        matches!(
            candidate.get_node(node),
            SymKind::FPToSI
                | SymKind::FPToUI
                | SymKind::SIToFP
                | SymKind::UIToFP
                | SymKind::FAdd
                | SymKind::FSub
                | SymKind::FMul
                | SymKind::FDiv
                | SymKind::FCvt
                | SymKind::Sqrt
                | SymKind::Fma
                | SymKind::FAddRound
                | SymKind::FSubRound
                | SymKind::FMulRound
                | SymKind::FDivRound
                | SymKind::FmaRound
                | SymKind::SqrtRound
                | SymKind::FCvtRound
                | SymKind::SIToFPRound
                | SymKind::UIToFPRound
                | SymKind::FPToSIRound
                | SymKind::FPToUIRound
        )
    })
}

fn relaxation_symbol_types(
    terms: &tir::Terms,
    rule: &Rule,
    guarded: &SemGraph,
    candidate: &SemGraph,
    register_width: u32,
) -> Result<Vec<tir::TypeId>, String> {
    let symbol_count = symbol_ids(guarded)
        .into_iter()
        .chain(symbol_ids(candidate))
        .max()
        .map_or(0, |id| id + 1) as usize;
    let mut symbol_types = vec![terms.int(u64::from(register_width)); symbol_count];
    for (symbol, requirement) in &rule.operand_registers {
        let ty = requirement
            .exclusive_float_type(terms)
            .map_err(|()| relaxation_error(rule, "unsupported float register format"))?
            .unwrap_or_else(|| terms.int(u64::from(requirement.width())));
        if let Some(slot) = symbol_types.get_mut(*symbol as usize) {
            *slot = ty;
        }
    }
    Ok(symbol_types)
}

fn prove_float_relaxation(
    terms: &tir::Terms,
    guarded: &SemGraph,
    candidate: &SemGraph,
    symbol_types: &[tir::TypeId],
    floating: bool,
) -> Option<GuardedRelaxationProof> {
    let candidate_root = candidate.root().unwrap();
    let full_root = guarded.root().unwrap();
    let mut proof_candidate_root = candidate_root;
    let mut proof_guarded_root = full_root;
    if matches!(
        candidate.get_node(candidate_root),
        SymKind::SExt | SymKind::ZExt
    ) && candidate.get_node(candidate_root) == guarded.get_node(full_root)
        && candidate.subgraph_eq(
            candidate.children(candidate_root).nth(1).unwrap(),
            guarded,
            guarded.children(full_root).nth(1).unwrap(),
        )
    {
        proof_candidate_root = candidate.children(candidate_root).next().unwrap();
        proof_guarded_root = guarded.children(full_root).next().unwrap();
    }
    let mut proof_candidate = SemGraph::new();
    tir_symbolic::sem::copy_subgraph(
        &mut proof_candidate,
        candidate,
        proof_candidate_root,
        &mut HashMap::new(),
    );
    let mut proof_guarded = SemGraph::new();
    tir_symbolic::sem::copy_subgraph(
        &mut proof_guarded,
        guarded,
        proof_guarded_root,
        &mut HashMap::new(),
    );
    if floating
        && float_refinement::ieee_arithmetic_refines(terms, guarded, candidate, symbol_types)
    {
        return Some(GuardedRelaxationProof::Proven);
    }
    if floating {
        match SmtOracle.refines_typed_outcome(terms, &proof_candidate, &proof_guarded, symbol_types)
        {
            tir::sem::ProofOutcome::Proven => return Some(GuardedRelaxationProof::Proven),
            tir::sem::ProofOutcome::Unsupported(_) => {}
            tir::sem::ProofOutcome::Disproven { .. } => {}
        }
    }
    None
}

/// The conjunction of definedness conditions of every partial-kind node reachable
/// from `root`, appended to `g`; `None` when the subgraph holds no partial op.
fn definedness_of(g: &mut SemGraph, root: NodeId, widths: &[Option<u32>]) -> Option<NodeId> {
    let mut conditions = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !seen.insert(node.index()) {
            continue;
        }
        stack.extend(g.children(node).collect::<Vec<_>>());
        if let Some(cond) = definedness_condition(g, node, widths) {
            conditions.push(cond);
        }
    }
    conditions.into_iter().reduce(|a, b| {
        let and = g.add_node(SymKind::And);
        g.add_edge(and, a);
        g.add_edge(and, b);
        and
    })
}

/// Whether relaxing `guarded` to its (guard-dropped) else arm is sound: the guard
/// region must lie inside the region where the pure op is undefined. Concretely,
/// prove `D(else) AND guard_cond` is unsatisfiable — the guard fires only where the
/// IR op is undefined, so outside the guard the instruction already computes the
/// pure op (the shared else arm) and the drop loses nothing.
///
/// This is the tractable form of `D(pattern) => guarded == pattern`: since the else
/// arm *is* the pattern, that obligation is `(D AND guard_cond) => (then == else)`,
/// discharged here by refuting its antecedent. `D`'s divisor-nonzero test and the
/// guard's divisor-zero test share the divisor symbol, so the conflict closes by
/// unit propagation without the SAT solver ever entering the divider circuit (the
/// full-equality form forces it through a divider miter — seconds, not
/// milliseconds).
fn relaxation_holds(
    guarded: &SemGraph,
    if_root: NodeId,
    else_arm: NodeId,
    register_width: u32,
) -> bool {
    let guard_cond = match guarded.children(if_root).next() {
        Some(cond) => cond,
        None => return false,
    };

    let mut g = SemGraph::new();
    let mut memo = HashMap::new();
    tir_symbolic::sem::copy_subgraph(&mut g, guarded, if_root, &mut memo);
    let cond = memo[&guard_cond.index()];
    let else_copy = memo[&else_arm.index()];

    let widths = infer_widths(&g, |id| match g.get_leaf_data(id) {
        Some(SymPayload::SymbolId(_)) => Some(register_width),
        _ => None,
    });
    // A total else arm (no partial op) has no definedness slack; the antecedent is
    // then the guard alone, so relaxing is sound only if the guard is never taken.
    let defined = definedness_of(&mut g, else_copy, &widths).unwrap_or_else(|| {
        let always = g.add_node(SymKind::Constant);
        g.set_leaf_data(always, SymPayload::Int(APInt::new(1, 1)));
        always
    });
    let conflict = g.add_node(SymKind::And);
    g.add_edge(conflict, defined);
    g.add_edge(conflict, cond);
    debug_assert_eq!(g.root(), Some(conflict));

    let mut never = SemGraph::new();
    let zero = never.add_node(SymKind::Constant);
    never.set_leaf_data(zero, SymPayload::Int(APInt::new(1, 0)));

    let symbol_count = symbol_ids(&g).into_iter().max().map_or(0, |m| m + 1) as usize;
    let symbol_widths = vec![register_width; symbol_count];
    SmtOracle.equivalent(&g, &never, &symbol_widths)
}

fn symbol_ids(g: &SemGraph) -> Vec<u32> {
    (0..g.len())
        .filter_map(|i| match g.get_leaf_data(NodeId::from_index(i)) {
            Some(SymPayload::SymbolId(id)) => Some(*id),
            _ => None,
        })
        .collect()
}
