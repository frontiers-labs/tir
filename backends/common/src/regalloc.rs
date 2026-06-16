//! Target-independent register allocation.
//!
//! The allocator works on machine IR produced by instruction selection, where
//! every register operand is carried in an op attribute as
//! [`RegisterAttr::Virtual`] (its `id` is the SSA value number) or
//! [`RegisterAttr::Physical`]. It reads the def/use role of each register operand
//! from the op's generated `attribute_roles` table, computes liveness, builds an
//! interference graph, and solves an optimal coloring with the shared PBQP solver
//! ([`tir::pbqp`]). The chosen physical registers are written back by rewriting
//! every `Virtual` attribute to `Physical`.
//!
//! The set of physical registers, their caller/callee-saved partitions, and the
//! calling convention are not hardcoded here: they come from [`RegisterInfo`],
//! which the TMDL backend emits from each target's `register_class` declarations.

use std::collections::{HashMap, HashSet};

use tir::attributes::{AttributeRole, AttributeValue, RegisterAttr};
use tir::pbqp::{self, INF_COST, PbqpMatrix, PbqpNodeId, PbqpProblem};
use tir::{
    BlockId, Context, OpId, Operation, OperationRef, Pass, PassError, PassTarget, Rewriter, ValueId,
};

use crate::liveness::{self, Liveness, PhysReg};

/// Allocation metadata for one register class, emitted by the TMDL backend from a
/// target's `register_class` traits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegClassInfo {
    pub name: &'static str,
    /// The physical register file this class draws from — the root of its TMDL
    /// inheritance chain. Classes that share a file (e.g. AArch64 `GPR` and
    /// `GPRsp`, which differ only in whether encoding 31 is `xzr` or `sp`) name the
    /// same physical register at a given index, so the allocator treats their
    /// indices as aliases. A standalone class is its own file.
    pub file: &'static str,
    /// Allocatable register indices, in preferred allocation order. A class with
    /// an empty order (e.g. the program-counter class) is never allocated.
    pub allocation_order: &'static [u16],
    pub caller_saved: &'static [u16],
    pub callee_saved: &'static [u16],
    /// Argument registers in calling-convention order.
    pub arguments: &'static [u16],
    pub return_values: &'static [u16],
    /// Indices reserved by the ABI and never allocated.
    pub reserved: &'static [u16],
}

impl RegClassInfo {
    pub fn is_callee_saved(&self, index: u16) -> bool {
        self.callee_saved.contains(&index)
    }

    pub fn is_caller_saved(&self, index: u16) -> bool {
        self.caller_saved.contains(&index)
    }
}

/// The register file of a target: every allocatable (and reserved) register class,
/// keyed by the class name used in [`RegisterAttr`] operands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterInfo {
    pub classes: &'static [RegClassInfo],
}

impl RegisterInfo {
    pub fn class(&self, name: &str) -> Option<&RegClassInfo> {
        self.classes.iter().find(|c| c.name == name)
    }

    /// Build a lookup from class name to its info for repeated queries.
    pub fn class_map(&self) -> HashMap<&'static str, &RegClassInfo> {
        self.classes.iter().map(|c| (c.name, c)).collect()
    }

    /// The physical-register identity of `p` for aliasing purposes: its register
    /// file (the root of the class's inheritance chain) plus index. Two
    /// `(class, index)` pairs that resolve to the same `(file, index)` are the same
    /// physical register even when reached through different classes — e.g.
    /// `("GPR", 7)` and `("GPRsp", 7)` on AArch64. An unknown class is treated as
    /// its own file.
    pub fn phys_key<'a>(&'a self, p: &'a PhysReg) -> (&'a str, u16) {
        let file = self.class(&p.0).map_or(p.0.as_str(), |c| c.file);
        (file, p.1)
    }

    /// The class that owns the calling convention's argument registers — the
    /// natural default for integer virtual registers whose class is otherwise
    /// undetermined (e.g. function parameters with no register attribute yet).
    pub fn default_integer_class(&self) -> Option<&RegClassInfo> {
        self.classes
            .iter()
            .find(|c| !c.arguments.is_empty())
            .or_else(|| self.classes.iter().find(|c| !c.allocation_order.is_empty()))
    }
}

