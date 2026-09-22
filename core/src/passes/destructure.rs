//! Destruction of unordered regions into a CFG, demand preserved.
//!
//! [`recover_cfg`] normalizes a callable's semantic regions into private
//! computation fragments, control definitions, and typed boundaries. PREPARE
//! chooses a dependency order without mutating the IR. FINISH follows joint
//! selector facts and simultaneous bindings through [`Gamma`] and [`Theta`]
//! boundaries until it reaches executable fragments, then emits each fragment
//! once with the block parameters its incoming routes require.
//!
//! A Theta remains lazy. Its predicate and shared dependencies form the head
//! demand domain, while continue-only and exit-only computations remain on
//! their respective outcomes. A selector with an ordinary data use is tested
//! by a consumer-local control definition. Eligible control definitions own
//! their branches directly, so region nesting does not require synthetic merge
//! and dispatch blocks.
//!
//! [`recover_structured`] retains the structural lowering for consumers that
//! require merge records. It is separate from predicative recovery.
//!
//! What the blocks are joined by is the caller's choice, through [`Edges`]:
//! the `cfg` dialect for core IR, a target's branches once the region holds
//! machine operations.

mod recovery;

use std::collections::{HashMap, HashSet};

pub(crate) use recovery::RecoveryError;
pub use recovery::{
    ControlDefinition, ControlId, ControlKind, ControlOutcome, DemandDomainId, DemandDomainKind,
    RecoveryPlan, ValueBinding,
};

use crate::analysis::AnalysisManager;
use crate::attributes::{AttributeValue, Predicate};
use crate::builtin::{CmpIOpBuilder, ConstantOp, ConstantOpBuilder, IntegerType, XOrIOp};
use crate::cfg::{BranchOpBuilder, CondBranchOpBuilder};
use crate::func::{FuncOp, ReturnOp, ReturnOpBuilder};
use crate::region::values_read;
use crate::{
    BlockId, Context, Gamma, OpHandle, OpId, Operation, OperationRef, Pass, PassError, PassTarget,
    RegionId, Theta, ValueId,
};

/// The test a branch decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Test {
    /// Whether `gate` selects its arm `index`: the predicate equals the index,
    /// or, for the last tested arm of a chain, whatever is left.
    Arm(usize),
    /// Whether the loop iterates again: its predicate result holds.
    Repeat,
}

