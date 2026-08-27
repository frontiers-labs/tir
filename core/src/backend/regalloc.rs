//! Target-independent register allocation.
//!
//! The allocator works on machine IR produced by instruction selection, where a
//! register operand is an SSA operand or result whose type is its register
//! class. It computes liveness, builds an interference graph, and solves an
//! optimal coloring with the shared PBQP solver ([`tir_pbqp`]). Nothing is
//! rewritten: the chosen registers are written onto the function's `asm.symbol`
//! as a [`crate::backend::RegAssignment`], which assembly printing and encoding
//! read.
//!
//! Register files come from [`RegisterInfo`]; allocation order and calling
//! convention policy come from the selected [`crate::backend::abi::AbiInfo`].

use std::collections::{HashMap, HashSet};

use tir::attributes::AttributeValue;
use tir::{
    AnalysisManager, BlockId, Context, OpId, Operation, OperationRef, Pass, PassError, PassTarget,
    Rewriter, ValueId,
};
use tir_pbqp::{self as pbqp, INF_COST, PbqpMatrix, PbqpNodeId, PbqpProblem};

use crate::backend::liveness::{self, Liveness, PhysReg};
use crate::backend::prealloc;
use crate::backend::registers::fresh_reg;
use crate::backend::{SymbolOp, VirtualCallOp, VirtualIndirectCallOp, VirtualReturnOp};
use crate::ptr::AllocaOp;

/// Architectural metadata for one register class.
#[derive(Debug, Clone)]
pub struct RegClassInfo {
    pub name: &'static str,
    /// The dialect whose TMDL description declares this class. Names the
    /// register-class type a value of this class carries (`!x86_64.GPR`).
    pub dialect: &'static str,
    /// The physical register file this class draws from — the root of its TMDL
    /// inheritance chain. Classes that share a file (e.g. AArch64 `GPR` and
    /// `GPRsp`, which differ only in whether encoding 31 is `xzr` or `sp`) name the
    /// same physical register at a given index, so the allocator treats their
    /// indices as aliases. A standalone class is its own file.
    pub file: &'static str,
    /// The file indices this class can encode, ascending. A class that views only
    /// part of its file (x86 `GPR32low`, whose REX-free encodings reach eax..edi)
    /// lists just those indices, and the allocator hands out nothing else.
    pub registers: &'static [u16],
    /// How many consecutive file indices one register of this class covers.
    /// 1 for ordinary classes; an RVV LMUL>1 group class covers 2/4/8 (e.g.
    /// `VRM2` index 8 is the architectural pair v8..v9).
    pub group_width: u16,
    /// Where this class's architectural view sits in its storage element and
    /// whether a write merges into it (see [`RegisterView`]).
    pub view: RegisterView,
    /// Renders one of this class's registers by encoding index, so a physical
    /// register prints as its assembly name wherever it appears.
    pub print_name: crate::backend::asm_desc::RegisterNamePrinter,
}

/// Two class records describe the same class when their architecture does; the
/// name printer is a rendering detail and is not compared.
impl PartialEq for RegClassInfo {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.dialect == other.dialect
            && self.file == other.file
            && self.registers == other.registers
            && self.group_width == other.group_width
            && self.view == other.view
    }
}

impl Eq for RegClassInfo {}

/// The [`RegClassInfo::print_name`] of a class with no assembly names, used by
/// tests and by classes a target does not print.
pub fn no_register_name(_index: u16, _prefer_abi: bool) -> Option<String> {
    None
}

