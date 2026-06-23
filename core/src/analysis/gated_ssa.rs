//! Prototype Gated SSA construction over block SSA.
//!
//! Block SSA encodes phis as block arguments: a predecessor forwards a value to
//! an argument through its branch (`br dest(args)`, `cond_br c, t(args), f(args)`).
//! Gated SSA makes the merge explicit by replacing each such phi with a gating
//! function (see Ottenstein, Ballance & MacCabe, "The program dependence web",
//! 1990):
//!
//! * a γ (gamma) gate selects between two inputs on a predicate, for a non-loop
//!   (if/else) merge;
//! * a μ (mu) gate merges a loop header's pre-loop input with its latched input.
//!
//! Mirroring [`DominatorTree`](super::DominatorTree), the form is built over the
//! blocks reachable from a root operation's region and exposed through the [`Dag`]
//! trait by delegating to an internal [`GenericDag`]. Operations are referenced by
//! [`OpId`]; their internals are never copied. The value graph is cyclic across μ
//! gates, so it is stored in a [`GenericDag`] rather than a [`PostOrderDag`].
//!
//! [`PostOrderDag`]: crate::graph::PostOrderDag

use std::collections::{HashMap, HashSet};

use crate::{
    BlockId, Context, OpId, Terminator, TypeId, ValueId,
    analysis::DominatorTree,
    builtin::{BranchOp, CondBranchOp},
    graph::{Dag, GenericDag, MutDag, NodeId},
};

/// A node of the gated SSA value graph. Each models the producer of one value and
/// references existing IR by id; its inputs are the node's out-edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GateNode {
    /// A value computed by an existing operation, referenced by [`OpId`]. Edges go
    /// to the producers of the operation's operands, in operand order.
    Op(OpId),
    /// A value entering the region from outside (function/entry-block argument, or
    /// a definition not modelled here). A leaf with no edges.
    Input(ValueId),
    /// γ gate replacing a non-loop block-argument phi. Edges are
    /// `[condition, true_input, false_input]`: the value taken when `cond` is
    /// true/false respectively.
    Gamma { value: ValueId, cond: ValueId },
    /// μ gate replacing a loop-header block-argument phi. Edges are
    /// `[init, latch]`: the pre-loop input and the value latched on the back edge.
    Mu { value: ValueId },
    /// An n-way phi that could not be reduced to a γ/μ gate. Edges are the incoming
    /// values in predecessor order.
    Phi { value: ValueId },
}

/// Gated SSA for the blocks reachable from a root operation's first region.
///
/// Implements [`Dag`] by delegating to an internal [`GenericDag`] whose nodes are
/// [`GateNode`]s. Build one with [`Self::new`].
pub struct GatedSsa {
    inner: GenericDag<GateNode, ()>,
    nodes: HashMap<ValueId, NodeId>,
}

impl GatedSsa {
    /// Build the gated SSA form rooted at `root`'s first region.
    pub fn new<O: Into<OpId>>(context: &Context, root: O) -> Self {
        let root = root.into();
        let blocks = region_blocks(context, root);
        let preds = predecessor_map(context, &blocks);
        let phis = collect_phis(context, &blocks, &preds);

        let mut builder = Builder {
            context,
            dom: DominatorTree::new(context, root),
            preds,
            phis,
            inner: GenericDag::new(),
            nodes: HashMap::new(),
        };

        // Materialize a node for every value defined in the region; recursion pulls
        // in operands and gate inputs.
        for &block in &blocks {
            let blk = context.get_block(block);
            for arg in blk.arguments() {
                builder.node_for_value(arg.id());
            }
            for op in blk.op_ids() {
                for &result in &context.get_op(op).results {
                    builder.node_for_value(result);
                }
            }
        }

        Self {
            inner: builder.inner,
            nodes: builder.nodes,
        }
    }

    /// The node producing `value`, if it is part of the form.
    pub fn node_of(&self, value: ValueId) -> Option<NodeId> {
        self.nodes.get(&value).copied()
    }

    /// The gate at `id`.
    pub fn gate(&self, id: NodeId) -> &GateNode {
        self.inner.get_node(id)
    }

    /// The value produced by `id`, or `None` for an [`GateNode::Op`] node (an op may
    /// have several results, so no single value identifies it).
    pub fn value_of(&self, id: NodeId) -> Option<ValueId> {
        match self.inner.get_node(id) {
            GateNode::Op(_) => None,
            GateNode::Input(v)
            | GateNode::Gamma { value: v, .. }
            | GateNode::Mu { value: v }
            | GateNode::Phi { value: v } => Some(*v),
        }
    }