/// One choice the allocator can make for a virtual register: a concrete physical
/// register, or spilling it to a stack slot.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Alternative {
    Phys(PhysReg),
    Spill,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegAllocError {
    /// A virtual register could not be colored or spilled (e.g. an over-constrained
    /// pre-coloring). Carries the offending vreg id.
    Infeasible(u32),
    /// A virtual register references a register class the target does not define.
    UnknownClass { vreg: u32, class: String },
    /// The PBQP instance itself was malformed.
    Solver(String),
}

/// The outcome of one allocation round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllocResult {
    /// Every virtual register received a physical register.
    Assigned(HashMap<u32, PhysReg>),
    /// The allocator chose to spill these virtual registers; the caller must insert
    /// spill code and re-run. Never empty.
    Spill(Vec<u32>),
}

/// Cost added for choosing a callee-saved register, modeling its prologue/epilogue
/// save/restore. Small, so it only breaks ties toward caller-saved scratch.
const CALLEE_SAVED_COST: u64 = 1;

/// Inputs that tune one allocation round.
pub struct AllocConfig<'a> {
    pub info: &'a RegisterInfo,
    pub liveness: &'a Liveness,
    /// Virtual registers pinned to a physical register (ABI args/return, fixed regs).
    pub precolor: &'a HashMap<u32, PhysReg>,
    /// Estimated cost of spilling a vreg (higher = less likely to be the one spilled).
    pub spill_cost: &'a dyn Fn(u32) -> u64,
}

/// Solve one register-allocation round over the analyzed function.
///
/// Each virtual register becomes a PBQP node whose alternatives are the allocatable
/// physical registers of its class plus a spill alternative; interference edges
/// forbid two simultaneously-live vregs from sharing a register. An optimal
/// assignment is read back from the PBQP solution. If the optimum spills any vreg,
/// the spilled set is returned so the caller can lower it and retry.
pub fn allocate(config: &AllocConfig) -> Result<AllocResult, RegAllocError> {
    let AllocConfig {
        info,
        liveness,
        precolor,
        spill_cost,
    } = config;

    // Deterministic node order.
    let vregs: Vec<u32> = liveness.vregs.iter().copied().collect();
    let node_of: HashMap<u32, usize> = vregs.iter().enumerate().map(|(i, &v)| (v, i)).collect();

    let default_class = info.default_integer_class();

    // Per-node alternative lists, resolved to concrete physical registers.
    let mut alternatives: Vec<Vec<Alternative>> = Vec::with_capacity(vregs.len());
    let mut node_classes: Vec<&RegClassInfo> = Vec::with_capacity(vregs.len());
    for &vreg in &vregs {
        let class = resolve_class(info, liveness, precolor, default_class, vreg)?;
        let mut alts: Vec<Alternative> = class
            .allocation_order
            .iter()
            .map(|&idx| Alternative::Phys((class.name.to_string(), idx)))
            .collect();
        alts.push(Alternative::Spill);
        alternatives.push(alts);
        node_classes.push(class);
    }

    let mut problem = PbqpProblem::new();
    for (i, &vreg) in vregs.iter().enumerate() {
        let costs = node_costs(
            info,
            &alternatives[i],
            node_classes[i],
            vreg,
            liveness,
            precolor,
            spill_cost,
        );
        // A node with no finite alternative is unallocatable and unspillable.
        if costs.iter().all(|&c| c >= INF_COST) {
            return Err(RegAllocError::Infeasible(vreg));
        }
        problem.add_node(costs);
    }

    // Interference edges: only between vregs whose classes share physical registers.
    for &(u, v) in &liveness.interference {
        let (Some(&iu), Some(&iv)) = (node_of.get(&u), node_of.get(&v)) else {
            continue;
        };
        if let Some(matrix) = interference_matrix(info, &alternatives[iu], &alternatives[iv]) {
            problem.add_edge(
                PbqpNodeId::from_index(iu),
                PbqpNodeId::from_index(iv),
                matrix,
            );
        }
    }

    let solution = pbqp::solve(&problem).map_err(|e| RegAllocError::Solver(format!("{e:?}")))?;

    let mut assignment = HashMap::new();
    let mut spilled = Vec::new();
    for (i, &vreg) in vregs.iter().enumerate() {
        match &alternatives[i][solution.choices[i]] {
            Alternative::Phys(p) => {
                assignment.insert(vreg, p.clone());
            }
            Alternative::Spill => spilled.push(vreg),
        }
    }

    if spilled.is_empty() {
        Ok(AllocResult::Assigned(assignment))
    } else {
        Ok(AllocResult::Spill(spilled))
    }
}

