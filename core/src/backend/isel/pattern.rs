//! Compilation of rule semantic expressions into matchable patterns.

use std::collections::{HashMap, HashSet};

use tir::{
    Context,
    graph::{Dag, MetaDag, NodeId, OperandConstraint},
    sem::{SemGraph, SemType, SymKind, SymPayload, TypeUnifier, infer_types},
};
use tir_symbolic::egraph::{Id, Pattern, PatternNode, Var};

use super::node::{SemEGraph, SemNode, class_int_binding, class_semantic_type};
use super::{ImmRange, RegisterRequirement};

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
    result_register: Option<RegisterRequirement>,
}

/// Per-pattern-node matching metadata.
#[derive(Clone, Default)]
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
    /// Storage capability and bit demand of a physical register operand.
    pub(crate) register: Option<RegisterRequirement>,
    /// A store's source register may be produced by a separate constant
    /// materializer; other register operands keep preferring immediate forms.
    pub(crate) materialized_constant: bool,
    /// Encoding range of an immediate operand (see `Rule::operand_imm_ranges`).
    pub(crate) imm_range: Option<ImmRange>,
    /// The symbolic value type inferred from the semantic operator signatures.
    pub(crate) semantic_type: Option<SemType>,
}

impl PatternNodeMeta {
    /// The node demands its class in a register: a physical-register operand or
    /// an explicit register constraint.
    pub(crate) fn demands_register(&self) -> bool {
        self.register.is_some() || self.constraint == Some(OperandConstraint::Register)
    }
}

impl CompiledIselPattern {
    fn match_types(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        matched: &tir_symbolic::egraph::EMatch<u32>,
    ) -> bool {
        let mut unifier = TypeUnifier::default();
        let nodes_match = self.node_meta.iter().enumerate().all(|(index, meta)| {
            if meta.is_boundary {
                return true;
            }
            let Some(expected) = &meta.semantic_type else {
                return true;
            };
            let class = matched.binding(Id::from_raw(index as u32));
            class_semantic_type(ctx, egraph, class)
                .is_none_or(|actual| unifier.unify(expected, &actual).is_ok())
        });
        nodes_match
            && self.result_register.is_none_or(|register| {
                class_semantic_type(ctx, egraph, matched.root)
                    .is_none_or(|actual| register.accepts(&actual))
            })
    }

    /// The operand symbols the pattern reads as registers.
    pub(crate) fn register_symbols(&self) -> HashSet<u32> {
        (0..self.pattern.len())
            .filter_map(|index| {
                let PatternNode::Var(Var::Symbol(symbol)) =
                    self.pattern.node(Id::from_raw(index as u32))
                else {
                    return None;
                };
                self.node_meta[index].demands_register().then_some(*symbol)
            })
            .collect()
    }

    /// Whether `class` may bind under `pattern_node`: a width requirement rejects
    /// a value *known* to be of a different width than the instruction operates
    /// at (a rewrite-introduced class of unknown width is produced at register
    /// width, so it still matches), an immediate range rejects a constant the
    /// encoding field cannot represent, and an immediate constraint requires a
    /// constant member. A store source register may bind a constant because the
    /// cover must then choose a materializing instruction for that class.
    pub(crate) fn boundary_ok(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        pattern_node: Id,
        class: Id,
    ) -> bool {
        self.boundary_ok_impl(egraph, ctx, pattern_node, class, false)
    }

    fn boundary_ok_impl(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        pattern_node: Id,
        class: Id,
        bool_binds_wide: bool,
    ) -> bool {
        let meta = &self.node_meta[pattern_node.index()];
        if let Some(required) = meta.register
            && let Some(actual) = class_semantic_type(ctx, egraph, class)
            && !required.accepts(&actual)
            && !(bool_binds_wide
                && matches!(actual, SemType::Bits(tir::sem::Width::Const(1)))
                && required.accepts_low_bits(&actual))
        {
            return false;
        }
        if let Some(range) = meta.imm_range
            && let Some(value) = class_int_binding(egraph, class)
            && !range.contains(&value)
        {
            return false;
        }
        match meta.constraint {
            Some(OperandConstraint::Register) => {
                meta.materialized_constant
                    || egraph
                        .nodes(class)
                        .iter()
                        .any(|n| n.kind != SymKind::Constant)
            }
            Some(OperandConstraint::Immediate) => egraph
                .nodes(class)
                .iter()
                .any(|n| n.kind == SymKind::Constant),
            None => true,
        }
    }

    /// Matches across the whole e-graph, honoring only the boundary constraints
    /// (register/immediate/width) — the entry used where no block-level legality
    /// applies (conditional-branch selection). Here a width-1 class may bind a
    /// register-width operand: a materialized i1 occupies its register as 0/1
    /// (the hand-written branch-if-nonzero fallbacks already test the full
    /// register), so a zero-compare branch reads the same bits either way.
    pub(crate) fn search(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
    ) -> Vec<tir_symbolic::egraph::EMatch<u32>> {
        self.search_with_legality(egraph, ctx, &|node, class| {
            self.boundary_ok_impl(egraph, ctx, node, class, true)
        })
    }

