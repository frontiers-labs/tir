//! Seeds the e-graph from gated SSA: each [`GateNode`] maps to a [`Node`] (op → `Node::Op`,
//! constant → `Node::Const`). The only cycle, a μ gate's latch back-edge, is broken with a placeholder.

use std::collections::HashMap;
use std::sync::Arc;

use tir_symbolic::egraph::{EGraph, Id};

use crate::analysis::{GSA, GateNode};
use crate::graph::{Dag, NodeId};
use crate::{BlockId, Commutative, ConstantLike, Context, OpId, OpInstance, ValueId};

use super::node::Node;

/// The seeded e-graph plus the driver's maps: each value's class, and each block argument's block.
pub struct Seeded {
    pub eg: EGraph<Node>,
    pub value_class: HashMap<ValueId, Id>,
    pub arg_block: HashMap<ValueId, BlockId>,
}

/// Build the e-graph for the value graph rooted at `root`.
pub fn seed(context: &Context, root: OpId, gsa: &GSA) -> Seeded {
    let mut seeder = Seeder {
        context,
        gsa,
        eg: EGraph::new(),
        id_of: HashMap::new(),
    };
    for i in 0..gsa.len() {
        seeder.seed(NodeId::from_index(i));
    }

    // The class of each value and the block of each block argument, in one IR walk.
    let mut value_class = HashMap::new();
    let mut arg_block = HashMap::new();
    let mut stack = context.get_op(root).regions.clone();
    while let Some(region) = stack.pop() {
        for block in context.get_region(region).iter(context.clone()) {
            for arg in block.arguments() {
                arg_block.insert(arg.id(), block.id());
                seeder.record(&mut value_class, arg.id());
            }
            for op_id in block.op_ids() {
                let instance = context.get_op(op_id);
                stack.extend(instance.regions.iter().copied());
                for &result in &instance.results {
                    seeder.record(&mut value_class, result);
                }
            }
        }
    }

    Seeded {
        eg: seeder.eg,
        value_class,
        arg_block,
    }
}

struct Seeder<'a> {
    context: &'a Context,
    gsa: &'a GSA,
    eg: EGraph<Node>,
    id_of: HashMap<NodeId, Id>,
}

impl Seeder<'_> {
    /// Translate `n` to its e-class, memoized.
    fn seed(&mut self, n: NodeId) -> Id {
        if let Some(&id) = self.id_of.get(&n) {
            return id;
        }
        let gate = *self.gsa.gate(n);
        let id = match gate {
            GateNode::Op(op) => self.seed_op(n, op),
            GateNode::Mu { value } => return self.seed_mu(n, value),
            GateNode::Input(_) | GateNode::Gamma { .. } | GateNode::Phi { .. } => {
                let args = self.kids(n);
                self.eg.add(Node::Gate(gate, args))
            }
        };
        self.id_of.insert(n, id);
        id
    }

    fn seed_op(&mut self, n: NodeId, op: OpId) -> Id {
        let instance = self.context.get_op(op);

        if let Some(constant) = instance.clone().as_interface::<dyn ConstantLike>() {
            return self.eg.add(Node::Const {
                value: constant.constant_value(),
                origin: Some(op),
            });
        }

        if is_pure_value(&instance) {
            let ty = self.context.get_value(instance.results[0]).ty();
            let mut args = self.kids(n);
            let commutative = instance.has_interface::<dyn Commutative>();
            if commutative {
                args.sort_by_key(|id| id.index());
            }
            return self.eg.add(Node::seeded(&instance, ty, commutative, args));
        }

        // A multi-result or effectful op is an opaque input leaf for the result this node stands for.
        let value = instance
            .results
            .iter()
            .copied()
            .find(|&r| self.gsa.node_of(r) == Some(n))
            .expect("an op node is one of its op's results");
        self.eg.add(Node::input(value))
    }

    /// μ gate: pre-register a placeholder so the latch back-edge resolves to it instead of recursing, then add the real μ and merge.
    fn seed_mu(&mut self, n: NodeId, value: ValueId) -> Id {
        let placeholder = self.eg.add(Node::input(value));
        self.id_of.insert(n, placeholder);
        let args = self.kids(n);
        let mu = self.eg.add(Node::Gate(GateNode::Mu { value }, args));
        self.eg.union(placeholder, mu);
        self.eg.rebuild();
        placeholder
    }

    /// The e-classes of `n`'s children, in edge order; collected first to release the gsa borrow before recursing.
    fn kids(&mut self, n: NodeId) -> Vec<Id> {
        let children: Vec<NodeId> = self.gsa.children(n).collect();
        children.into_iter().map(|c| self.seed(c)).collect()
    }

    /// Record `value`'s class, if it is modeled.
    fn record(&self, value_class: &mut HashMap<ValueId, Id>, value: ValueId) {
        if let Some(node) = self.gsa.node_of(value) {
            value_class.insert(value, self.id_of[&node]);
        }
    }
}

/// A pure value op the e-graph may reason about: one result, no regions, and a declared semantic expression.
fn is_pure_value(instance: &Arc<OpInstance>) -> bool {
    instance.results.len() == 1
        && instance.regions.is_empty()
        && instance
            .clone()
            .as_dyn_op()
            .semantic_expr(&mut crate::sem::SemGraph::new())
            .is_some()
}
