//! Weighted path chaining over the finalized machine CFG. The objective is the
//! sum of edge affinities whose transfers can become fallthroughs. Static
//! affinities use the allocator's loop-depth prior; they are not profile counts.
//! Instructions and labeled edges stay explicit, and the object writer checks
//! the proposed order before the region permutation becomes visible.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use tir::{
    AnalysisManager, BlockId, Context, OperationRef, Pass, PassError, PassTarget, Terminator,
};

use crate::backend::binary::{FixupTarget, ObjectFormatInfo, encode_with, valid_block_layout};
use crate::backend::{
    ASSIGNMENT_ATTR, ControlFlow, MachineInstruction, RegAssignment, SymbolOp, branch_successors,
    fallthrough_branch, symbol_body_blocks,
};

const MAX_BLOCKS: usize = 4096;
const MAX_EDGES: usize = 16384;
const WEIGHT_SCALE: u64 = 1024;

/// Orders finalized machine blocks to make weighted unconditional edges fall through.
#[derive(Clone)]
pub struct MachineBlockLayoutPass {
    format: ObjectFormatInfo,
}

impl MachineBlockLayoutPass {
    /// Use the selected target's object format to check branch encodability.
    pub fn new(format: ObjectFormatInfo) -> Self {
        Self { format }
    }

    fn parse(arguments: &str) -> Result<Self, String> {
        let target = arguments.trim();
        if target.is_empty() {
            return Err(
                "machine-block-layout requires a target, for example machine-block-layout<x86_64>"
                    .into(),
            );
        }
        let target = crate::backend::select_target(target, None, None)?;
        let format = target
            .object_format()
            .ok_or_else(|| "target does not support object emission".to_string())?;
        Ok(Self::new(format))
    }
}

crate::register_pass!(
    MachineBlockLayoutPass,
    "machine-block-layout",
    MachineBlockLayoutPass::parse
);

impl Pass for MachineBlockLayoutPass {
    fn name(&self) -> &'static str {
        "machine-block-layout"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let blocks = symbol_body_blocks(context, op.op());
        if blocks.len() < 3 || blocks.len() > MAX_BLOCKS {
            return Ok(());
        }
        let assignment = RegAssignment::of_op(op.op(), ASSIGNMENT_ATTR);
        if !layout_supported(context, &blocks, &assignment) {
            return Ok(());
        }

        let successors = successors(context, &blocks);
        let edge_count: usize = successors.values().map(Vec::len).sum();
        if edge_count > MAX_EDGES {
            return Ok(());
        }
        let reachable = reachable_blocks(blocks[0], &successors);
        let depths = super::machine_cfg::loop_depths(&blocks, &successors);
        let ordinal: HashMap<_, _> = blocks
            .iter()
            .enumerate()
            .map(|(index, &block)| (block, index))
            .collect();

        let mut edges = Vec::new();
        for &block in &blocks {
            if !reachable.contains(&block) {
                continue;
            }
            let outgoing = &successors[&block];
            let block_weight = WEIGHT_SCALE
                .saturating_mul(10_u64.saturating_pow(depths[&block].min(9)))
                / outgoing.len().max(1) as u64;
            for &successor in outgoing {
                if fallthrough_branch(context, block, successor).is_some() {
                    edges.push(Edge {
                        source: block,
                        target: successor,
                        weight: block_weight,
                        source_ordinal: ordinal[&block],
                        target_ordinal: ordinal[&successor],
                    });
                }
            }
        }
        edges.sort_unstable_by_key(|edge| {
            (
                Reverse(edge.weight),
                edge.source_ordinal,
                edge.target_ordinal,
            )
        });

        let mut chains = pinned_chains(context, &blocks);
        for edge in &edges {
            let Some(source_chain) = chains
                .iter()
                .position(|chain| chain.last() == Some(&edge.source))
            else {
                continue;
            };
            let Some(target_chain) = chains
                .iter()
                .position(|chain| chain.first() == Some(&edge.target))
            else {
                continue;
            };
            if source_chain == target_chain || chains[target_chain].contains(&blocks[0]) {
                continue;
            }
            let tail = chains.remove(target_chain);
            let source_chain = source_chain - usize::from(target_chain < source_chain);
            chains[source_chain].extend(tail);
        }

        let final_block_needs_end = requires_original_successor(context, *blocks.last().unwrap());
        chains.sort_by_key(|chain| {
            let class = if chain.contains(&blocks[0]) {
                0
            } else if final_block_needs_end && chain.contains(blocks.last().unwrap()) {
                2
            } else {
                1
            };
            (
                class,
                chain.iter().map(|block| ordinal[block]).min().unwrap(),
            )
        });
        let candidate: Vec<_> = chains.into_iter().flatten().collect();
        if (final_block_needs_end && candidate.last() != blocks.last())
            || score(&candidate, &edges) <= score(&blocks, &edges)
            || !valid_block_layout(context, op.op(), &candidate, &self.format)
        {
            return Ok(());
        }

