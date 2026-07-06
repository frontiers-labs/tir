use std::collections::{HashMap, HashSet};

use tir_adt::APInt;
use tir_graph::{Dag, GenericDag, MutDag, NodeId};

use crate::lang::{SymKind, SymPayload};

/// Infer each node's integer bit-width bottom-up; `leaf_width` supplies `Symbol`
/// widths, `None` means unknown and propagates. Relies on children having lower
/// indices than parents (holds for post-order graphs); result indexed by node index.
pub fn infer_widths<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    leaf_width: impl Fn(NodeId) -> Option<u32>,
) -> Vec<Option<u32>> {
    let count = graph.len();
    let mut widths = vec![None; count];

    for index in 0..count {
        let id = NodeId::from_index(index);
        let children: Vec<NodeId> = graph.children(id).collect();
        let child_width = |slot: usize| children.get(slot).and_then(|c| widths[c.index()]);

        let width = match *graph.get_kind(id) {
            SymKind::Symbol => leaf_width(id),
            SymKind::Constant => match graph.get_leaf_data(id) {
                Some(SymPayload::<V>::Int(value)) => Some(value.width()),
                _ => None,
            },

            // Arithmetic/logic/shifts: as wide as the left input.
            SymKind::Add
            | SymKind::Sub
            | SymKind::Mul
            | SymKind::Div
            | SymKind::UDiv
            | SymKind::SRem
            | SymKind::URem
            | SymKind::Neg
            | SymKind::And
            | SymKind::Or
            | SymKind::Xor
            | SymKind::ShiftLeft
            | SymKind::ShiftRightLogic
            | SymKind::ShiftRightArithmetic
            | SymKind::Not
            | SymKind::Clamp
            | SymKind::Log2Ceil
            | SymKind::Sqrt
            | SymKind::Fma
            | SymKind::FAdd
            | SymKind::FSub
            | SymKind::FMul
            | SymKind::FDiv => child_width(0),

            SymKind::Eq
            | SymKind::Ne
            | SymKind::Lt
            | SymKind::Le
            | SymKind::Gt
            | SymKind::Ge
            | SymKind::ULt
            | SymKind::ULe
            | SymKind::UGt
            | SymKind::UGe => Some(1),

            SymKind::Concat => match (child_width(0), child_width(1)) {
                (Some(hi), Some(lo)) => Some(hi + lo),
                _ => None,
            },

            // As wide as its arms (the then-branch).
            SymKind::If => child_width(1),

            SymKind::Extract => {
                match (
                    children.get(1).and_then(|&c| const_u64(graph, c)),
                    children.get(2).and_then(|&c| const_u64(graph, c)),
                ) {
                    (Some(high), Some(low)) if high >= low => Some((high - low + 1) as u32),
                    _ => None,
                }
            }

            SymKind::SExt | SymKind::ZExt => children
                .get(1)
                .and_then(|&c| const_u64(graph, c))
                .map(|w| w as u32),

            SymKind::LoadMemory => children
                .get(1)
                .and_then(|&c| const_u64(graph, c))
                .map(|bytes| (bytes as u32) * 8),
            SymKind::StoreMemory => None,

            SymKind::LoadReserved => children
                .get(1)
                .and_then(|&c| const_u64(graph, c))
                .map(|bytes| (bytes as u32) * 8),
            SymKind::AtomicRmw => children
                .get(2)
                .and_then(|&c| const_u64(graph, c))
                .map(|bytes| (bytes as u32) * 8),
            SymKind::StoreConditional => Some(1),
            SymKind::Fence => None,

            // No scalar width: element widths come from the runtime value, not structure.
            SymKind::Map
            | SymKind::Zip
            | SymKind::IterConcat
            | SymKind::Split
            | SymKind::Reduce
            | SymKind::Arg => None,
        };

        widths[index] = width;
    }

    widths
}

/// Rewrite a behavior-derived pattern into the form isel matches against, returning
/// the new graph, root, and forced widths (indexed by new node index). The rewrites
/// (see `canon_rebuild`) only simplify the selection pattern, never execution semantics.
pub fn canonicalize_for_selection<V: Clone>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    root: NodeId,
    immediate_symbols: &HashSet<u32>,
) -> (GenericDag<SymKind, SymPayload<V>>, NodeId, Vec<Option<u32>>) {
    let mut out = GenericDag::new();
    let mut memo: HashMap<usize, NodeId> = HashMap::new();
    let mut forced: HashMap<usize, u32> = HashMap::new();
    let new_root = canon_rebuild(
        graph,
        root,
        immediate_symbols,
        &mut out,
        &mut memo,
        &mut forced,
    );

    let mut widths = vec![None; out.len()];
    for (index, width) in forced {
        if let Some(slot) = widths.get_mut(index) {
            *slot = Some(width);
        }
    }
    (out, new_root, widths)
}

fn const_u64<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
) -> Option<u64> {
    match graph.get_leaf_data(node)? {
        SymPayload::Int(v) => Some(v.to_u64()),
        _ => None,
    }
}

fn is_shift(kind: SymKind) -> bool {
    matches!(
        kind,
        SymKind::ShiftLeft | SymKind::ShiftRightLogic | SymKind::ShiftRightArithmetic
    )
}

fn extract_from_zero_hi<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
) -> Option<(NodeId, u64)> {
    if *graph.get_node(node) != SymKind::Extract {
        return None;
    }
    let children: Vec<NodeId> = graph.children(node).collect();
    if children.len() != 3 || const_u64(graph, children[2]) != Some(0) {
        return None;
    }
    const_u64(graph, children[1]).map(|hi| (children[0], hi))
}

