//! Destruction of unordered regions into a CFG, demand preserved.
//!
//! A callable whose body is an unordered region becomes blocks. Every
//! operation the region's results demand is moved, never copied, into a block,
//! in topological order, and a [`Gamma`] or [`Theta`] met on the way becomes
//! the blocks its evaluation rule names.
//!
//! A gate becomes the chain of tests its arms describe, each arm a block
//! entered on its ports, all of them leaving for the merge block that adopts
//! the gate's results. A loop becomes a header entered on the ports, holding
//! the predicate's cone and what both the continue and the exit cone demand;
//! the header branches to a continue block holding what only the continue cone
//! demands, which jumps back, and to an exit block holding what only the exit
//! cone demands, which jumps to the merge block adopting the loop's results.
//! An operation in no cone is never run and is not placed. Hoisting what both
//! cones demand above the branch speculates nothing: it runs once whichever
//! way the branch goes, exactly as the definitional semantics run it.
//!
//! The values keep their identity: a port becomes an argument of the block
//! entering the region, a result the argument of the block continuing after
//! the operation. Nothing is renamed, so readers already placed go on naming
//! what they read.
//!
//! What the blocks are joined by is the caller's choice, through [`Edges`]:
//! the `cfg` dialect for core IR, a target's branches once the region holds
//! machine operations.

use std::collections::HashSet;

use crate::analysis::AnalysisManager;
use crate::attributes::{AttributeValue, Predicate};
use crate::builtin::{CmpIOpBuilder, ConstantOpBuilder, IntegerType};
use crate::cfg::{BranchOpBuilder, CondBranchOpBuilder};
use crate::func::{FuncOp, ReturnOpBuilder};
use crate::region::{topological_order, values_read};
use crate::{
    BlockId, Context, Gamma, OpHandle, OpId, Operation, OperationRef, Pass, PassError, PassTarget,
    RegionId, Rewriter, Theta, ValueId,
};

/// The test a branch decides.
#[derive(Clone, Copy, Debug)]
pub enum Test {
    /// Whether `gate` selects its arm `index`: the predicate equals the index,
    /// or, for the last tested arm of a chain, whatever is left.
    Arm(usize),
    /// Whether the loop iterates again: its predicate result holds.
    Repeat,
}

/// An edge into `dest`, entering it on `args`, dependencies last.
#[derive(Clone, Debug)]
pub struct Edge {
    pub dest: BlockId,
    pub args: Vec<ValueId>,
}

impl Edge {
    pub fn to(dest: BlockId) -> Self {
        Self {
            dest,
            args: Vec::new(),
        }
    }

    pub fn with(dest: BlockId, args: &[ValueId]) -> Self {
        Self {
            dest,
            args: args.to_vec(),
        }
    }
}

/// How blocks are joined: the branch and jump operations of the IR the region
/// holds, and how a callable's body hands its results back.
pub trait Edges {
    /// End `block` with a jump along `edge`.
    fn jump(&self, block: BlockId, edge: &Edge);

    /// End `block` with a branch deciding `test` of `op`: along `taken` when
    /// it holds, along `fallthrough` otherwise.
    fn branch(
        &self,
        block: BlockId,
        op: &OpHandle,
        test: Test,
        taken: &Edge,
        fallthrough: &Edge,
    ) -> Result<(), PassError>;

    /// End `block` by leaving the callable with `values` and `deps`.
    fn leave(&self, block: BlockId, values: &[ValueId], deps: &[ValueId]) -> Result<(), PassError>;
}

/// The `cfg` dialect's edges over core IR.
pub struct CfgEdges<'a> {
    pub context: &'a Context,
}