/// A handle to a register class: a pointer to its `'static` [`RegClassInfo`].
///
/// Register classes are per-dialect statics emitted by the TMDL backend — the
/// generated `RegClass::X.id()` and `register_info().classes` point at the same
/// table — so a class's identity is the identity of that pointer. Equality and
/// hashing are by pointer; ordering is by name so codegen that sorts physical
/// registers stays deterministic across builds. Derefs to [`RegClassInfo`], so a
/// its architectural properties read directly through the handle.
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

    /// The dialect this class belongs to (see [`RegClassInfo::dialect`]).
    pub fn dialect(self) -> &'static str {
        self.0.dialect
    }

    /// The physical register file this class draws from (see [`RegClassInfo::file`]).
    pub fn file(self) -> &'static str {
        self.0.file
    }

    /// Whether this class can encode file index `index` (see
    /// [`RegClassInfo::registers`]).
    pub fn contains(self, index: u16) -> bool {
        self.0.registers.contains(&index)
    }

    /// Whether both classes name the same physical registers at equal indices: one
    /// file, one view offset (an x86 high-byte class views its file at bit 8 and
    /// shares no register with an offset-0 class) and one group width. Only then
    /// can a value be constrained by both at once.
    pub fn shares_view_with(self, other: RegClassId) -> bool {
        self.file() == other.file()
            && self.view.bit_offset == other.view.bit_offset
            && self.group_width == other.group_width
    }

    /// Whether every register this class can encode is also encodable by `other`
    /// through the same architectural view — so a value constrained to both must
    /// come from this, the narrower, class.
    pub fn is_subclass_of(self, other: RegClassId) -> bool {
        self.shares_view_with(other) && self.registers.iter().all(|index| other.contains(*index))
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

    pub fn default_integer_class(&self, abi: &crate::backend::abi::AbiInfo) -> Option<RegClassId> {
        self.default_class(abi, crate::backend::abi::ValueKind::Int)
    }

    /// The class a value of `kind` lives in when no instruction naming it has
    /// pinned a narrower one: the file the ABI passes such values through.
    pub fn default_class(
        &self,
        abi: &crate::backend::abi::AbiInfo,
        kind: crate::backend::abi::ValueKind,
    ) -> Option<RegClassId> {
        abi.args
            .iter()
            .find(|sequence| sequence.kind == kind)
            .and_then(|sequence| sequence.regs.first())
            .map(|register| register.0)
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
    /// A virtual register is referenced through register classes that cannot both
    /// be honored (see [`Liveness::class_conflicts`]). Carries the offending vreg.
    ClassConflict(u32),
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
    pub abi: &'a crate::backend::abi::AbiInfo,
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
    allocate_with_affinities(config, &[])
}

fn allocate_with_affinities(
    config: &AllocConfig,
    affinities: &[(u32, u32)],
) -> Result<AllocResult, RegAllocError> {
    let AllocConfig {
        info,
        abi,
        liveness,
        precolor,
        spill_cost,
    } = config;

    // Deterministic node order.
    let vregs: Vec<u32> = liveness.vregs.iter().copied().collect();
    let node_of: HashMap<u32, usize> = vregs.iter().enumerate().map(|(i, &v)| (v, i)).collect();

    let default_class = info.default_integer_class(abi);

    // Per-node alternative lists, resolved to concrete physical registers.
    let mut alternatives: Vec<Vec<Alternative>> = Vec::with_capacity(vregs.len());
    for &vreg in &vregs {
        let class = resolve_class(liveness, precolor, default_class, vreg)?;
        // A vreg referenced through several classes over one view (an x86 value
        // that is both a REX-free operand and a SIB index) is allocatable only
        // from the indices all of them encode.
        let allowed = liveness.allowed_indices.get(&vreg);
        let mut alts: Vec<Alternative> = allocation_order(abi, class)
            .into_iter()
            .filter(|(_, index)| allowed.is_none_or(|allowed| allowed.contains(index)))
            .map(Alternative::Phys)
            .collect();
        alts.push(Alternative::Spill);
        alternatives.push(alts);
    }

    let mut problem = PbqpProblem::new();
    for (i, &vreg) in vregs.iter().enumerate() {
        let costs = node_costs(
            info,
            &alternatives[i],
            vreg,
            liveness,
            precolor,
            abi,
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

    for &(u, v) in affinities {
        let (Some(&iu), Some(&iv)) = (node_of.get(&u), node_of.get(&v)) else {
            continue;
        };
        if iu == iv {
            continue;
        }
        problem.add_edge(
            PbqpNodeId::from_index(iu),
            PbqpNodeId::from_index(iv),
            affinity_matrix(&alternatives[iu], &alternatives[iv]),
        );
    }

    crate::memstats::pbqp_census(
        "regalloc",
        problem.node_count(),
        problem.edge_count(),
        problem.matrix_bytes(),
    );

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

fn affinity_matrix(left: &[Alternative], right: &[Alternative]) -> PbqpMatrix {
    let mut matrix = PbqpMatrix::zero(left.len(), right.len());
    for (i, l) in left.iter().enumerate() {
        for (j, r) in right.iter().enumerate() {
            if let (Alternative::Phys(lp), Alternative::Phys(rp)) = (l, r)
                && lp.0.span(lp.1) != rp.0.span(rp.1)
            {
                matrix.set(i, j, 1);
            }
        }
    }
    matrix
}

/// Determine the register class a virtual register must be allocated from: the
/// intersection of the class its operands constrain it to and its pinned
/// register's class, falling back to the target's default integer class.
///
/// A pin says *which* register the value lives in; the operand class says which
/// registers the instruction that reads it can encode. Both hold, so the narrower
/// wins — widening to the pin's class would hand out an index the encoding drops,
/// silently naming a different register. A pin that the narrow class cannot encode
/// leaves no legal color and fails allocation loudly.
fn resolve_class(
    liveness: &Liveness,
    precolor: &HashMap<u32, PhysReg>,
    default_class: Option<RegClassId>,
    vreg: u32,
) -> Result<RegClassId, RegAllocError> {
    if liveness.class_conflicts.contains_key(&vreg) {
        return Err(RegAllocError::ClassConflict(vreg));
    }
    let constraint = liveness.vreg_class.get(&vreg).copied();
    let pinned = precolor.get(&vreg).map(|(class, _)| *class);
    match (constraint, pinned) {
        (Some(constraint), Some(pinned)) if constraint.is_subclass_of(pinned) => Ok(constraint),
        (_, Some(pinned)) => Ok(pinned),
        (Some(constraint), None) => Ok(constraint),
        (None, None) => default_class.ok_or(RegAllocError::Infeasible(vreg)),
    }
}

/// Build the cost vector for one node's alternatives, honoring pre-coloring,
/// forbidden physical registers, and the callee-saved bias.
fn node_costs(
    info: &RegisterInfo,
    alternatives: &[Alternative],
    vreg: u32,
    liveness: &Liveness,
    precolor: &HashMap<u32, PhysReg>,
    abi: &crate::backend::abi::AbiInfo,
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
                if abi
                    .callee_saved
                    .iter()
                    .any(|saved| info.phys_overlap(saved, p))
                {
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

fn allocation_order(abi: &crate::backend::abi::AbiInfo, class: RegClassId) -> Vec<PhysReg> {
    let mut result = Vec::new();
    for register in abi.caller_saved.iter().chain(abi.callee_saved) {
        if register.0.file() == class.file()
            && class.contains(register.1)
            && register.1 % class.group_width.max(1) == 0
        {
            let candidate = (class, register.1);
            let is_role = std::iter::once(&abi.sp)
                .chain(abi.ra.iter())
                .chain(abi.fp.iter())
                .chain(abi.reserved)
                .any(|role| candidate.0.overlaps(candidate.1, role.0, role.1));
            if !is_role && !result.contains(&candidate) {
                result.push(candidate);
            }
        }
    }
    result
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

    /// Build a store of `value` (of class `class`) to `[frame + offset]`.
    fn emit_spill_store(
        &self,
        context: &Context,
        value: ValueId,
        class: RegClassId,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Build a load from `[frame + offset]` defining `value`, which the caller
    /// has already created with the class's type.
    fn emit_spill_reload(
        &self,
        context: &Context,
        value: ValueId,
        class: RegClassId,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Build a register-to-register copy of `src` into `dst` (both of class
    /// `class`). Either end is a value the copy defines or reads, or a physical
    /// register named directly. Only reached on targets whose instructions have
    /// tied (two-address) operands or a calling convention, so the default
    /// panics.
    fn emit_copy(
        &self,
        context: &Context,
        class: RegClassId,
        dst: crate::backend::RegSlot,
        src: crate::backend::RegSlot,
    ) -> Box<dyn Operation> {
        let _ = (context, class, dst, src);
        unimplemented!("this target has tied operands but no copy emitter")
    }

    /// Prologue instructions reserving a frame of `size` bytes (e.g. `addi sp, sp,
    /// -size`) and saving the callee-saved registers the allocation used, each at
    /// its reserved `[frame + offset]` slot. Inserted at the top of the entry block
    /// when the frame is non-empty.
    fn emit_prologue(
        &self,
        _context: &Context,
        _abi: &crate::backend::abi::AbiInfo,
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
        _abi: &crate::backend::abi::AbiInfo,
        _size: u32,
        _saves: &[(PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    /// Build instruction(s) that materialize `[frame + offset]` into the virtual
    /// register `dst`.
    fn emit_frame_address(
        &self,
        _context: &Context,
        _dst: ValueId,
        class: RegClassId,
        _frame: &PhysReg,
        _offset: i64,
    ) -> Result<Vec<Box<dyn Operation>>, PassError> {
        Err(PassError::InvalidRuleSet(format!(
            "stack allocation addresses are not supported for register class {}",
            class.name()
        )))
    }
}

/// A register allocation pass. Runs on each `asm.symbol` op produced by instruction
/// selection: it computes liveness over the symbol's body, pre-colors the calling
/// convention's argument and return registers, solves an optimal coloring with
/// [`allocate`], spills and retries when the optimum demands it, and finally
/// records where every value went in the symbol's
/// [`crate::backend::RegAssignment`].
pub struct RegisterAllocationPass {
    target: Box<dyn TargetRegAlloc>,
    abi: &'static crate::backend::abi::AbiInfo,
    /// Safety valve against a non-converging spill loop.
    max_rounds: usize,
}

impl RegisterAllocationPass {
    pub fn with_abi(
        target: Box<dyn TargetRegAlloc>,
        abi: &'static crate::backend::abi::AbiInfo,
    ) -> Self {
        Self {
            target,
            abi,
            max_rounds: 16,
        }
    }
}

impl Pass for RegisterAllocationPass {
    fn name(&self) -> &'static str {
        "register-allocation"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let info = self.target.register_info();
        let blocks = symbol_body_blocks(context, op);
        if blocks.is_empty() {
            return Ok(());
        }

        let precolor = self.lower_fixed_registers(context, rewriter, op, &blocks)?;
        let coalescable_copies = collect_coalescable_copies(context, &blocks)?;
        let affinities: Vec<_> = coalescable_copies
            .iter()
            .map(|copy| (copy.src, copy.dst))
            .collect();

        let (outgoing_size, has_calls) = outgoing_stack_layout(context, &blocks)?;
        let mut frame = FramePlan::new(self.abi);
        frame.reserve_outgoing(outgoing_size);
        let stack_allocas = collect_stack_allocas(context, &blocks, &mut frame);
        self.rematerialize_stack_allocas(context, rewriter, &blocks, &stack_allocas, &mut frame)?;
        // Rematerializing left the allocations naming nothing, so they go now
        // rather than after allocation: a definition the function still holds is
        // a live range the allocator may pick to spill, and the spill code it
        // would write names a value this erasure is about to retire.
        erase_stack_allocas(context, rewriter, &stack_allocas)?;
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

            let result = allocate_with_affinities(
                &AllocConfig {
                    info: &info,
                    abi: self.abi,
                    liveness: &liveness,
                    precolor: &precolor,
                    spill_cost: &spill_cost,
                },
                &affinities,
            )
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

        for copy in &coalescable_copies {
            if !context.has_operation(copy.op) {
                continue;
            }
            // Read the endpoints as they stand: spilling and an earlier
            // coalesce may have renamed either end since collection.
            let erasable = matches!(
                copy_endpoints(context, copy.op),
                Some((src, dst)) if matches!(
                    (assignment.get(&src), assignment.get(&dst)),
                    (Some(src), Some(dst)) if src.0.span(src.1) == dst.0.span(dst.1)
                )
            );
            if erasable {
                // Both ends live in one register, so the copy is a self-move.
                // Its destination value stays: the assignment placed it, and a
                // two-address instruction may define it again.
                rewriter.erase_op_keeping_results(&op_ref_in(context, copy.block, copy.op))?;
            } else {
                strip_attr(context, copy.op, prealloc::COALESCABLE_COPY_ATTR);
            }
        }
        // Preserve the callee-saved registers the allocation used for this
        // function's caller. Frame-slot targets reserve a normal frame slot;
        // push/pop targets keep saves outside the stable frame area.
        let saves = callee_saved_slots(&assignment, &mut frame, self.abi.callee_saved);

        let frame_size = frame.prologue_adjustment(has_calls, saves.len());
        let stack_args = collect_stack_arg_loads(context, &blocks)?;
        self.insert_incoming_stack_arg_loads(
            context,
            rewriter,
            &blocks,
            &assignment,
            &stack_args,
            &frame,
            frame_size,
            saves.len(),
        )?;
        // Allocation ends by recording where every value went; the ops it
        // decided about are untouched.
        // Coalescing and spilling retire values; the map describes the ones the
        // function still names.
        let map: crate::backend::RegAssignment = assignment
            .iter()
            .map(|(&vreg, &register)| (ValueId::from_number(vreg), register))
            .filter(|(value, _)| context.has_value(*value))
            .collect();
        let mut attrs = op.op().attributes().to_vec();
        let pins = context.sym(crate::backend::ARG_PINS_ATTR);
        attrs.retain(|attr| Some(attr.name) != pins);
        attrs.push(context.named_attribute(crate::backend::ASSIGNMENT_ATTR, map.to_attribute()));
        context.set_op_attributes(op.op().id, attrs);
        if frame_size > 0 || !saves.is_empty() {
            self.insert_frame(context, rewriter, &blocks, frame_size, &saves)?;
        }

        Ok(())
    }
}

impl RegisterAllocationPass {
    fn frame_register(&self) -> PhysReg {
        self.abi.sp
    }

    /// Replace every use of an `alloca` result with a frame address computed
    /// immediately before that use. Rematerializing keeps each address live for a
    /// single instruction instead of spanning the whole function, which is what
    /// lets bodies with many address-taken locals allocate at all: an alloca
    /// value cannot be spilled (nothing would write the slot), so a long-lived
    /// one pins a register forever.
    fn rematerialize_stack_allocas(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[BlockId],
        allocas: &[StackAlloca],
        frame: &mut FramePlan,
    ) -> Result<(), PassError> {
        if allocas.is_empty() {
            return Ok(());
        }
        let default_class = self.target.register_info().default_integer_class(self.abi);
        let frame_register = self.frame_register();
        let mut sites = Vec::new();
        for alloca in allocas {
            let class = slot_class_of(context, blocks, alloca.value)
                .or(default_class)
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(format!(
                        "stack allocation %{} has no register class",
                        alloca.value.number()
                    ))
                })?;
            sites.push((alloca, class));
        }
        for &block in blocks {
            for op_id in context.get_block(block).op_ids() {
                if !context.has_operation(op_id) {
                    continue;
                }
                let operands = context.get_op(op_id).operands().to_vec();
                for &(alloca, class) in &sites {
                    if op_id == alloca.op_id || !operands.contains(&alloca.value) {
                        continue;
                    }
                    let fresh = fresh_reg(context, class);
                    frame.temps.insert(fresh.number());
                    let target = op_ref_in(context, block, op_id);
                    for address in self.target.emit_frame_address(
                        context,
                        fresh,
                        class,
                        &frame_register,
                        alloca.offset,
                    )? {
                        rewriter.insert_op_before(&target, address.as_ref())?;
                    }
                    for (index, operand) in operands.iter().enumerate() {
                        if *operand == alloca.value {
                            context.set_op_operand(op_id, index, fresh);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Turn each instruction's pre-coloring constraints into point constraints:
    /// a value an instruction must read or write in a fixed register is copied
    /// in or out right there, and the pin moves to the copy's own value. A
    /// register is a constraint at one instruction, not a home for a whole live
    /// range, so nothing else has to avoid it.
    fn lower_fixed_registers(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        op: &OperationRef,
        blocks: &[BlockId],
    ) -> Result<HashMap<u32, PhysReg>, PassError> {
        let mut precolor = HashMap::new();
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                if op.attr(crate::backend::PINS_ATTR).is_none() {
                    continue;
                }
                let op_ref = op_ref_in(context, block_id, op_id);
                for slot in crate::backend::reg_slots(&op) {
                    let crate::backend::RegSlot::Value(value) = slot.slot else {
                        continue;
                    };
                    let Some((class, index)) = crate::backend::slot_pin(&op, slot.port.name) else {
                        continue;
                    };
                    let Some(position) = slot.position else {
                        continue;
                    };
                    let fixed = fresh_reg(context, class);
                    let copy = if slot.port.def {
                        let copy = self.target.emit_copy(
                            context,
                            class,
                            crate::backend::RegSlot::Value(value),
                            crate::backend::RegSlot::Value(fixed),
                        );
                        insert_after(context, rewriter, block_id, op_id, copy.as_ref())?;
                        context.set_op_result(op_id, position, fixed);
                        copy
                    } else {
                        let copy = self.target.emit_copy(
                            context,
                            class,
                            crate::backend::RegSlot::Value(fixed),
                            crate::backend::RegSlot::Value(value),
                        );
                        rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                        context.set_op_operand(op_id, position, fixed);
                        copy
                    };
                    mark_coalescable(context, copy.id());
                    precolor.insert(fixed.number(), (class, index));
                }
                strip_attr(context, op_id, crate::backend::PINS_ATTR);
            }
        }
        // The calling convention's argument pins name values the entry copies
        // already made local, so they pin only the boundary itself.
        let arg_pins = crate::backend::RegAssignment::of_op(op.op(), crate::backend::ARG_PINS_ATTR);
        for (value, register) in arg_pins.iter() {
            precolor.insert(value.number(), register);
        }
        Ok(precolor)
    }

    fn spill_all(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        liveness: &Liveness,
        blocks: &[BlockId],
        vregs: &[u32],
        frame: &mut FramePlan,
    ) -> Result<(), PassError> {
        let info = self.target.register_info();
        let default_class = info.default_integer_class(self.abi);
        let frame_reg = self.frame_register();

        for &vreg in vregs {
            let class = liveness
                .vreg_class
                .get(&vreg)
                .copied()
                .or(default_class)
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(format!("spilled vreg {vreg} has no register class"))
                })?;
            let spilled = ValueId::from_number(vreg);
            let offset = frame.alloc_slot();

            for &block_id in blocks {
                // Re-read the op list each pass since we mutate the block.
                let op_ids = context.get_block(block_id).op_ids();
                for op_id in op_ids {
                    if !context.has_operation(op_id) {
                        continue;
                    }
                    let op = context.get_op(op_id);
                    let uses: Vec<usize> = op
                        .operands()
                        .iter()
                        .enumerate()
                        .filter(|(_, operand)| **operand == spilled)
                        .map(|(index, _)| index)
                        .collect();
                    let defines: Vec<usize> = op
                        .results()
                        .iter()
                        .enumerate()
                        .filter(|(_, result)| **result == spilled)
                        .map(|(index, _)| index)
                        .collect();
                    if uses.is_empty() && defines.is_empty() {
                        continue;
                    }

                    // A read-modify-write occurrence (a two-address destination
                    // lowered to a tie) must keep the read and the write in one
                    // register: reload into a single fresh register, rename both
                    // directions to it, and store it back after.
                    let fresh = fresh_reg(context, class);
                    frame.temps.insert(fresh.number());
                    let op_ref = op_ref_in(context, block_id, op_id);
                    if !uses.is_empty() {
                        let reload = self
                            .target
                            .emit_spill_reload(context, fresh, class, &frame_reg, offset);
                        rewriter.insert_op_before(&op_ref, reload.as_ref())?;
                    }
                    for index in uses {
                        context.set_op_operand(op_id, index, fresh);
                    }
                    for index in defines.iter().copied() {
                        context.set_op_result(op_id, index, fresh);
                    }
                    if !defines.is_empty() {
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
                for op in self.target.emit_prologue(context, self.abi, size, saves) {
                    rewriter.insert_op_before(&target, op.as_ref())?;
                }
            }
        }
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                if !context.get_op(op_id).is::<VirtualReturnOp>() {
                    continue;
                }
                let target = op_ref_in(context, block_id, op_id);
                for op in self.target.emit_epilogue(context, self.abi, size, saves) {
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
        frame_plan: &FramePlan,
        frame_size: u32,
        pushed_saves: usize,
    ) -> Result<(), PassError> {
        if args.is_empty() {
            return Ok(());
        }
        let Some(&entry) = blocks.first() else {
            return Ok(());
        };
        let frame_register = self.frame_register();
        for arg in args {
            let load_id = arg.load;
            let op = context.get_op(load_id);
            let value = op.results().first().copied().ok_or_else(|| {
                PassError::InvalidRuleSet(format!(
                    "stack argument load for %{} has no virtual definition",
                    arg.value.number()
                ))
            })?;
            let dst = assignment[&value.number()];
            if dst.0.file() != arg.class.file() || dst.0.group_width != arg.class.group_width {
                return Err(PassError::InvalidRuleSet(format!(
                    "stack argument %{} assigned to {:?}, expected class {}",
                    arg.value.number(),
                    dst,
                    arg.class.name()
                )));
            }
            let offset =
                frame_plan.incoming_stack_arg_offset(frame_size, pushed_saves, arg.stack_index);
            let load =
                self.target
                    .emit_spill_reload(context, value, dst.0, &frame_register, offset);
            let target = op_ref_in(context, entry, load_id);
            rewriter.replace_op(&target, load.as_ref())?;
        }
        Ok(())
    }
}

fn outgoing_stack_layout(context: &Context, blocks: &[BlockId]) -> Result<(u32, bool), PassError> {
    let mut size = 0;
    let mut has_calls = false;
    for &block in blocks {
        for op_id in context.get_block(block).op_ids() {
            let op = context.get_op(op_id);
            let outgoing = if let Some(call) = op.clone().as_op::<VirtualCallOp>() {
                call.outgoing_stack_size()
            } else if let Some(call) = op.as_op::<VirtualIndirectCallOp>() {
                call.outgoing_stack_size()
            } else {
                continue;
            };
            has_calls = true;
            let outgoing = u32::try_from(outgoing).map_err(|_| {
                PassError::InvalidRuleSet("outgoing call frame exceeds 32-bit size".to_string())
            })?;
            size = size.max(outgoing);
        }
    }
    Ok((size, has_calls))
}

/// Tracks stack-slot assignment across spill rounds and owns the target-neutral
/// ABI layout formulas for the stable frame area.
struct FramePlan {
    align: u32,
    slot_size: u32,
    save_style: crate::backend::abi::SaveStyle,
    next_offset: i64,
    rounds: usize,
    /// Fresh registers introduced by reload/store range-splitting. They have tiny
    /// live ranges and must land in a register; protecting them from re-spilling
    /// forces the allocator to spill a longer-lived value instead, so pressure
    /// drops monotonically and the spill loop converges.
    temps: HashSet<u32>,
}

impl FramePlan {
    fn new(abi: &crate::backend::abi::AbiInfo) -> Self {
        Self {
            align: abi.stack.align,
            slot_size: abi.stack.slot_size,
            save_style: abi.stack.save_style,
            next_offset: 0,
            rounds: 0,
            temps: HashSet::new(),
        }
    }

    fn reserve_outgoing(&mut self, size: u32) {
        self.next_offset = self.next_offset.max(i64::from(size));
    }

    fn alloc_slot(&mut self) -> i64 {
        self.alloc(self.slot_size, self.slot_size)
    }

    fn alloc_stack_allocation(&mut self, size: u32, align: u32) -> i64 {
        self.alloc(size, align)
    }

    fn alloc_callee_save(&mut self) -> i64 {
        if self.save_style == crate::backend::abi::SaveStyle::FrameSlots {
            self.alloc_slot()
        } else {
            0
        }
    }

    fn alloc(&mut self, size: u32, align: u32) -> i64 {
        let align = i64::from(align.max(1));
        self.next_offset = ((self.next_offset + align - 1) / align) * align;
        let offset = self.next_offset;
        self.next_offset += i64::from(size);
        offset
    }

    fn stable_size(&self) -> u32 {
        align_to(self.next_offset as u32, self.align)
    }

    fn prologue_adjustment(&self, has_calls: bool, pushed_saves: usize) -> u32 {
        let stable_size = self.stable_size();
        if self.save_style != crate::backend::abi::SaveStyle::PushPop || !has_calls {
            return stable_size;
        }
        stable_size
            + align_delta(
                pushed_saves as u32 * self.slot_size + stable_size,
                self.align,
                self.slot_size % self.align.max(1),
            )
    }

    fn incoming_stack_arg_offset(
        &self,
        prologue_adjustment: u32,
        pushed_saves: usize,
        stack_index: usize,
    ) -> i64 {
        let slot = i64::from(self.slot_size);
        match self.save_style {
            crate::backend::abi::SaveStyle::FrameSlots => {
                i64::from(prologue_adjustment) + stack_index as i64 * slot
            }
            crate::backend::abi::SaveStyle::PushPop => {
                pushed_saves as i64 * slot
                    + i64::from(prologue_adjustment)
                    + slot
                    + stack_index as i64 * slot
            }
        }
    }
}

fn align_to(size: u32, align: u32) -> u32 {
    if size == 0 {
        return 0;
    }
    let align = align.max(1);
    size.div_ceil(align) * align
}

fn align_delta(offset: u32, align: u32, desired_remainder: u32) -> u32 {
    let align = align.max(1);
    (desired_remainder + align - offset % align) % align
}

struct StackAlloca {
    op_id: OpId,
    block: BlockId,
    value: ValueId,
    offset: i64,
}

fn collect_stack_allocas(
    context: &Context,
    blocks: &[BlockId],
    frame: &mut FramePlan,
) -> Vec<StackAlloca> {
    let mut allocas = Vec::new();
    for &block in blocks {
        for op_id in context.get_block(block).op_ids() {
            let op = context.get_op(op_id);
            let Some(allocation) = op.clone().as_op::<AllocaOp>() else {
                continue;
            };
            let Some(result) = op.results().first().copied() else {
                continue;
            };
            allocas.push(StackAlloca {
                op_id,
                block,
                value: result,
                offset: frame
                    .alloc_stack_allocation(allocation.size() as u32, allocation.align() as u32),
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
pub(crate) fn symbol_body_blocks(context: &Context, op: &OperationRef) -> Vec<BlockId> {
    let Some(&region_id) = op.op().regions().first() else {
        return Vec::new();
    };
    context
        .get_region(region_id)
        .iter(context.clone())
        .map(|b| b.id())
        .collect()
}

pub(crate) fn op_ref_in(context: &Context, block_id: BlockId, op_id: OpId) -> OperationRef {
    OperationRef::new(
        context.get_op(op_id),
        Some(context.get_block(block_id)),
        None,
    )
}

/// Mark a copy the allocator may coalesce away: it reads the endpoints as an
/// affinity and erases the copy when both land in one register.
fn mark_coalescable(context: &Context, op_id: OpId) {
    let mut attributes = context.get_op(op_id).attributes().to_vec();
    attributes
        .push(context.named_attribute(prealloc::COALESCABLE_COPY_ATTR, AttributeValue::Bool(true)));
    context.set_op_attributes(op_id, attributes);
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
    frame: &mut FramePlan,
    abi_callee_saved: &[PhysReg],
) -> Vec<(PhysReg, i64)> {
    // Keyed by the ABI's own spelling of the register: a value allocated
    // through a narrower view of one (x86 `ebx` for `rbx`) names the same
    // physical register, and it is saved once, in full.
    let mut regs: Vec<PhysReg> = assignment
        .values()
        .filter_map(|p| {
            abi_callee_saved
                .iter()
                .find(|candidate| p.0.overlaps(p.1, candidate.0, candidate.1))
                .copied()
        })
        .collect();
    regs.sort();
    regs.dedup();
    regs.into_iter()
        .map(|p| (p, frame.alloc_callee_save()))
        .collect()
}

struct CoalescableCopy {
    block: BlockId,
    op: OpId,
    src: u32,
    dst: u32,
}

/// The copies marked with [`prealloc::COALESCABLE_COPY_ATTR`]:
/// their endpoint registers seed the coalescing affinity, and after allocation
/// a copy whose endpoints landed in one register is erased. Endpoints are
/// recorded before the spill loop so a copy whose registers were renamed by
/// spill splitting is never erased (its inserted reload/store still needs it).
fn collect_coalescable_copies(
    context: &Context,
    blocks: &[BlockId],
) -> Result<Vec<CoalescableCopy>, PassError> {
    let mut copies = Vec::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            if !has_attr(context, op_id, prealloc::COALESCABLE_COPY_ATTR) {
                continue;
            }
            let (src, dst) = copy_endpoints(context, op_id).ok_or_else(|| {
                PassError::InvalidRuleSet(format!(
                    "coalescable copy {op_id:?} does not move one virtual register to another"
                ))
            })?;
            copies.push(CoalescableCopy {
                block: block_id,
                op: op_id,
                src,
                dst,
            });
        }
    }
    Ok(copies)
}

/// The `(source, destination)` virtual registers of a copy op: its first read
/// and first written virtual register.
fn copy_endpoints(context: &Context, op_id: OpId) -> Option<(u32, u32)> {
    let regs = liveness::op_regs(&context.get_op(op_id));
    let src = regs.uses.first()?.number();
    let dst = regs.defs.first()?.number();
    Some((src, dst))
}

fn has_attr(context: &Context, op_id: OpId, name: &str) -> bool {
    context.get_op(op_id).attr(name).is_some()
}

fn strip_attr(context: &Context, op_id: OpId, name: &str) {
    let stripped = context.sym(name);
    let mut attrs = context.get_op(op_id).attributes().to_vec();
    attrs.retain(|attr| Some(attr.name) != stripped);
    context.set_op_attributes(op_id, attrs);
}

struct IncomingStackArg {
    value: ValueId,
    class: RegClassId,
    stack_index: usize,
    load: OpId,
}

/// The placeholder loads the ABI precolor pass marked with
/// [`prealloc::ABI_STACK_INDEX_ATTR`], one per incoming stack argument.
fn collect_stack_arg_loads(
    context: &Context,
    blocks: &[BlockId],
) -> Result<Vec<IncomingStackArg>, PassError> {
    let mut args = Vec::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            let Some(stack_index) =
                op.attr(prealloc::ABI_STACK_INDEX_ATTR)
                    .and_then(|value| match value {
                        AttributeValue::UInt(index) => Some(index as usize),
                        _ => None,
                    })
            else {
                continue;
            };
            let value = op.results().first().copied().ok_or_else(|| {
                PassError::InvalidRuleSet(
                    "stack argument load has no virtual definition".to_string(),
                )
            })?;
            let class = crate::backend::value_class(context, value).ok_or_else(|| {
                PassError::InvalidRuleSet(
                    "stack argument load defines a value with no register class".to_string(),
                )
            })?;
            args.push(IncomingStackArg {
                value,
                class,
                stack_index,
                load: op_id,
            });
        }
    }
    Ok(args)
}

/// The register class `value` is read through, for a value whose own type does
/// not name one (an `alloca` address): the class of the first register slot
/// naming it.
fn slot_class_of(context: &Context, blocks: &[BlockId], value: ValueId) -> Option<RegClassId> {
    if let Some(class) = crate::backend::value_class(context, value) {
        return Some(class);
    }
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            for slot in crate::backend::reg_slots(&context.get_op(op_id)) {
                if slot.slot == crate::backend::RegSlot::Value(value) {
                    return slot.port.class;
                }
            }
        }
    }
    None
}

/// A fresh value of `class`: the type a machine instruction reads it through.
/// Count how many times each virtual register is referenced (def or use) across the
/// body, used to weight spill cost so the least-used register spills first.
fn reference_counts(context: &Context, blocks: &[BlockId]) -> HashMap<u32, u32> {
    let mut counts = HashMap::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            let regs = liveness::op_regs(&op);
            for value in regs.defs.iter().chain(regs.uses.iter()) {
                *counts.entry(value.number()).or_insert(0) += 1;
            }
        }
    }
    counts
}
