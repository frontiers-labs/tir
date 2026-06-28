//! Region-aware dominator and post-dominator trees.
//!
//! Unlike LLVM, an operation may hold nested regions, so the control-flow graph
//! spans more than the blocks of a single region. Following MLIR's notion of
//! dominance (see "Extending Dominance to MLIR Regions", LLVM Dev Mtg 2023), the
//! tree is built over a unified CFG whose nodes are basic blocks drawn from the
//! root operation's region and, transitively, every nested region reachable from
//! it.
//!
//! The resulting tree is exposed through the [`Dag`] trait by delegating to an
//! internal [`PostOrderDag`]; the custom constructors build the edge structure
//! from immediate dominators.

use std::collections::{HashMap, HashSet};

use crate::{
    BlockId, Context, OpId, Operation, Terminator, TypeId,
    graph::{Dag, MutDag, NodeId, PostOrderDag},
};

/// A node of the (post-)dominator tree. Wraps a real basic block, or the virtual
/// exit that roots a post-dominator tree when the CFG has several sinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DomNode {
    Block(BlockId),
    /// Virtual exit; only present in post-dominator trees.
    Exit,
}

/// A dominator or post-dominator tree over the blocks reachable from a root
/// operation's single region.
///
/// Implements [`Dag`] by delegating to an internal [`PostOrderDag`] whose nodes
/// are laid out so the tree root is the last node. Build one with [`Self::new`]
/// (dominance) or [`Self::post_dominator`] (post-dominance).
pub struct DominatorTree {
    inner: PostOrderDag<DomNode, ()>,
    idoms: HashMap<DomNode, DomNode>,
    node_ids: HashMap<DomNode, NodeId>,
}

struct Cfg {
    entry: Option<BlockId>,
    /// Every reachable block maps to its successors (possibly empty for sinks).
    succ: HashMap<DomNode, Vec<DomNode>>,
}

impl DominatorTree {
    /// Build the dominator tree rooted at the entry block of `root`'s region.
    pub fn new<O: Into<OpId>>(context: &Context, root: O) -> Self {
        let cfg = build_cfg(context, root.into());
        match cfg.entry {
            Some(entry) => {
                let root = DomNode::Block(entry);
                Self::from_tree(root, dominators(root, &cfg.succ))
            }
            None => Self::empty(),
        }
    }

    /// Build the post-dominator tree for `root`'s region. Blocks with no
    /// successors (returns, nested-region exits) are joined under a virtual
    /// [`DomNode::Exit`], which becomes the tree root.
    pub fn post_dominator<O: Into<OpId>>(context: &Context, root: O) -> Self {
        let cfg = build_cfg(context, root.into());
        if cfg.entry.is_none() {
            return Self::empty();
        }

        let mut reversed: HashMap<DomNode, Vec<DomNode>> = HashMap::new();
        let mut sinks = Vec::new();
        for (&from, tos) in &cfg.succ {
            if tos.is_empty() {
                sinks.push(from);
            }
            for &to in tos {
                reversed.entry(to).or_default().push(from);
            }
        }
        reversed.insert(DomNode::Exit, sinks);

        Self::from_tree(DomNode::Exit, dominators(DomNode::Exit, &reversed))
    }

    fn empty() -> Self {
        Self {
            inner: PostOrderDag::new(),
            idoms: HashMap::new(),
            node_ids: HashMap::new(),
        }
    }

    fn from_tree(root: DomNode, idoms: HashMap<DomNode, DomNode>) -> Self {
        let mut children: HashMap<DomNode, Vec<DomNode>> = HashMap::new();
        for (&node, &parent) in &idoms {
            children.entry(parent).or_default().push(node);
        }

        // Lay nodes out children-before-parent so the root is the last node and
        // every edge points to a lower index, as `PostOrderDag` requires.
        let order = domtree_postorder(root, &children);

        let mut inner = PostOrderDag::new();
        let mut node_ids = HashMap::new();
        for &node in &order {
            node_ids.insert(node, inner.add_node(node));
        }
        for &node in &order {
            if let Some(parent) = idoms.get(&node) {
                inner.add_edge(node_ids[parent], node_ids[&node]);
            }
        }

        Self {
            inner,
            idoms,
            node_ids,
        }
    }

    /// The block carried by `id`, or `None` for the virtual post-dom exit.
    pub fn block(&self, id: NodeId) -> Option<BlockId> {
        match self.inner.get_node(id) {
            DomNode::Block(block) => Some(*block),
            DomNode::Exit => None,
        }
    }

    /// The tree node for `block`, if it is part of the tree.
    pub fn node_of(&self, block: BlockId) -> Option<NodeId> {
        self.node_ids.get(&DomNode::Block(block)).copied()
    }