        context
            .reorder_region_blocks(op.op().regions()[0], candidate)
            .map_err(PassError::InvalidRuleSet)
    }
}

#[derive(Clone, Copy)]
struct Edge {
    source: BlockId,
    target: BlockId,
    weight: u64,
    source_ordinal: usize,
    target_ordinal: usize,
}

fn successors(context: &Context, blocks: &[BlockId]) -> HashMap<BlockId, Vec<BlockId>> {
    let members: HashSet<_> = blocks.iter().copied().collect();
    blocks
        .iter()
        .copied()
        .map(|block| {
            let mut outgoing = Vec::new();
            for op_id in context.get_block(block).op_ids() {
                let op = context.get_op(op_id);
                if op
                    .clone()
                    .as_interface::<dyn MachineInstruction>()
                    .is_none()
                {
                    continue;
                }
                for successor in branch_successors(op.as_dyn_op().as_ref()) {
                    if members.contains(&successor) && !outgoing.contains(&successor) {
                        outgoing.push(successor);
                    }
                }
            }
            (block, outgoing)
        })
        .collect()
}

fn reachable_blocks(
    entry: BlockId,
    successors: &HashMap<BlockId, Vec<BlockId>>,
) -> HashSet<BlockId> {
    let mut reachable = HashSet::new();
    let mut pending = vec![entry];
    while let Some(block) = pending.pop() {
        if reachable.insert(block) {
            pending.extend(successors[&block].iter().copied());
        }
    }
    reachable
}

fn pinned_chains(context: &Context, blocks: &[BlockId]) -> Vec<Vec<BlockId>> {
    let mut chains = vec![vec![blocks[0]]];
    for pair in blocks.windows(2) {
        if requires_original_successor(context, pair[0]) {
            chains.last_mut().unwrap().push(pair[1]);
        } else {
            chains.push(vec![pair[1]]);
        }
    }
    chains
}

fn layout_supported(context: &Context, blocks: &[BlockId], assignment: &RegAssignment) -> bool {
    blocks.iter().all(|&block| {
        let final_instruction = final_instruction(context, block);
        let Some(final_op) = final_instruction.as_ref() else {
            return block != *blocks.last().unwrap();
        };
        for op_id in context.get_block(block).op_ids() {
            let op = context.get_op(op_id);
            let Some(instruction) = op.clone().as_interface::<dyn MachineInstruction>() else {
                continue;
            };
            if instruction.info().control_flow == ControlFlow::None
                || !branch_successors(op.clone().as_dyn_op().as_ref()).is_empty()
                || (op.id == final_op.id && safe_exit(context, &op))
                || instruction
                    .info()
                    .encode
                    .and_then(|spec| encode_with(&op, spec, assignment))
                    .is_some_and(|encoded| {
                        encoded
                            .fixups
                            .iter()
                            .any(|fixup| matches!(fixup.target, FixupTarget::Symbol(_)))
                    })
            {
                continue;
            }
            return false;
        }
        true
    })
}

fn requires_original_successor(context: &Context, block: BlockId) -> bool {
    let Some(op) = final_instruction(context, block) else {
        return true;
    };
    let successors = branch_successors(op.clone().as_dyn_op().as_ref());
    if successors.len() == 1 && fallthrough_branch(context, block, successors[0]).is_some() {
        return false;
    }
    !safe_exit(context, &op)
}

fn final_instruction(context: &Context, block: BlockId) -> Option<tir::OpHandle> {
    context
        .get_block(block)
        .op_ids()
        .into_iter()
        .rev()
        .map(|id| context.get_op(id))
        .find(|op| {
            matches!(
                crate::backend::asm_item(op),
                crate::backend::AsmItem::Instruction
            )
        })
}

fn safe_exit(context: &Context, op: &tir::OpHandle) -> bool {
    op.clone()
        .as_interface::<dyn MachineInstruction>()
        .is_some_and(|instruction| {
            instruction.info().control_flow == ControlFlow::Unconditional
                && !instruction.info().effects.writes
        })
        && crate::backend::reg_ports(op).is_empty()
        && op
            .attributes()
            .iter()
            .all(|attribute| context.resolve(attribute.name) == "operand_segment_sizes")
        && op
            .clone()
            .as_interface::<dyn Terminator>()
            .is_some_and(|terminator| terminator.successors().is_empty())
}

fn score(order: &[BlockId], edges: &[Edge]) -> u64 {
    let positions: HashMap<_, _> = order
        .iter()
        .enumerate()
        .map(|(index, &block)| (block, index))
        .collect();
    edges
        .iter()
        .filter(|edge| positions[&edge.source] + 1 == positions[&edge.target])
        .map(|edge| edge.weight)
        .sum()
}
