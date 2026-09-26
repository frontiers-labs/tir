//! Shared helpers for the tir-symbolic unit tests.

use tir_adt::{APInt, Dag, NodeId};
use tir_symbolic::lang::{SymKind, SymPayload};

/// Deterministic PRNG so randomized tests are reproducible without a dependency.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub type Graph = Dag<SymKind, SymPayload<()>>;

pub fn sym(g: &mut Graph, id: u32) -> NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}

pub fn con(g: &mut Graph, width: u32, value: u64) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(width, value)));
    node
}

pub fn signed_con(g: &mut Graph, width: u32, value: i64) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new_signed(width, value)));
    node
}

pub fn arg(g: &mut Graph, index: u64) -> NodeId {
    let node = g.add_node(SymKind::Arg);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(32, index)));
    node
}

pub fn op(g: &mut Graph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = g.add_node(kind);
    for &child in children {
        g.add_edge(node, child);
    }
    node
}