    /// The immediate (post-)dominator of `block`, or `None` for the tree root or
    /// blocks outside the tree.
    pub fn idom(&self, block: BlockId) -> Option<BlockId> {
        match self.idoms.get(&DomNode::Block(block))? {
            DomNode::Block(block) => Some(*block),
            DomNode::Exit => None,
        }
    }

    /// Whether `a` (post-)dominates `b`, reflexively. In a [`Self::new`] tree this
    /// is ordinary dominance; in a [`Self::post_dominator`] tree it is
    /// post-dominance.
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        let target = DomNode::Block(a);
        let mut current = DomNode::Block(b);
        if !self.node_ids.contains_key(&current) {
            return false;
        }
        loop {
            if current == target {
                return true;
            }
            match self.idoms.get(&current) {
                Some(&parent) => current = parent,
                None => return false,
            }
        }
    }

    pub fn op_dominates<O1: Operation, O2: Operation>(
        &self,
        ctx: &Context,
        a: &O1,
        b: &O2,
    ) -> bool {
        let a_block = a.parent_block();
        let b_block = b.parent_block();

        if let (Some(a_block), Some(b_block)) = (a_block, b_block) {
            if a_block == b_block {
                let block = ctx.get_block(a_block);
                block.is_before(a.id(), b.id())
            } else {
                self.dominates(a_block, b_block)
            }
        } else {
            false
        }
    }
}

