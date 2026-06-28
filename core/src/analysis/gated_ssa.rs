use std::collections::{HashMap, HashSet};

use crate::{
    BlockId, BranchGuard, BranchTerminator, Context, LoopLike, OpId, RegionGuard, RegionId,
    Terminator, TypeId, ValueId,
    analysis::DominatorTree,
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
pub struct GSA {
    inner: GenericDag<GateNode, ()>,
    nodes: HashMap<ValueId, NodeId>,
}

impl GSA {
    /// Build the gated SSA form rooted at `root`'s first region.
    pub fn new<O: Into<OpId>>(context: &Context, root: O) -> Self {
        let root = root.into();
        let blocks = region_blocks(context, root);
        let preds = predecessor_map(context, &blocks);
        let phis = collect_phis(context, &blocks, &preds);
        let StructuredGates {
            gamma,
            mu,
            loop_result,
        } = structured_gates(context, &blocks);

        let mut builder = Builder {
            context,
            dom: DominatorTree::new(context, root),
            preds,
            phis,
            gamma,
            mu,
            loop_result,
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

impl Dag for GSA {
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

/// One incoming control-flow edge of a block: the predecessor and the values it
/// forwards to the block's arguments, in argument order.
struct Edge {
    pred: BlockId,
    args: Vec<ValueId>,
}

struct Builder<'a> {
    context: &'a Context,
    dom: DominatorTree,
    /// Block-SSA predecessors reaching each block, with the values forwarded along
    /// the edge.
    preds: HashMap<BlockId, Vec<Edge>>,
    /// Block arguments that are phis (their block has branch predecessors), mapped to
    /// their block and argument index.
    phis: HashMap<ValueId, (BlockId, usize)>,
    /// Result of a structured conditional ([`RegionGuard`]) → `(cond, true, false)` γ
    /// inputs.
    gamma: HashMap<ValueId, (ValueId, ValueId, ValueId)>,
    /// Loop-carried region argument ([`LoopCarried`]) → `(init, latch)` μ inputs.
    mu: HashMap<ValueId, (ValueId, ValueId)>,
    /// Structured loop result → its carried region argument; the result shares the
    /// carried value's μ node.
    loop_result: HashMap<ValueId, ValueId>,
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

        if let Some((gate, inputs)) = self.gate_plan(value) {
            let node = self.inner.add_node(gate);
            self.nodes.insert(value, node);
            for input in inputs {
                let child = self.node_for_value(input);
                self.inner.add_edge(node, child);
            }
            return node;
        }

        // A structured loop's result is the carried value at exit: it shares the μ node
        // of its carried region argument rather than getting one of its own.
        if let Some(&carried) = self.loop_result.get(&value) {
            let node = self.node_for_value(carried);
            self.nodes.insert(value, node);
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

    /// The gate replacing `value` and the inputs feeding it, in edge order, if `value`
    /// is an unstructured phi, a structured γ result, or a structured μ carried value.
    fn gate_plan(&self, value: ValueId) -> Option<(GateNode, Vec<ValueId>)> {
        if let Some(&(block, index)) = self.phis.get(&value) {
            return Some(self.plan_gate(block, index, value));
        }
        if let Some(&(cond, t, f)) = self.gamma.get(&value) {
            return Some((GateNode::Gamma { value, cond }, vec![cond, t, f]));
        }
        if let Some(&(init, latch)) = self.mu.get(&value) {
            return Some((GateNode::Mu { value }, vec![init, latch]));
        }
        None
    }

    /// Classify the phi `value` (argument `index` of `block`) into a gate and list
    /// its inputs in edge order.
    fn plan_gate(&self, block: BlockId, index: usize, value: ValueId) -> (GateNode, Vec<ValueId>) {
        let incoming = self.incoming(block, index);

        if let Some(gate) = self.mu_gate(block, value, &incoming) {
            return gate;
        }
        if let Some(gate) = self.gamma_gate(block, value, &incoming) {
            return gate;
        }

        let inputs = incoming.into_iter().map(|(_, v)| v).collect();
        (GateNode::Phi { value }, inputs)
    }

    /// μ gate for a loop header: some incoming edge comes from a block the header
    /// dominates (the back edge). Inputs are `[init, latch]`.
    fn mu_gate(
        &self,
        block: BlockId,
        value: ValueId,
        incoming: &[(BlockId, ValueId)],
    ) -> Option<(GateNode, Vec<ValueId>)> {
        if !incoming
            .iter()
            .any(|&(pred, _)| self.dom.dominates(block, pred))
        {
            return None;
        }

        let mut init = None;
        let mut latch = None;
        for &(pred, v) in incoming {
            if self.dom.dominates(block, pred) {
                latch.get_or_insert(v);
            } else {
                init.get_or_insert(v);
            }
        }
        match (init, latch) {
            (Some(init), Some(latch)) => Some((GateNode::Mu { value }, vec![init, latch])),
            _ => None,
        }
    }

    /// γ gate for a two-way merge: the immediately dominating terminator is a
    /// [`BranchGuard`], and each incoming edge is reached through one of its guarded
    /// successors. Inputs are `[condition, true_input, false_input]`.
    fn gamma_gate(
        &self,
        block: BlockId,
        value: ValueId,
        incoming: &[(BlockId, ValueId)],
    ) -> Option<(GateNode, Vec<ValueId>)> {
        if incoming.len() != 2 {
            return None;
        }
        let idom = self.dom.idom(block)?;
        let term = self.context.get_block(idom).op_ids().last().copied()?;
        let guarded = self
            .context
            .get_op(term)
            .as_interface::<dyn BranchGuard>()?
            .guarded_successors();

        let mut cond = None;
        let mut true_val = None;
        let mut false_val = None;
        for &(pred, v) in incoming {
            for &(succ, c, taken_when_true) in &guarded {
                if self.dom.dominates(succ, pred) {
                    cond = Some(c);
                    if taken_when_true {
                        true_val.get_or_insert(v);
                    } else {
                        false_val.get_or_insert(v);
                    }
                    break;
                }
            }
        }
        match (cond, true_val, false_val) {
            (Some(cond), Some(t), Some(f)) => {
                Some((GateNode::Gamma { value, cond }, vec![cond, t, f]))
            }
            _ => None,
        }
    }

    /// The `(predecessor, forwarded value)` pairs feeding argument `index` of `block`.
    fn incoming(&self, block: BlockId, index: usize) -> Vec<(BlockId, ValueId)> {
        self.preds
            .get(&block)
            .into_iter()
            .flatten()
            .filter_map(|edge| edge.args.get(index).map(|&v| (edge.pred, v)))
            .collect()
    }
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

/// Map each block to its incoming [`BranchTerminator`] edges. Terminators that forward
/// no block arguments (returns, yields, structured-region edges) do not implement the
/// interface and so contribute no predecessors.
fn predecessor_map(context: &Context, blocks: &[BlockId]) -> HashMap<BlockId, Vec<Edge>> {
    let mut preds: HashMap<BlockId, Vec<Edge>> = HashMap::new();
    for &block in blocks {
        let Some(&term) = context.get_block(block).op_ids().last() else {
            continue;
        };
        let Some(branch) = context.get_op(term).as_interface::<dyn BranchTerminator>() else {
            continue;
        };
        for (succ, args) in branch.successor_operands() {
            preds
                .entry(succ)
                .or_default()
                .push(Edge { pred: block, args });
        }
    }
    preds
}

/// The block-argument values that are phis: arguments of any block with branch
/// predecessors, mapped to their block and argument index.
fn collect_phis(
    context: &Context,
    blocks: &[BlockId],
    preds: &HashMap<BlockId, Vec<Edge>>,
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

/// Structured-control-flow gates collected from the region's ops.
struct StructuredGates {
    /// γ: result of a [`RegionGuard`] op → `(cond, true_input, false_input)`.
    gamma: HashMap<ValueId, (ValueId, ValueId, ValueId)>,
    /// μ: carried region argument of a [`LoopCarried`] op → `(init, latch)`.
    mu: HashMap<ValueId, (ValueId, ValueId)>,
    /// Structured loop result → its carried region argument.
    loop_result: HashMap<ValueId, ValueId>,
}

/// Scan the region's ops for structured control flow: a [`RegionGuard`] op that
/// produces a value yields a γ over its arms' yielded values; a [`LoopCarried`] op
/// yields a μ over its carried region argument, with its result aliasing that argument.
fn structured_gates(context: &Context, blocks: &[BlockId]) -> StructuredGates {
    let mut gamma = HashMap::new();
    let mut mu = HashMap::new();
    let mut loop_result = HashMap::new();

    for &block in blocks {
        for op in context.get_block(block).op_ids() {
            let instance = context.get_op(op);
            // A structured gate exists only when the op produces a value: a resultless
            // `scf.if`/loop is side-effecting and carries nothing.
            let Some(result) = instance.results.first().copied() else {
                continue;
            };

            if let Some(guard) = instance.clone().as_interface::<dyn RegionGuard>() {
                if let Some(inputs) = gamma_inputs(context, guard.as_ref()) {
                    gamma.insert(result, inputs);
                }
            } else if let Some(lp) = instance.clone().as_interface::<dyn LoopLike>() {
                mu.insert(lp.carried_arg(), (lp.init(), lp.latched()));
                loop_result.insert(result, lp.carried_arg());
            }
        }
    }

    StructuredGates {
        gamma,
        mu,
        loop_result,
    }
}

/// The γ inputs `(cond, true_input, false_input)` of a two-armed [`RegionGuard`]: the
/// condition and the values its true/false regions yield.
fn gamma_inputs(context: &Context, guard: &dyn RegionGuard) -> Option<(ValueId, ValueId, ValueId)> {
    let mut cond = None;
    let mut true_val = None;
    let mut false_val = None;
    for (region, c, taken_when_true) in guard.guarded_regions() {
        let yielded = region_yield_value(context, region)?;
        cond = Some(c);
        if taken_when_true {
            true_val = Some(yielded);
        } else {
            false_val = Some(yielded);
        }
    }
    Some((cond?, true_val?, false_val?))
}

/// The single value yielded by a structured region's terminator.
fn region_yield_value(context: &Context, region: RegionId) -> Option<ValueId> {
    let block = context
        .get_region(region)
        .iter(context.clone())
        .next()?
        .id();
    let terminator = context.get_block(block).op_ids().last().copied()?;
    context.get_op(terminator).operands.first().copied()
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

    fn children(gs: &GSA, node: NodeId) -> Vec<NodeId> {
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

        let gs = GSA::new(&context, func_with_region(&context, region.id()));

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

        let gs = GSA::new(&context, func_with_region(&context, region.id()));

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

        let gs = GSA::new(&context, func_with_region(&context, region.id()));

        let node = gs.node_of(arg_id).unwrap();
        assert_eq!(*gs.gate(node), GateNode::Input(arg_id));
    }

    /// A region yielding a single value through its terminator.
    fn yielding_region(context: &Context, value: ValueId, def: impl Operation) -> RegionId {
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        let mut b = IRBuilder::new(block);
        b.insert(def);
        b.insert(crate::scf::ops::r#yield(context, Operand::from(value)).build());
        region.id()
    }

    #[test]
    fn gamma_for_scf_if_result() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let i32 = IntegerType::new(&context, 32);
        let cond = context.create_value(i1, None);
        let cond_id = cond.id();

        let c_true = ops::constant(&context, 1, i32).build();
        let true_val = c_true.result();
        let true_op = c_true.id();
        let then_region = yielding_region(&context, true_val, c_true);

        let c_false = ops::constant(&context, 0, i32).build();
        let false_val = c_false.result();
        let false_op = c_false.id();
        let else_region = yielding_region(&context, false_val, c_false);

        let region = context.create_region();
        let entry = context.create_block(vec![cond]);
        region.add_block(entry.id());

        let if_op = crate::scf::ops::r#if(
            &context,
            cond_id,
            Some(i32),
            Some(then_region),
            Some(else_region),
        )
        .build();
        let if_result = if_op.result();
        let mut eb = IRBuilder::new(entry.clone());
        eb.insert(if_op);
        eb.insert(ops::r#return(&context, Operand::from(if_result)).build());

        let gs = GSA::new(&context, func_with_region(&context, region.id()));

        let node = gs.node_of(if_result).unwrap();
        assert_eq!(
            *gs.gate(node),
            GateNode::Gamma {
                value: if_result,
                cond: cond_id,
            }
        );

        let kids = children(&gs, node);
        assert_eq!(kids.len(), 3);
        // Order is [condition, true_input, false_input].
        assert_eq!(gs.value_of(kids[0]), Some(cond_id));
        assert_eq!(gs.op_of(kids[1]), Some(true_op));
        assert_eq!(gs.op_of(kids[2]), Some(false_op));
    }

    /// A loop body carrying `acc`: `%next = addi %acc, 1; scf.yield %next`. Returns the
    /// body region, the carried block argument, and the latch (`addi`) op id.
    fn counting_body(context: &Context, i32: TypeId) -> (RegionId, ValueId, OpId) {
        let acc = context.create_value(i32, None);
        let acc_id = acc.id();
        let region = context.create_region();
        let block = context.create_block(vec![acc]);
        region.add_block(block.id());

        let step = ops::constant(context, 1, i32).build();
        let step_val = step.result();
        let next = ops::addi(context, acc_id, step_val, i32).build();
        let next_val = next.result();
        let next_op = next.id();
        let mut bb = IRBuilder::new(block);
        bb.insert(step);
        bb.insert(next);
        bb.insert(crate::scf::ops::r#yield(context, Operand::from(next_val)).build());
        (region.id(), acc_id, next_op)
    }

    /// Assert the loop's result shares one μ node with its carried argument, gated over
    /// `[init, latch]` with the latch reading the μ back.
    fn assert_mu(gs: &GSA, result: ValueId, acc_id: ValueId, init_op: OpId, latch_op: OpId) {
        let node = gs.node_of(result).unwrap();
        assert_eq!(gs.node_of(acc_id), Some(node));
        assert_eq!(*gs.gate(node), GateNode::Mu { value: acc_id });

        let kids = children(gs, node);
        assert_eq!(kids.len(), 2);
        // Order is [init, latch].
        assert_eq!(gs.op_of(kids[0]), Some(init_op));
        assert_eq!(gs.op_of(kids[1]), Some(latch_op));
        assert!(children(gs, kids[1]).contains(&node));
    }

    #[test]
    fn mu_for_scf_for_result() {
        let context = Context::with_default_dialects();
        let index = crate::builtin::IndexType::new(&context);
        let i32 = IntegerType::new(&context, 32);
        let lb = context.create_value(index, None);
        let ub = context.create_value(index, None);
        let step = context.create_value(index, None);

        let (body, acc_id, latch_op) = counting_body(&context, i32);

        let init = ops::constant(&context, 0, i32).build();
        let init_val = init.result();
        let init_op = init.id();

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        region.add_block(entry.id());

        let for_op = crate::scf::ops::r#for(
            &context,
            lb.id(),
            ub.id(),
            step.id(),
            Operand::from(init_val),
            Some(i32),
            Some(body),
        )
        .build();
        let for_result = for_op.result();
        let mut eb = IRBuilder::new(entry.clone());
        eb.insert(init);
        eb.insert(for_op);
        eb.insert(ops::r#return(&context, Operand::from(for_result)).build());

        let gs = GSA::new(&context, func_with_region(&context, region.id()));
        assert_mu(&gs, for_result, acc_id, init_op, latch_op);
    }

    #[test]
    fn mu_for_scf_while_result() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let i32 = IntegerType::new(&context, 32);
        let cond = context.create_value(i1, None);

        let (body, acc_id, latch_op) = counting_body(&context, i32);

        let init = ops::constant(&context, 0, i32).build();
        let init_val = init.result();
        let init_op = init.id();

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        region.add_block(entry.id());

        let while_op = crate::scf::ops::r#while(
            &context,
            cond.id(),
            Operand::from(init_val),
            Some(i32),
            Some(body),
        )
        .build();
        let while_result = while_op.result();
        let mut eb = IRBuilder::new(entry.clone());
        eb.insert(init);
        eb.insert(while_op);
        eb.insert(ops::r#return(&context, Operand::from(while_result)).build());

        let gs = GSA::new(&context, func_with_region(&context, region.id()));
        assert_mu(&gs, while_result, acc_id, init_op, latch_op);
    }
}