    /// The operation referenced by `id`, if it is an [`GateNode::Op`] node.
    pub fn op_of(&self, id: NodeId) -> Option<OpId> {
        match self.inner.get_node(id) {
            GateNode::Op(op) => Some(*op),
            _ => None,
        }
    }
}

impl Dag for GatedSsa {
    type Node = GateNode;
    type Leaf = ();

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn get_node(&self, id: NodeId) -> &Self::Node {
        self.inner.get_node(id)
    }

    fn get_leaf_data(&self, id: NodeId) -> Option<&Self::Leaf> {
        self.inner.get_leaf_data(id)
    }

    fn get_original_op(&self, id: NodeId) -> Option<OpId> {
        self.inner.get_original_op(id)
    }

    fn get_actual_type(&self, id: NodeId) -> Option<TypeId> {
        self.inner.get_actual_type(id)
    }

    fn root(&self) -> Option<NodeId> {
        self.inner.root()
    }

    fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> {
        self.inner.children(id)
    }

    fn postorder(&self, start: NodeId) -> impl Iterator<Item = NodeId> {
        self.inner.postorder(start)
    }

    fn preorder(&self, start: NodeId) -> impl Iterator<Item = NodeId> {
        self.inner.preorder(start)
    }
}

struct Builder<'a> {
    context: &'a Context,
    dom: DominatorTree,
    /// Block-SSA predecessors reaching a block through a `br`/`cond_br`, each paired
    /// with the forwarding terminator.
    preds: HashMap<BlockId, Vec<(BlockId, OpId)>>,
    /// Block arguments that are phis (their block has branch predecessors), mapped to
    /// their block and argument index.
    phis: HashMap<ValueId, (BlockId, usize)>,
    inner: GenericDag<GateNode, ()>,
    nodes: HashMap<ValueId, NodeId>,
}

impl Builder<'_> {
    /// The node producing `value`, materializing it (and, transitively, its inputs)
    /// on first request. The node is memoized before its edges are added so back
    /// edges through μ gates resolve to it instead of recursing forever.
    fn node_for_value(&mut self, value: ValueId) -> NodeId {
        if let Some(&node) = self.nodes.get(&value) {
            return node;
        }

        if let Some(&(block, index)) = self.phis.get(&value) {
            let (gate, inputs) = self.plan_gate(block, index, value);
            let node = self.inner.add_node(gate);
            self.nodes.insert(value, node);
            for input in inputs {
                let child = self.node_for_value(input);
                self.inner.add_edge(node, child);
            }
            return node;
        }

        if let Some(op) = self.context.get_value(value).defining_op() {
            let node = self.inner.add_node(GateNode::Op(op));
            self.nodes.insert(value, node);
            for &operand in &self.context.get_op(op).operands {
                let child = self.node_for_value(operand);
                self.inner.add_edge(node, child);
            }
            return node;
        }

        let node = self.inner.add_node(GateNode::Input(value));
        self.nodes.insert(value, node);
        node
    }

    /// Classify the phi `value` (argument `index` of `block`) into a gate and list
    /// its inputs in edge order.
    fn plan_gate(&self, block: BlockId, index: usize, value: ValueId) -> (GateNode, Vec<ValueId>) {
        let incoming = self.incoming(block, index);

        // Loop header: an incoming edge comes from a block this header dominates.
        if incoming
            .iter()
            .any(|(pred, _)| self.dom.dominates(block, *pred))
        {
            let mut init = None;
            let mut latch = None;
            for &(pred, v) in &incoming {
                if self.dom.dominates(block, pred) {
                    latch.get_or_insert(v);
                } else {
                    init.get_or_insert(v);
                }
            }
            if let (Some(init), Some(latch)) = (init, latch) {
                return (GateNode::Mu { value }, vec![init, latch]);
            }
        }

        // Two-way merge gated by the condition of the dominating `cond_br`.
        if incoming.len() == 2
            && let Some((cond, true_dest, false_dest)) = self.gamma_cond(block)
        {
            let mut true_val = None;
            let mut false_val = None;
            for &(pred, v) in &incoming {
                if self.dom.dominates(true_dest, pred) {
                    true_val.get_or_insert(v);
                } else if self.dom.dominates(false_dest, pred) {
                    false_val.get_or_insert(v);
                }
            }
            if let (Some(t), Some(f)) = (true_val, false_val) {
                return (GateNode::Gamma { value, cond }, vec![cond, t, f]);
            }
        }

        let inputs = incoming.into_iter().map(|(_, v)| v).collect();
        (GateNode::Phi { value }, inputs)
    }

    /// The `(predecessor, forwarded value)` pairs feeding argument `index` of `block`.
    fn incoming(&self, block: BlockId, index: usize) -> Vec<(BlockId, ValueId)> {
        self.preds
            .get(&block)
            .into_iter()
            .flatten()
            .filter_map(|&(pred, term)| {
                forwarded_value(self.context, term, block, index).map(|v| (pred, v))
            })
            .collect()
    }

    /// The condition and successors of the `cond_br` immediately dominating `block`,
    /// if any — the predicate gating a γ merge at `block`.
    fn gamma_cond(&self, block: BlockId) -> Option<(ValueId, BlockId, BlockId)> {
        let idom = self.dom.idom(block)?;
        let term = self.context.get_block(idom).op_ids().last().copied()?;
        let cond_br = self.context.get_op(term).as_op::<CondBranchOp>()?;
        Some((
            cond_br.condition(),
            cond_br.true_dest(),
            cond_br.false_dest(),
        ))
    }
}