impl Dag for DominatorTree {
    type Node = DomNode;
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

/// Collect the unified CFG over blocks reachable from `root`'s first region,
/// descending into nested regions held by operations along the way.
fn build_cfg(context: &Context, root: OpId) -> Cfg {
    let entry = context
        .get_op(root)
        .regions
        .first()
        .and_then(|region| context.get_region(*region).iter(context.clone()).next())
        .map(|block| block.id());

    let mut succ: HashMap<DomNode, Vec<DomNode>> = HashMap::new();
    let Some(entry) = entry else {
        return Cfg { entry, succ };
    };

    let mut seen = HashSet::new();
    let mut stack = vec![entry];
    seen.insert(entry);

    while let Some(block_id) = stack.pop() {
        let block = context.get_block(block_id);
        let op_ids = block.op_ids();
        let mut edges = Vec::new();

        // Structured control flow: any op carrying regions flows into each
        // region's entry block.
        for op_id in &op_ids {
            for region_id in &context.get_op(*op_id).regions {
                if let Some(child) = context.get_region(*region_id).iter(context.clone()).next() {
                    edges.push(child.id());
                }
            }
        }

        // Unstructured control flow: the terminator's successors.
        if let Some(terminator) = op_ids.last() {
            edges.extend(terminator_successors(context, *terminator));
        }

        for &target in &edges {
            if seen.insert(target) {
                stack.push(target);
            }
        }

        succ.insert(
            DomNode::Block(block_id),
            edges.into_iter().map(DomNode::Block).collect(),
        );
    }

    Cfg {
        entry: Some(entry),
        succ,
    }
}

fn terminator_successors(context: &Context, op: OpId) -> Vec<BlockId> {
    let instance = context.get_op(op);
    if let Some(terminator) = instance.clone().as_interface::<dyn Terminator>() {
        return terminator.successors();
    }
    Vec::new()
}

/// Immediate dominators of every node reachable from `entry`, by Cooper, Harvey
/// and Kennedy's "A Simple, Fast Dominance Algorithm". The entry maps to itself
/// and is omitted from the result.
fn dominators(entry: DomNode, succ: &HashMap<DomNode, Vec<DomNode>>) -> HashMap<DomNode, DomNode> {
    let order = postorder(entry, succ);
    let count = order.len();
    if count == 0 {
        return HashMap::new();
    }

    let po_index: HashMap<DomNode, usize> =
        order.iter().enumerate().map(|(i, &n)| (n, i)).collect();

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (&from, tos) in succ {
        let Some(&from_idx) = po_index.get(&from) else {
            continue;
        };
        for to in tos {
            if let Some(&to_idx) = po_index.get(to) {
                preds[to_idx].push(from_idx);
            }
        }
    }

    let entry_idx = count - 1;
    let mut idom: Vec<Option<usize>> = vec![None; count];
    idom[entry_idx] = Some(entry_idx);

    let mut changed = true;
    while changed {
        changed = false;
        // Reverse postorder, skipping the entry.
        for node in (0..entry_idx).rev() {
            let mut new_idom: Option<usize> = None;
            for &pred in &preds[node] {
                if idom[pred].is_none() {
                    continue;
                }
                new_idom = Some(match new_idom {
                    None => pred,
                    Some(current) => intersect(pred, current, &idom),
                });
            }
            if new_idom.is_some() && new_idom != idom[node] {
                idom[node] = new_idom;
                changed = true;
            }
        }
    }

    let mut result = HashMap::new();
    for node in 0..count {
        if node == entry_idx {
            continue;
        }
        if let Some(parent) = idom[node] {
            result.insert(order[node], order[parent]);
        }
    }
    result
}

fn intersect(mut a: usize, mut b: usize, idom: &[Option<usize>]) -> usize {
    while a != b {
        while a < b {
            a = idom[a].expect("ancestor dominators are resolved before use");
        }
        while b < a {
            b = idom[b].expect("ancestor dominators are resolved before use");
        }
    }
    a
}

/// Iterative postorder DFS over `succ` from `entry`; the entry is emitted last.
fn postorder(entry: DomNode, succ: &HashMap<DomNode, Vec<DomNode>>) -> Vec<DomNode> {
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    let mut stack: Vec<(DomNode, usize)> = vec![(entry, 0)];
    visited.insert(entry);

    while let Some(&(node, index)) = stack.last() {
        let children = succ.get(&node).map(Vec::as_slice).unwrap_or(&[]);
        if index < children.len() {
            stack.last_mut().unwrap().1 += 1;
            let next = children[index];
            if visited.insert(next) {
                stack.push((next, 0));
            }
        } else {
            order.push(node);
            stack.pop();
        }
    }

    order
}

/// Post-order over the dominator tree so children precede their parent.
fn domtree_postorder(root: DomNode, children: &HashMap<DomNode, Vec<DomNode>>) -> Vec<DomNode> {
    let mut order = Vec::new();
    let mut stack: Vec<(DomNode, usize)> = vec![(root, 0)];

    while let Some(&(node, index)) = stack.last() {
        let kids = children.get(&node).map(Vec::as_slice).unwrap_or(&[]);
        if index < kids.len() {
            stack.last_mut().unwrap().1 += 1;
            stack.push((kids[index], 0));
        } else {
            order.push(node);
            stack.pop();
        }
    }

    order
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::*;
    use crate::{
        Block, Context, IRBuilder, Operand, Operation, RegionId,
        builtin::{IntegerType, UnitType, ops},
    };

    fn block_succs(tree: &DominatorTree, block: BlockId) -> HashSet<BlockId> {
        let node = tree.node_of(block).unwrap();
        tree.children(node)
            .filter_map(|child| tree.block(child))
            .collect()
    }

    fn yield_region(context: &Context) -> RegionId {
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        IRBuilder::new(block).insert(crate::scf::ops::r#yield(context, Operand::none()).build());
        region.id()
    }

    fn func_with_region(context: &Context, region: RegionId) -> OpId {
        ops::func(context, "f", UnitType::new(context), Some(region))
            .build()
            .id()
    }

    fn terminate(block: &Arc<Block>, op: impl Operation) {
        IRBuilder::new(block.clone()).insert(op);
    }

    #[test]
    fn diamond_dominators() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let cond = context.create_value(i1, None);
        let cond_id = cond.id();

        let region = context.create_region();
        let entry = context.create_block(vec![cond]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);
        let merge = context.create_block(vec![]);
        for block in [&entry, &t, &f, &merge] {
            region.add_block(block.id());
        }

        terminate(
            &entry,
            ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build(),
        );
        terminate(&t, ops::br(&context, vec![], merge.id()).build());
        terminate(&f, ops::br(&context, vec![], merge.id()).build());
        terminate(&merge, ops::r#return(&context, Operand::none()).build());

        let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));

        assert_eq!(dt.len(), 4);
        assert_eq!(dt.idom(entry.id()), None);
        assert_eq!(dt.idom(t.id()), Some(entry.id()));
        assert_eq!(dt.idom(f.id()), Some(entry.id()));
        assert_eq!(dt.idom(merge.id()), Some(entry.id()));

        assert!(dt.dominates(entry.id(), entry.id()));
        assert!(dt.dominates(entry.id(), merge.id()));
        assert!(!dt.dominates(t.id(), merge.id()));

        let root = dt.root().unwrap();
        assert_eq!(dt.block(root), Some(entry.id()));
        assert_eq!(
            block_succs(&dt, entry.id()),
            HashSet::from([t.id(), f.id(), merge.id()])
        );
    }

    #[test]
    fn loop_back_edge_dominators() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let cond = context.create_value(i1, None);
        let cond_id = cond.id();

        let region = context.create_region();
        let entry = context.create_block(vec![cond]);
        let header = context.create_block(vec![]);
        let body = context.create_block(vec![]);
        let exit = context.create_block(vec![]);
        for block in [&entry, &header, &body, &exit] {
            region.add_block(block.id());
        }