impl CfgEdges<'_> {
    fn cond_br(&self, block: BlockId, condition: ValueId, taken: &Edge, fallthrough: &Edge) {
        let (taken_values, taken_deps) = self.split(&taken.args);
        let (fallthrough_values, fallthrough_deps) = self.split(&fallthrough.args);
        let op = CondBranchOpBuilder::new(self.context)
            .condition(condition)
            .true_args(taken_values)
            .false_args(fallthrough_values)
            .attr("true_dest", AttributeValue::Block(taken.dest))
            .attr("false_dest", AttributeValue::Block(fallthrough.dest))
            .build();
        for dep in taken_deps.into_iter().chain(fallthrough_deps) {
            self.context.append_dep_operand(op.id(), dep);
        }
        self.context.get_block(block).append(op.id());
    }

    /// The values of `args` apart from the dependencies among them.
    fn split(&self, args: &[ValueId]) -> (Vec<ValueId>, Vec<ValueId>) {
        args.iter()
            .partition(|&&arg| !self.context.get_value(arg).is_dependency())
    }
}

impl Edges for CfgEdges<'_> {
    fn jump(&self, block: BlockId, edge: &Edge) {
        let (values, deps) = self.split(&edge.args);
        let op = BranchOpBuilder::new(self.context)
            .dest_args(values)
            .attr("dest", AttributeValue::Block(edge.dest))
            .build();
        for dep in deps {
            self.context.append_dep_operand(op.id(), dep);
        }
        self.context.get_block(block).append(op.id());
    }

    fn branch(
        &self,
        block: BlockId,
        op: &OpHandle,
        test: Test,
        taken: &Edge,
        fallthrough: &Edge,
    ) -> Result<(), PassError> {
        let (condition, holds) = match test {
            Test::Repeat => (theta(op)?.predicate(), true),
            Test::Arm(index) => {
                let predicate = gamma(op)?.predicate();
                let ty = self.context.get_value(predicate).ty();
                if ty == IntegerType::new(self.context, 1) {
                    (predicate, index == 1)
                } else {
                    let holder = self.context.get_block(block);
                    let index = holder.append_op(
                        ConstantOpBuilder::new(self.context)
                            .value(index as i64)
                            .result_type(ty)
                            .build(),
                    );
                    let equal = holder.append_op(
                        CmpIOpBuilder::new(self.context)
                            .lhs(predicate)
                            .rhs(index.result())
                            .predicate(Predicate::Eq)
                            .result_type(IntegerType::new(self.context, 1))
                            .build(),
                    );
                    (equal.result(), true)
                }
            }
        };
        if holds {
            self.cond_br(block, condition, taken, fallthrough);
        } else {
            self.cond_br(block, condition, fallthrough, taken);
        }
        Ok(())
    }

    fn leave(&self, block: BlockId, values: &[ValueId], deps: &[ValueId]) -> Result<(), PassError> {
        let mut builder = ReturnOpBuilder::new(self.context);
        if let Some(&value) = values.first() {
            builder = builder.value(value);
        }
        for &dep in deps {
            builder = builder.dep_operand(dep);
        }
        self.context.get_block(block).append(builder.build().id());
        Ok(())
    }
}

fn theta(op: &OpHandle) -> Result<Box<dyn Theta>, PassError> {
    op.clone()
        .as_interface::<dyn Theta>()
        .ok_or_else(|| decline(op, "not a loop"))
}

fn gamma(op: &OpHandle) -> Result<Box<dyn Gamma>, PassError> {
    op.clone()
        .as_interface::<dyn Gamma>()
        .ok_or_else(|| decline(op, "not a gate"))
}

fn is_structured(op: &OpHandle) -> bool {
    op.clone().as_interface::<dyn Theta>().is_some()
        || op.clone().as_interface::<dyn Gamma>().is_some()
}

fn decline(op: &OpHandle, reason: &str) -> PassError {
    PassError::InvalidRuleSet(format!("cannot destructure {}: {reason}", op.name()))
}

/// Where a loop's blocks went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopBlocks {
    /// Entered on the ports; decides whether to iterate.
    pub header: BlockId,
    /// Holds what only the next iteration demands; jumps back to the header.
    pub continue_: BlockId,
    /// Entered on the loop's results.
    pub merge: BlockId,
}

/// Where a gate's blocks went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GateBlocks {
    /// Holds the first test of the chain.
    pub head: BlockId,
    /// Entered on the gate's results.
    pub merge: BlockId,
}

