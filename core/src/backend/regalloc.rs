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
    AnalysisManager, BlockId, Context, OpId, Operation, OperationRef, Pass, PassError, PassTarget,
    PreservedAnalyses, Rewriter, ValueId,
};

use crate::backend::liveness::{self, Liveness, PhysReg};

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
    /// How many consecutive file indices one register of this class covers.
    /// 1 for ordinary classes; an RVV LMUL>1 group class covers 2/4/8 (e.g.
    /// `VRM2` index 8 is the architectural pair v8..v9).
    pub group_width: u16,
}

impl RegClassInfo {
    pub fn is_callee_saved(&self, index: u16) -> bool {
        self.callee_saved.contains(&index)
    }

    pub fn is_caller_saved(&self, index: u16) -> bool {
        self.caller_saved.contains(&index)
    }
}

/// A handle to a register class: a pointer to its `'static` [`RegClassInfo`].
///
/// Register classes are per-dialect statics emitted by the TMDL backend — the
/// generated `RegClass::X.id()` and `register_info().classes` point at the same
/// table — so a class's identity is the identity of that pointer. Equality and
/// hashing are by pointer; ordering is by name so codegen that sorts physical
/// registers stays deterministic across builds. Derefs to [`RegClassInfo`], so a
/// class's traits (`allocation_order`, `arguments`, `is_callee_saved`, …) read
/// directly through the handle.
#[derive(Clone, Copy)]
pub struct RegClassId(&'static RegClassInfo);

impl RegClassId {
    pub const fn new(info: &'static RegClassInfo) -> Self {
        RegClassId(info)
    }

    pub fn info(self) -> &'static RegClassInfo {
        self.0
    }

    pub fn name(self) -> &'static str {
        self.0.name
    }

    /// The physical register file this class draws from (see [`RegClassInfo::file`]).
    pub fn file(self) -> &'static str {
        self.0.file
    }

    /// The span of file indices a register of this class at `index` covers: its
    /// file, start index, and group width (RVV LMUL>1 groups cover 2/4/8).
    pub fn span(self, index: u16) -> (&'static str, u16, u16) {
        (self.0.file, index, self.0.group_width.max(1))
    }

    /// Whether a register of this class at `index` overlaps `other` at
    /// `other_index`: same file and intersecting index spans. For width-1 classes
    /// this is file+index equality; a group register (RVV `VRM2` v8..v9) overlaps
    /// every register it covers, and aliasing classes over one file (`GPR`/`GPRsp`
    /// index 7) overlap at equal indices.
    pub fn overlaps(self, index: u16, other: RegClassId, other_index: u16) -> bool {
        let (fa, sa, wa) = self.span(index);
        let (fb, sb, wb) = other.span(other_index);
        fa == fb && sa < sb + wb && sb < sa + wa
    }
}

impl std::ops::Deref for RegClassId {
    type Target = RegClassInfo;
    fn deref(&self) -> &RegClassInfo {
        self.0
    }
}

impl PartialEq for RegClassId {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for RegClassId {}

impl std::hash::Hash for RegClassId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(self.0, state);
    }
}

impl PartialOrd for RegClassId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RegClassId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.name.cmp(other.0.name)
    }
}

impl std::fmt::Debug for RegClassId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RegClassId({})", self.0.name)
    }
}

/// How a register class's architectural view maps onto its storage element.
/// `bit_offset` is where the view starts within the element (x86 high-byte `ah`
/// begins at bit 8); `merge` preserves the element's untouched bits on write
/// (x86 8/16-bit writes) rather than zero-extending the value across the whole
/// element (the default, matching x86 32-bit and AArch64 scalar-FP writes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegisterView {
    pub bit_offset: u32,
    pub merge: bool,
}

/// The register file of a target: every allocatable (and reserved) register class,
/// keyed by the class name used in [`RegisterAttr`] operands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterInfo {
    pub classes: &'static [RegClassInfo],
}

impl RegisterInfo {
    pub fn class(&self, name: &str) -> Option<RegClassId> {
        self.classes
            .iter()
            .find(|c| c.name == name)
            .map(RegClassId::new)
    }

    /// Whether two physical registers overlap: same file and intersecting index
    /// spans. A group register (RVV `VRM2` v8..v9) overlaps every register it
    /// covers; aliasing classes over one file (`GPR`/`GPRsp` index 7) overlap at
    /// equal indices. Delegates to [`RegClassId::overlaps`].
    pub fn phys_overlap(&self, a: &PhysReg, b: &PhysReg) -> bool {
        a.0.overlaps(a.1, b.0, b.1)
    }