/// Determine the register class a virtual register must be allocated from: its
/// pinned register's class, the class discovered from its operands, or the target's
/// default integer class.
fn resolve_class<'a>(
    info: &'a RegisterInfo,
    liveness: &Liveness,
    precolor: &HashMap<u32, PhysReg>,
    default_class: Option<&'a RegClassInfo>,
    vreg: u32,
) -> Result<&'a RegClassInfo, RegAllocError> {
    let name = precolor
        .get(&vreg)
        .map(|(c, _)| c.as_str())
        .or_else(|| liveness.vreg_class.get(&vreg).map(String::as_str));

    match name {
        Some(name) => info.class(name).ok_or_else(|| RegAllocError::UnknownClass {
            vreg,
            class: name.to_string(),
        }),
        None => default_class.ok_or(RegAllocError::Infeasible(vreg)),
    }
}

/// Build the cost vector for one node's alternatives, honoring pre-coloring,
/// forbidden physical registers, and the callee-saved bias.
fn node_costs(
    info: &RegisterInfo,
    alternatives: &[Alternative],
    class: &RegClassInfo,
    vreg: u32,
    liveness: &Liveness,
    precolor: &HashMap<u32, PhysReg>,
    spill_cost: &dyn Fn(u32) -> u64,
) -> Vec<u64> {
    let pinned = precolor.get(&vreg);
    let forbidden = liveness.forbidden.get(&vreg);

    alternatives
        .iter()
        .map(|alt| match alt {
            Alternative::Phys(p) => {
                if let Some(target) = pinned {
                    // Pinned vregs accept only their target register. Compare by
                    // physical identity so a precolor reached through one class
                    // (e.g. an ABI `GPR` arg) matches an alternative in an aliasing
                    // class (`GPRsp`). A pin on a register the vreg is also live
                    // across a clobber of (e.g. an incoming argument that survives
                    // a call) is unsatisfiable: every alternative goes infinite so
                    // allocation fails loudly instead of silently producing a
                    // clobbered value.
                    let conflict = forbidden.is_some_and(|set| {
                        set.iter()
                            .any(|f| info.phys_key(f) == info.phys_key(target))
                    });
                    return if !conflict && info.phys_key(p) == info.phys_key(target) {
                        0
                    } else {
                        INF_COST
                    };
                }
                if forbidden
                    .is_some_and(|set| set.iter().any(|f| info.phys_key(f) == info.phys_key(p)))
                {
                    return INF_COST;
                }
                if class.is_callee_saved(p.1) {
                    CALLEE_SAVED_COST
                } else {
                    0
                }
            }
            // A pinned vreg cannot spill; otherwise spilling costs its estimate.
            Alternative::Spill => {
                if pinned.is_some() {
                    INF_COST
                } else {
                    spill_cost(vreg)
                }
            }
        })
        .collect()
}

/// Build the interference matrix between two nodes, or `None` if their alternative
/// sets share no physical register (so they can never conflict and no edge is
/// needed). Two alternatives conflict when they resolve to the same physical
/// register; spilling never conflicts.
fn interference_matrix(
    info: &RegisterInfo,
    left: &[Alternative],
    right: &[Alternative],
) -> Option<PbqpMatrix> {
    let mut matrix = PbqpMatrix::zero(left.len(), right.len());
    let mut any = false;
    for (i, l) in left.iter().enumerate() {
        for (j, r) in right.iter().enumerate() {
            if let (Alternative::Phys(lp), Alternative::Phys(rp)) = (l, r) {
                // Conflict when the two alternatives are the same physical register,
                // comparing by register file so aliasing classes (e.g. `GPR` and
                // `GPRsp` sharing index 7) correctly interfere.
                if info.phys_key(lp) == info.phys_key(rp) {
                    matrix.set(i, j, INF_COST);
                    any = true;
                }
            }
        }
    }
    any.then_some(matrix)
}

// ---------------------------------------------------------------------------
// Target interface + allocation pass
// ---------------------------------------------------------------------------

