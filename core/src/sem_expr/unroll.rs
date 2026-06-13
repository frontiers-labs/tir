use std::collections::HashMap;

use crate::{
    graph::{Dag, MutDag, NodeId},
    utils::APInt,
};

use super::{ExprKind, ExprPayload, ExprPostGraph};

/// Rewrite a semantic-expression graph so that every `Loop` with constant bounds
/// is replaced by its unrolled expansion: the `step` subexpression is rebuilt
/// once per iteration with `IndVar` folded to the iteration constant and `Acc`
/// wired to the previous iteration's value. Loops whose bounds are not constant
/// are copied verbatim, so a backend that cannot lower them (e.g. SMT, which has
/// no iteration) can detect and reject them.
pub fn unroll_loops(graph: &ExprPostGraph, root: NodeId) -> (ExprPostGraph, NodeId) {
    let mut out = ExprPostGraph::new();
    let mut memo: HashMap<usize, NodeId> = HashMap::new();
    let mut frames: Vec<(i64, NodeId)> = Vec::new();
    let new_root = rebuild(graph, root, &mut out, &mut memo, &mut frames);
    (out, new_root)
}

/// Evaluate a bound subexpression to a constant, resolving `IndVar` through the
/// active unrolling frames. Returns `None` for anything that is not statically
/// known (symbols, the accumulator, unsupported operators).
fn const_eval(graph: &ExprPostGraph, node: NodeId, frames: &[(i64, NodeId)]) -> Option<i64> {
    let children: Vec<NodeId> = graph.children(node).collect();
    match *graph.get_node(node) {
        ExprKind::Constant => match graph.get_leaf_data(node)? {
            ExprPayload::Int(v) => Some(v.to_i64()),
            _ => None,
        },
        ExprKind::IndVar => frames.last().map(|&(i, _)| i),
        ExprKind::Add => {
            Some(const_eval(graph, children[0], frames)? + const_eval(graph, children[1], frames)?)
        }
        ExprKind::Sub => {
            Some(const_eval(graph, children[0], frames)? - const_eval(graph, children[1], frames)?)
        }
        ExprKind::Mul => {
            Some(const_eval(graph, children[0], frames)? * const_eval(graph, children[1], frames)?)
        }
        _ => None,
    }
}

fn rebuild(
    graph: &ExprPostGraph,
    node: NodeId,
    out: &mut ExprPostGraph,
    memo: &mut HashMap<usize, NodeId>,
    frames: &mut Vec<(i64, NodeId)>,
) -> NodeId {
    // Memoize only frame-independent nodes; inside a loop expansion the same
    // source node maps to a different output node per iteration.
    let memoizable = frames.is_empty();
    if memoizable && let Some(&existing) = memo.get(&node.index()) {
        return existing;
    }

    let kind = *graph.get_node(node);
    let children: Vec<NodeId> = graph.children(node).collect();

    let result = match kind {
        ExprKind::IndVar => {
            let &(i, _) = frames.last().expect("IndVar outside a loop");
            int_const(out, i)
        }
        ExprKind::Acc => frames.last().expect("Acc outside a loop").1,
        ExprKind::Loop => {
            match (
                const_eval(graph, children[0], frames),
                const_eval(graph, children[1], frames),
            ) {
                (Some(start), Some(end)) => {
                    let mut acc = rebuild(graph, children[2], out, memo, frames);
                    for i in start..end {
                        frames.push((i, acc));
                        acc = rebuild(graph, children[3], out, memo, frames);
                        frames.pop();
                    }
                    acc
                }
                // Symbolic bounds: keep the loop intact for the backend to reject.
                _ => copy_subtree(graph, node, out, &mut HashMap::new()),
            }
        }
        _ if children.is_empty() => {
            let new = out.add_node(kind);
            if let Some(data) = graph.get_leaf_data(node) {
                out.set_leaf_data(new, data.clone());
            }
            new
        }
        _ => {
            let new_children: Vec<NodeId> = children
                .iter()
                .map(|&c| rebuild(graph, c, out, memo, frames))
                .collect();
            let new = out.add_node(kind);
            for c in new_children {
                out.add_edge(new, c);
            }
            new
        }
    };

    if memoizable {
        memo.insert(node.index(), result);
    }
    result
}

