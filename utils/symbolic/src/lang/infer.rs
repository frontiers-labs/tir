use std::collections::{HashMap, HashSet};

use tir_adt::APInt;
use tir_graph::{Dag, GenericDag, MutDag, NodeId};

use crate::lang::types::TypeUnifier;
use crate::lang::{SemType, SymKind, SymPayload, TypeError, Width, WidthRule, scalar_op};

/// Infer semantic value types by instantiating each operator's polymorphic
/// signature and unifying it with the types of its operands. `seed` supplies
/// externally known types, normally the IR types of symbol leaves and roots.
pub fn infer_types<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    seed: impl Fn(NodeId) -> Option<SemType>,
) -> Result<Vec<SemType>, TypeError> {
    let mut inference = TypeUnifier::default();
    let mut types: Vec<SemType> = Vec::with_capacity(graph.len());

    for index in 0..graph.len() {
        let node = NodeId::from_index(index);
        let children: Vec<NodeId> = graph.children(node).collect();
        let child = |slot: usize| types[children[slot].index()].clone();
        let kind = *graph.get_kind(node);
        let inferred = match kind {
            SymKind::Symbol | SymKind::Arg => inference.fresh_type(),
            SymKind::Constant => inference.fresh_bits(),
            SymKind::FAdd | SymKind::FSub | SymKind::FMul | SymKind::FDiv => {
                let ty = inference.fresh_float();
                inference.unify(&child(0), &ty)?;
                inference.unify(&child(1), &ty)?;
                ty
            }
            SymKind::Eq
            | SymKind::Ne
            | SymKind::Lt
            | SymKind::Le
            | SymKind::Gt
            | SymKind::Ge
            | SymKind::ULt
            | SymKind::ULe
            | SymKind::UGt
            | SymKind::UGe => {
                let operand = inference.fresh_bits();
                inference.unify(&child(0), &operand)?;
                inference.unify(&child(1), &operand)?;
                SemType::bits(1)
            }
            SymKind::Add
            | SymKind::Sub
            | SymKind::Mul
            | SymKind::Div
            | SymKind::UDiv
            | SymKind::SRem
            | SymKind::URem
            | SymKind::Or
            | SymKind::And
            | SymKind::Xor
            | SymKind::Xnor => {
                let operand = inference.fresh_bits();
                inference.unify(&child(0), &operand)?;
                inference.unify(&child(1), &operand)?;
                operand
            }
            SymKind::Neg | SymKind::Not => {
                let operand = inference.fresh_bits();
                inference.unify(&child(0), &operand)?;
                operand
            }
            SymKind::ShiftLeft | SymKind::ShiftRightArithmetic | SymKind::ShiftRightLogic => {
                let value = inference.fresh_bits();
                let amount = inference.fresh_bits();
                inference.unify(&child(0), &value)?;
                inference.unify(&child(1), &amount)?;
                value
            }
            SymKind::Concat => {
                let lhs = inference.fresh_width();
                let rhs = inference.fresh_width();
                inference.unify(&child(0), &SemType::Bits(lhs.clone()))?;
                inference.unify(&child(1), &SemType::Bits(rhs.clone()))?;
                SemType::Bits(Width::Add(Box::new(lhs), Box::new(rhs)))
            }
            SymKind::Bitcast => {
                let width = inference.fresh_width();
                inference.unify(&child(0), &SemType::RawBits(width.clone()))?;
                SemType::RawBits(width)
            }
            SymKind::If => {
                inference.unify(&child(0), &SemType::bits(1))?;
                let result = inference.fresh_type();
                inference.unify(&child(1), &result)?;
                inference.unify(&child(2), &result)?;
                result
            }
            SymKind::SExt | SymKind::ZExt => {
                let value = inference.fresh_bits();
                let width = inference.fresh_bits();
                inference.unify(&child(0), &value)?;
                inference.unify(&child(1), &width)?;
                const_u64(graph, children[1])
                    .map(|width| SemType::bits(width as u32))
                    .unwrap_or_else(|| inference.fresh_bits())
            }
            SymKind::Extract => {
                let value = inference.fresh_bits();
                inference.unify(&child(0), &value)?;
                for slot in 1..3 {
                    let bound = inference.fresh_bits();
                    inference.unify(&child(slot), &bound)?;
                }
                match (const_u64(graph, children[1]), const_u64(graph, children[2])) {
                    (Some(high), Some(low)) if high >= low => {
                        SemType::bits((high - low + 1) as u32)
                    }
                    _ => inference.fresh_bits(),
                }
            }
            SymKind::Clamp | SymKind::Log2Ceil | SymKind::Sqrt => child(0),
            SymKind::Fma => {
                let ty = inference.fresh_float();
                for slot in 0..3 {
                    inference.unify(&child(slot), &ty)?;
                }
                ty
            }
            SymKind::LoadMemory | SymKind::LoadReserved => const_u64(graph, children[1])
                .map(|bytes| SemType::RawBits(Width::Const(bytes as u32 * 8)))
                .unwrap_or_else(|| SemType::RawBits(inference.fresh_width())),
            SymKind::StoreConditional => SemType::bits(1),
            SymKind::AtomicRmw => const_u64(graph, children[2])
                .map(|bytes| SemType::bits(bytes as u32 * 8))
                .unwrap_or_else(|| inference.fresh_bits()),
            SymKind::StoreMemory | SymKind::Fence => SemType::Unit,
            SymKind::Split => SemType::Iterator(Box::new(inference.fresh_type())),
            SymKind::Zip => {
                let lhs = inference.fresh_type();
                let rhs = inference.fresh_type();
                inference.unify(&child(0), &SemType::Iterator(Box::new(lhs.clone())))?;
                inference.unify(&child(1), &SemType::Iterator(Box::new(rhs.clone())))?;
                SemType::Iterator(Box::new(SemType::Pair(Box::new(lhs), Box::new(rhs))))
            }
            SymKind::Map => {
                let element = inference.fresh_type();
                inference.unify(&child(0), &SemType::Iterator(Box::new(element)))?;
                SemType::Iterator(Box::new(child(1)))
            }
            SymKind::IterConcat => inference.fresh_bits(),
            SymKind::Reduce => child(1),
            SymKind::StateAssign
            | SymKind::StateStore
            | SymKind::StateStoreConditional
            | SymKind::StateFence
            | SymKind::StateTrap
            | SymKind::StateBlock
            | SymKind::StateIf
            | SymKind::StateTry
            | SymKind::StateHandler => SemType::State,
        };
        if let Some(expected) = seed(node) {
            inference.unify(&inferred, &expected)?;
        }
        types.push(inferred);
    }

    Ok(types.iter().map(|ty| inference.resolve(ty)).collect())
}

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

        let kind = *graph.get_kind(id);
        let width = if let Some(op) = scalar_op(kind) {
            match op.width {
                WidthRule::First => child_width(0),
                WidthRule::Bool => Some(1),
                WidthRule::Sum => match (child_width(0), child_width(1)) {
                    (Some(lhs), Some(rhs)) => Some(lhs + rhs),
                    _ => None,
                },
            }
        } else {
            match kind {
                SymKind::Symbol => leaf_width(id),
                SymKind::Constant => match graph.get_leaf_data(id) {
                    Some(SymPayload::<V>::Int(value)) => Some(value.width()),
                    _ => None,
                },

                SymKind::Clamp
                | SymKind::Bitcast
                | SymKind::Log2Ceil
                | SymKind::Sqrt
                | SymKind::Fma
                | SymKind::FAdd
                | SymKind::FSub
                | SymKind::FMul
                | SymKind::FDiv => child_width(0),

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
                _ => unreachable!("operator has no width rule"),
            }
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

/// The shift-amount operand's source with its implicit encoding mask stripped:
/// `Extract(amt, k, 0)` / `Clamp(amt, _, _)` -> `amt` (the shift encoding masks
/// the amount, so the mask is redundant for matching).
fn shift_amount_src<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    src: NodeId,
) -> NodeId {
    match *graph.get_node(src) {
        SymKind::Extract => {
            let ec: Vec<NodeId> = graph.children(src).collect();
            if ec.len() == 3 && const_u64(graph, ec[2]) == Some(0) {
                ec[0]
            } else {
                src
            }
        }
        SymKind::Clamp => graph.children(src).next().unwrap_or(src),
        _ => src,
    }
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

/// Whether `node` is a low slice `Extract(x, hi, 0)` (lo == 0), a re-view of the
/// low bits of `x`. The hi bound may be constant or symbolic (`XLEN - 1`).
fn is_low_extract<V>(graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>, node: NodeId) -> bool {
    *graph.get_node(node) == SymKind::Extract && {
        let children: Vec<NodeId> = graph.children(node).collect();
        children.len() == 3 && const_u64(graph, children[2]) == Some(0)
    }
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

    // SExt(shr(Extract(v, hi, 0), amt), _) -> shr(v, amt) forced to width hi+1:
    // a word right shift computes the low hi+1 bits of its narrowed operand,
    // held sign-extended in the register (the register form), so matching sees
    // the narrow shift — the mirror of the `SExt(Extract(shl ...))` collapse
    // that covers the word left shift (`sllw`), whose extract sits outside.
    if kind == SymKind::SExt
        && children.len() == 2
        && matches!(
            *graph.get_node(children[0]),
            SymKind::ShiftRightLogic | SymKind::ShiftRightArithmetic
        )
    {
        let shift = children[0];
        let sc: Vec<NodeId> = graph.children(shift).collect();
        if sc.len() == 2
            && let Some((value_src, hi)) = extract_from_zero_hi(graph, sc[0])
        {
            let value = canon_rebuild(graph, value_src, immediate_symbols, out, memo, forced);
            forced.insert(value.index(), (hi + 1) as u32);
            let amount = canon_rebuild(
                graph,
                shift_amount_src(graph, sc[1]),
                immediate_symbols,
                out,
                memo,
                forced,
            );
            let new_node = out.add_node(*graph.get_node(shift));
            out.add_edge(new_node, value);
            out.add_edge(new_node, amount);
            forced.insert(new_node.index(), (hi + 1) as u32);
            memo.insert(node.index(), new_node);
            return new_node;
        }
    }

    // Extract(x, hi, 0) -> x (the value model: a low slice is the low hi+1 bits
    // of x, so the pattern matches the narrow value directly) — e.g. arm64 `mul`
    // = `extract(rn * rm, XLEN - 1, 0)` roots the same-width `Mul`. A constant hi
    // forces the width; a symbolic hi (`XLEN - 1`, a full-width identity) leaves
    // it to inference. A high slice (`lo > 0`, e.g. `smulh`) is left intact.
    //
    // Sound today because every symbolic-hi slice in the model is a full-width
    // identity (hi + 1 == the register width), so dropping it changes nothing.
    // It would be unsound for a symbolic *partial* slice — e.g. a hypothetical
    // `extract(x, vl - 1, 0)` with `vl` a dynamic sub-width — which this would
    // mis-collapse to the full value; add a width guard here if such a behavior
    // is introduced.
    if is_low_extract(graph, node) {
        let mut ch = graph.children(node);
        let source = ch.next().expect("extract has a value operand");
        let hi = const_u64(graph, ch.next().expect("extract has a hi operand"));
        let inner = canon_rebuild(graph, source, immediate_symbols, out, memo, forced);
        if let Some(hi) = hi {
            forced.insert(inner.index(), (hi + 1) as u32);
        }
        memo.insert(node.index(), inner);
        return inner;
    }

    // Shift-amount mask strip (mask is implicit in the encoding):
    //   Shift(v, Extract(amt, k, 0)) / Shift(v, Clamp(amt, _, _)) -> Shift(v, amt)
    if is_shift(kind) && children.len() == 2 {
        let value = canon_rebuild(graph, children[0], immediate_symbols, out, memo, forced);
        let amount = canon_rebuild(
            graph,
            shift_amount_src(graph, children[1]),
            immediate_symbols,
            out,
            memo,
            forced,
        );
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

#[cfg(test)]
mod type_tests {
    use tir_graph::{GenericDag, MutDag};

    use super::*;
    use crate::lang::{FloatFormat, SemType, infer_types};

    type Graph = GenericDag<SymKind, SymPayload<()>>;

    fn binary(kind: SymKind) -> (Graph, NodeId, NodeId, NodeId) {
        let mut graph = Graph::new();
        let lhs = graph.add_node(SymKind::Symbol);
        graph.set_leaf_data(lhs, SymPayload::SymbolId(0));
        let rhs = graph.add_node(SymKind::Symbol);
        graph.set_leaf_data(rhs, SymPayload::SymbolId(1));
        let root = graph.add_node(kind);
        graph.add_edge(root, lhs);
        graph.add_edge(root, rhs);
        (graph, lhs, rhs, root)
    }

    #[test]
    fn integer_binary_operation_is_width_polymorphic() {
        let (graph, lhs, rhs, root) = binary(SymKind::Add);
        let types = infer_types(&graph, |node| {
            (node == lhs || node == rhs).then(|| SemType::bits(32))
        })
        .unwrap();

        assert_eq!(types[root.index()], SemType::bits(32));
    }

    #[test]
    fn integer_binary_operation_rejects_mixed_widths() {
        let (graph, lhs, rhs, _) = binary(SymKind::Add);
        let error = infer_types(&graph, |node| {
            if node == lhs {
                Some(SemType::bits(32))
            } else if node == rhs {
                Some(SemType::bits(64))
            } else {
                None
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("width mismatch"));
    }

    #[test]
    fn float_operation_preserves_the_operand_format() {
        let (graph, lhs, rhs, root) = binary(SymKind::FAdd);
        let f32 = SemType::Float(FloatFormat::new(8, 23));
        let types = infer_types(&graph, |node| {
            (node == lhs || node == rhs).then(|| f32.clone())
        })
        .unwrap();

        assert_eq!(types[root.index()], f32);
    }

    #[test]
    fn bitcast_accepts_a_float_and_preserves_its_bit_width() {
        let mut graph = Graph::new();
        let input = graph.add_node(SymKind::Symbol);
        graph.set_leaf_data(input, SymPayload::SymbolId(0));
        let root = graph.add_node(SymKind::Bitcast);
        graph.add_edge(root, input);

        let types = infer_types(&graph, |node| {
            (node == input).then(|| SemType::Float(FloatFormat::new(8, 23)))
        })
        .unwrap();

        assert_eq!(types[root.index()], SemType::raw_bits(32));
    }

    #[test]
    fn raw_memory_bits_admit_a_float_interpretation() {
        let mut graph = Graph::new();
        let address = graph.add_node(SymKind::Symbol);
        graph.set_leaf_data(address, SymPayload::SymbolId(0));
        let bytes = graph.add_node(SymKind::Constant);
        graph.set_leaf_data(bytes, SymPayload::Int(APInt::new(8, 4)));
        let metadata = graph.add_node(SymKind::Constant);
        graph.set_leaf_data(metadata, SymPayload::Int(APInt::new(1, 0)));
        let load = graph.add_node(SymKind::LoadMemory);
        graph.add_edge(load, address);
        graph.add_edge(load, bytes);
        graph.add_edge(load, metadata);

        let types = infer_types(&graph, |_| None).unwrap();
        assert_eq!(types[load.index()], SemType::RawBits(Width::Const(32)));

        let mut unifier = TypeUnifier::default();
        unifier
            .unify(
                &types[load.index()],
                &SemType::Float(FloatFormat::new(8, 23)),
            )
            .unwrap();
    }
}
