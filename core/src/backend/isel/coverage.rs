//! The instruction-selection coverage matrix: for every semantic kind the
//! builtin dialect's op semantics can produce, how the target realizes it —
//! `rooted` by an instruction, `bridged` by a proved axiom, a zero-cost
//! register `view`, or `UNCOVERED` (a documented gap needing a rooted
//! instruction; no bridge search covers it).
//!
//! The kinds are computed, not hand-listed: each row is a [`SymKind`] reached by
//! walking the [`semantic_expr`](tir::Operation::semantic_expr) of every op in
//! [`REPRESENTATIVE_MODULE`] — the same interface the isel graph builder
//! consumes. The *module* is hand-maintained, though: a value-producing builtin
//! op absent from it contributes no kinds and is silently missing from the
//! matrix, so the module must be kept in sync with the builtin dialect (see
//! [`REPRESENTATIVE_MODULE`]). `tir axioms --report` renders the matrix to
//! `backends/<target>/src/isel.coverage`; a freshness test diffs the committed
//! file, so a kind regressing to `UNCOVERED` fails.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tir::graph::{Dag, NodeId};
use tir::sem::{SemGraph, SymKind};
use tir::{Context, OpInstance, Operation, builtin::ops};

use super::axioms::parse_axiom;
use super::pattern::compile_isel_pattern;
use super::theory::enabled_axioms;
use super::{Rule, RuleKind};
use tir_symbolic::egraph::{PatternNode, Var};

/// How a target realizes a semantic kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CoverageStatus {
    /// An instruction pattern roots the kind directly.
    Rooted,
    /// A proved axiom rewrites the kind onto rooted kinds.
    Bridged,
    /// A zero-cost register re-view (the low-slice `Extract` value model).
    View,
    /// No rooted instruction and no bridge search: a documented gap.
    Uncovered,
}

impl CoverageStatus {
    fn label(self) -> &'static str {
        match self {
            CoverageStatus::Rooted => "rooted",
            CoverageStatus::Bridged => "bridged",
            CoverageStatus::View => "view",
            CoverageStatus::Uncovered => "UNCOVERED",
        }
    }
}

/// One coverage-matrix row.
pub struct CoverageRow {
    pub kind: SymKind,
    pub status: CoverageStatus,
    pub via: String,
}