/// The value `term` forwards to argument `index` of successor `succ`.
fn forwarded_value(context: &Context, term: OpId, succ: BlockId, index: usize) -> Option<ValueId> {
    let instance = context.get_op(term);
    if let Some(br) = instance.clone().as_op::<BranchOp>() {
        return br.dest_args().get(index).copied();
    }
    if let Some(cond_br) = instance.as_op::<CondBranchOp>() {
        if cond_br.true_dest() == succ
            && let Some(&v) = cond_br.true_args().get(index)
        {
            return Some(v);
        }
        if cond_br.false_dest() == succ {
            return cond_br.false_args().get(index).copied();
        }
    }
    None
}

/// Every block reachable from `root`'s first region, descending into nested regions
/// just as the dominator-tree CFG builder does.
fn region_blocks(context: &Context, root: OpId) -> Vec<BlockId> {
    let entry = context
        .get_op(root)
        .regions
        .first()
        .and_then(|region| context.get_region(*region).iter(context.clone()).next())
        .map(|block| block.id());

    let mut order = Vec::new();
    let Some(entry) = entry else {
        return order;
    };

    let mut seen = HashSet::new();
    let mut stack = vec![entry];
    seen.insert(entry);

    while let Some(block_id) = stack.pop() {
        order.push(block_id);
        let op_ids = context.get_block(block_id).op_ids();
        let mut edges = Vec::new();

        for op_id in &op_ids {
            for region_id in &context.get_op(*op_id).regions {
                if let Some(child) = context.get_region(*region_id).iter(context.clone()).next() {
                    edges.push(child.id());
                }
            }
        }
        if let Some(&term) = op_ids.last() {
            let instance = context.get_op(term);
            if let Some(terminator) = instance.as_interface::<dyn Terminator>() {
                edges.extend(terminator.successors());
            }
        }

        for target in edges {
            if seen.insert(target) {
                stack.push(target);
            }
        }
    }

    order
}

/// Map each block to the `br`/`cond_br` edges (predecessor block, terminator) that
/// target it. Structured-region edges are excluded: they forward no block arguments.
fn predecessor_map(
    context: &Context,
    blocks: &[BlockId],
) -> HashMap<BlockId, Vec<(BlockId, OpId)>> {
    let mut preds: HashMap<BlockId, Vec<(BlockId, OpId)>> = HashMap::new();
    for &block in blocks {
        let Some(&term) = context.get_block(block).op_ids().last() else {
            continue;
        };
        let instance = context.get_op(term);
        if let Some(br) = instance.clone().as_op::<BranchOp>() {
            preds.entry(br.dest()).or_default().push((block, term));
        } else if let Some(cond_br) = instance.as_op::<CondBranchOp>() {
            preds
                .entry(cond_br.true_dest())
                .or_default()
                .push((block, term));
            preds
                .entry(cond_br.false_dest())
                .or_default()
                .push((block, term));
        }
    }
    preds
}