/// An edge into `dest`, entering it on `args`, states last.
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
    /// it holds, along `fallthrough` otherwise. A branch needing a block of
    /// its own on the way takes one from `mint`, which lists it.
    fn branch(
        &self,
        block: BlockId,
        op: &OpHandle,
        test: Test,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError>;

    /// End `block` with the branch owned by a recovery control definition.
    /// `predicate` is the value after FINISH has applied region bindings. It
    /// can differ from the source predicate recorded in `control`.
    #[allow(clippy::too_many_arguments)]
    fn branch_control(
        &self,
        _control: &ControlDefinition,
        _outcome: usize,
        _predicate: ValueId,
        _bindings: &[ValueBinding],
        _block: BlockId,
        _taken: &Edge,
        _fallthrough: &Edge,
        _mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        Err(PassError::InvalidRuleSet(
            "this edge adapter does not implement producer-owned recovery control".into(),
        ))
    }

    /// End `block` by leaving the callable with `values` and `deps`.
    fn leave(&self, block: BlockId, values: &[ValueId], deps: &[ValueId]) -> Result<(), PassError>;

    /// Whether `test` of `op` is already decided: the edge it picks is taken
    /// outright, and an arm no edge reaches is never placed.
    fn decided(&self, _op: &OpHandle, _test: Test) -> Option<bool> {
        None
    }

    /// The selected outcome of a recovery control, when selection proved it.
    fn decided_control(&self, _control: &ControlDefinition, _outcome: usize) -> Option<bool> {
        None
    }

    /// What deciding `test` of `op` reads besides the operation's own operands
    /// and results: a machine test bound to registers its region's tiles
    /// define, which are demanded along with the test.
    fn test_reads(&self, _op: &OpHandle, _test: Test) -> Vec<ValueId> {
        Vec::new()
    }

    /// Every value the selected recovery control reads. Generic CFG recovery
    /// reads the current structured predicate plus [`Edges::test_reads`]. A
    /// target override names the complete selected branch input set instead;
    /// a fused branch need not retain an erased source predicate.
    fn control_reads(&self, control: &ControlDefinition, _outcome: usize) -> Vec<ValueId> {
        vec![control.source_predicate]
    }

    /// Map semantic source values to the values emitted by selection.
    fn value(&self, source: ValueId) -> ValueId {
        source
    }

    /// Whether selection changed a semantic value's representation type.
    /// Generic recovery requires the original type; target adapters may name
    /// register-class representations selected by their instruction rules.
    fn compatible_type(&self, source: crate::TypeId, selected: crate::TypeId) -> bool {
        source == selected
    }

    /// Whether an instruction was selected by a formal constant-materializer
    /// rule. Its fixed-register reads may include a target's hardwired zero.
    fn is_literal(&self, _op: OpId) -> bool {
        false
    }

    /// The recovery demand domain assigned to a selected operation.
    fn execution_domain(&self, _op: OpId) -> Option<DemandDomainId> {
        None
    }

    /// Operations `op` runs after besides those defining what it reads: a
    /// machine instruction's implicit register inputs, defined by an
    /// instruction selection put ahead of it. They are demanded along with
    /// `op`, and the region's insertion order keeps them ahead of it.
    fn implicit_inputs(&self, _op: OpId) -> Vec<OpId> {
        Vec::new()
    }
}

/// The `cfg` dialect's edges over core IR.
pub struct CfgEdges<'a> {
    pub context: &'a Context,
}