/// The structure a destructured callable had, by the blocks that survive it.
#[derive(Debug, Default)]
pub struct Destructured {
    pub loops: Vec<LoopBlocks>,
    pub gates: Vec<GateBlocks>,
}

/// Turn `region`, a callable's body, into blocks joined by `edges`. An
/// unordered region becomes blocks outright, the first entered on its ports;
/// an ordered one keeps its blocks, each structured operation in them
/// replaced by the blocks it stands for.
pub fn destructure(
    context: &Context,
    rewriter: &mut Rewriter,
    region: RegionId,
    edges: &dyn Edges,
) -> Result<Destructured, PassError> {
    let mut lowering = Lowering {
        context,
        edges,
        blocks: Vec::new(),
        record: Destructured::default(),
    };
    let handle = context.get_region(region);
    if !handle.is_nodes() {
        for block in handle.block_ids() {
            lowering.ordered(rewriter, block)?;
        }
        for block in lowering.blocks {
            context.get_region(region).add_block(block);
        }
        return Ok(lowering.record);
    }
    let entry = lowering.entered_on(&handle.ports());
    lowering.blocks.push(entry);
    let (values, deps) = (handle.value_results(), handle.dep_results());
    let last = lowering.region(rewriter, region, entry)?;
    edges.leave(last, &values, &deps)?;
    context.replace_region_with_blocks(region, lowering.blocks);
    Ok(lowering.record)
}

struct Lowering<'a> {
    context: &'a Context,
    edges: &'a dyn Edges,
    blocks: Vec<BlockId>,
    record: Destructured,
}

