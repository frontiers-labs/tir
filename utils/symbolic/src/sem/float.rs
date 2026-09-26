//! Float-comparison semantics, shared by the IR's `fp.cmp` operation and by
//! backend flag composition so both prove against the very same graph.

use tir_adt::{NodeId, Predicate};

use super::SemGraph;
use crate::lang::{SymKind, SymPayload};

/// Build the target-independent semantic graph for an `fp.cmp` predicate.
pub fn cmpf_semantics<A>(g: &mut SemGraph<A>, predicate: Predicate) -> Option<NodeId> {
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
fn ordered_equal<A>(g: &mut SemGraph<A>, lhs: NodeId, rhs: NodeId) -> NodeId {
    let left_ge = binary(g, SymKind::Ge, lhs, rhs);
    let right_ge = binary(g, SymKind::Ge, rhs, lhs);
    binary(g, SymKind::And, left_ge, right_ge)
}

fn symbol<A>(g: &mut SemGraph<A>, index: u32) -> NodeId {
    let leaf = g.add_node(SymKind::Symbol);
    g.set_leaf_data(leaf, SymPayload::SymbolId(index));
    leaf
}

fn binary<A>(g: &mut SemGraph<A>, kind: SymKind, lhs: NodeId, rhs: NodeId) -> NodeId {
    let node = g.add_node(kind);
    g.add_edge(node, lhs);
    g.add_edge(node, rhs);
    node
}
