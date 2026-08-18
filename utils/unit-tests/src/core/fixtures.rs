//! Fixtures shared by the core test modules.

use tir::backend::regalloc::{RegClassId, RegClassInfo, RegisterInfo, RegisterView};
use tir::graph::{MutDag, NodeId};
use tir::sem::{SemGraph, SymKind, SymPayload};
use tir_adt::APInt;

/// A single eight-register class `R` over its own file, the shared
/// register-class fixture for the regalloc, liveness and encoding tests.
pub static R_CLASSES: [RegClassInfo; 1] = [RegClassInfo {
    name: "R",
    file: "R",
    registers: &[0, 1, 2, 3, 4, 5, 6, 7],
    group_width: 1,
    view: RegisterView {
        bit_offset: 0,
        merge: false,
    },
}];

pub const fn r() -> RegClassId {
    RegClassId::new(&R_CLASSES[0])
}

pub fn register_info() -> RegisterInfo {
    RegisterInfo {
        classes: &R_CLASSES,
    }
}

/// Isel pattern-graph builders: symbols, constants and operator nodes.
pub fn symbol(g: &mut SemGraph, id: u32) -> NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}

pub fn constant(g: &mut SemGraph, value: u64, width: u32) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(width, value)));
    node
}

pub fn nary(g: &mut SemGraph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = g.add_node(kind);
    for &child in children {
        g.add_edge(node, child);
    }
    node
}

pub fn binary(g: &mut SemGraph, kind: SymKind, lhs: NodeId, rhs: NodeId) -> NodeId {
    nary(g, kind, &[lhs, rhs])
}

/// A one-operator pattern over two fresh symbols.
pub fn atomic_pattern(kind: SymKind) -> SemGraph {
    let mut g = SemGraph::new();
    let lhs = symbol(&mut g, 0);
    let rhs = symbol(&mut g, 1);
    binary(&mut g, kind, lhs, rhs);
    g
}