/// Target-specific knowledge the allocation pass needs but cannot derive from the
/// register file alone: the spill frame layout and the instructions that move a
/// register to and from a stack slot. The register file itself comes from
/// [`TargetRegAlloc::register_info`], which backends wire to their generated
/// `register_info()`.
pub trait TargetRegAlloc: Send + Sync {
    fn register_info(&self) -> RegisterInfo;

    /// Bytes reserved for each spill slot.
    fn slot_size(&self) -> u32 {
        8
    }

    /// The physical register spill loads/stores are addressed relative to (the
    /// stack pointer, after the prologue has reserved the frame).
    fn frame_register(&self) -> PhysReg;

    /// Build a store of virtual register `value` (of class `class`) to
    /// `[frame + offset]`.
    fn emit_spill_store(
        &self,
        context: &Context,
        value: u32,
        class: &str,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Build a load from `[frame + offset]` into virtual register `value`.
    fn emit_spill_reload(
        &self,
        context: &Context,
        value: u32,
        class: &str,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Prologue instructions reserving a frame of `size` bytes (e.g. `addi sp, sp,
    /// -size`). Inserted at the top of the entry block when any vreg spills.
    fn emit_prologue(&self, _context: &Context, _size: u32) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    /// Epilogue instructions releasing the frame, inserted before each terminator.
    fn emit_epilogue(&self, _context: &Context, _size: u32) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }
}

/// A register allocation pass. Runs on each `asm.symbol` op produced by instruction
/// selection: it computes liveness over the symbol's body, pre-colors the calling
/// convention's argument and return registers, solves an optimal coloring with
/// [`allocate`], spills and retries when the optimum demands it, and finally
/// rewrites every virtual register operand to its assigned physical register.
pub struct RegisterAllocationPass {
    target: Box<dyn TargetRegAlloc>,
    /// Safety valve against a non-converging spill loop.
    max_rounds: usize,
}

impl RegisterAllocationPass {
    pub fn new(target: Box<dyn TargetRegAlloc>) -> Self {
        Self {
            target,
            max_rounds: 16,
        }
    }
}

impl Pass for RegisterAllocationPass {
    fn name(&self) -> &'static str {
        "register-allocation"
    }

    fn target(&self) -> PassTarget {
        PassTarget::Operation("symbol")
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        let info = self.target.register_info();
        let blocks = symbol_body_blocks(context, op);
        if blocks.is_empty() {
            return Ok(());
        }

        let precolor = abi_precolor(context, op, &info, &blocks);

        let mut frame = FrameState::new(self.target.slot_size());
        let assignment = loop {
            let liveness = liveness::analyze(context, &blocks, |b| block_successors(context, b));
            let use_counts = reference_counts(context, &blocks);
            // Spill the least-used value first. Reload/store temps are unspillable:
            // they have single-instruction ranges and must occupy a register, so
            // forcing a longer-lived value to spill instead is what actually relieves
            // pressure and lets the spill loop converge (spilling a temp would just
            // reload it at the same congested point, cascading without progress).
            let protected = frame.temps.clone();
            let spill_cost = |v: u32| -> u64 {
                if protected.contains(&v) {
                    INF_COST
                } else {
                    10 * (*use_counts.get(&v).unwrap_or(&1)) as u64
                }
            };

            let result = allocate(&AllocConfig {
                info: &info,
                liveness: &liveness,
                precolor: &precolor,
                spill_cost: &spill_cost,
            })
            .map_err(|e| PassError::InvalidRuleSet(format!("register allocation failed: {e:?}")))?;

            match result {
                AllocResult::Assigned(map) => break map,
                AllocResult::Spill(vregs) => {
                    if frame.rounds >= self.max_rounds {
                        return Err(PassError::InvalidRuleSet(
                            "register allocation did not converge while spilling".to_string(),
                        ));
                    }
                    frame.rounds += 1;
                    self.spill_all(context, rewriter, &liveness, &blocks, &vregs, &mut frame)?;
                }
            }
        };

        rewrite_registers(context, &blocks, &assignment);

        if frame.size() > 0 {
            self.insert_frame(context, rewriter, &blocks, frame.size())?;
        }

        Ok(())
    }
}

