//! Guarded-edge facts inherited down the dominator tree.
//!
//! A guarded CFG edge `u -> v` carries the fact `condition == holds`. When `v`
//! is a non-entry block entered through exactly that one edge, "the edge
//! dominates" collapses to "`v` dominates", so the fact holds throughout `v`
//! and every block `v` dominates (LLVM GVN's dominated-equality argument). This
//! generalizes isel's per-block `edge_fact` rule from the block itself to every
//! dominator of the block.

use std::collections::{HashMap, HashSet};

use crate::{
    BlockId, BranchGuard, Context, OpId, Terminator, ValueId,
    analysis::{Analysis, AnalysisManager, DominatorTree},
};

/// The fact a guarded CFG edge carries: on this edge, `condition` is known to
/// equal `holds`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeFact {
    pub condition: ValueId,
    pub holds: bool,
}

/// Per-block guarded-edge facts, each holding throughout its block, ordered
/// outermost dominator first. Build through [`AnalysisManager`].
pub struct DominatingEdgeFacts {
    /// A block's own contribution (the `v == block` case), if any.
    own: HashMap<BlockId, EdgeFact>,
    /// Every fact holding throughout a block, dominators outermost first.
    facts: HashMap<BlockId, Vec<EdgeFact>>,
}

impl DominatingEdgeFacts {
    /// Facts holding throughout `block`: for every dominator `v` of `block`
    /// (including `block` itself) that is a non-entry block with exactly one
    /// incoming CFG edge whose source terminator guards it, the guard's fact.
    /// Ordered outermost dominator first.
    pub fn facts(&self, block: BlockId) -> &[EdgeFact] {
        self.facts.get(&block).map_or(&[], Vec::as_slice)
    }

    /// The fact contributed by `block` itself, if any (the `v == block` case).
    pub fn own_fact(&self, block: BlockId) -> Option<EdgeFact> {
        self.own.get(&block).copied()
    }

    fn compute(context: &Context, root: OpId, dom: &DominatorTree) -> Self {
        let cfg = collect_cfg(context, root);

        let mut own = HashMap::new();
        for (&block, edges) in &cfg.in_edges {
            if cfg.entry_blocks.contains(&block) {
                continue;
            }
            if let [Some(fact)] = edges.as_slice() {
                own.insert(block, *fact);
            }
        }

        // For each reachable block, gather own facts up its dominator chain.
        let mut facts = HashMap::new();
        for &block in &cfg.blocks {
            let mut chain = Vec::new();
            let mut current = Some(block);
            while let Some(b) = current {
                if let Some(fact) = own.get(&b) {
                    chain.push(*fact);
                }
                current = dom.idom(b);
            }
            if !chain.is_empty() {
                chain.reverse();
                facts.insert(block, chain);
            }
        }

        Self { own, facts }
    }
}

/// The unified CFG facts the analysis needs: every reachable block, which are
/// region entries, and each block's guarded/unguarded incoming edges.
struct Cfg {
    blocks: Vec<BlockId>,
    entry_blocks: HashSet<BlockId>,
    in_edges: HashMap<BlockId, Vec<Option<EdgeFact>>>,
}

/// Walk the same unified CFG the dominator tree covers, recording per-block
/// incoming edges (mirrors `dominance::build_cfg` descent and isel's
/// `record_cfg` edge classification).
fn collect_cfg(context: &Context, root: OpId) -> Cfg {
    let mut blocks = Vec::new();
    let mut entry_blocks = HashSet::new();
    let mut in_edges: HashMap<BlockId, Vec<Option<EdgeFact>>> = HashMap::new();

    let entry = context
        .get_op(root)
        .regions()
        .first()
        .and_then(|region| context.get_region(*region).iter(context.clone()).next())
        .map(|block| block.id());
    let Some(entry) = entry else {
        return Cfg {
            blocks,
            entry_blocks,
            in_edges,
        };
    };

    let mut seen = HashSet::new();
    let mut stack = vec![entry];
    seen.insert(entry);
    entry_blocks.insert(entry);

    while let Some(block_id) = stack.pop() {
        blocks.push(block_id);
        let block = context.get_block(block_id);
        let op_ids = block.op_ids();
        let mut targets = Vec::new();

        // Structured control flow: nested region entries carry an implicit edge.
        for op_id in &op_ids {
            for region_id in context.get_op(*op_id).regions() {
                if let Some(child) = context.get_region(region_id).iter(context.clone()).next() {
                    entry_blocks.insert(child.id());
                    targets.push(child.id());
                }
            }
        }

        // Unstructured control flow: the terminator's successor edges.
        if let Some(&terminator) = op_ids.last() {
            let inst = context.get_op(terminator);
            if let Some(guard) = inst.clone().as_interface::<dyn BranchGuard>() {
                for (dest, condition, holds) in guard.guarded_successors() {
                    in_edges
                        .entry(dest)
                        .or_default()
                        .push(Some(EdgeFact { condition, holds }));
                    targets.push(dest);
                }
            } else if let Some(term) = inst.clone().as_interface::<dyn Terminator>() {
                for dest in term.successors() {
                    in_edges.entry(dest).or_default().push(None);
                    targets.push(dest);
                }
            }
        }

        for target in targets {
            if seen.insert(target) {
                stack.push(target);
            }
        }
    }

    Cfg {
        blocks,
        entry_blocks,
        in_edges,
    }
}

impl Analysis for DominatingEdgeFacts {
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
        Self::compute(context, op, &analyses.get::<DominatorTree>(context, op))
    }
}