    /// The class that owns the calling convention's argument registers — the
    /// natural default for integer virtual registers whose class is otherwise
    /// undetermined (e.g. function parameters with no register attribute yet).
    pub fn default_integer_class(&self) -> Option<RegClassId> {
        self.classes
            .iter()
            .find(|c| !c.arguments.is_empty())
            .or_else(|| self.classes.iter().find(|c| !c.allocation_order.is_empty()))
            .map(RegClassId::new)
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
    let mut node_classes: Vec<RegClassId> = Vec::with_capacity(vregs.len());
    for &vreg in &vregs {
        let class = resolve_class(liveness, precolor, default_class, vreg)?;
        let mut alts: Vec<Alternative> = class
            .allocation_order
            .iter()
            .map(|&idx| Alternative::Phys((class, idx)))
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
                assignment.insert(vreg, *p);
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
fn resolve_class(
    liveness: &Liveness,
    precolor: &HashMap<u32, PhysReg>,
    default_class: Option<RegClassId>,
    vreg: u32,
) -> Result<RegClassId, RegAllocError> {
    precolor
        .get(&vreg)
        .map(|(c, _)| *c)
        .or_else(|| liveness.vreg_class.get(&vreg).copied())
        .or(default_class)
        .ok_or(RegAllocError::Infeasible(vreg))
}

/// Build the cost vector for one node's alternatives, honoring pre-coloring,
/// forbidden physical registers, and the callee-saved bias.
fn node_costs(
    info: &RegisterInfo,
    alternatives: &[Alternative],
    class: RegClassId,
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
                    let conflict = forbidden
                        .is_some_and(|set| set.iter().any(|f| info.phys_overlap(f, target)));
                    return if !conflict && p.0.span(p.1) == target.0.span(target.1) {
                        0
                    } else {
                        INF_COST
                    };
                }
                if forbidden.is_some_and(|set| set.iter().any(|f| info.phys_overlap(f, p))) {
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
                // Conflict when the two alternatives overlap: the same register
                // through aliasing classes (`GPR`/`GPRsp` index 7), or a group
                // register covering another (`VRM2` v8..v9 vs `VR` v9).
                if info.phys_overlap(lp, rp) {
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

    /// Stack frame alignment in bytes.
    fn frame_align(&self) -> u32 {
        self.slot_size()
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
        class: RegClassId,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Build a load from `[frame + offset]` into virtual register `value`.
    fn emit_spill_reload(
        &self,
        context: &Context,
        value: u32,
        class: RegClassId,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Build a register-to-register copy of virtual register `src` into virtual
    /// register `dst` (both of class `class`). Only reached on targets whose
    /// instructions have tied (two-address) operands, so the default panics.
    fn emit_copy(
        &self,
        context: &Context,
        class: RegClassId,
        dst: u32,
        src: u32,
    ) -> Box<dyn Operation> {
        let _ = (context, class, dst, src);
        unimplemented!("this target has tied operands but no copy emitter")
    }

    /// Whether callee-saved registers are preserved in the spill frame (each at its
    /// `[frame + offset]` slot, so the generic code reserves a slot per saved
    /// register). Targets that save via a dedicated mechanism (x86-64 `push`/`pop`)
    /// return `false`: no frame slot is reserved and the `offset` field of each
    /// save is unused.
    fn saves_on_frame(&self) -> bool {
        true
    }

    /// Prologue instructions reserving a frame of `size` bytes (e.g. `addi sp, sp,
    /// -size`) and saving the callee-saved registers the allocation used, each at
    /// its reserved `[frame + offset]` slot. Inserted at the top of the entry block
    /// when the frame is non-empty.
    fn emit_prologue(
        &self,
        _context: &Context,
        _size: u32,
        _saves: &[(PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    /// Epilogue instructions restoring the saved callee-saved registers and
    /// releasing the frame, inserted before each terminator.
    fn emit_epilogue(
        &self,
        _context: &Context,
        _size: u32,
        _saves: &[(PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    /// Stack offset, relative to the post-prologue frame register, where
    /// incoming stack argument `stack_index` lives.
    fn incoming_stack_arg_offset(
        &self,
        frame_size: u32,
        _saves: &[(PhysReg, i64)],
        stack_index: usize,
    ) -> i64 {
        frame_size as i64 + (stack_index as i64 * self.slot_size() as i64)
    }

    /// Build a load from an incoming stack argument into an already allocated
    /// physical register. Only called for symbols whose argument list exceeds
    /// the ABI register bank.
    fn emit_incoming_stack_arg_load(
        &self,
        _context: &Context,
        dst: &PhysReg,
        _frame: &PhysReg,
        _offset: i64,
    ) -> Result<Box<dyn Operation>, PassError> {
        Err(PassError::InvalidRuleSet(format!(
            "stack-passed arguments are not supported for register class {}",
            dst.0.name()
        )))
    }

    /// Build instruction(s) that materialize `[frame + offset]` into `dst`.
    fn emit_frame_address(
        &self,
        _context: &Context,
        dst: &PhysReg,
        _frame: &PhysReg,
        _offset: i64,
    ) -> Result<Vec<Box<dyn Operation>>, PassError> {
        Err(PassError::InvalidRuleSet(format!(
            "stack allocation addresses are not supported for register class {}",
            dst.0.name()
        )))
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
        _analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        let info = self.target.register_info();
        let blocks = symbol_body_blocks(context, op);
        if blocks.is_empty() {
            return Ok(PreservedAnalyses::all());
        }

        self.lower_tied_operands(context, rewriter, &blocks)?;
        self.lower_block_args(context, rewriter, &blocks)?;

        let abi = abi_precolor(context, op, &info, self.target.as_ref(), rewriter, &blocks)?;

        let mut frame = FrameState::new(self.target.slot_size());
        let stack_allocas = collect_stack_allocas(context, &blocks, &mut frame);
        let assignment = loop {
            // Recomputed each round: spills insert ops within blocks but never add
            // or remove edges, so the CFG is stable across rounds.
            let successors = block_successors(context, &blocks);
            let liveness = liveness::analyze(context, &blocks, |b| {
                successors.get(&b).cloned().unwrap_or_default()
            });
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
                precolor: &abi.precolor,
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

        // Preserve the callee-saved registers the allocation used for this
        // function's caller. Frame-based targets reserve a slot per register;
        // push/pop targets handle framing themselves.
        let saves = callee_saved_slots(&assignment, &mut frame, self.target.saves_on_frame());

        let frame_size = frame.size(self.target.frame_align());
        self.insert_stack_alloca_addresses(
            context,
            rewriter,
            &blocks,
            &assignment,
            &stack_allocas,
        )?;
        erase_stack_allocas(context, rewriter, &stack_allocas)?;
        self.insert_incoming_stack_arg_loads(
            context,
            rewriter,
            &blocks,
            &assignment,
            &abi.stack_args,
            FrameLayout {
                size: frame_size,
                saves: &saves,
            },
        )?;
        if frame_size > 0 || !saves.is_empty() {
            self.insert_frame(context, rewriter, &blocks, frame_size, &saves)?;
        }

        Ok(PreservedAnalyses::none())
    }
}

impl RegisterAllocationPass {
    /// Lower tied (two-address) operands ahead of allocation. Instruction
    /// selection emits `op {dst = %r (ReadWrite), dst_tied = %x, ...}` for an
    /// instruction whose behavior reads its destination (e.g. the x86
    /// `dst = dst + src`): the op defines `%r` but must read `%x` through the same
    /// register. Insert `copy %r <- %x` ahead of the op and drop the marker
    /// attribute, leaving a plain read-modify-write of `%r` that liveness and
    /// coloring already model.
    fn lower_tied_operands(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[BlockId],
    ) -> Result<(), PassError> {
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                let mut ties = Vec::new();
                for attr in &op.attributes {
                    let Some(base) = attr.name.strip_suffix("_tied") else {
                        continue;
                    };
                    if role_of(&op, base) != AttributeRole::ReadWrite {
                        continue;
                    }
                    let AttributeValue::Register(RegisterAttr::Virtual { id: src, .. }) =
                        &attr.value
                    else {
                        continue;
                    };
                    let Some(AttributeValue::Register(RegisterAttr::Virtual { id: dst, class })) =
                        op.attributes
                            .iter()
                            .find(|a| a.name == base)
                            .map(|a| &a.value)
                    else {
                        continue;
                    };
                    let class = class.ok_or_else(|| {
                        PassError::InvalidRuleSet(format!(
                            "tied operand {} has no register class",
                            attr.name
                        ))
                    })?;
                    ties.push((attr.name.clone(), *dst, *src, class));
                }
                if ties.is_empty() {
                    continue;
                }
                let op_ref = op_ref_in(context, block_id, op_id);
                for (_, dst, src, class) in &ties {
                    let copy = self.target.emit_copy(context, *class, *dst, *src);
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                }
                let mut attrs = op.attributes.clone();
                attrs.retain(|a| !ties.iter().any(|(name, ..)| a.name == *name));
                context.set_op_attributes(op_id, attrs);
            }
        }
        Ok(())
    }

    /// Lower forwarded block arguments on unconditional branches to explicit
    /// copies ahead of allocation. A `vbr` carries the values forwarded to its
    /// destination's block parameters as SSA operands (`dest_args`); nothing
    /// otherwise connects a predecessor's forwarded value to the successor's
    /// parameter register. For each edge, insert `copy param <- arg` before the
    /// branch and clear `dest_args`, leaving the parameter defined in every
    /// predecessor. The copies of one edge form a parallel copy (all sources read
    /// before any destination is written), so they are sequentialized with a fresh
    /// temporary per cycle (e.g. a loop edge that swaps two parameters).
    fn lower_block_args(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[BlockId],
    ) -> Result<(), PassError> {
        let info = self.target.register_info();
        let default_class = info.default_integer_class();
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                if op.name != "vbr" || op.operands.is_empty() {
                    continue;
                }
                let args: Vec<u32> = op.operands.iter().map(|v| v.number()).collect();
                let Some(dest) = op.attributes.iter().find_map(|a| match &a.value {
                    AttributeValue::Block(b) if a.name == "dest" => Some(*b),
                    _ => None,
                }) else {
                    return Err(PassError::InvalidRuleSet(
                        "vbr with block arguments is missing its 'dest' target".to_string(),
                    ));
                };
                let params: Vec<u32> = context
                    .get_block(dest)
                    .arguments()
                    .iter()
                    .map(|v| v.id().number())
                    .collect();
                if params.len() != args.len() {
                    return Err(PassError::InvalidRuleSet(format!(
                        "branch forwards {} argument(s) to a block with {} parameter(s)",
                        args.len(),
                        params.len()
                    )));
                }

                // Pair each parameter with its forwarded value and the register
                // class to copy it in (from either endpoint's uses, else the
                // default integer class).
                let mut pairs: Vec<(u32, u32, RegClassId)> = Vec::new();
                for (&param, &arg) in params.iter().zip(args.iter()) {
                    if param == arg {
                        continue;
                    }
                    let class = vreg_class_in(context, blocks, arg)
                        .or_else(|| vreg_class_in(context, blocks, param))
                        .or(default_class)
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet(format!(
                                "block argument vreg {arg} has no register class"
                            ))
                        })?;
                    pairs.push((param, arg, class));
                }

                let op_ref = op_ref_in(context, block_id, op_id);
                for (dst, src, class) in sequence_parallel_copies(context, pairs) {
                    let copy = self.target.emit_copy(context, class, dst, src);
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                }
                context.set_op_operands(op_id, Vec::new());
            }
        }
        Ok(())
    }

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
        let default_class = info.default_integer_class();
        let frame_reg = self.target.frame_register();

        for &vreg in vregs {
            let class = liveness
                .vreg_class
                .get(&vreg)
                .copied()
                .or(default_class)
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(format!("spilled vreg {vreg} has no register class"))
                })?;
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

                    // A read-modify-write occurrence (a ReadWrite attribute, e.g. a
                    // lowered two-address destination) must keep the read and the
                    // write in one register: reload into a single fresh register,
                    // rename both directions to it, and store it back after.
                    if uses && defines {
                        let fresh = context.create_value(ty, None).id().number();
                        frame.temps.insert(fresh);
                        let reload = self
                            .target
                            .emit_spill_reload(context, fresh, class, &frame_reg, offset);
                        let op_ref = op_ref_in(context, block_id, op_id);
                        rewriter.insert_op_before(&op_ref, reload.as_ref())?;
                        rename_attr(context, op_id, vreg, fresh, RoleClass::Read);
                        rename_attr(context, op_id, vreg, fresh, RoleClass::Write);
                        let store = self
                            .target
                            .emit_spill_store(context, fresh, class, &frame_reg, offset);
                        insert_after(context, rewriter, block_id, op_id, store.as_ref())?;
                    } else if uses {
                        let fresh = context.create_value(ty, None).id().number();
                        frame.temps.insert(fresh);
                        let reload = self
                            .target
                            .emit_spill_reload(context, fresh, class, &frame_reg, offset);
                        let op_ref = op_ref_in(context, block_id, op_id);
                        rewriter.insert_op_before(&op_ref, reload.as_ref())?;
                        rename_attr(context, op_id, vreg, fresh, RoleClass::Read);
                    } else if defines {
                        let fresh = context.create_value(ty, None).id().number();
                        frame.temps.insert(fresh);
                        rename_attr(context, op_id, vreg, fresh, RoleClass::Write);
                        let store = self
                            .target
                            .emit_spill_store(context, fresh, class, &frame_reg, offset);
                        insert_after(context, rewriter, block_id, op_id, store.as_ref())?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Insert the prologue at the entry block's top and an epilogue before every
    /// terminator, once the frame size and callee-saved set are known.
    fn insert_frame(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[BlockId],
        size: u32,
        saves: &[(PhysReg, i64)],
    ) -> Result<(), PassError> {
        if let Some(&entry) = blocks.first() {
            let op_ids = context.get_block(entry).op_ids();
            if let Some(&first) = op_ids.first() {
                let target = op_ref_in(context, entry, first);
                for op in self.target.emit_prologue(context, size, saves) {
                    rewriter.insert_op_before(&target, op.as_ref())?;
                }
            }
        }
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                if context.get_op(op_id).name != "vret" {
                    continue;
                }
                let target = op_ref_in(context, block_id, op_id);
                for op in self.target.emit_epilogue(context, size, saves) {
                    rewriter.insert_op_before(&target, op.as_ref())?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_incoming_stack_arg_loads(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[BlockId],
        assignment: &HashMap<u32, PhysReg>,
        args: &[IncomingStackArg],
        layout: FrameLayout<'_>,
    ) -> Result<(), PassError> {
        if args.is_empty() {
            return Ok(());
        }
        let Some(&entry) = blocks.first() else {
            return Ok(());
        };
        let op_ids = context.get_block(entry).op_ids();
        let Some(&first) = op_ids.first() else {
            return Ok(());
        };
        let target = op_ref_in(context, entry, first);
        let frame = self.target.frame_register();
        for arg in args {
            let Some(dst) = assignment.get(&arg.vreg) else {
                continue;
            };
            if dst.0 != arg.class {
                return Err(PassError::InvalidRuleSet(format!(
                    "stack argument vreg {} assigned to {:?}, expected class {}",
                    arg.vreg,
                    dst,
                    arg.class.name()
                )));
            }
            let offset =
                self.target
                    .incoming_stack_arg_offset(layout.size, layout.saves, arg.stack_index);
            let load = self
                .target
                .emit_incoming_stack_arg_load(context, dst, &frame, offset)?;
            rewriter.insert_op_before(&target, load.as_ref())?;
        }
        Ok(())
    }

    fn insert_stack_alloca_addresses(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[BlockId],
        assignment: &HashMap<u32, PhysReg>,
        allocas: &[StackAlloca],
    ) -> Result<(), PassError> {
        if allocas.is_empty() {
            return Ok(());
        }
        let Some(&entry) = blocks.first() else {
            return Ok(());
        };
        let op_ids = context.get_block(entry).op_ids();
        let Some(&first) = op_ids.first() else {
            return Ok(());
        };
        let target = op_ref_in(context, entry, first);
        let frame = self.target.frame_register();
        for alloca in allocas {
            let Some(dst) = assignment.get(&alloca.vreg) else {
                continue;
            };
            for op in self
                .target
                .emit_frame_address(context, dst, &frame, alloca.offset)?
            {
                rewriter.insert_op_before(&target, op.as_ref())?;
            }
        }
        Ok(())
    }
}

struct FrameLayout<'a> {
    size: u32,
    saves: &'a [(PhysReg, i64)],
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

    fn size(&self, align: u32) -> u32 {
        let size = self.next_offset as u32;
        if size == 0 {
            return 0;
        }
        let align = align.max(1);
        size.div_ceil(align) * align
    }
}

struct StackAlloca {
    op_id: OpId,
    block: BlockId,
    vreg: u32,
    offset: i64,
}

fn collect_stack_allocas(
    context: &Context,
    blocks: &[BlockId],
    frame: &mut FrameState,
) -> Vec<StackAlloca> {
    let mut allocas = Vec::new();
    for &block in blocks {
        for op_id in context.get_block(block).op_ids() {
            let op = context.get_op(op_id);
            if op.dialect != "ptr" || op.name != "alloca" {
                continue;
            }
            let Some(result) = op.results.first() else {
                continue;
            };
            allocas.push(StackAlloca {
                op_id,
                block,
                vreg: result.number(),
                offset: frame.alloc_slot(),
            });
        }
    }
    allocas
}

fn erase_stack_allocas(
    context: &Context,
    rewriter: &mut Rewriter,
    allocas: &[StackAlloca],
) -> Result<(), PassError> {
    for alloca in allocas {
        if context.has_operation(alloca.op_id) {
            let op_ref = op_ref_in(context, alloca.block, alloca.op_id);
            rewriter.erase_op(&op_ref)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RoleClass {
    Read,
    Write,
}

fn is_vreg(r: &liveness::RegRef, vreg: u32) -> bool {
    matches!(r, liveness::RegRef::Virtual { id, .. } if *id == vreg)
}

/// The control-flow successors of each block, for liveness's inter-block
/// dataflow. A machine block may hold several branch-shaped ops — a mid-block
/// conditional jump for the taken edge plus a trailing virtual branch for the
/// fallthrough — so a block's successors are the union of `Terminator::successors`
/// over every op it contains, not just its last op's.
fn block_successors(context: &Context, blocks: &[BlockId]) -> HashMap<BlockId, Vec<BlockId>> {
    let mut map = HashMap::new();
    for &block_id in blocks {
        let mut succs = Vec::new();
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            if let Some(term) = op.as_interface::<dyn tir::Terminator>() {
                for succ in term.successors() {
                    if !succs.contains(&succ) {
                        succs.push(succ);
                    }
                }
            }
        }
        map.insert(block_id, succs);
    }
    map
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

/// The callee-saved physical registers the allocation actually used, each paired
/// with a freshly reserved frame slot. A callee-saved register belongs to the
/// caller; if this function colors a value into one it must save and restore it
/// around the body. Deterministic order (by class then index) keeps codegen
/// stable.
fn callee_saved_slots(
    assignment: &HashMap<u32, PhysReg>,
    frame: &mut FrameState,
    on_frame: bool,
) -> Vec<(PhysReg, i64)> {
    let mut regs: Vec<PhysReg> = assignment
        .values()
        .filter(|p| p.0.is_callee_saved(p.1))
        .copied()
        .collect();
    regs.sort();
    regs.dedup();
    regs.into_iter()
        .map(|p| (p, if on_frame { frame.alloc_slot() } else { 0 }))
        .collect()
}

/// Sequentialize a parallel copy: a set of `dst <- src` moves (destinations
/// unique) whose sources are all read before any destination is written. Emit a
/// copy whose destination is not still needed as a source first; when only cycles
/// remain, save one destination into a fresh temporary and reroute the reads of it
/// through the temporary, which frees that destination and breaks the cycle. Each
/// tuple carries the register class to copy the destination in; a temporary
/// inherits the class and type of the value it saves.
fn sequence_parallel_copies(
    context: &Context,
    mut copies: Vec<(u32, u32, RegClassId)>,
) -> Vec<(u32, u32, RegClassId)> {
    let mut result = Vec::new();
    while !copies.is_empty() {
        if let Some(i) = copies
            .iter()
            .position(|(dst, _, _)| !copies.iter().any(|(_, src, _)| src == dst))
        {
            result.push(copies.remove(i));
        } else {
            // Only cycles remain: break one by saving its destination.
            let (dst, _, class) = copies[0];
            let ty = context.get_value(ValueId::from_number(dst)).ty();
            let temp = context.create_value(ty, None).id().number();
            result.push((temp, dst, class));
            for (_, src, _) in copies.iter_mut() {
                if *src == dst {
                    *src = temp;
                }
            }
        }
    }
    result
}

/// Compute the calling-convention pre-coloring: each argument register pinned in
/// order, and the returned value pinned to the first return register.
///
/// A physical-register constraint applies at a program point, not to a whole live
/// range: an argument register pins the value at entry, a return register pins it
/// at the `vret`. When the same vreg carries both constraints with *different*
/// registers (e.g. an identity function whose argument register differs from the
/// return register), pinning it twice would silently overwrite one constraint and
/// miscompile. Such a return is broken with a copy (`fresh <- vreg`) inserted
/// before the `vret`, so the argument keeps its entry pin while `fresh` takes the
/// return pin. This mutates the IR, so it runs once, outside the spill loop.
struct AbiPrecolor {
    precolor: HashMap<u32, PhysReg>,
    stack_args: Vec<IncomingStackArg>,
}

struct IncomingStackArg {
    vreg: u32,
    class: RegClassId,
    stack_index: usize,
}

fn abi_precolor(
    context: &Context,
    op: &OperationRef,
    info: &RegisterInfo,
    target: &dyn TargetRegAlloc,
    rewriter: &mut Rewriter,
    blocks: &[BlockId],
) -> Result<AbiPrecolor, PassError> {
    let mut precolor: HashMap<u32, PhysReg> = HashMap::new();
    let mut stack_args = Vec::new();

    // Argument vregs: the symbol's `arg_regs` attribute carries each argument's
    // register class (assigned by the target's function lowering, e.g. vectors
    // in `VR`, floats in `FPR32`/`FPR64`, everything else in `GPR`). Each
    // argument takes the next calling-convention register of its class, with
    // the slot counter shared across classes of one register *file*: an f32 and
    // an f64 argument draw fa0 and fa1 from the same fa0..fa7 sequence.
    let mut next_slot: HashMap<&str, usize> = HashMap::new();
    if let Some(AttributeValue::Array(args)) = op
        .op()
        .attributes
        .iter()
        .find(|a| a.name == "arg_regs")
        .map(|a| &a.value)
    {
        for attr in args {
            let AttributeValue::Register(RegisterAttr::Virtual { id, class }) = attr else {
                continue;
            };
            let Some(rc) = class.or_else(|| info.default_integer_class()) else {
                continue;
            };
            let slot = next_slot.entry(rc.file).or_insert(0);
            if let Some(&reg) = rc.arguments.get(*slot) {
                let pin = (rc, reg);
                if let Some(prev) = precolor.insert(*id, pin)
                    && prev != pin
                {
                    return Err(PassError::InvalidRuleSet(format!(
                        "argument vreg {id} pinned to conflicting registers {prev:?} and {pin:?}"
                    )));
                }
            } else {
                stack_args.push(IncomingStackArg {
                    vreg: *id,
                    class: rc,
                    stack_index: *slot - rc.arguments.len(),
                });
            }
            *slot += 1;
        }
    }

    // Return value: the operand of the `vret` terminator, in the first return
    // register of the value's class.
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let body_op = context.get_op(op_id);
            if body_op.name != "vret" {
                continue;
            }
            let Some(value) = body_op.operands.first().copied() else {
                continue;
            };
            let vreg = value.number();
            let class = vreg_class_in(context, blocks, vreg);
            let Some(rc) = class.or_else(|| info.default_integer_class()) else {
                continue;
            };
            let Some(&ret_reg) = rc.return_values.first() else {
                continue;
            };
            let ret_pin = (rc, ret_reg);

            match precolor.get(&vreg) {
                // Already pinned to exactly the return register (or returned by
                // several `vret`s): nothing to do.
                Some(existing) if *existing == ret_pin => {}
                // Pinned elsewhere (an argument register): break the point
                // conflict with a copy into a fresh vreg that takes the return
                // pin, and retarget the `vret` at it.
                Some(_) => {
                    let ty = context.get_value(value).ty();
                    let fresh = context.create_value(ty, None).id().number();
                    let copy = target.emit_copy(context, rc, fresh, vreg);
                    let op_ref = op_ref_in(context, block_id, op_id);
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                    context.set_op_operand(op_id, 0, ValueId::from_number(fresh));
                    precolor.insert(fresh, ret_pin);
                }
                None => {
                    precolor.insert(vreg, ret_pin);
                }
            }
        }
    }

    Ok(AbiPrecolor {
        precolor,
        stack_args,
    })
}

/// The register class a virtual register is referenced with, from the first
/// class-qualified register attribute naming it.
fn vreg_class_in(context: &Context, blocks: &[BlockId], vreg: u32) -> Option<RegClassId> {
    let class_of = |value: &AttributeValue| match value {
        AttributeValue::Register(RegisterAttr::Virtual { id, class: Some(c) }) if *id == vreg => {
            Some(*c)
        }
        _ => None,
    };
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            for attr in &context.get_op(op_id).attributes {
                if let Some(c) = class_of(&attr.value) {
                    return Some(c);
                }
                if let AttributeValue::Array(items) = &attr.value
                    && let Some(c) = items.iter().find_map(&class_of)
                {
                    return Some(c);
                }
            }
        }
    }
    None
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
                class: *class,
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
                        class: *class,
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
    use std::sync::Arc;
    use tir::builtin::{IntegerType, ops};
    use tir::{Block, IRBuilder};

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
                group_width: 1,
            }],
        }
    }

    /// The `RegClassId` for class `name` in `info`. Register-class tables are
    /// promoted statics, so ids from repeated `three_reg_info()` calls (or any
    /// `RegisterInfo` over the same literal) share one pointer identity.
    fn id_of(info: &RegisterInfo, name: &str) -> RegClassId {
        info.class(name).unwrap()
    }

    fn r_id() -> RegClassId {
        id_of(&three_reg_info(), "R")
    }

    fn liveness_with(vregs: &[u32], edges: &[(u32, u32)]) -> Liveness {
        let mut lv = Liveness::default();
        for &v in vregs {
            lv.vregs.insert(v);
            lv.vreg_class.insert(v, r_id());
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

    fn addi(context: &Context, block: &Arc<Block>, a: ValueId, b: ValueId, ty: tir::TypeId) -> u32 {
        let mut builder = IRBuilder::new(block.clone());
        builder
            .insert(ops::addi(context, a, b, ty).build())
            .result()
            .number()
    }

    // A value defined in the entry block and read in a successor block must not
    // share a register with a temporary defined in the entry block after it: the
    // cross-block liveness edge forces distinct registers. Without real CFG
    // successors the two look non-interfering and the allocator may coalesce them,
    // clobbering the cross-block value.
    #[test]
    fn cross_block_liveness_forces_distinct_registers() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None);
        let a_id = a.id();
        let entry = context.create_block(vec![a]);
        let succ = context.create_block(vec![]);

        let v = addi(&context, &entry, a_id, a_id, ty);
        let w = ValueId::from_number(addi(&context, &entry, a_id, a_id, ty));
        addi(&context, &entry, w, w, ty); // `w` dies in the entry block
        addi(&context, &succ, ValueId::from_number(v), a_id, ty); // `v` read across the edge

        let blocks = [entry.id(), succ.id()];
        let liveness = liveness::analyze(&context, &blocks, |blk| {
            if blk == entry.id() {
                vec![succ.id()]
            } else {
                vec![]
            }
        });

        let info = three_reg_info();
        let precolor = HashMap::new();
        let map = assigned(
            allocate(&AllocConfig {
                info: &info,
                liveness: &liveness,
                precolor: &precolor,
                spill_cost: &|_| 100,
            })
            .unwrap(),
        );
        assert_ne!(
            map[&v],
            map[&w.number()],
            "a value live across a block edge must not reuse a later entry-block register",
        );
    }

    // Sequentializing a parallel copy that swaps two registers (a loop edge
    // forwarding `^loop(%b, %a)` from within `^loop(%a, %b)`) needs one temporary
    // to break the cycle: naive `a<-b; b<-a` would lose a's value.
    #[test]
    fn parallel_copy_swap_uses_a_temporary() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None).id().number();
        let b = context.create_value(ty, None).id().number();

        let seq = sequence_parallel_copies(&context, vec![(a, b, r_id()), (b, a, r_id())]);
        assert_eq!(seq.len(), 3, "a swap needs a saving temporary");

        // Simulate: each register starts holding its own original value; applying
        // the emitted copies in order must swap a and b.
        let mut regs: HashMap<u32, u32> = HashMap::new();
        regs.insert(a, a);
        regs.insert(b, b);
        for (dst, src, _) in &seq {
            let v = *regs.get(src).expect("source read before it is written");
            regs.insert(*dst, v);
        }
        assert_eq!(regs[&a], b, "a receives b's original value");
        assert_eq!(regs[&b], a, "b receives a's original value");
    }

    // A non-cyclic parallel copy is ordered so a value is never overwritten before
    // it is read: `c<-a` must precede `a<-b`.
    #[test]
    fn parallel_copy_orders_reads_before_writes() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None).id().number();
        let b = context.create_value(ty, None).id().number();
        let c = context.create_value(ty, None).id().number();