impl RegisterAllocationPass {
    /// Lower every spilled virtual register by splitting its live range: each def is
    /// renamed to a fresh register and followed by a store; each use is preceded by a
    /// reload into a fresh register. The fresh registers are short-lived and get
    /// colored on the next round.
    fn spill_all(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        liveness: &Liveness,
        blocks: &[BlockId],
        vregs: &[u32],
        frame: &mut FrameState,
    ) -> Result<(), PassError> {
        let info = self.target.register_info();
        let default_class = info.default_integer_class().map(|c| c.name);
        let frame_reg = self.target.frame_register();

        for &vreg in vregs {
            let class = liveness
                .vreg_class
                .get(&vreg)
                .map(String::as_str)
                .or(default_class)
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(format!("spilled vreg {vreg} has no register class"))
                })?
                .to_string();
            let ty = context.get_value(ValueId::from_number(vreg)).ty();
            let offset = frame.alloc_slot();

            for &block_id in blocks {
                // Re-read the op list each pass since we mutate the block.
                let op_ids = context.get_block(block_id).op_ids();
                for op_id in op_ids {
                    if !context.has_operation(op_id) {
                        continue;
                    }
                    let op = context.get_op(op_id);
                    let regs = liveness::op_regs(&op);
                    let defines = regs.defs.iter().any(|r| is_vreg(r, vreg));
                    let uses = regs.uses.iter().any(|r| is_vreg(r, vreg));

                    if uses {
                        let fresh = context.create_value(ty, None).id().number();
                        frame.temps.insert(fresh);
                        let reload = self
                            .target
                            .emit_spill_reload(context, fresh, &class, &frame_reg, offset);
                        let op_ref = op_ref_in(context, block_id, op_id);
                        rewriter.insert_op_before(&op_ref, reload.as_ref())?;
                        rename_attr(context, op_id, vreg, fresh, RoleClass::Read);
                    }

                    if defines {
                        let fresh = context.create_value(ty, None).id().number();
                        frame.temps.insert(fresh);
                        rename_attr(context, op_id, vreg, fresh, RoleClass::Write);
                        let store = self
                            .target
                            .emit_spill_store(context, fresh, &class, &frame_reg, offset);
                        insert_after(context, rewriter, block_id, op_id, store.as_ref())?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Insert the prologue at the entry block's top and an epilogue before every
    /// terminator, once the frame size is known.
    fn insert_frame(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[BlockId],
        size: u32,
    ) -> Result<(), PassError> {
        if let Some(&entry) = blocks.first() {
            let op_ids = context.get_block(entry).op_ids();
            if let Some(&first) = op_ids.first() {
                let target = op_ref_in(context, entry, first);
                for op in self.target.emit_prologue(context, size) {
                    rewriter.insert_op_before(&target, op.as_ref())?;
                }
            }
        }
        for &block_id in blocks {
            let op_ids = context.get_block(block_id).op_ids();
            if let Some(&term) = op_ids.last() {
                let target = op_ref_in(context, block_id, term);
                for op in self.target.emit_epilogue(context, size) {
                    rewriter.insert_op_before(&target, op.as_ref())?;
                }
            }
        }
        Ok(())
    }
}

/// Tracks spill stack-slot assignment across spill rounds.
struct FrameState {
    slot_size: u32,
    next_offset: i64,
    rounds: usize,
    /// Fresh registers introduced by reload/store range-splitting. They have tiny
    /// live ranges and must land in a register; protecting them from re-spilling
    /// forces the allocator to spill a longer-lived value instead, so pressure
    /// drops monotonically and the spill loop converges.
    temps: HashSet<u32>,
}

impl FrameState {
    fn new(slot_size: u32) -> Self {
        Self {
            slot_size,
            next_offset: 0,
            rounds: 0,
            temps: HashSet::new(),
        }
    }

    fn alloc_slot(&mut self) -> i64 {
        let offset = self.next_offset;
        self.next_offset += self.slot_size as i64;
        offset
    }

    fn size(&self) -> u32 {
        self.next_offset as u32
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RoleClass {
    Read,
    Write,
}

fn is_vreg(r: &liveness::RegRef, vreg: u32) -> bool {
    matches!(r, liveness::RegRef::Virtual { id, .. } if *id == vreg)
}

/// The control-flow successors of `block`: the destination of every
/// `Block`-valued attribute carried by its operations (branch targets). All
/// branches name their targets explicitly, so this captures the full CFG edge
/// set without relying on fallthrough.
fn block_successors(context: &Context, block: BlockId) -> Vec<BlockId> {
    let mut successors = Vec::new();
    for op_id in context.get_block(block).op_ids() {
        for attr in &context.get_op(op_id).attributes {
            if let AttributeValue::Block(target) = attr.value
                && !successors.contains(&target)
            {
                successors.push(target);
            }
        }
    }
    successors
}

/// The blocks of an `asm.symbol` op's body region, in program order.
fn symbol_body_blocks(context: &Context, op: &OperationRef) -> Vec<BlockId> {
    let Some(&region_id) = op.op().regions.first() else {
        return Vec::new();
    };
    context
        .get_region(region_id)
        .iter(context.clone())
        .map(|b| b.id())
        .collect()
}

fn op_ref_in(context: &Context, block_id: BlockId, op_id: OpId) -> OperationRef {
    OperationRef::new(
        context.get_op(op_id),
        Some(context.get_block(block_id)),
        None,
    )
}

/// Insert `new_op` immediately after `op_id` in its block (before the following op,
/// or appended if `op_id` is last — which spill stores never are).
fn insert_after(
    context: &Context,
    rewriter: &mut Rewriter,
    block_id: BlockId,
    op_id: OpId,
    new_op: &dyn Operation,
) -> Result<(), PassError> {
    let op_ids = context.get_block(block_id).op_ids();
    let pos = op_ids.iter().position(|&id| id == op_id);
    match pos.and_then(|p| op_ids.get(p + 1).copied()) {
        Some(next) => {
            let target = op_ref_in(context, block_id, next);
            rewriter.insert_op_before(&target, new_op)
        }
        None => Err(PassError::RewriteFailed(op_id)),
    }
}

/// Compute the calling-convention pre-coloring: each argument register pinned in
/// order, and the returned value pinned to the first return register.
fn abi_precolor(
    context: &Context,
    op: &OperationRef,
    info: &RegisterInfo,
    blocks: &[BlockId],
) -> HashMap<u32, PhysReg> {
    let mut precolor = HashMap::new();
    let Some(class) = info.default_integer_class() else {
        return precolor;
    };

    // Argument vregs: the entry block's arguments, in order.
    if let Some(&entry) = blocks.first() {
        let args = context.get_block(entry).arguments().to_vec();
        for (arg, &reg) in args.iter().zip(class.arguments.iter()) {
            precolor.insert(arg.id().number(), (class.name.to_string(), reg));
        }
    }

    // Return value: the operand of the `vret` terminator.
    if let Some(&ret_reg) = class.return_values.first() {
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let body_op = context.get_op(op_id);
                if body_op.name == "vret"
                    && let Some(value) = body_op.operands.first()
                {
                    precolor.insert(value.number(), (class.name.to_string(), ret_reg));
                }
            }
        }
    }

    let _ = op;
    precolor
}

/// Count how many times each virtual register is referenced (def or use) across the
/// body, used to weight spill cost so the least-used register spills first.
fn reference_counts(context: &Context, blocks: &[BlockId]) -> HashMap<u32, u32> {
    let mut counts = HashMap::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            let regs = liveness::op_regs(&op);
            for r in regs.defs.iter().chain(regs.uses.iter()) {
                if let liveness::RegRef::Virtual { id, .. } = r {
                    *counts.entry(*id).or_insert(0) += 1;
                }
            }
        }
    }
    counts
}

/// Rewrite a single op's register attributes: replace virtual register `from` with
/// virtual register `to` in attributes matching the given role direction.
fn rename_attr(context: &Context, op_id: OpId, from: u32, to: u32, role_class: RoleClass) {
    let op = context.get_op(op_id);
    let mut attrs = op.attributes.clone();
    let mut changed = false;
    for attr in &mut attrs {
        let role = role_of(&op, &attr.name);
        let matches_dir = match role_class {
            RoleClass::Read => matches!(role, AttributeRole::Use | AttributeRole::ReadWrite),
            RoleClass::Write => {
                matches!(
                    role,
                    AttributeRole::Def | AttributeRole::ReadWrite | AttributeRole::Clobber
                )
            }
        };
        if !matches_dir {
            continue;
        }
        if let AttributeValue::Register(RegisterAttr::Virtual { id, class }) = &attr.value
            && *id == from
        {
            attr.value = AttributeValue::Register(RegisterAttr::Virtual {
                id: to,
                class: class.clone(),
            });
            changed = true;
        }
    }
    if changed {
        context.set_op_attributes(op_id, attrs);
    }
}

/// Rewrite every virtual register operand in the body to its assigned physical
/// register.
fn rewrite_registers(context: &Context, blocks: &[BlockId], assignment: &HashMap<u32, PhysReg>) {
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            let mut attrs = op.attributes.clone();
            let mut changed = false;
            for attr in &mut attrs {
                if let AttributeValue::Register(RegisterAttr::Virtual { id, .. }) = &attr.value
                    && let Some((class, index)) = assignment.get(id)
                {
                    attr.value = AttributeValue::Register(RegisterAttr::Physical {
                        class: class.clone(),
                        index: *index,
                    });
                    changed = true;
                }
            }
            if changed {
                context.set_op_attributes(op_id, attrs);
            }
        }
    }
}