impl CfgEdges<'_> {
    fn cond_br(&self, block: BlockId, condition: ValueId, taken: &Edge, fallthrough: &Edge) {
        let op = CondBranchOpBuilder::new(self.context)
            .condition(condition)
            .true_args(values_then_states(self.context, &taken.args))
            .false_args(values_then_states(self.context, &fallthrough.args))
            .true_dest(taken.dest)
            .false_dest(fallthrough.dest)
            .build();
        self.context.get_block(block).append(op.id());
    }

    fn branch_value(
        &self,
        block: BlockId,
        predicate: ValueId,
        test: Test,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) {
        let (condition, holds) = match test {
            Test::Repeat => {
                // Restructure negates a head-tested loop's exit into a tail
                // repeat (`xori(cmp, 1)`). Branch on the comparison with the
                // edges swapped instead of materializing the negation.
                match unnegate(self.context, predicate) {
                    Some(inner) => (inner, false),
                    None => (predicate, true),
                }
            }
            Test::Arm(index) => {
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
        // Two edges into one block carrying different values are two edges a
        // phi cannot tell apart, so one of them goes through a block of its own.
        let mut hop = taken.clone();
        if taken.dest == fallthrough.dest && taken.args != fallthrough.args {
            hop = Edge::to(mint());
            self.jump(hop.dest, taken);
        }
        if holds {
            self.cond_br(block, condition, &hop, fallthrough);
        } else {
            self.cond_br(block, condition, fallthrough, &hop);
        }
    }
}

impl Edges for CfgEdges<'_> {
    fn jump(&self, block: BlockId, edge: &Edge) {
        let op = BranchOpBuilder::new(self.context)
            .dest_args(values_then_states(self.context, &edge.args))
            .dest(edge.dest)
            .build();
        self.context.get_block(block).append(op.id());
    }

    fn branch(
        &self,
        block: BlockId,
        op: &OpHandle,
        test: Test,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        let predicate = match test {
            Test::Repeat => theta(op)?.predicate(),
            Test::Arm(_) => gamma(op)?.predicate(),
        };
        self.branch_value(block, predicate, test, taken, fallthrough, mint);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn branch_control(
        &self,
        control: &ControlDefinition,
        outcome: usize,
        predicate: ValueId,
        _bindings: &[ValueBinding],
        block: BlockId,
        taken: &Edge,
        fallthrough: &Edge,
        mint: &mut dyn FnMut() -> BlockId,
    ) -> Result<(), PassError> {
        let test = match control.outcomes.get(outcome) {
            Some(ControlOutcome::Exact(value)) => Test::Arm(*value as usize),
            Some(ControlOutcome::DefaultFrom(_))
                if control.predicate_type == IntegerType::new(self.context, 1) =>
            {
                Test::Repeat
            }
            _ => {
                return Err(PassError::InvalidRuleSet(
                    "generic CFG cannot emit a non-boolean default control outcome".into(),
                ));
            }
        };
        self.branch_value(block, predicate, test, taken, fallthrough, mint);
        Ok(())
    }

    fn leave(&self, block: BlockId, values: &[ValueId], deps: &[ValueId]) -> Result<(), PassError> {
        let mut builder = ReturnOpBuilder::new(self.context);
        if let Some(&value) = values.first() {
            builder = builder.value(value);
        }
        for &dep in deps {
            builder = builder.state(dep);
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

/// The comparison a negated loop predicate tests: restructure spells a
/// head-tested loop's repeat as `xori(cmp, 1)`.
pub(crate) fn unnegate(context: &Context, predicate: ValueId) -> Option<ValueId> {
    if context.get_value(predicate).ty() != IntegerType::new(context, 1) {
        return None;
    }
    let def = context.get_value(predicate).defining_op()?;
    let instance = context.get_op(def);
    if !instance.is::<XOrIOp>() {
        return None;
    }
    let operands = instance.operands();
    let (lhs, rhs) = (*operands.first()?, *operands.get(1)?);
    if is_one(context, lhs) {
        Some(rhs)
    } else if is_one(context, rhs) {
        Some(lhs)
    } else {
        None
    }
}

fn is_one(context: &Context, value: ValueId) -> bool {
    if context.get_value(value).ty() != IntegerType::new(context, 1) {
        return false;
    }
    let Some(def) = context.get_value(value).defining_op() else {
        return false;
    };
    let instance = context.get_op(def);
    instance.is::<ConstantOp>() && matches!(instance.attr("value"), Some(AttributeValue::Int(1)))
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
    /// Holds what only the next iteration demands and jumps back to the
    /// header; the header itself when the next iteration demands nothing more.
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

/// Explicit compatibility entry point for consumers that require structured
/// merge and loop records, such as SPIR-V emission.
pub fn recover_structured(
    context: &Context,
    region: RegionId,
    edges: &dyn Edges,
) -> Result<Destructured, PassError> {
    lower(context, region, edges)
}

/// Analyze and recover a generic CFG with predicate-owned control.
pub fn recover_cfg(
    context: &Context,
    region: RegionId,
    edges: &dyn Edges,
) -> Result<(), PassError> {
    if !context.get_region(region).is_nodes() {
        let blocks = context.get_region(region).block_ids();
        let has_structured = blocks.iter().any(|&block| {
            context
                .get_block(block)
                .op_ids()
                .iter()
                .any(|&op| is_structured(&context.get_op(op)))
        });
        if blocks.len() == 1 && has_structured {
            linear_ordered_to_nodes(context, region, blocks[0])?;
        } else {
            restructure_ordered_root(context, region)?;
        }
    }
    RecoveryPlan::analyze(context, region)?.emit(context, region, edges)
}

fn restructure_ordered_root(context: &Context, region: RegionId) -> Result<(), PassError> {
    let owner = context
        .get_region(region)
        .parent_op()
        .ok_or_else(|| PassError::InvalidRuleSet("ordered callable region has no owner".into()))?;
    let owner = context.get_op(owner);
    if !owner.is::<FuncOp>() || owner.regions().first() != Some(&region) {
        return Err(PassError::InvalidRuleSet(
            "ordered CFG recovery requires a function body".into(),
        ));
    }
    let mut restructure = crate::passes::restructure::RestructureNodesPass::new();
    restructure.run(&OperationRef::new(owner), context, &AnalysisManager::new())?;
    Ok(())
}

/// Convert the common ordered compatibility form, one block containing
/// semantic structured operations and a return, into the unordered form PCFR
/// analyzes. The block order is already represented by SSA and state chains;
/// this only changes ownership and the return boundary.
fn linear_ordered_to_nodes(
    context: &Context,
    region: RegionId,
    block: BlockId,
) -> Result<(), PassError> {
    let handle = context.get_block(block);
    let mut ops = handle.op_ids();
    let return_id = ops
        .pop()
        .ok_or_else(|| PassError::InvalidRuleSet("ordered callable block is empty".into()))?;
    let return_op = context.get_op(return_id);
    if !return_op.is::<ReturnOp>() {
        return Err(PassError::InvalidRuleSet(
            "ordered structured compatibility requires a func.return terminator".into(),
        ));
    }
    let old_ports = handle.arguments();
    let ports: Vec<crate::Value> = old_ports
        .iter()
        .map(|port| context.create_value(port.ty(), None))
        .collect();
    let renames: Vec<(ValueId, ValueId)> = old_ports
        .iter()
        .zip(&ports)
        .map(|(old, new)| (old.id(), new.id()))
        .collect();
    for &op in &ops {
        rename_within(context, op, &renames);
    }
    let results: Vec<ValueId> = return_op
        .value_operands()
        .iter()
        .chain(return_op.state_operands().iter())
        .map(|value| {
            renames
                .iter()
                .find(|(old, _)| old == value)
                .map_or(*value, |&(_, new)| new)
        })
        .collect();
    for &op in &ops {
        handle.remove_op(op);
    }
    context.erase_op(&OperationRef::new(return_op))?;
    let staged = context.create_nodes_region(ports, ops, results).id();
    context.replace_region_with_nodes(region, staged);
    Ok(())
}

pub(crate) fn recover_with_plan(
    context: &Context,
    region: RegionId,
    edges: &dyn Edges,
    plan: &RecoveryPlan,
) -> Result<(), RecoveryError> {
    recovery::emit_recovery(context, region, edges, plan)
}

fn lower(
    context: &Context,
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
            lowering.ordered(block)?;
        }
        for block in lowering.blocks {
            context.get_region(region).add_block(block);
        }
        return Ok(lowering.record);
    }
    let entry = lowering.entered_on(&handle.ports());
    lowering.blocks.push(entry);
    let (values, deps) = (handle.value_results(), handle.state_results());
    let last = lowering.region(region, entry)?;
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

/// Operations in `region` needed by `roots`, following the dependencies each
/// caller uses for its form of recovery.
pub(super) fn cone(
    context: &Context,
    region: RegionId,
    roots: &[ValueId],
    mut inputs: impl FnMut(OpId) -> Vec<OpId>,
) -> HashSet<OpId> {
    let mut cone = HashSet::new();
    let mut pending: Vec<OpId> = roots
        .iter()
        .filter_map(|&value| context.get_value(value).defining_op())
        .collect();
    while let Some(op) = pending.pop() {
        if context.parent_nodes_region(op) != Some(region) || !cone.insert(op) {
            continue;
        }
        pending.extend(inputs(op));
    }
    cone
}

/// Stable Kahn order for a node region. Ties retain insertion order.
pub(super) fn stable_order(
    context: &Context,
    region: RegionId,
    mut inputs: impl FnMut(OpId) -> Vec<OpId>,
) -> Result<Vec<OpId>, PassError> {
    let ops = context.get_region(region).op_ids();
    let positions: HashMap<OpId, usize> = ops
        .iter()
        .enumerate()
        .map(|(index, &op)| (op, index))
        .collect();
    let mut pending = vec![0; ops.len()];
    let mut readers = vec![HashSet::new(); ops.len()];
    for (index, &op) in ops.iter().enumerate() {
        for input in inputs(op) {
            if let Some(&dependency) = positions.get(&input)
                && readers[dependency].insert(index)
            {
                pending[index] += 1;
            }
        }
    }
    let ranks: Vec<_> = (0..ops.len()).collect();
    let order = stable_group_order(&readers, &mut pending, &ranks);
    if order.len() != ops.len() {
        return Err(PassError::InvalidRuleSet(
            "an unordered region contains a dependency cycle".into(),
        ));
    }
    Ok(order.into_iter().map(|index| ops[index]).collect())
}

/// Stable Kahn order for contracted groups and the operations inside them.
pub(super) fn stable_group_order(
    outgoing: &[HashSet<usize>],
    pending: &mut [usize],
    ranks: &[usize],
) -> Vec<usize> {
    let mut ready: std::collections::BTreeSet<_> = pending
        .iter()
        .enumerate()
        .filter_map(|(node, &count)| (count == 0).then_some((ranks[node], node)))
        .collect();
    let mut order = Vec::with_capacity(pending.len());
    while let Some((_, node)) = ready.pop_first() {
        order.push(node);
        for &reader in &outgoing[node] {
            pending[reader] -= 1;
            if pending[reader] == 0 {
                ready.insert((ranks[reader], reader));
            }
        }
    }
    order
}

impl Lowering<'_> {
    fn branch(
        &mut self,
        block: BlockId,
        op: &OpHandle,
        test: Test,
        taken: &Edge,
        fallthrough: &Edge,
    ) -> Result<(), PassError> {
        let context = self.context;
        let blocks = &mut self.blocks;
        self.edges
            .branch(block, op, test, taken, fallthrough, &mut || {
                mint(context, blocks)
            })
    }

    fn block(&mut self) -> BlockId {
        let block = self.context.create_block(vec![]).id();
        self.blocks.push(block);
        block
    }

    /// A block adopting `values` as its arguments, the states last. Not
    /// listed yet: a merge block is listed after the blocks it joins, so that
    /// every block follows the ones dominating it.
    fn entered_on(&mut self, values: &[crate::Value]) -> BlockId {
        let block = self.context.create_block(vec![]).id();
        let ids: Vec<ValueId> = values.iter().map(crate::Value::id).collect();
        for value in values_then_states(self.context, &ids) {
            self.context.adopt_block_argument(block, value);
        }
        block
    }

    /// Place everything `region`'s results demand from `block` on; the block
    /// the results are available in.
    fn region(&mut self, region: RegionId, block: BlockId) -> Result<BlockId, PassError> {
        let results = self.context.get_region(region).results();
        let demanded = cone(self.context, region, &results, |op| self.inputs(op));
        let order = self.order(region)?;
        self.require_effects_demanded(&order, &demanded)?;
        self.ops(region, &order, &demanded, block)
    }

    /// Every operation `op` runs after: those defining what it reads, what
    /// its tests and the tests of everything nested in it read, and its
    /// implicit inputs.
    fn inputs(&self, op: OpId) -> Vec<OpId> {
        let mut read = values_read(self.context, op);
        self.test_reads_under(op, &mut read);
        let mut inputs: Vec<OpId> = read
            .into_iter()
            .filter_map(|value| self.context.get_value(value).defining_op())
            .collect();
        inputs.extend(self.edges.implicit_inputs(op));
        inputs
    }

    /// What the tests of `op` and of every structured operation nested in it
    /// read: a nested gate's branch reads registers defined wherever its
    /// region's enclosing regions define them.
    fn test_reads_under(&self, op: OpId, read: &mut Vec<ValueId>) {
        let instance = self.context.get_op(op);
        if let Some(gamma) = instance.clone().as_interface::<dyn Gamma>() {
            for index in 0..gamma.arms().len().saturating_sub(1) {
                read.extend(self.edges.test_reads(&instance, Test::Arm(index)));
            }
        } else if instance.clone().as_interface::<dyn Theta>().is_some() {
            read.extend(self.edges.test_reads(&instance, Test::Repeat));
        }
        for region in instance.regions() {
            for child in self.context.get_region(region).op_ids() {
                self.test_reads_under(child, read);
            }
        }
    }

    /// The region's operations in a topological order, ties broken by
    /// insertion order: the order a machine region was emitted in keeps an
    /// instruction's implicit inputs ahead of it.
    fn order(&self, region: RegionId) -> Result<Vec<OpId>, PassError> {
        let mut order = stable_order(self.context, region, |op| self.inputs(op))?;
        self.sink_leaves(&mut order);
        abut_implicit_inputs(self.edges, &mut order);
        Ok(order)
    }

    /// Refuse a region whose cones leave an effect out. [`Self::ops`] moves
    /// only what a cone holds, so an operation in none of them is not deferred,
    /// it is dropped, and the blocks come out running a body the source never
    /// wrote. An operation leaving no dependency behind computes a value and is
    /// free to go when nothing reads it.
    fn require_effects_demanded(
        &self,
        order: &[OpId],
        placed: &HashSet<OpId>,
    ) -> Result<(), PassError> {
        for &op in order {
            if placed.contains(&op) {
                continue;
            }
            let op = self.context.get_op(op);
            if op.state_results().is_empty() {
                continue;
            }
            return Err(PassError::InvalidRuleSet(format!(
                "{}.{} leaves a dependency no result demands",
                op.dialect(),
                op.name()
            )));
        }
        Ok(())
    }

    /// Move the operations of `region` that are in `placed`, in `order`, into
    /// `block` and whatever blocks a structured one among them opens.
    fn ops(
        &mut self,
        region: RegionId,
        order: &[OpId],
        placed: &HashSet<OpId>,
        mut block: BlockId,
    ) -> Result<BlockId, PassError> {
        for &op_id in order.iter().filter(|op| placed.contains(op)) {
            let op = self.context.get_op(op_id);
            if is_structured(&op) {
                let merge = self.entered_on(&self.values(&op.results()));
                self.structured(&op, block, merge)?;
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
    fn ordered(&mut self, block: BlockId) -> Result<(), PassError> {
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
            let merge = self.context.split_block(block, position + 1).id();
            for result in values_then_states(self.context, &op.results()) {
                self.context.adopt_block_argument(merge, result);
            }
            self.structured(&op, block, merge)?;
            self.blocks.push(merge);
            block = merge;
        }
    }

    fn structured(
        &mut self,
        op: &OpHandle,
        block: BlockId,
        merge: BlockId,
    ) -> Result<(), PassError> {
        if op.clone().as_interface::<dyn Theta>().is_some() {
            self.theta(op, block, merge)
        } else {
            self.gamma(op, block, merge)
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
            .filter(|&value| {
                self.context
                    .is_state_type(self.context.get_value(value).ty())
            })
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
            let argument = self
                .context
                .append_block_argument(block, self.context.get_value(value).ty())
                .id();
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

    fn gamma(&mut self, op: &OpHandle, block: BlockId, merge: BlockId) -> Result<(), PassError> {
        let gamma = gamma(op)?;
        let binding = gamma.binding();
        let inputs = op.operands()[binding.operands.clone()].to_vec();

        // The arms the chain of tests can reach: a test already decided
        // takes its arm and ends the chain, or skips it; the last arm takes
        // whatever reaches it.
        let regions = gamma.arms();
        let last = regions.len() - 1;
        let mut reachable = Vec::new();
        for index in 0..=last {
            let decided = (index < last)
                .then(|| self.edges.decided(op, Test::Arm(index)))
                .flatten();
            if decided != Some(false) {
                reachable.push(index);
            }
            if index == last || decided == Some(true) {
                break;
            }
        }

        // An arm that computes nothing is not a block: what it produces rides
        // the edge into it, straight to the merge block.
        let mut arms: HashMap<usize, Edge> = HashMap::new();
        for &index in &reachable {
            let arm = regions[index];
            let handle = self.context.get_region(arm);
            let results = handle.results();
            if cone(self.context, arm, &results, |op| self.inputs(op)).is_empty() {
                let ports: Vec<ValueId> = handle.ports().iter().map(|port| port.id()).collect();
                let forwarded: Vec<ValueId> = results
                    .iter()
                    .map(
                        |result| match ports.iter().position(|port| port == result) {
                            Some(index) => inputs[index],
                            None => *result,
                        },
                    )
                    .collect();
                arms.insert(index, Edge::with(merge, &forwarded));
                continue;
            }
            let entry = self.entered_on(&handle.ports());
            self.blocks.push(entry);
            let end = self.region(arm, entry)?;
            self.edges.jump(end, &Edge::with(merge, &results));
            arms.insert(index, Edge::with(entry, &inputs));
        }

        // The chain: each reachable arm but the last is tested for, the one
        // after it being where a failed test falls through to.
        let mut current = block;
        for (place, &index) in reachable.iter().enumerate() {
            let Some(&following) = reachable.get(place + 1) else {
                if place == 0 {
                    self.edges.jump(current, &arms[&index]);
                }
                break;
            };
            let next = if place + 2 == reachable.len() {
                arms[&following].clone()
            } else {
                Edge::to(self.block())
            };
            self.branch(current, op, Test::Arm(index), &arms[&index], &next)?;
            current = next.dest;
        }
        self.record.gates.push(GateBlocks { head: block, merge });
        self.context.erase_op(&OperationRef::new(op.clone()))
    }

    fn theta(&mut self, op: &OpHandle, block: BlockId, merge: BlockId) -> Result<(), PassError> {
        let theta = theta(op)?;
        let binding = theta.binding();
        let body = theta.body();
        let handle = self.context.get_region(body);
        let inits = op.operands()[binding.operands.clone()].to_vec();
        let header = self.entered_on(&handle.ports());
        self.blocks.push(header);
        self.edges.jump(block, &Edge::with(header, &inits));

        let results = handle.results();
        let mut continue_values = results[binding.continue_.clone()].to_vec();
        let mut exit_values = results[binding.exit.clone()].to_vec();

        let mut tested = vec![theta.predicate()];
        tested.extend(self.edges.test_reads(op, Test::Repeat));
        let demand = |roots: &[ValueId]| cone(self.context, body, roots, |op| self.inputs(op));
        let predicate = demand(&tested);
        let continue_cone = demand(&continue_values);
        let exit_cone = demand(&exit_values);
        let header_ops: HashSet<OpId> = predicate
            .iter()
            .copied()
            .chain(continue_cone.intersection(&exit_cone).copied())
            .collect();
        let continue_only: HashSet<OpId> = continue_cone.difference(&header_ops).copied().collect();
        let exit_only: HashSet<OpId> = exit_cone.difference(&header_ops).copied().collect();
        let order = self.order(body)?;
        let placed: HashSet<OpId> = header_ops
            .iter()
            .chain(continue_only.iter())
            .chain(exit_only.iter())
            .copied()
            .collect();
        self.require_effects_demanded(&order, &placed)?;

        let header_end = self.ops(body, &order, &header_ops, header)?;
        // A cone that computes nothing is not a block either: the header's own
        // branch carries what it leaves with.
        let continue_ = if continue_only.is_empty() {
            Edge::with(header, &continue_values)
        } else {
            let (block, entered) = self.cone_block(&continue_only, &mut continue_values);
            let end = self.ops(body, &order, &continue_only, block)?;
            self.edges.jump(end, &Edge::with(header, &continue_values));
            Edge::with(block, &entered)
        };
        let exit = if exit_only.is_empty() {
            Edge::with(merge, &exit_values)
        } else {
            let (block, entered) = self.cone_block(&exit_only, &mut exit_values);
            let end = self.ops(body, &order, &exit_only, block)?;
            self.edges.jump(end, &Edge::with(merge, &exit_values));
            Edge::with(block, &entered)
        };
        self.branch(header_end, op, Test::Repeat, &continue_, &exit)?;

        self.record.loops.push(LoopBlocks {
            header,
            continue_: continue_.dest,
            merge,
        });
        self.context.erase_op(&OperationRef::new(op.clone()))
    }
}

impl Lowering<'_> {
    /// Move every operation that reads nothing — a literal, an address — to
    /// just ahead of its first reader, or to the end when only what leaves the
    /// block reads it, so a value is not held in a register from the region's
    /// start to its use. A reader is anything [`Self::inputs`] says runs after
    /// the leaf, the tests of nested gates included. Order inside a block is a
    /// scheduling matter the backend derives later; this is the one choice the
    /// derivation keeps.
    fn sink_leaves(&self, order: &mut Vec<OpId>) {
        let edges = self.edges;
        // What an instruction implicitly reads is placed by that instruction.
        let implicit: HashSet<OpId> = order
            .iter()
            .flat_map(|&op| edges.implicit_inputs(op))
            .collect();
        let inputs: Vec<HashSet<OpId>> = order
            .iter()
            .map(|&op| self.inputs(op).into_iter().collect())
            .collect();
        let place: Vec<(usize, usize)> = order
            .iter()
            .enumerate()
            .map(|(index, &op)| {
                let instance = self.context.get_op(op);
                let leaf = instance.operands().is_empty()
                    && instance.regions().is_empty()
                    && instance.state_results().is_empty()
                    && inputs[index].is_empty()
                    && !implicit.contains(&op);
                if !leaf {
                    return (index, 1);
                }
                let reader = inputs[index + 1..]
                    .iter()
                    .position(|later| later.contains(&op));
                match reader {
                    Some(distance) => {
                        // What the reader implicitly reads sits right ahead of
                        // it and stays there.
                        let reader = index + 1 + distance;
                        let ahead = edges
                            .implicit_inputs(order[reader])
                            .iter()
                            .filter_map(|input| order.iter().position(|op| op == input))
                            .min()
                            .unwrap_or(reader);
                        (ahead.min(reader), 0)
                    }
                    None => (order.len(), 0),
                }
            })
            .collect();
        let mut placed: Vec<(usize, OpId)> = order
            .iter()
            .copied()
            .zip(place)
            .map(|(op, key)| (key.0 * 2 + key.1, op))
            .collect();
        placed.sort_by_key(|&(key, _)| key);
        *order = placed.into_iter().map(|(_, op)| op).collect();
    }
}

/// Put what an instruction implicitly reads right ahead of it: a rule's
/// prelude defines a register nothing names, and the order the block ends up
/// with pairs a register's reader with the latest writer ahead of it.
fn abut_implicit_inputs(edges: &dyn Edges, order: &mut Vec<OpId>) {
    let mut index = 0;
    while index < order.len() {
        let op = order[index];
        for input in edges.implicit_inputs(op) {
            let Some(at) = order.iter().position(|held| *held == input) else {
                continue;
            };
            order.remove(at);
            let target = order
                .iter()
                .position(|held| *held == op)
                .expect("still held");
            order.insert(target, input);
        }
        index = order
            .iter()
            .position(|held| *held == op)
            .expect("still held")
            + 1;
    }
}

fn mint(context: &Context, blocks: &mut Vec<BlockId>) -> BlockId {
    let block = context.create_block(vec![]).id();
    blocks.push(block);
    block
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
            context.set_region_results(region, results);
        }
        for child in handle.op_ids() {
            rename_within(context, child, renames);
        }
    }
}

/// `destructure`: a callable's unordered body becomes `cfg` blocks.
#[derive(Clone)]
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
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let Some(&body) = op.op().regions().first() else {
            return Ok(());
        };
        recover_cfg(context, body, &CfgEdges { context })?;
        Ok(())
    }
}

/// `ids` with the states moved last: the order every block keeps its
/// arguments in, whatever order the op that produced them grew them in.
fn values_then_states(context: &Context, ids: &[ValueId]) -> Vec<ValueId> {
    let mut ordered = context.values_among(ids).to_vec();
    ordered.extend(context.states_among(ids));
    ordered
}
