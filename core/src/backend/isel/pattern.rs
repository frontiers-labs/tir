//! Compilation of rule semantic expressions into matchable patterns.

use std::collections::{HashMap, HashSet};

use tir::{
    Context,
    graph::{Dag, Matchable, MetaDag, NodeId, OperandConstraint},
    sem::{SemGraph, SymKind, SymPayload},
};
use tir_symbolic::egraph::{Id, Pattern, PatternNode, Var};

use super::ImmRange;
use super::node::{SemEGraph, SemNode, class_int_binding, class_is_float, class_width};

/// A rule's pattern compiled for e-matching: the [`Pattern`] itself plus the
/// per-pattern-node metadata the matcher and the PBQP cover consult.
pub(crate) struct CompiledIselPattern {
    pub(crate) rule_index: usize,
    pub(crate) pattern: Pattern<SemNode, u32>,
    /// Matching metadata for each pattern node (indexed by pattern node id).
    pub(crate) node_meta: Vec<PatternNodeMeta>,
    /// Number of type-constrained pattern nodes — how "specific" this pattern is.
    /// At equal instruction cost, a more specific match is preferred, so an i32
    /// `addw` (one typed node) beats the untyped `add` for an i32 value, while the
    /// untyped `add`/`and` still match every other width.
    pub(crate) specificity: usize,
}

/// Per-pattern-node matching metadata.
#[derive(Clone, Copy, Default)]
pub(crate) struct PatternNodeMeta {
    /// An operand capture point (a `Var::Symbol` leaf).
    pub(crate) is_boundary: bool,
    /// A constant template: pure, folded into the encoding, never consumed by
    /// the match — boundary-like for the cover.
    pub(crate) is_constant: bool,
    /// Whether any number of matches may embed this node's class (operands and
    /// constants).
    pub(crate) duplicable: bool,
    pub(crate) constraint: Option<OperandConstraint>,
    /// Required value width of the bound class (see `Rule::operand_widths`).
    pub(crate) width: Option<u32>,
    /// Encoding range of an immediate operand (see `Rule::operand_imm_ranges`).
    pub(crate) imm_range: Option<ImmRange>,
    /// Whether the bound value must (`true`) or must not (`false`) be a float
    /// (see `Rule::operand_floats`).
    pub(crate) float: Option<bool>,
}

impl CompiledIselPattern {
    /// Whether `class` may bind under `pattern_node`: a width requirement rejects
    /// a value *known* to be of a different width than the instruction operates
    /// at (a rewrite-introduced class of unknown width is produced at register
    /// width, so it still matches), an immediate range rejects a constant the
    /// encoding field cannot represent, and a register/immediate constraint
    /// requires a non-constant/constant member.
    pub(crate) fn boundary_ok(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        pattern_node: Id,
        class: Id,
    ) -> bool {
        let meta = &self.node_meta[pattern_node.index()];
        if let Some(required) = meta.width
            && let Some(actual) = class_width(ctx, egraph, class)
            && actual != required
        {
            return false;
        }
        if let Some(range) = meta.imm_range
            && let Some(value) = class_int_binding(egraph, class)
            && !range.contains(&value)
        {
            return false;
        }
        if let Some(required) = meta.float
            && let Some(actual) = class_is_float(ctx, egraph, class)
            && actual != required
        {
            return false;
        }
        match meta.constraint {
            Some(OperandConstraint::Register) => egraph
                .nodes(class)
                .iter()
                .any(|n| n.kind != SymKind::Constant),
            Some(OperandConstraint::Immediate) => egraph
                .nodes(class)
                .iter()
                .any(|n| n.kind == SymKind::Constant),
            None => true,
        }
    }

    /// Matches across the whole e-graph, honoring only the boundary constraints
    /// (register/immediate/width) — the entry used where no block-level legality
    /// applies (conditional-branch selection).
    pub(crate) fn search(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
    ) -> Vec<tir_symbolic::egraph::EMatch<u32>> {
        self.pattern.search_with_legality(egraph, &|node, class| {
            self.boundary_ok(egraph, ctx, node, class)
        })
    }
}