        terminate(&entry, ops::br(&context, vec![], header.id()).build());
        terminate(
            &header,
            ops::cond_br(&context, cond_id, vec![], vec![], body.id(), exit.id()).build(),
        );
        terminate(&body, ops::br(&context, vec![], header.id()).build());
        terminate(&exit, ops::r#return(&context, Operand::none()).build());

        let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));

        assert_eq!(dt.idom(header.id()), Some(entry.id()));
        assert_eq!(dt.idom(body.id()), Some(header.id()));
        assert_eq!(dt.idom(exit.id()), Some(header.id()));
        assert!(dt.dominates(header.id(), body.id()));
        assert!(!dt.dominates(body.id(), exit.id()));
    }

    #[test]
    fn structured_if_dominators() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let cond = context.create_value(i1, None);
        let cond_id = cond.id();

        let region = context.create_region();
        let entry = context.create_block(vec![cond]);
        region.add_block(entry.id());

        let then_region = yield_region(&context);
        let else_region = yield_region(&context);
        let then_entry = context
            .get_region(then_region)
            .iter(context.clone())
            .next()
            .unwrap()
            .id();
        let else_entry = context
            .get_region(else_region)
            .iter(context.clone())
            .next()
            .unwrap()
            .id();

        let if_op = crate::scf::ops::r#if(
            &context,
            cond_id,
            None,
            Some(then_region),
            Some(else_region),
        )
        .build();

        let mut builder = IRBuilder::new(entry.clone());
        builder.insert(if_op);
        builder.insert(ops::r#return(&context, Operand::none()).build());

        let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));

        // The block holding scf.if dominates the entries of both nested regions.
        assert_eq!(dt.idom(then_entry), Some(entry.id()));
        assert_eq!(dt.idom(else_entry), Some(entry.id()));
        assert!(dt.dominates(entry.id(), then_entry));
        assert!(dt.dominates(entry.id(), else_entry));
        assert!(!dt.dominates(then_entry, else_entry));
    }

    #[test]
    fn diamond_post_dominators() {
        let context = Context::with_default_dialects();
        let i1 = IntegerType::new(&context, 1);
        let cond = context.create_value(i1, None);
        let cond_id = cond.id();

        let region = context.create_region();
        let entry = context.create_block(vec![cond]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);
        let merge = context.create_block(vec![]);
        for block in [&entry, &t, &f, &merge] {
            region.add_block(block.id());
        }

        terminate(
            &entry,
            ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build(),
        );
        terminate(&t, ops::br(&context, vec![], merge.id()).build());
        terminate(&f, ops::br(&context, vec![], merge.id()).build());
        terminate(&merge, ops::r#return(&context, Operand::none()).build());

        let pdt = DominatorTree::post_dominator(&context, func_with_region(&context, region.id()));

        // The merge block post-dominates every block; the root is the virtual exit.
        let root = pdt.root().unwrap();
        assert_eq!(pdt.block(root), None);
        assert_eq!(pdt.idom(merge.id()), None);
        assert_eq!(pdt.idom(entry.id()), Some(merge.id()));
        assert_eq!(pdt.idom(t.id()), Some(merge.id()));
        assert_eq!(pdt.idom(f.id()), Some(merge.id()));

        assert!(pdt.dominates(merge.id(), entry.id()));
        assert!(pdt.dominates(merge.id(), t.id()));
        assert!(!pdt.dominates(t.id(), entry.id()));
    }

    #[test]
    fn single_block_tree() {
        let context = Context::with_default_dialects();
        let region = context.create_region();
        let entry = context.create_block(vec![]);
        region.add_block(entry.id());
        terminate(&entry, ops::r#return(&context, Operand::none()).build());

        let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));
        assert_eq!(dt.len(), 1);
        assert_eq!(dt.block(dt.root().unwrap()), Some(entry.id()));
        assert_eq!(dt.idom(entry.id()), None);
        assert!(dt.dominates(entry.id(), entry.id()));
    }

    #[test]
    fn for_loop_as_root() {
        let context = Context::with_default_dialects();
        let index = crate::builtin::IndexType::new(&context);
        let lb = context.create_value(index, None);
        let ub = context.create_value(index, None);
        let step = context.create_value(index, None);

        let body = yield_region(&context);
        let body_entry = context
            .get_region(body)
            .iter(context.clone())
            .next()
            .unwrap()
            .id();

        let for_op = crate::scf::ops::r#for(
            &context,
            lb.id(),
            ub.id(),
            step.id(),
            Operand::none(),
            None,
            Some(body),
        )
        .build();

        // An scf.for can itself be the root: its single body region is the tree.
        let dt = DominatorTree::new(&context, for_op.id());
        assert_eq!(dt.len(), 1);
        assert_eq!(dt.block(dt.root().unwrap()), Some(body_entry));
    }
}