/// Structural verbatim copy of a subtree, preserving `Loop`/`IndVar`/`Acc`.
fn copy_subtree(
    graph: &ExprPostGraph,
    node: NodeId,
    out: &mut ExprPostGraph,
    memo: &mut HashMap<usize, NodeId>,
) -> NodeId {
    if let Some(&existing) = memo.get(&node.index()) {
        return existing;
    }
    // Children must exist (lower index) before the parent in a post-order graph.
    let new_children: Vec<NodeId> = graph
        .children(node)
        .collect::<Vec<_>>()
        .into_iter()
        .map(|c| copy_subtree(graph, c, out, memo))
        .collect();
    let new = out.add_node(*graph.get_node(node));
    if let Some(data) = graph.get_leaf_data(node) {
        out.set_leaf_data(new, data.clone());
    }
    for nc in new_children {
        out.add_edge(new, nc);
    }
    memo.insert(node.index(), new);
    new
}

fn int_const(out: &mut ExprPostGraph, value: i64) -> NodeId {
    let node = out.add_node(ExprKind::Constant);
    // Mirror the TMDL literal lowering: non-negative values are unsigned at their
    // minimal width; negatives keep a sign bit. A signed 1-bit `1` would be `-1`.
    let payload = if value < 0 {
        let width = 64 - value.unsigned_abs().leading_zeros() + 1;
        APInt::new_signed(width, value)
    } else {
        let v = value as u64;
        let width = if v == 0 { 1 } else { 64 - v.leading_zeros() };
        APInt::new(width, v)
    };
    out.set_leaf_data(node, ExprPayload::Int(payload));
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sem_expr::{Value, execute};

    fn sym(g: &mut ExprPostGraph, id: u32) -> NodeId {
        let node = g.add_node(ExprKind::Symbol);
        g.set_leaf_data(node, ExprPayload::SymbolId(id));
        node
    }
    fn con(g: &mut ExprPostGraph, v: i64) -> NodeId {
        let node = g.add_node(ExprKind::Constant);
        g.set_leaf_data(node, ExprPayload::Int(APInt::new_signed(64, v)));
        node
    }
    fn op(g: &mut ExprPostGraph, k: ExprKind, ch: &[NodeId]) -> NodeId {
        let node = g.add_node(k);
        for &c in ch {
            g.add_edge(node, c);
        }
        node
    }
    fn as_i64(v: Value) -> i64 {
        match v {
            Value::Int(i) => i.to_i64(),
            Value::Float(_) => panic!(),
        }
    }

    #[test]
    fn constant_loop_unrolls_and_matches_native_execution() {
        // acc = base; for i in 0..4 { acc = acc + i }  ==> base + 6.
        let mut g = ExprPostGraph::new();
        let start = con(&mut g, 0);
        let end = con(&mut g, 4);
        let base = sym(&mut g, 0);
        let acc = g.add_node(ExprKind::Acc);
        let ind = g.add_node(ExprKind::IndVar);
        let step = op(&mut g, ExprKind::Add, &[acc, ind]);
        let root = op(&mut g, ExprKind::Loop, &[start, end, base, step]);

        let (unrolled, new_root) = unroll_loops(&g, root);
        // No Loop/IndVar/Acc nodes survive a constant unroll.
        for idx in 0..unrolled.len() {
            let k = *unrolled.get_node(NodeId::from_index(idx));
            assert!(!matches!(
                k,
                ExprKind::Loop | ExprKind::IndVar | ExprKind::Acc
            ));
        }
        assert_eq!(new_root, unrolled.root().unwrap());
        assert_eq!(
            as_i64(execute(&unrolled, &[Value::Int(APInt::new_signed(64, 10))])),
            16
        );
    }

    #[test]
    fn symbolic_loop_is_kept_verbatim() {
        let mut g = ExprPostGraph::new();
        let start = con(&mut g, 0);
        let end = sym(&mut g, 0); // symbolic bound
        let init = con(&mut g, 0);
        let acc = g.add_node(ExprKind::Acc);
        let ind = g.add_node(ExprKind::IndVar);
        let step = op(&mut g, ExprKind::Add, &[acc, ind]);
        let root = op(&mut g, ExprKind::Loop, &[start, end, init, step]);

        let (out, new_root) = unroll_loops(&g, root);
        assert_eq!(*out.get_node(new_root), ExprKind::Loop);
        // Native interpretation still works on the preserved loop.
        assert_eq!(
            as_i64(execute(&out, &[Value::Int(APInt::new_signed(64, 5))])),
            10
        );
    }
}