/// The block-argument values that are phis: arguments of any block with branch
/// predecessors, mapped to their block and argument index.
fn collect_phis(
    context: &Context,
    blocks: &[BlockId],
    preds: &HashMap<BlockId, Vec<(BlockId, OpId)>>,
) -> HashMap<ValueId, (BlockId, usize)> {
    let mut phis = HashMap::new();
    for &block in blocks {
        if !preds.contains_key(&block) {
            continue;
        }
        for (index, arg) in context.get_block(block).arguments().iter().enumerate() {
            phis.insert(arg.id(), (block, index));
        }
    }
    phis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Context, IRBuilder, Operand, Operation, RegionId,
        builtin::{IntegerType, UnitType, ops},
    };

    fn func_with_region(context: &Context, region: RegionId) -> OpId {
        ops::func(context, "f", UnitType::new(context), Some(region))
            .build()
            .id()
    }

    fn children(gs: &GatedSsa, node: NodeId) -> Vec<NodeId> {
        gs.children(node).collect()
    }

    #[test]
    fn gamma_for_diamond_merge() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let i32 = IntegerType::new(&context, 32);
        let cond = context.create_value(i1, None);
        let cond_id = cond.id();

        let region = context.create_region();
        let entry = context.create_block(vec![cond]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);
        let merge_arg = context.create_value(i32, None);
        let merge_arg_id = merge_arg.id();
        let merge = context.create_block(vec![merge_arg]);
        for block in [&entry, &t, &f, &merge] {
            region.add_block(block.id());
        }

        IRBuilder::new(entry.clone())
            .insert(ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build());

        let c_true = ops::constant(&context, 1, i32).build();
        let true_val = c_true.result();
        let mut tb = IRBuilder::new(t.clone());
        tb.insert(c_true);
        tb.insert(ops::br(&context, vec![true_val], merge.id()).build());

        let c_false = ops::constant(&context, 0, i32).build();
        let false_val = c_false.result();
        let mut fb = IRBuilder::new(f.clone());
        fb.insert(c_false);
        fb.insert(ops::br(&context, vec![false_val], merge.id()).build());

        IRBuilder::new(merge.clone())
            .insert(ops::r#return(&context, Operand::from(merge_arg_id)).build());

        let gs = GatedSsa::new(&context, func_with_region(&context, region.id()));

        let node = gs.node_of(merge_arg_id).unwrap();
        assert_eq!(
            *gs.gate(node),
            GateNode::Gamma {
                value: merge_arg_id,
                cond: cond_id,
            }
        );

        let kids = children(&gs, node);
        assert_eq!(kids.len(), 3);
        // Order is [condition, true_input, false_input].
        assert_eq!(gs.value_of(kids[0]), Some(cond_id));
        assert_eq!(gs.op_of(kids[1]), Some(true_val_op(&context, true_val)));
        assert_eq!(gs.op_of(kids[2]), Some(true_val_op(&context, false_val)));
    }

    fn true_val_op(context: &Context, value: ValueId) -> OpId {
        context.get_value(value).defining_op().unwrap()
    }

    #[test]
    fn mu_for_loop_header() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let i32 = IntegerType::new(&context, 32);
        let cond = context.create_value(i1, None);
        let cond_id = cond.id();

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let iv = context.create_value(i32, None);
        let iv_id = iv.id();
        let header = context.create_block(vec![iv]);
        let body = context.create_block(vec![]);
        let exit = context.create_block(vec![]);
        for block in [&entry, &header, &body, &exit] {
            region.add_block(block.id());
        }

        let init = ops::constant(&context, 0, i32).build();
        let init_val = init.result();
        let init_op = init.id();
        let mut eb = IRBuilder::new(entry.clone());
        eb.insert(init);
        eb.insert(ops::br(&context, vec![init_val], header.id()).build());

        IRBuilder::new(header.clone())
            .insert(ops::cond_br(&context, cond_id, vec![], vec![], body.id(), exit.id()).build());

        let step = ops::constant(&context, 1, i32).build();
        let step_val = step.result();
        let next = ops::addi(&context, iv_id, step_val, i32).build();
        let next_val = next.result();
        let next_op = next.id();
        let mut bb = IRBuilder::new(body.clone());
        bb.insert(step);
        bb.insert(next);
        bb.insert(ops::br(&context, vec![next_val], header.id()).build());

        IRBuilder::new(exit.clone()).insert(ops::r#return(&context, Operand::from(iv_id)).build());

        let gs = GatedSsa::new(&context, func_with_region(&context, region.id()));

        let node = gs.node_of(iv_id).unwrap();
        assert_eq!(*gs.gate(node), GateNode::Mu { value: iv_id });

        let kids = children(&gs, node);
        assert_eq!(kids.len(), 2);
        // Order is [init, latch].
        assert_eq!(gs.op_of(kids[0]), Some(init_op));
        assert_eq!(gs.op_of(kids[1]), Some(next_op));

        // The latch (`addi`) reads the μ node back, closing the loop.
        let latch_inputs = children(&gs, kids[1]);
        assert!(latch_inputs.contains(&node));
    }

    #[test]
    fn entry_arguments_are_inputs() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);
        let arg = context.create_value(i32, None);
        let arg_id = arg.id();

        let region = context.create_region();
        let entry = context.create_block(vec![arg]);
        region.add_block(entry.id());
        IRBuilder::new(entry.clone())
            .insert(ops::r#return(&context, Operand::from(arg_id)).build());

        let gs = GatedSsa::new(&context, func_with_region(&context, region.id()));

        let node = gs.node_of(arg_id).unwrap();
        assert_eq!(*gs.gate(node), GateNode::Input(arg_id));
    }
}