        let seq = sequence_parallel_copies(&context, vec![(a, b, r_id()), (c, a, r_id())]);
        assert_eq!(seq.len(), 2, "no cycle, no temporary");
        let mut regs: HashMap<u32, u32> = HashMap::new();
        regs.insert(a, a);
        regs.insert(b, b);
        regs.insert(c, c);
        for (dst, src, _) in &seq {
            let v = *regs.get(src).unwrap();
            regs.insert(*dst, v);
        }
        assert_eq!(regs[&a], b);
        assert_eq!(regs[&c], a, "c must capture a's original value, not b's");
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
        precolor.insert(1u32, (r_id(), 0u16));
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        assert_eq!(map[&1], (r_id(), 0));
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
            .extend([(r_id(), 0u16), (r_id(), 1u16)]);
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
            (r_id(), 2),
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
            group_width: 1,
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
            group_width: 1,
        },
    ];

    fn two_class_liveness(class1: RegClassId, class2: RegClassId) -> Liveness {
        let mut lv = Liveness::default();
        lv.vregs.insert(1);
        lv.vreg_class.insert(1, class1);
        lv.vregs.insert(2);
        lv.vreg_class.insert(2, class2);
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
        let liveness = two_class_liveness(id_of(&info, "GPR"), id_of(&info, "GPRsp"));
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
                group_width: 1,
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
                group_width: 1,
            },
        ];
        let info = RegisterInfo { classes: CLASSES };
        let liveness = two_class_liveness(id_of(&info, "A"), id_of(&info, "B"));
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        assert_eq!(map[&1], (id_of(&info, "A"), 0));
        assert_eq!(map[&2], (id_of(&info, "B"), 0));
    }

    #[test]
    fn group_registers_interfere_by_span() {
        // An RVV-style LMUL=2 group class: a `VRM2` register covers two `VR`
        // indices, so an interfering single-register vreg must land outside the
        // group's span. With only v0..v2 available, the group takes (VRM2, 0)
        // = v0..v1 and the scalar is pushed to v2 (not v1, which overlaps).
        static CLASSES: &[RegClassInfo] = &[
            RegClassInfo {
                name: "VR",
                file: "VR",
                allocation_order: &[0, 1, 2],
                caller_saved: &[0, 1, 2],
                callee_saved: &[],
                arguments: &[],
                return_values: &[],
                reserved: &[],
                group_width: 1,
            },
            RegClassInfo {
                name: "VRM2",
                file: "VR",
                allocation_order: &[0],
                caller_saved: &[0],
                callee_saved: &[],
                arguments: &[],
                return_values: &[],
                reserved: &[],
                group_width: 2,
            },
        ];
        let info = RegisterInfo { classes: CLASSES };
        let vrm2 = id_of(&info, "VRM2");
        let vr = id_of(&info, "VR");
        assert!(info.phys_overlap(&(vrm2, 0), &(vr, 1)));
        assert!(!info.phys_overlap(&(vrm2, 0), &(vr, 2)));
        // The overlap API is also exposed directly on the class handle.
        assert!(vrm2.overlaps(0, vr, 1));
        assert!(!vrm2.overlaps(0, vr, 2));

        let liveness = two_class_liveness(vrm2, vr);
        let precolor = HashMap::new();
        let result = allocate(&AllocConfig {
            info: &info,
            liveness: &liveness,
            precolor: &precolor,
            spill_cost: &|_| 100,
        })
        .unwrap();

        let map = assigned(result);
        assert_eq!(map[&1], (vrm2, 0));
        assert_eq!(map[&2], (vr, 2));
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
                group_width: 1,
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
                group_width: 1,
            },
        ];
        let info = RegisterInfo { classes: CLASSES };
        let mut liveness = Liveness::default();
        liveness.vregs.insert(1);
        liveness.vreg_class.insert(1, id_of(&info, "GPRsp"));
        liveness
            .forbidden
            .entry(1)
            .or_default()
            .insert((id_of(&info, "GPR"), 0u16));
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
            (id_of(&info, "GPRsp"), 1),
            "a forbidden index aliases across the shared file"
        );
    }
}