pub(crate) fn compile_isel_pattern(
    rule_index: usize,
    expr: &SemGraph,
    operand_constraints: &[(u32, OperandConstraint)],
    operand_widths: &[(u32, u32)],
    operand_imm_ranges: &[(u32, ImmRange)],
    operand_floats: &[(u32, bool)],
) -> Option<CompiledIselPattern> {
    let root = expr.root()?;
    let mut pattern = Pattern::new();
    let mut node_meta = Vec::new();
    let mut memo = HashMap::new();
    let pattern_root = compile_isel_pattern_node(
        expr,
        root,
        &mut pattern,
        &mut node_meta,
        &mut memo,
        operand_constraints,
        operand_widths,
        operand_imm_ranges,
        operand_floats,
    )?;
    pattern.set_root(pattern_root);

    // A pattern that is a bare operand symbol — a register-to-register copy like
    // x86 `mov dst, src` — selects nothing: it would match every e-class as
    // "compute x by copying x", rooting a self-referential instruction.
    if pattern.len() == 1 && node_meta[0].is_boundary {
        return None;
    }

    let specificity = (0..pattern.len())
        .map(|index| Id::from_raw(index as u32))
        .filter(|&n| matches!(pattern.node(n), PatternNode::Node(node) if node.ty.is_some()))
        .count();

    Some(CompiledIselPattern {
        rule_index,
        pattern,
        node_meta,
        specificity,
    })
}

#[allow(clippy::too_many_arguments)]
fn compile_isel_pattern_node(
    expr: &SemGraph,
    node: NodeId,
    pattern: &mut Pattern<SemNode, u32>,
    node_meta: &mut Vec<PatternNodeMeta>,
    memo: &mut HashMap<NodeId, Id>,
    operand_constraints: &[(u32, OperandConstraint)],
    operand_widths: &[(u32, u32)],
    operand_imm_ranges: &[(u32, ImmRange)],
    operand_floats: &[(u32, bool)],
) -> Option<Id> {
    if let Some(compiled) = memo.get(&node).copied() {
        return Some(compiled);
    }

    let compiled = match expr.get_node(node) {
        SymKind::Symbol => {
            let Some(SymPayload::SymbolId(symbol)) = expr.get_leaf_data(node) else {
                return None;
            };
            let compiled = pattern.var(Var::Symbol(*symbol));
            node_meta.push(PatternNodeMeta {
                is_boundary: true,
                duplicable: true,
                constraint: operand_constraints
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, c)| *c),
                width: operand_widths
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, w)| *w),
                imm_range: operand_imm_ranges
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, r)| *r),
                float: operand_floats
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, f)| *f),
                ..Default::default()
            });
            compiled
        }
        SymKind::Constant => match expr.get_leaf_data(node) {
            Some(SymPayload::Int(value)) => {
                let compiled = pattern.add(SemNode {
                    kind: SymKind::Constant,
                    payload: Some(super::SemPayload::Expr(SymPayload::Int(value.clone()))),
                    ty: expr.get_actual_type(node),
                    children: Vec::new(),
                });
                // A constant is pure and folds into the encoding, so any number of
                // matches may embed the same constant class.
                node_meta.push(PatternNodeMeta {
                    is_constant: true,
                    duplicable: true,
                    ..Default::default()
                });
                compiled
            }
            _ => return None,
        },
        kind => {
            // Children compile first: a pattern node's operands must have
            // smaller ids than the node itself.
            let children = expr
                .children(node)
                .map(|child| {
                    compile_isel_pattern_node(
                        expr,
                        child,
                        pattern,
                        node_meta,
                        memo,
                        operand_constraints,
                        operand_widths,
                        operand_imm_ranges,
                        operand_floats,
                    )
                })
                .collect::<Option<Vec<Id>>>()?;
            let compiled = pattern.add(SemNode {
                kind: *kind,
                payload: None,
                ty: expr.get_actual_type(node),
                children,
            });
            node_meta.push(PatternNodeMeta::default());
            compiled
        }
    };

    memo.insert(node, compiled);
    Some(compiled)
}
/// The semantic kinds for which the rule set provides an atomic materializer (a
/// pattern whose root is that kind with only operand boundaries beneath it).
pub(crate) fn atomic_kinds(patterns: &[CompiledIselPattern]) -> HashSet<SymKind> {
    let ctx = Context::default();
    let mut kinds = HashSet::new();
    for compiled in patterns {
        let root = compiled.pattern.root();
        let PatternNode::Node(root_node) = compiled.pattern.node(root) else {
            continue;
        };
        if root_node.kind.num_children(&ctx) == 0 {
            continue;
        }
        let children = root_node.children.clone();
        if !children.is_empty()
            && children.iter().all(|&child| {
                matches!(
                    compiled.pattern.node(child),
                    PatternNode::Var(Var::Symbol(_))
                )
            })
        {
            kinds.insert(root_node.kind);
        }
    }
    kinds
}
