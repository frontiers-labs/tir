use std::collections::{HashMap, HashSet};

use tir::graph::{Dag, MutDag, NodeId, subgraphs_equal};
use tir::sem::{
    FloatFormat, SemGraph, SemType, SmtOracle, SymKind, SymPayload, Width, infer_types,
};
use tir_adt::APInt;

pub(super) fn ieee_arithmetic_refines(
    full: &SemGraph,
    candidate: &SemGraph,
    symbol_types: &[SemType],
) -> bool {
    let (Some(full_root), Some(candidate_root)) = (full.root(), candidate.root()) else {
        return false;
    };
    let Some(rounded) = rounded_kind(*candidate.get_kind(candidate_root)) else {
        return false;
    };
    let Ok(types) = infer_types(candidate, |node| match candidate.get_leaf_data(node) {
        Some(SymPayload::SymbolId(id)) => symbol_types.get(*id as usize).cloned(),
        _ => None,
    }) else {
        return false;
    };
    let SemType::Float(format) = &types[candidate_root.index()] else {
        return false;
    };
    let Some((width, nan_mask)) = quiet_nan_mask(format) else {
        return false;
    };
    let operands: Vec<_> = candidate.children(candidate_root).collect();
    let shared: HashSet<_> = full
        .postorder(full_root)
        .filter(|&node| {
            if *full.get_kind(node) != rounded {
                return false;
            }
            let children: Vec<_> = full.children(node).collect();
            children.len() == operands.len() + 1
                && matches!(
                    full.get_leaf_data(children[operands.len()]),
                    Some(SymPayload::Int(mode)) if mode.width() == 3 && mode.to_u64() == 0
                )
                && operands
                    .iter()
                    .zip(&children)
                    .all(|(&lhs, &rhs)| subgraphs_equal(candidate, lhs, full, rhs))
        })
        .collect();
    if shared.is_empty() {
        return false;
    }

    let mut proof = SemGraph::new();
    let source = proof.add_node(SymKind::Symbol);
    proof.set_leaf_data(source, SymPayload::SymbolId(0));
    let mut memo: HashMap<_, _> = shared.into_iter().map(|node| (node, source)).collect();
    let target = copy_result(full, full_root, &mut proof, &mut memo);
    let source_nan = node(&mut proof, SymKind::Ne, &[source, source]);
    let source_bits = node(&mut proof, SymKind::Bitcast, &[source]);
    let target_bits = node(&mut proof, SymKind::Bitcast, &[target]);
    let same_bits = node(&mut proof, SymKind::Eq, &[source_bits, target_bits]);
    let mask = constant(&mut proof, width, nan_mask);
    let target_class = node(&mut proof, SymKind::And, &[target_bits, mask]);
    let quiet_nan = node(&mut proof, SymKind::Eq, &[target_class, mask]);
    node(&mut proof, SymKind::If, &[source_nan, quiet_nan, same_bits]);

    let mut always = SemGraph::new();
    constant(&mut always, 1, 1);
    let mut proof_types = vec![SemType::Float(format.clone())];
    proof_types.extend_from_slice(symbol_types);
    SmtOracle.equivalent_typed(&proof, &always, &proof_types)
}

fn rounded_kind(kind: SymKind) -> Option<SymKind> {
    Some(match kind {
        SymKind::FAdd => SymKind::FAddRound,
        SymKind::FSub => SymKind::FSubRound,
        SymKind::FMul => SymKind::FMulRound,
        SymKind::FDiv => SymKind::FDivRound,
        SymKind::Sqrt => SymKind::SqrtRound,
        SymKind::Fma => SymKind::FmaRound,
        _ => return None,
    })
}

fn quiet_nan_mask(format: &FloatFormat) -> Option<(u32, u64)> {
    match (&format.exponent, &format.mantissa) {
        (Width::Const(8), Width::Const(23)) => Some((32, 0x7fc00000)),
        (Width::Const(11), Width::Const(52)) => Some((64, 0x7ff8000000000000)),
        _ => None,
    }
}

fn copy_result(
    full: &SemGraph,
    root: NodeId,
    proof: &mut SemGraph,
    memo: &mut HashMap<NodeId, NodeId>,
) -> NodeId {
    if let Some(&copied) = memo.get(&root) {
        return copied;
    }
    let children: Vec<_> = full
        .children(root)
        .map(|child| copy_result(full, child, proof, memo))
        .collect();
    let copied = node(proof, *full.get_kind(root), &children);
    if let Some(payload) = full.get_leaf_data(root) {
        let payload = match payload {
            SymPayload::SymbolId(id) => SymPayload::SymbolId(id + 1),
            payload => payload.clone(),
        };
        proof.set_leaf_data(copied, payload);
    }
    memo.insert(root, copied);
    copied
}

fn node(graph: &mut SemGraph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = graph.add_node(kind);
    for &child in children {
        graph.add_edge(node, child);
    }
    node
}

fn constant(graph: &mut SemGraph, width: u32, value: u64) -> NodeId {
    let node = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(node, SymPayload::Int(APInt::new(width, value)));
    node
}