fn is_immediate_leaf<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    immediate_symbols: &HashSet<u32>,
) -> bool {
    *graph.get_node(node) == SymKind::Symbol
        && matches!(
            graph.get_leaf_data(node),
            Some(SymPayload::SymbolId(id)) if immediate_symbols.contains(id)
        )
}

fn canon_rebuild<V: Clone>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    immediate_symbols: &HashSet<u32>,
    out: &mut GenericDag<SymKind, SymPayload<V>>,
    memo: &mut HashMap<usize, NodeId>,
    forced: &mut HashMap<usize, u32>,
) -> NodeId {
    if let Some(&existing) = memo.get(&node.index()) {
        return existing;
    }

    let kind = *graph.get_node(node);
    let children: Vec<NodeId> = graph.children(node).collect();

    // Collapse `sext/zext(load/imm, XLEN)` to the bare load or immediate: source IR
    // types the load result and carries constants at use width, rather than wrapping.
    if matches!(kind, SymKind::SExt | SymKind::ZExt)
        && children.len() == 2
        && (*graph.get_node(children[0]) == SymKind::LoadMemory
            || is_immediate_leaf(graph, children[0], immediate_symbols))
    {
        let inner = canon_rebuild(graph, children[0], immediate_symbols, out, memo, forced);
        memo.insert(node.index(), inner);
        return inner;
    }

    // SExt(Extract(inner, hi, 0), _) -> inner forced to width hi+1.
    if kind == SymKind::SExt
        && children.len() == 2
        && let Some((source, hi)) = extract_from_zero_hi(graph, children[0])
    {
        let inner = canon_rebuild(graph, source, immediate_symbols, out, memo, forced);
        forced.insert(inner.index(), (hi + 1) as u32);
        memo.insert(node.index(), inner);
        return inner;
    }

    // Shift-amount mask strip (mask is implicit in the encoding):
    //   Shift(v, Extract(amt, k, 0)) / Shift(v, Clamp(amt, _, _)) -> Shift(v, amt)
    if is_shift(kind) && children.len() == 2 {
        let value = canon_rebuild(graph, children[0], immediate_symbols, out, memo, forced);
        let amount = {
            let src = children[1];
            let stripped = match *graph.get_node(src) {
                SymKind::Extract => {
                    let ec: Vec<NodeId> = graph.children(src).collect();
                    (ec.len() == 3 && const_u64(graph, ec[2]) == Some(0)).then_some(ec[0])
                }
                SymKind::Clamp => graph.children(src).next(),
                _ => None,
            };
            canon_rebuild(
                graph,
                stripped.unwrap_or(src),
                immediate_symbols,
                out,
                memo,
                forced,
            )
        };
        let new_node = out.add_node(kind);
        out.add_edge(new_node, value);
        out.add_edge(new_node, amount);
        memo.insert(node.index(), new_node);
        return new_node;
    }

    // Normalize the load's metadata child to 0 so source IR loads match both signed
    // and unsigned target forms; signedness lives in the surrounding SExt/ZExt.
    if kind == SymKind::LoadMemory && children.len() == 3 {
        let address = canon_rebuild(graph, children[0], immediate_symbols, out, memo, forced);
        let bytes = canon_rebuild(graph, children[1], immediate_symbols, out, memo, forced);
        let zero = out.add_node(SymKind::Constant);
        out.set_leaf_data(zero, SymPayload::Int(APInt::new(1, 0)));
        let new_node = out.add_node(kind);
        out.add_edge(new_node, address);
        out.add_edge(new_node, bytes);
        out.add_edge(new_node, zero);
        memo.insert(node.index(), new_node);
        return new_node;
    }

    // Collapse the explicit store truncation `extract(rs, 31, 0)` to the inner value,
    // forcing its width; source IR already carries the stored value's width.
    if kind == SymKind::StoreMemory && children.len() == 4 {
        let address = canon_rebuild(graph, children[0], immediate_symbols, out, memo, forced);
        let bytes = canon_rebuild(graph, children[1], immediate_symbols, out, memo, forced);
        let value_src = children[2];
        let value = if let Some((source, hi)) = extract_from_zero_hi(graph, value_src) {
            let inner = canon_rebuild(graph, source, immediate_symbols, out, memo, forced);
            forced.insert(inner.index(), (hi + 1) as u32);
            inner
        } else {
            canon_rebuild(graph, value_src, immediate_symbols, out, memo, forced)
        };
        let address_space = canon_rebuild(graph, children[3], immediate_symbols, out, memo, forced);
        let new_node = out.add_node(kind);
        out.add_edge(new_node, address);
        out.add_edge(new_node, bytes);
        out.add_edge(new_node, value);
        out.add_edge(new_node, address_space);
        memo.insert(node.index(), new_node);
        return new_node;
    }

    // Default: copy leaves, rebuild operations from canonicalized children.
    let new_node = if children.is_empty() {
        let new_node = out.add_node(kind);
        if let Some(data) = graph.get_leaf_data(node) {
            out.set_leaf_data(new_node, data.clone());
        }
        new_node
    } else {
        let new_children: Vec<NodeId> = children
            .iter()
            .map(|&child| canon_rebuild(graph, child, immediate_symbols, out, memo, forced))
            .collect();
        let new_node = out.add_node(kind);
        for child in new_children {
            out.add_edge(new_node, child);
        }
        new_node
    };
    memo.insert(node.index(), new_node);
    new_node
}