fn role_of(op: &tir::OpInstance, name: &str) -> AttributeRole {
    op.attribute_roles
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, r)| *r)
        .unwrap_or(AttributeRole::None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn three_reg_info() -> RegisterInfo {
        RegisterInfo {
            classes: &[RegClassInfo {
                name: "R",
                file: "R",
                allocation_order: &[0, 1, 2],
                caller_saved: &[0, 1, 2],
                callee_saved: &[],
                arguments: &[0, 1],
                return_values: &[0],
                reserved: &[],
            }],
        }
    }

    fn liveness_with(vregs: &[u32], edges: &[(u32, u32)]) -> Liveness {
        let mut lv = Liveness::default();
        for &v in vregs {
            lv.vregs.insert(v);
            lv.vreg_class.insert(v, "R".to_string());
        }
        for &(a, b) in edges {
            lv.interference.insert((a.min(b), a.max(b)));
        }
        lv
    }

    fn assigned(result: AllocResult) -> HashMap<u32, PhysReg> {
        match result {
            AllocResult::Assigned(map) => map,
            other => panic!("expected an assignment, got {other:?}"),
        }
    }

    #[test]
    fn mutually_live_vregs_get_distinct_registers() {
        let info = three_reg_info();
        let liveness = liveness_with(&[1, 2, 3], &[(1, 2), (1, 3), (2, 3)]);
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        let regs: BTreeSet<u16> = map.values().map(|(_, i)| *i).collect();
        assert_eq!(
            regs.len(),
            3,
            "all three vregs must occupy distinct registers"
        );
    }

    #[test]
    fn over_subscribed_clique_forces_a_spill() {
        let info = three_reg_info();
        // Four mutually-live vregs, only three registers: exactly one must spill.
        let liveness = liveness_with(
            &[1, 2, 3, 4],
            &[(1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)],
        );
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        match result {
            AllocResult::Spill(spilled) => assert_eq!(spilled.len(), 1),
            other => panic!("expected a spill, got {other:?}"),
        }
    }

    #[test]
    fn spill_picks_the_cheapest_vreg() {
        let info = three_reg_info();
        let liveness = liveness_with(
            &[1, 2, 3, 4],
            &[(1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)],
        );
        let precolor = HashMap::new();
        // vreg 4 is far cheaper to spill than the rest.
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|v| if v == 4 { 1 } else { 1000 },
        })
        .unwrap();

        assert_eq!(result, AllocResult::Spill(vec![4]));
    }

    #[test]
    fn precoloring_pins_a_vreg_and_repels_interferers() {
        let info = three_reg_info();
        let liveness = liveness_with(&[1, 2], &[(1, 2)]);
        let mut precolor = HashMap::new();
        precolor.insert(1u32, ("R".to_string(), 0u16));
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        assert_eq!(map[&1], ("R".to_string(), 0));
        assert_ne!(
            map[&2].1, 0,
            "an interfering vreg cannot reuse the pinned register"
        );
    }

    #[test]
    fn clique_larger_than_register_file_spills_the_excess() {
        // A k-register file and an n-vreg clique must spill exactly n - k of them.
        let info = three_reg_info(); // 3 registers
        let vregs: Vec<u32> = (0..6).collect();
        let mut edges = Vec::new();
        for i in 0..vregs.len() {
            for j in (i + 1)..vregs.len() {
                edges.push((vregs[i], vregs[j]));
            }
        }
        let liveness = liveness_with(&vregs, &edges);
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();
        match result {
            AllocResult::Spill(s) => assert_eq!(s.len(), 6 - 3),
            other => panic!("expected spilling, got {other:?}"),
        }
    }

    #[test]
    fn forbidden_register_is_avoided() {
        let info = three_reg_info();
        let mut liveness = liveness_with(&[1], &[]);
        liveness
            .forbidden
            .entry(1)
            .or_default()
            .extend([("R".to_string(), 0u16), ("R".to_string(), 1u16)]);
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        assert_eq!(
            map[&1],
            ("R".to_string(), 2),
            "only the unforbidden register remains"
        );
    }

    // Two register classes (`GPR` and `GPRsp`) over one shared file with a single
    // allocatable register, mirroring AArch64's slot-31 aliasing.
    static ALIASING_CLASSES: &[RegClassInfo] = &[
        RegClassInfo {
            name: "GPR",
            file: "GPR",
            allocation_order: &[0],
            caller_saved: &[0],
            callee_saved: &[],
            arguments: &[0],
            return_values: &[],
            reserved: &[],
        },
        RegClassInfo {
            name: "GPRsp",
            file: "GPR",
            allocation_order: &[0],
            caller_saved: &[0],
            callee_saved: &[],
            arguments: &[],
            return_values: &[],
            reserved: &[],
        },
    ];

    fn two_class_liveness(class1: &str, class2: &str) -> Liveness {
        let mut lv = Liveness::default();
        lv.vregs.insert(1);
        lv.vreg_class.insert(1, class1.to_string());
        lv.vregs.insert(2);
        lv.vreg_class.insert(2, class2.to_string());
        lv.interference.insert((1, 2));
        lv
    }

    #[test]
    fn aliasing_classes_share_physical_registers() {
        // The two interfering vregs live in different classes that share one file
        // with a single register, so they cannot both be colored: one must spill.
        // Without file-based aliasing, `("GPR", 0)` and `("GPRsp", 0)` would look
        // distinct and the allocator would wrongly color both.
        let info = RegisterInfo {
            classes: ALIASING_CLASSES,
        };
        let liveness = two_class_liveness("GPR", "GPRsp");
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        match result {
            AllocResult::Spill(spilled) => assert_eq!(spilled.len(), 1),
            other => panic!("expected a spill from the shared file, got {other:?}"),
        }
    }

    #[test]
    fn distinct_files_do_not_alias() {
        // Same shape, but the classes belong to different files, so both vregs can
        // independently take index 0.
        static CLASSES: &[RegClassInfo] = &[
            RegClassInfo {
                name: "A",
                file: "A",
                allocation_order: &[0],
                caller_saved: &[0],
                callee_saved: &[],
                arguments: &[],
                return_values: &[],
                reserved: &[],
            },
            RegClassInfo {
                name: "B",
                file: "B",
                allocation_order: &[0],
                caller_saved: &[0],
                callee_saved: &[],
                arguments: &[],
                return_values: &[],
                reserved: &[],
            },
        ];
        let info = RegisterInfo { classes: CLASSES };
        let liveness = two_class_liveness("A", "B");
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        assert_eq!(map[&1], ("A".to_string(), 0));
        assert_eq!(map[&2], ("B".to_string(), 0));
    }

    #[test]
    fn forbidden_register_aliases_across_classes() {
        // A `GPRsp` vreg forbidding `("GPR", 0)` — a clobber expressed through the
        // aliasing base class — must avoid index 0 and take the other register.
        static CLASSES: &[RegClassInfo] = &[
            RegClassInfo {
                name: "GPR",
                file: "GPR",
                allocation_order: &[0, 1],
                caller_saved: &[0, 1],
                callee_saved: &[],
                arguments: &[],
                return_values: &[],
                reserved: &[],
            },
            RegClassInfo {
                name: "GPRsp",
                file: "GPR",
                allocation_order: &[0, 1],
                caller_saved: &[0, 1],
                callee_saved: &[],
                arguments: &[],
                return_values: &[],
                reserved: &[],
            },
        ];
        let info = RegisterInfo { classes: CLASSES };
        let mut liveness = Liveness::default();
        liveness.vregs.insert(1);
        liveness.vreg_class.insert(1, "GPRsp".to_string());
        liveness
            .forbidden
            .entry(1)
            .or_default()
            .insert(("GPR".to_string(), 0u16));
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        assert_eq!(
            map[&1],
            ("GPRsp".to_string(), 1),
            "a forbidden index aliases across the shared file"
        );
    }
}