impl Lowering<'_> {
    fn block(&mut self) -> BlockId {
        let block = self.context.create_block(vec![]).id();
        self.blocks.push(block);
        block
    }

    /// A block adopting `values` as its arguments, dependencies among them
    /// placed as such. Not listed yet: a merge block is listed after the
    /// blocks it joins, so that every block follows the ones dominating it.
    fn entered_on(&mut self, values: &[crate::Value]) -> BlockId {
        let block = self.context.create_block(vec![]).id();
        for value in values {
            if value.is_dependency() {
                self.context.adopt_dep_block_argument(block, value.id());
            } else {
                self.context.adopt_block_argument(block, value.id());
            }
        }
        block
    }

    /// Place everything `region`'s results demand from `block` on; the block
    /// the results are available in.
    fn region(
        &mut self,
        rewriter: &mut Rewriter,
        region: RegionId,
        block: BlockId,
    ) -> Result<BlockId, PassError> {
        let results = self.context.get_region(region).results();
        let demanded = self.cone(region, &results);
        let order = self.order(region)?;
        self.ops(rewriter, region, &order, &demanded, block)
    }

    fn order(&self, region: RegionId) -> Result<Vec<OpId>, PassError> {
        topological_order(self.context, region)
            .map_err(|error| PassError::InvalidRuleSet(error.to_string()))
    }

    /// The operations of `region` that computing `roots` demands.
    fn cone(&self, region: RegionId, roots: &[ValueId]) -> HashSet<OpId> {
        let mut cone = HashSet::new();
        let mut pending: Vec<ValueId> = roots.to_vec();
        while let Some(value) = pending.pop() {
            let Some(op) = self.context.get_value(value).defining_op() else {
                continue;
            };
            if self.context.parent_nodes_region(op) != Some(region) || !cone.insert(op) {
                continue;
            }
            pending.extend(values_read(self.context, op));
        }
        cone
    }

    /// Move the operations of `region` that are in `placed`, in `order`, into
    /// `block` and whatever blocks a structured one among them opens.
    fn ops(
        &mut self,
        rewriter: &mut Rewriter,
        region: RegionId,
        order: &[OpId],
        placed: &HashSet<OpId>,
        mut block: BlockId,
    ) -> Result<BlockId, PassError> {
        for &op_id in order.iter().filter(|op| placed.contains(op)) {
            let op = self.context.get_op(op_id);
            if is_structured(&op) {
                let merge = self.entered_on(&self.values(&op.results()));
                self.structured(rewriter, &op, block, merge)?;
                self.blocks.push(merge);
                block = merge;
            } else {
                self.context.remove_from_region(region, op_id);
                self.context.get_block(block).append(op_id);
            }
        }
        Ok(block)
    }

    /// Replace each structured operation of the ordered `block` by its blocks,
    /// the rest of the block continuing after them on the operation's results.
    fn ordered(&mut self, rewriter: &mut Rewriter, block: BlockId) -> Result<(), PassError> {
        let mut block = block;
        loop {
            let ops = self.context.get_block(block).op_ids();
            let Some(position) = ops
                .iter()
                .position(|&op| is_structured(&self.context.get_op(op)))
            else {
                return Ok(());
            };
            let op = self.context.get_op(ops[position]);
            let merge = rewriter.split_block(block, position + 1).id();
            for result in op.value_results() {
                self.context.adopt_block_argument(merge, result);
            }
            for result in op.dep_results() {
                self.context.adopt_dep_block_argument(merge, result);
            }
            self.structured(rewriter, &op, block, merge)?;
            self.blocks.push(merge);
            block = merge;
        }
    }

    fn structured(
        &mut self,
        rewriter: &mut Rewriter,
        op: &OpHandle,
        block: BlockId,
        merge: BlockId,
    ) -> Result<(), PassError> {
        if op.clone().as_interface::<dyn Theta>().is_some() {
            self.theta(rewriter, op, block, merge)
        } else {
            self.gamma(rewriter, op, block, merge)
        }
    }

    fn values(&self, ids: &[ValueId]) -> Vec<crate::Value> {
        ids.iter().map(|&id| self.context.get_value(id)).collect()
    }

    /// The block a loop's cone runs in, entered on every memory state the cone
    /// reads from the header: the states are renamed on the way in, so that
    /// what a block consumes is defined in that block, as in any threaded CFG.
    /// Answers the block and what the edge into it carries; `leaving` is
    /// renamed along with the cone's operations.
    fn cone_block(
        &mut self,
        cone: &HashSet<OpId>,
        leaving: &mut [ValueId],
    ) -> (BlockId, Vec<ValueId>) {
        let block = self.block();
        let mut entered = Vec::new();
        let mut renames: Vec<(ValueId, ValueId)> = Vec::new();
        let mut read: Vec<ValueId> = cone
            .iter()
            .flat_map(|&op| values_read(self.context, op))
            .chain(leaving.iter().copied())
            .filter(|&value| self.context.get_value(value).is_dependency())
            .filter(|&value| {
                self.context
                    .get_value(value)
                    .defining_op()
                    .is_none_or(|def| !cone.contains(&def))
            })
            .collect();
        read.sort();
        read.dedup();
        for value in read {
            let argument = self.context.append_dep_block_argument(block).id();
            entered.push(value);
            renames.push((value, argument));
        }
        for &op in cone {
            rename_within(self.context, op, &renames);
        }
        for value in leaving.iter_mut() {
            if let Some(&(_, new)) = renames.iter().find(|(old, _)| old == value) {
                *value = new;
            }
        }
        (block, entered)
    }

    fn gamma(
        &mut self,
        rewriter: &mut Rewriter,
        op: &OpHandle,
        block: BlockId,
        merge: BlockId,
    ) -> Result<(), PassError> {
        let gamma = gamma(op)?;
        let binding = gamma.forwarded();
        let mut inputs = op.value_operands()[binding.operands.clone()].to_vec();
        inputs.extend(op.dep_operands());

        let mut arms = Vec::new();
        for arm in gamma.arms() {
            let entry = self.entered_on(&self.context.get_region(arm).ports());
            self.blocks.push(entry);
            let results = self.context.get_region(arm).results();
            let last = self.region(rewriter, arm, entry)?;
            self.edges.jump(last, &Edge::with(merge, &results));
            arms.push(entry);
        }

        let last = arms.len() - 1;
        let mut current = block;
        for index in 0..last {
            let next = if index + 1 == last {
                Edge::with(arms[last], &inputs)
            } else {
                Edge::to(self.block())
            };
            let arm = Edge::with(arms[index], &inputs);
            self.edges
                .branch(current, op, Test::Arm(index), &arm, &next)?;
            current = next.dest;
        }
        if last == 0 {
            self.edges.jump(current, &Edge::with(arms[0], &inputs));
        }
        self.record.gates.push(GateBlocks { head: block, merge });
        rewriter.erase_op(&OperationRef::new(op.clone()))
    }

    fn theta(
        &mut self,
        rewriter: &mut Rewriter,
        op: &OpHandle,
        block: BlockId,
        merge: BlockId,
    ) -> Result<(), PassError> {
        let theta = theta(op)?;
        let binding = theta.carried();
        let body = theta.body();
        let handle = self.context.get_region(body);
        let mut inits = op.value_operands()[binding.operands.clone()].to_vec();
        inits.extend(op.dep_operands());
        let header = self.entered_on(&handle.ports());
        self.blocks.push(header);
        self.edges.jump(block, &Edge::with(header, &inits));

        let values = handle.value_results();
        let deps = handle.dep_results();
        let chains = deps.len() / 2;
        let mut continue_values = values[binding.continue_.clone()].to_vec();
        continue_values.extend(&deps[..chains]);
        let mut exit_values = values[binding.exit.clone()].to_vec();
        exit_values.extend(&deps[chains..]);

        let predicate = self.cone(body, &[theta.predicate()]);
        let continue_cone = self.cone(body, &continue_values);
        let exit_cone = self.cone(body, &exit_values);
        let header_ops: HashSet<OpId> = predicate
            .iter()
            .copied()
            .chain(continue_cone.intersection(&exit_cone).copied())
            .collect();
        let continue_only: HashSet<OpId> = continue_cone.difference(&header_ops).copied().collect();
        let exit_only: HashSet<OpId> = exit_cone.difference(&header_ops).copied().collect();
        let order = self.order(body)?;

        let header_end = self.ops(rewriter, body, &order, &header_ops, header)?;
        let (continue_, continue_entered) = self.cone_block(&continue_only, &mut continue_values);
        let continue_end = self.ops(rewriter, body, &order, &continue_only, continue_)?;
        self.edges
            .jump(continue_end, &Edge::with(header, &continue_values));
        let (exit, exit_entered) = self.cone_block(&exit_only, &mut exit_values);
        let exit_end = self.ops(rewriter, body, &order, &exit_only, exit)?;
        self.edges.jump(exit_end, &Edge::with(merge, &exit_values));
        self.edges.branch(
            header_end,
            op,
            Test::Repeat,
            &Edge::with(continue_, &continue_entered),
            &Edge::with(exit, &exit_entered),
        )?;

        self.record.loops.push(LoopBlocks {
            header,
            continue_,
            merge,
        });
        rewriter.erase_op(&OperationRef::new(op.clone()))
    }
}

/// Rename the reads of `op` and of everything nested in it.
fn rename_within(context: &Context, op: OpId, renames: &[(ValueId, ValueId)]) {
    let instance = context.get_op(op);
    for (index, operand) in instance.operands().iter().enumerate() {
        if let Some(&(_, new)) = renames.iter().find(|(old, _)| old == operand) {
            context.set_op_operand(op, index, new);
        }
    }
    for region in instance.regions() {
        let handle = context.get_region(region);
        let results: Vec<ValueId> = handle
            .results()
            .iter()
            .map(|result| {
                renames
                    .iter()
                    .find(|(old, _)| old == result)
                    .map_or(*result, |&(_, new)| new)
            })
            .collect();
        if results != handle.results() {
            let deps = handle.dep_results().len();
            context.set_region_results(region, results, deps);
        }
        for child in handle.op_ids() {
            rename_within(context, child, renames);
        }
    }
}

/// `destructure`: a callable's unordered body becomes `cfg` blocks.
pub struct DestructurePass;

impl DestructurePass {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DestructurePass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(DestructurePass, "destructure");

impl Pass for DestructurePass {
    fn name(&self) -> &'static str {
        "destructure"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        destructure(context, rewriter, body, &CfgEdges { context })?;
        Ok(())
    }
}
