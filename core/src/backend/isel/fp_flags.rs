use std::collections::HashMap;

use tir::{
    Context, HasResourceSemantics, NodeId, OpId,
    sem::{SemGraph, SymKind, SymPayload, canonicalize_for_selection},
};

use super::{CaptureBindings, FpFlags, FunctionSelection, Rule};

pub(super) fn accepts(
    context: &Context,
    fs: &FunctionSelection,
    op: OpId,
    rule: &Rule,
    captures: &CaptureBindings,
) -> bool {
    let Some(interface) = context
        .get_op(op)
        .as_interface::<dyn HasResourceSemantics>()
    else {
        return true;
    };
    let semantics = interface.resource_semantics();
    let Some(raised) = semantics.raised_flags else {
        return true;
    };
    let FpFlags::Exact(pattern) = &rule.fp_flags else {
        return false;
    };
    let bind = |payload: &SymPayload<tir::ValueId>| {
        match payload {
            SymPayload::SymbolId(symbol) => captures
                .entries
                .iter()
                .find_map(|(candidate, class)| (candidate == symbol).then_some(*class)),
            SymPayload::Value(value) => fs
                .class_values
                .iter()
                .find_map(|(class, values)| values.contains(value).then_some(*class)),
            _ => None,
        }
        .map(|class| fs.egraph.find(class).0)
    };
    let normalize = |graph: &SemGraph, root| {
        let mut normalized = SemGraph::new();
        let root = normalize_node(graph, root, &mut normalized, &mut HashMap::new(), &bind)?;
        for node in normalized.preorder(root) {
            if let Some(SymPayload::SymbolId(class)) = normalized.get_leaf_data(node)
                && let Some(declared) = normalized
                    .get_annotation(node)
                    .and_then(|m| m.actual_type)
                    .and_then(|ty| tir::sem::egraph::semantic_type(context, ty))
                && let Some(bound) = tir::sem::egraph::class_semantic_type(
                    context,
                    &fs.egraph,
                    tir_relational::ClassId::from_raw(*class),
                )
            {
                tir::sem::TypeUnifier::default()
                    .unify(&declared, &bound)
                    .ok()?;
            }
        }
        let types = tir::sem::infer_types(&normalized, |node| {
            normalized
                .get_annotation(node)
                .and_then(|m| m.actual_type)
                .and_then(|ty| tir::sem::egraph::semantic_type(context, ty))
                .or_else(|| match normalized.get_leaf_data(node) {
                    Some(SymPayload::SymbolId(class)) => tir::sem::egraph::class_semantic_type(
                        context,
                        &fs.egraph,
                        tir_relational::ClassId::from_raw(*class),
                    ),
                    _ => None,
                })
        })
        .ok()?;
        for (index, ty) in types.iter().enumerate() {
            let node = NodeId::from_index(index);
            if let Some(SymPayload::Int(value)) = normalized.get_leaf_data(node) {
                normalized.set_leaf_data(
                    node,
                    SymPayload::Int(super::pattern::widen_pattern_literal(value, ty)),
                );
            }
        }
        Some(canonicalize_for_selection(
            &normalized,
            root,
            &Default::default(),
        ))
    };
    let Some((source, source_root, _)) = normalize(&semantics.graph, raised) else {
        return false;
    };
    let Some((target, target_root, _)) = normalize(pattern, pattern.root().expect("flags term"))
    else {
        return false;
    };
    source.subgraph_eq(source_root, &target, target_root)
}

fn normalize_node(
    source: &SemGraph,
    node: NodeId,
    target: &mut SemGraph,
    memo: &mut HashMap<NodeId, NodeId>,
    bind: &impl Fn(&SymPayload<tir::ValueId>) -> Option<u32>,
) -> Option<NodeId> {
    if let Some(&copied) = memo.get(&node) {
        return Some(copied);
    }
    let kind = *source.get_node(node);
    let children: Vec<_> = source
        .children(node)
        .enumerate()
        .map(|(index, child)| {
            if kind == SymKind::StateRead && index == 0 {
                Some(target.add_node(SymKind::StateBlock))
            } else {
                normalize_node(source, child, target, memo, bind)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    let copied = target.add_node(kind);
    if let Some(payload) = source.get_leaf_data(node) {
        target.set_leaf_data(
            copied,
            if kind == SymKind::Symbol {
                SymPayload::SymbolId(bind(payload)?)
            } else {
                payload.clone()
            },
        );
    }
    if let Some(ty) = source.get_annotation(node).and_then(|m| m.actual_type) {
        target.annotation_mut(copied).actual_type = Some(ty);
    }
    for child in children {
        target.add_edge(copied, child);
    }
    memo.insert(node, copied);
    Some(copied)
}
