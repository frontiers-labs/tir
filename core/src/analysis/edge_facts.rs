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
    analysis::{Analysis, AnalysisManager, DominatorTree, PreservedAnalyses},
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
        .regions
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
            for region_id in &context.get_op(*op_id).regions {
                if let Some(child) = context.get_region(*region_id).iter(context.clone()).next() {
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

    /// Derived from dominance, so it dies with it.
    fn is_invalidated(&self, preserved: &PreservedAnalyses) -> bool {
        !preserved.is_preserved::<Self>() || !preserved.is_preserved::<DominatorTree>()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        Block, Context, IRBuilder, Operand, Operation, RegionId,
        builtin::{IntegerType, UnitType, ops},
    };

    fn analyze(context: &Context, region: RegionId) -> DominatingEdgeFacts {
        let root = ops::func(context, "f", UnitType::new(context), Some(region))
            .build()
            .id();
        let dom = DominatorTree::new(context, root);
        DominatingEdgeFacts::compute(context, root, &dom)
    }

    fn cond(context: &Context) -> ValueId {
        let i1 = IntegerType::new(context, 1);
        context.create_value(i1, None).id()
    }

    fn terminate(block: &Arc<Block>, op: impl Operation) {
        IRBuilder::new(block.clone()).insert(op);
    }

    #[test]
    fn diamond_then_else_get_fact_join_does_not() {
        let context = Context::with_default_dialects();
        let c = cond(&context);

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);
        let merge = context.create_block(vec![]);
        for block in [&entry, &t, &f, &merge] {
            region.add_block(block.id());
        }

        terminate(
            &entry,
            ops::cond_br(&context, c, vec![], vec![], t.id(), f.id()).build(),
        );
        terminate(&t, ops::br(&context, vec![], merge.id()).build());
        terminate(&f, ops::br(&context, vec![], merge.id()).build());
        terminate(&merge, ops::r#return(&context, Operand::none()).build());

        let facts = analyze(&context, region.id());

        assert_eq!(
            facts.own_fact(t.id()),
            Some(EdgeFact {
                condition: c,
                holds: true
            })
        );
        assert_eq!(
            facts.own_fact(f.id()),
            Some(EdgeFact {
                condition: c,
                holds: false
            })
        );
        assert_eq!(
            facts.facts(t.id()),
            &[EdgeFact {
                condition: c,
                holds: true
            }]
        );
        assert_eq!(facts.own_fact(merge.id()), None);
        assert!(facts.facts(merge.id()).is_empty());
        // Entry has an implicit incoming edge, so no fact.
        assert_eq!(facts.own_fact(entry.id()), None);
    }

    #[test]
    fn nested_diamond_inherits_outer_then_own_ordered() {
        let context = Context::with_default_dialects();
        let c1 = cond(&context);
        let c2 = cond(&context);

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let outer_t = context.create_block(vec![]);
        let outer_f = context.create_block(vec![]);
        let inner_t = context.create_block(vec![]);
        let inner_f = context.create_block(vec![]);
        for block in [&entry, &outer_t, &outer_f, &inner_t, &inner_f] {
            region.add_block(block.id());
        }

        terminate(
            &entry,
            ops::cond_br(&context, c1, vec![], vec![], outer_t.id(), outer_f.id()).build(),
        );
        terminate(
            &outer_t,
            ops::cond_br(&context, c2, vec![], vec![], inner_t.id(), inner_f.id()).build(),
        );
        terminate(&outer_f, ops::r#return(&context, Operand::none()).build());
        terminate(&inner_t, ops::r#return(&context, Operand::none()).build());
        terminate(&inner_f, ops::r#return(&context, Operand::none()).build());

        let facts = analyze(&context, region.id());

        assert_eq!(
            facts.facts(inner_t.id()),
            &[
                EdgeFact {
                    condition: c1,
                    holds: true
                },
                EdgeFact {
                    condition: c2,
                    holds: true
                },
            ]
        );
        assert_eq!(
            facts.own_fact(inner_t.id()),
            Some(EdgeFact {
                condition: c2,
                holds: true
            })
        );
    }

    #[test]
    fn loop_header_back_edge_gets_no_fact() {
        let context = Context::with_default_dialects();
        let c = cond(&context);

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let header = context.create_block(vec![]);
        let body = context.create_block(vec![]);
        let exit = context.create_block(vec![]);
        for block in [&entry, &header, &body, &exit] {
            region.add_block(block.id());
        }

        terminate(&entry, ops::br(&context, vec![], header.id()).build());
        terminate(
            &header,
            ops::cond_br(&context, c, vec![], vec![], body.id(), exit.id()).build(),
        );
        terminate(&body, ops::br(&context, vec![], header.id()).build());
        terminate(&exit, ops::r#return(&context, Operand::none()).build());

        let facts = analyze(&context, region.id());

        // Two incoming edges (entry, back edge) disqualify the header.
        assert_eq!(facts.own_fact(header.id()), None);
        assert!(facts.facts(header.id()).is_empty());
    }

    #[test]
    fn single_pred_unguarded_edge_gets_no_fact() {
        let context = Context::with_default_dialects();

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let next = context.create_block(vec![]);
        for block in [&entry, &next] {
            region.add_block(block.id());
        }

        terminate(&entry, ops::br(&context, vec![], next.id()).build());
        terminate(&next, ops::r#return(&context, Operand::none()).build());

        let facts = analyze(&context, region.id());
        assert_eq!(facts.own_fact(next.id()), None);
        assert!(facts.facts(next.id()).is_empty());
    }

    #[test]
    fn region_entry_excluded() {
        let context = Context::with_default_dialects();
        let c = cond(&context);

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        region.add_block(entry.id());

        let then_region = context.create_region();
        let then_block = context.create_block(vec![]);
        then_region.add_block(then_block.id());
        terminate(
            &then_block,
            crate::scf::ops::r#yield(&context, Operand::none()).build(),
        );
        let then_entry = then_block.id();

        let if_op = crate::scf::ops::r#if(&context, c, None, Some(then_region.id()), None).build();
        let mut builder = IRBuilder::new(entry.clone());
        builder.insert(if_op);
        builder.insert(ops::r#return(&context, Operand::none()).build());

        let facts = analyze(&context, region.id());
        // The nested region's entry has an implicit incoming edge.
        assert_eq!(facts.own_fact(then_entry), None);
        assert!(facts.facts(then_entry).is_empty());
    }

    #[test]
    fn cond_br_identical_successors_gets_no_fact() {
        let context = Context::with_default_dialects();
        let c = cond(&context);

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let target = context.create_block(vec![]);
        for block in [&entry, &target] {
            region.add_block(block.id());
        }

        terminate(
            &entry,
            ops::cond_br(&context, c, vec![], vec![], target.id(), target.id()).build(),
        );
        terminate(&target, ops::r#return(&context, Operand::none()).build());

        let facts = analyze(&context, region.id());
        // Both guarded edges land on `target`: two in-edges, so no single fact.
        assert_eq!(facts.own_fact(target.id()), None);
        assert!(facts.facts(target.id()).is_empty());
    }
}