/// A module exercising one instance of every value-producing builtin op, so
/// walking its ops enumerates every kind the builtin dialect can produce. Each
/// op's own `semantic_expr` is independent, so one instance per op suffices.
///
/// This list is a hand-maintained mirror of the value-producing ops in
/// `core/src/dialects/builtin/{arith,float}.rs`; it must be kept in sync with
/// them. A new such op added there without a line here produces no coverage row
/// and its kinds go silently unchecked. There is no op-registry enumeration to
/// catch the omission — keeping this module current is a manual obligation.
const REPRESENTATIVE_MODULE: &str = "\
module {
  func @coverage(%w: !i64, %v: !i64, %n: !i16, %a: !f32, %b: !f32) -> !i64 {
    %add = addi %w, %v : !i64
    %sub = subi %w, %v : !i64
    %mul = muli %w, %v : !i64
    %and = andi %w, %v : !i64
    %or = ori %w, %v : !i64
    %xor = xori %w, %v : !i64
    %shl = shli %w, %v : !i64
    %shr = shrui %w, %v : !i64
    %sar = shrsi %w, %v : !i64
    %eq = cmpi %w, %v {predicate = \"eq\"} : !i1
    %ne = cmpi %w, %v {predicate = \"ne\"} : !i1
    %lt = cmpi %w, %v {predicate = \"slt\"} : !i1
    %ge = cmpi %w, %v {predicate = \"sge\"} : !i1
    %ult = cmpi %w, %v {predicate = \"ult\"} : !i1
    %uge = cmpi %w, %v {predicate = \"uge\"} : !i1
    %sext = extsi %n : !i64
    %zext = extui %n : !i64
    %trunc = trunci %w : !i16
    %fadd = addf %a, %b : !f32
    %fsub = subf %a, %b : !f32
    %fmul = mulf %a, %b : !f32
    %fdiv = divf %a, %b : !f32
    return %w
  }
  module_end
}
";

/// Leaf kinds carry no materializer obligation.
fn is_leaf_kind(kind: SymKind) -> bool {
    matches!(kind, SymKind::Symbol | SymKind::Constant | SymKind::Arg)
}

/// Every operator [`SymKind`] the builtin dialect's op semantics can produce,
/// found by walking each op's `semantic_expr` in [`REPRESENTATIVE_MODULE`].
fn reachable_builtin_kinds() -> BTreeSet<SymKind> {
    let context = Context::with_default_dialects();
    let module = tir::parse::ir::parse_ir::<ops::ModuleOp>(&context, REPRESENTATIVE_MODULE)
        .expect("representative coverage module must parse");
    let mut kinds = BTreeSet::new();
    collect_op_kinds(&context, &context.get_op(module.id()), &mut kinds);
    kinds.retain(|&k| !is_leaf_kind(k));
    kinds
}

/// Recurse through `op` and its nested regions, unioning every kind each op's
/// semantic expression introduces.
fn collect_op_kinds(context: &Context, op: &Arc<OpInstance>, out: &mut BTreeSet<SymKind>) {
    let mut graph = SemGraph::new();
    if op.clone().as_dyn_op().semantic_expr(&mut graph).is_some() {
        for i in 0..graph.len() {
            out.insert(*graph.get_node(NodeId::from_index(i)));
        }
    }
    for region_id in &op.regions {
        let region = context.get_region(*region_id);
        for block in region.iter(context.clone()) {
            for op_id in block.op_ids() {
                collect_op_kinds(context, &context.get_op(op_id), out);
            }
        }
    }
}

/// The two root views of the rule set, compiled in one pass:
/// - `value_rooted`: each value-materializer rule's rooted kind mapped to the
///   rule names — a [`RuleKind::Value`] rule whose pattern root binds only
///   operand symbols. The value filter drops branch-fusion rules (a `cmp+b.eq`
///   roots `Eq` only in a branch), the all-symbol filter drops address/fused
///   arithmetic (a store's address `Add`, an `madd`'s inner `Mul`).
/// - `broad_roots`: every kind any rule pattern roots — the broad capability
///   test the family axioms gate on, matching `discover_rewrites`.
fn root_kinds(rules: &[Rule]) -> (BTreeMap<SymKind, BTreeSet<&'static str>>, BTreeSet<SymKind>) {
    let mut value_rooted: BTreeMap<SymKind, BTreeSet<&'static str>> = BTreeMap::new();
    let mut broad_roots = BTreeSet::new();
    for rule in rules {
        let Some(compiled) = compile_isel_pattern(
            0,
            &rule.pattern,
            &rule.operand_constraints,
            &rule.operand_registers,
            &rule.operand_imm_ranges,
            rule.result_register,
        ) else {
            continue;
        };
        let PatternNode::Node(node) = compiled.pattern.node(compiled.pattern.root()) else {
            continue;
        };
        broad_roots.insert(node.kind);
        if rule.kind != RuleKind::Value {
            continue;
        }
        let all_symbols = !node.children.is_empty()
            && node.children.iter().all(|&child| {
                matches!(
                    compiled.pattern.node(child),
                    PatternNode::Var(Var::Symbol(_))
                )
            });
        if all_symbols {
            value_rooted.entry(node.kind).or_default().insert(rule.name);
        }
    }
    (value_rooted, broad_roots)
}

/// The kinds a proved axiom bridges, mapped to the axiom names that bridge them:
/// the target-specific discovered bridges plus the target-independent families
/// installed under the same capability gating as `discover_rewrites`.
fn bridged_kinds(
    broad_roots: &BTreeSet<SymKind>,
    discovered_axioms: &[String],
) -> BTreeMap<SymKind, BTreeSet<String>> {
    let roots = |kind: SymKind| broad_roots.contains(&kind);
    let mut bridged: BTreeMap<SymKind, BTreeSet<String>> = BTreeMap::new();
    let mut record = |kind: SymKind, name: String| {
        bridged.entry(kind).or_default().insert(name);
    };

    for text in discovered_axioms {
        let axiom = parse_axiom(text).expect("discovered axiom must parse");
        if let Some(kind) = axiom.lhs_kind() {
            record(kind, axiom.name().to_string());
        }
    }

    for axiom in enabled_axioms(roots) {
        if let Some(kind) = axiom.lhs_kind() {
            record(kind, axiom.name().to_string());
        }
    }
    bridged
}

/// The coverage matrix for `rules`, given the axioms discovered over the same
/// rule set. `target` prefixes rooting instruction names in the `via` column.
pub fn discover_coverage(
    target: &str,
    rules: &[Rule],
    discovered_axioms: &[String],
) -> Vec<CoverageRow> {
    let (rooted, broad_roots) = root_kinds(rules);
    let bridged = bridged_kinds(&broad_roots, discovered_axioms);
    reachable_builtin_kinds()
        .into_iter()
        .map(|kind| {
            let (status, via) = if let Some(names) = rooted.get(&kind) {
                let via = names
                    .iter()
                    .map(|name| format!("{target}.{name}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                (CoverageStatus::Rooted, via)
            } else if kind == SymKind::Extract {
                (
                    CoverageStatus::View,
                    "register re-view (low-slice)".to_string(),
                )
            } else if let Some(names) = bridged.get(&kind) {
                (
                    CoverageStatus::Bridged,
                    names.iter().cloned().collect::<Vec<_>>().join(", "),
                )
            } else {
                (
                    CoverageStatus::Uncovered,
                    "requires rooted instruction (no bridge search)".to_string(),
                )
            };
            CoverageRow { kind, status, via }
        })
        .collect()
}

/// Render the coverage matrix as the committed `isel.coverage` file.
pub fn render_coverage_file(rows: &[CoverageRow]) -> String {
    let mut out = String::from(
        "; Instruction-selection coverage over the builtin dialect's kinds.\n\
         ; Generated by `tir axioms --report`; regenerate after adding instructions.\n\
         ; view = zero-cost register re-view; UNCOVERED = documented gap.\n\n",
    );
    out.push_str(&format!("{:<22}{:<11}{}\n", "kind", "status", "via"));
    for row in rows {
        out.push_str(&format!(
            "{:<22}{:<11}{}\n",
            format!("{:?}", row.kind),
            row.status.label(),
            row.via,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachable_kinds_cover_the_integer_subset() {
        let kinds = reachable_builtin_kinds();
        for expected in [
            SymKind::Add,
            SymKind::Sub,
            SymKind::Mul,
            SymKind::And,
            SymKind::Or,
            SymKind::Xor,
            SymKind::ShiftLeft,
            SymKind::ShiftRightLogic,
            SymKind::ShiftRightArithmetic,
            SymKind::Eq,
            SymKind::Ne,
            SymKind::Lt,
            SymKind::Ge,
            SymKind::ULt,
            SymKind::UGe,
            SymKind::SExt,
            SymKind::ZExt,
            SymKind::Extract,
        ] {
            assert!(
                kinds.contains(&expected),
                "missing reachable kind {expected:?}"
            );
        }
        assert!(
            !kinds.contains(&SymKind::Constant),
            "leaf kinds are excluded"
        );
    }
}
