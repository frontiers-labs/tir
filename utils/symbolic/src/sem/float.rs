//! Float-comparison semantics, shared by the IR's `cmpf` operation and by
//! backend flag composition so both prove against the very same graph.

use tir_adt::Predicate;
use tir_graph::{MutDag, NodeId};

use super::ValueId;
use crate::lang::{SymKind, SymPayload};

/// The semantic-graph builder [`cmpf_semantics`] writes into.
trait SemBuilder: MutDag<Node = SymKind, Leaf = SymPayload<ValueId>> {}

impl<T> SemBuilder for T where T: MutDag<Node = SymKind, Leaf = SymPayload<ValueId>> {}

/// Build the target-independent semantic graph for a `cmpf` predicate.
pub fn cmpf_semantics(
    g: &mut impl MutDag<Node = SymKind, Leaf = SymPayload<ValueId>>,
    predicate: Predicate,
) -> Option<NodeId> {
    let lhs = symbol(g, 0);
    let rhs = symbol(g, 1);
    Some(match predicate {
        Predicate::Oeq => ordered_equal(g, lhs, rhs),
        Predicate::Une => {
            let equal = ordered_equal(g, lhs, rhs);
            let one = g.add_node(SymKind::Constant);
            g.set_leaf_data(one, SymPayload::Int(tir_adt::APInt::new(1, 1)));
            binary(g, SymKind::Xor, equal, one)
        }
        Predicate::Olt => binary(g, SymKind::Lt, lhs, rhs),
        Predicate::Ogt => binary(g, SymKind::Lt, rhs, lhs),
        Predicate::Oge => binary(g, SymKind::Ge, lhs, rhs),
        Predicate::Ole => binary(g, SymKind::Ge, rhs, lhs),
        _ => return None,
    })
}

/// Ordered equality without an atomic float `eq`: both `>=` directions hold,
/// which is false whenever either operand is NaN.
fn ordered_equal(g: &mut impl SemBuilder, lhs: NodeId, rhs: NodeId) -> NodeId {
    let left_ge = binary(g, SymKind::Ge, lhs, rhs);
    let right_ge = binary(g, SymKind::Ge, rhs, lhs);
    binary(g, SymKind::And, left_ge, right_ge)
}

fn symbol(g: &mut impl SemBuilder, index: u32) -> NodeId {
    let leaf = g.add_node(SymKind::Symbol);
    g.set_leaf_data(leaf, SymPayload::SymbolId(index));
    leaf
}

fn binary(g: &mut impl SemBuilder, kind: SymKind, lhs: NodeId, rhs: NodeId) -> NodeId {
    let node = g.add_node(kind);
    g.add_edge(node, lhs);
    g.add_edge(node, rhs);
    node
}