    pub(crate) fn search_with_legality(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<tir_symbolic::egraph::EMatch<u32>> {
        self.pattern
            .search_with_legality(egraph, allowed)
            .into_iter()
            .filter(|matched| self.match_types(egraph, ctx, matched))
            .collect()
    }
}

pub(crate) fn compile_isel_pattern(
    rule_index: usize,
    expr: &SemGraph,
    operand_constraints: &[(u32, OperandConstraint)],
    operand_registers: &[(u32, RegisterRequirement)],
    operand_imm_ranges: &[(u32, ImmRange)],
    result_register: Option<RegisterRequirement>,
) -> Option<CompiledIselPattern> {
    let root = expr.root()?;
    let inferred_types = infer_types(expr, |_| None).ok()?;
    let mut pattern = Pattern::new();
    let mut node_meta = Vec::new();
    let mut memo = HashMap::new();
    let pattern_root = compile_isel_pattern_node(
        expr,
        root,
        &mut pattern,
        &mut node_meta,
        &mut memo,
        &inferred_types,
        operand_constraints,
        operand_registers,
        operand_imm_ranges,
    )?;
    pattern.set_root(pattern_root);

    if *expr.get_node(root) == SymKind::StoreMemory
        && let Some(value) = expr.children(root).nth(2)
        && let Some(pattern_value) = memo.get(&value)
    {
        node_meta[pattern_value.index()].materialized_constant = true;
    }

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
        result_register,
    })
}

#[allow(clippy::too_many_arguments)]
fn compile_isel_pattern_node(
    expr: &SemGraph,
    node: NodeId,
    pattern: &mut Pattern<SemNode, u32>,
    node_meta: &mut Vec<PatternNodeMeta>,
    memo: &mut HashMap<NodeId, Id>,
    inferred_types: &[SemType],
    operand_constraints: &[(u32, OperandConstraint)],
    operand_registers: &[(u32, RegisterRequirement)],
    operand_imm_ranges: &[(u32, ImmRange)],
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
                register: operand_registers
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, requirement)| *requirement),
                imm_range: operand_imm_ranges
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, r)| *r),
                semantic_type: Some(inferred_types[node.index()].clone()),
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
                    semantic_type: Some(inferred_types[node.index()].clone()),
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
                        inferred_types,
                        operand_constraints,
                        operand_registers,
                        operand_imm_ranges,
                    )
                })
                .collect::<Option<Vec<Id>>>()?;
            let compiled = pattern.add(SemNode {
                kind: *kind,
                payload: None,
                ty: expr.get_actual_type(node),
                children,
            });
            node_meta.push(PatternNodeMeta {
                semantic_type: Some(inferred_types[node.index()].clone()),
                ..Default::default()
            });
            compiled
        }
    };

    memo.insert(node, compiled);
    Some(compiled)
}
/// The immediate ranges of the rule set's zero-form constant materializers:
/// rules whose pattern is `Add(ZExt(0b0, W), imm)` with an immediate-constrained,
/// range-annotated `imm` (the TMDL-derived `addi rd, zero_reg, imm` li form).
/// Program constants fitting one of these ranges can be covered by a real
/// instruction, so the constant bridge injects the matching shape for them.
pub(crate) fn constant_materializer_ranges(patterns: &[CompiledIselPattern]) -> Vec<ImmRange> {
    patterns
        .iter()
        .filter_map(|compiled| {
            let root = compiled.pattern.root();
            let PatternNode::Node(root_node) = compiled.pattern.node(root) else {
                return None;
            };
            if root_node.kind != SymKind::Add || root_node.children.len() != 2 {
                return None;
            }
            let mut has_zero_zext = false;
            let mut imm_range = None;
            for &child in &root_node.children {
                match compiled.pattern.node(child) {
                    PatternNode::Node(node)
                        if node.kind == SymKind::ZExt && node.children.len() == 2 =>
                    {
                        let zero_value = matches!(
                            compiled.pattern.node(node.children[0]),
                            PatternNode::Node(zero)
                                if zero.kind == SymKind::Constant
                                    && matches!(
                                        &zero.payload,
                                        Some(super::SemPayload::Expr(SymPayload::Int(v)))
                                            if v.to_u64() == 0
                                    )
                        );
                        let wildcard_width =
                            matches!(compiled.pattern.node(node.children[1]), PatternNode::Var(_));
                        has_zero_zext = zero_value && wildcard_width;
                    }
                    PatternNode::Var(Var::Symbol(_)) => {
                        let meta = &compiled.node_meta[child.index()];
                        if meta.constraint == Some(OperandConstraint::Immediate) {
                            imm_range = meta.imm_range;
                        }
                    }
                    _ => {}
                }
            }
            has_zero_zext.then_some(imm_range).flatten()
        })
        .collect()
}
