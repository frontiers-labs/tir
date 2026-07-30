//! Compilation of rule semantic expressions into matchable patterns.

use std::collections::{HashMap, HashSet};

use tir::{
    Context,
    graph::{Dag, MetaDag, NodeId, OperandConstraint},
    sem::{SemGraph, SemType, SymKind, SymPayload, TypeUnifier, infer_types},
};
use tir_symbolic::egraph::{EMatch, Id, Pattern, PatternNode, Substitution, Var};

use super::node::{
    SemEGraph, SemNode, class_int_binding, class_semantic_type, low_extract_source,
    low_extract_width,
};
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
    copy: bool,
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
    pub(crate) fn is_copy(&self) -> bool {
        self.copy
    }

    pub(crate) fn constant_materializer_range(&self) -> Option<ImmRange> {
        let root = self.pattern.root();
        if let PatternNode::Var(Var::Symbol(_)) = self.pattern.node(root) {
            let meta = &self.node_meta[root.index()];
            return (meta.constraint == Some(OperandConstraint::Immediate))
                .then_some(meta.imm_range)
                .flatten();
        }
        None
    }

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
    /// constant member. Register constraints are checked by the cover: a constant
    /// may bind here only if a selected materializer makes it available in a
    /// register.
    pub(crate) fn boundary_ok(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        pattern_node: Id,
        class: Id,
    ) -> bool {
        let meta = &self.node_meta[pattern_node.index()];
        if let Some(required) = meta.register
            && let Some(actual) = class_semantic_type(ctx, egraph, class)
            && !required.accepts(&actual)
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
            Some(OperandConstraint::Register) => true,
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
    #[cfg(test)]
    pub(crate) fn search(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
    ) -> Vec<tir_symbolic::egraph::EMatch<u32>> {
        self.search_with_legality(egraph, ctx, &|node, class| {
            self.boundary_ok(egraph, ctx, node, class)
        })
    }

    pub(crate) fn search_with_legality(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<tir_symbolic::egraph::EMatch<u32>> {
        if self.copy {
            return self.search_low_bit_copies(egraph, ctx);
        }
        self.pattern
            .search_with_legality(egraph, allowed)
            .into_iter()
            .filter(|matched| self.match_types(egraph, ctx, matched))
            .collect()
    }

    pub(crate) fn search_roots_with_legality(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        roots: impl IntoIterator<Item = Id>,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<tir_symbolic::egraph::EMatch<u32>> {
        if self.copy {
            return self.search_low_bit_copies_at(egraph, ctx, roots);
        }
        self.pattern
            .search_roots_with_legality(egraph, roots, allowed)
            .into_iter()
            .filter(|matched| self.match_types(egraph, ctx, matched))
            .collect()
    }

    pub(crate) fn search_roots(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        roots: impl IntoIterator<Item = Id>,
    ) -> Vec<tir_symbolic::egraph::EMatch<u32>> {
        self.search_roots_with_legality(egraph, ctx, roots, &|node, class| {
            self.boundary_ok(egraph, ctx, node, class)
        })
    }

    /// A copy rule roots on the low-bit `Extract` view of a wider class, binding
    /// its bare symbol to the view's source — rooting on the source class itself
    /// would make the copy self-referential.
    fn search_low_bit_copies(&self, egraph: &SemEGraph, ctx: &Context) -> Vec<EMatch<u32>> {
        let roots: Vec<_> = egraph.classes().map(|class| class.id()).collect();
        self.search_low_bit_copies_at(egraph, ctx, roots)
    }

    fn search_low_bit_copies_at(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        roots: impl IntoIterator<Item = Id>,
    ) -> Vec<EMatch<u32>> {
        let root_node = self.pattern.root();
        let PatternNode::Var(var @ Var::Symbol(_)) = self.pattern.node(root_node) else {
            unreachable!("copy pattern is a bare symbol")
        };
        let meta = &self.node_meta[root_node.index()];
        if meta.constraint == Some(OperandConstraint::Immediate) {
            return Vec::new();
        }
        roots
            .into_iter()
            .filter_map(|root| {
                let root = egraph.find(root);
                let source = low_extract_source(egraph, root)?;
                let width = low_extract_width(egraph, root)?;
                let source_ty = class_semantic_type(ctx, egraph, source)?;
                let writes_the_view = self.result_register.is_some_and(|register| {
                    register.capability.integer && register.capability.width == width
                });
                let reads_the_source = meta
                    .register
                    .is_none_or(|register| register.accepts_low_view_source(&source_ty));
                if !writes_the_view || !reads_the_source {
                    return None;
                }
                let mut subst = Substitution::new();
                subst.insert(var.clone(), source);
                Some(EMatch {
                    root,
                    subst,
                    bindings: vec![Some(source)],
                })
            })
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
    let root = canonical_pattern_root(expr, expr.root()?);
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

    // A bare register-to-register copy cannot root on its own operand class
    // without becoming self-referential. A bare immediate rule is different:
    // it encodes the captured constant and therefore materializes that class.
    let copy = pattern.len() == 1 && node_meta[0].demands_register();

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
        copy,
    })
}

fn canonical_pattern_root(expr: &SemGraph, root: NodeId) -> NodeId {
    if *expr.get_node(root) != SymKind::Add {
        return root;
    }
    let children: Vec<NodeId> = expr.children(root).collect();
    let [lhs, rhs] = children.as_slice() else {
        return root;
    };
    if is_extended_zero(expr, *lhs) {
        *rhs
    } else if is_extended_zero(expr, *rhs) {
        *lhs
    } else {
        root
    }
}

fn is_extended_zero(expr: &SemGraph, node: NodeId) -> bool {
    if *expr.get_node(node) != SymKind::ZExt {
        return false;
    }
    let children: Vec<NodeId> = expr.children(node).collect();
    let [value, _] = children.as_slice() else {
        return false;
    };
    matches!(
        expr.get_leaf_data(*value),
        Some(SymPayload::Int(value)) if value.to_u64() == 0
    )
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
