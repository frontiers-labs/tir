use super::{NoMemory, eval_node, integer_view};
use crate::lang::{SymKind, SymPayload, Value, scalar_op};
use tir_adt::APInt;
use tir_graph::{Dag, NodeId};

/// Evaluate a pure integer expression, returning `None` for unsupported kinds,
/// unavailable symbols, or invalid operand bounds. Uses the ordinary interpreter's
/// integer semantics after checking each operation's preconditions.
pub fn execute_pure<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    symbols: &[Value],
) -> Option<APInt> {
    let mut cache = vec![None; graph.len()];
    eval_pure_node(graph, graph.root()?, symbols, &mut cache)
}

fn eval_pure_node<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    symbols: &[Value],
    cache: &mut Vec<Option<Value>>,
) -> Option<APInt> {
    if let Some(value) = &cache[node.index()] {
        return integer_view(value.clone());
    }
    let kind = *graph.get_kind(node);
    let operands = graph
        .children(node)
        .map(|child| eval_pure_node(graph, child, symbols, cache))
        .collect::<Option<Vec<_>>>()?;
    let leaf = match (kind, graph.get_leaf_data(node)) {
        (SymKind::Symbol, Some(SymPayload::SymbolId(id))) => {
            Some(integer_view(symbols.get(*id as usize)?.clone())?)
        }
        (SymKind::Constant, Some(SymPayload::Int(value))) => Some(value.clone()),
        _ => None,
    };
    let value = if let Some(value) = leaf {
        if !operands.is_empty() {
            return None;
        }
        value
    } else {
        if !valid_operands(kind, &operands) {
            return None;
        }
        let value = match eval_node(graph, node, symbols, cache, &mut Vec::new(), &mut NoMemory) {
            Ok(value) => value,
            Err(error) => match error {},
        };
        integer_view(value)?
    };
    cache[node.index()] = Some(Value::Int(value.clone()));
    Some(value)
}

fn valid_operands(kind: SymKind, operands: &[APInt]) -> bool {
    if let Some(op) = scalar_op(kind) {
        return operands.len() == op.arity
            && (kind != SymKind::Concat
                || operands[0]
                    .width()
                    .checked_add(operands[1].width())
                    .is_some());
    }
    match (kind, operands) {
        (SymKind::Bitcast | SymKind::Log2Ceil, [_]) => true,
        (SymKind::If, [condition, _, _]) => condition.width() == 1,
        (SymKind::Clamp, [value, min, max]) => {
            value.width() == min.width() && value.width() == max.width()
        }
        (SymKind::SExt | SymKind::ZExt, [value, width]) => {
            let width = width.to_u64();
            width >= u64::from(value.width()) && u32::try_from(width).is_ok()
        }
        (SymKind::Extract, [value, high, low]) => {
            low.to_u64() <= high.to_u64() && high.to_u64() < u64::from(value.width())
        }
        _ => false,
    }
}
