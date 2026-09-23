//! Liveness analysis over machine IR.
//!
//! Register operands are SSA operands and results, read through [`op_regs`]
//! (see [`crate::analysis::defuse`]); a virtual register is a value, named here
//! by its value number.
//!
//! The analysis computes, per block, the standard backward live-in sets, then
//! replays a backward scan to derive the interference the register allocator
//! consumes: which virtual registers are simultaneously live (so must get distinct
//! physical registers) and which physical registers each virtual register is live
//! across (so must avoid — e.g. a call's caller-saved clobbers).
//!
//! A block may hold several terminators: a conditional branch followed by the
//! fallthrough `vbr`, with the block-argument copies of the fallthrough edge
//! between them. Each terminator's successors are therefore live at that
//! terminator, not at the block's end — otherwise a copy redefining a
//! parameter for the fallthrough would hide the parameter's liveness along the
//! conditional edge, and its register could be reused ahead of the branch.

use std::collections::{BTreeSet, HashMap, HashSet};

use tir::backend::regalloc::RegClassId;
use tir::{BlockId, Context, OpId, Terminator, ValueId};

pub use crate::analysis::defuse::{OpRegs, PhysReg, execution_regs, op_regs};

use crate::backend::registers::value_class;

/// Per-op register information cached for the backward scans.
struct OpInfo {
    /// Virtual registers written by this op.
    def_vregs: Vec<u32>,
    /// Virtual registers read by this op.
    use_vregs: Vec<u32>,
    /// Physical registers written/clobbered by this op.
    clobbers: Vec<PhysReg>,
    /// Physical registers read by this op (e.g. a fixed-register protocol like a
    /// shift count in `cl`). Their live range keeps the allocator from parking an
    /// unrelated vreg in the register between its def and this read.
    phys_uses: Vec<PhysReg>,
    /// A copy the pre-allocation lowerings marked coalescable: its ends name the
    /// same value at that point, so the def alone must not keep them apart.
    coalescable_copy: bool,
    /// Control-flow successors of this op when it is a terminator.
    successors: Vec<BlockId>,
}

struct BlockInfo {
    block: BlockId,
    /// Block-argument value ids — defined at block entry.
    params: Vec<u32>,
    ops: Vec<OpInfo>,
}

/// The result of liveness analysis: the interference relation the allocator needs.
#[derive(Debug, Default)]
pub struct Liveness {
    /// Unordered pairs of virtual registers that are simultaneously live.
    pub interference: HashSet<(u32, u32)>,
    /// Physical registers each virtual register is live across and so must avoid.
    pub forbidden: HashMap<u32, HashSet<PhysReg>>,
    /// The architectural view each virtual register is allocated through: the
    /// narrowest of every class it is referenced by (see
    /// [`RegClassId::is_subclass_of`]). Governs the width of the copies and spill
    /// code the allocator emits for it.
    pub vreg_class: HashMap<u32, RegClassId>,
    /// The file indices a virtual register may be assigned: the intersection of
    /// the register sets of every class it is referenced through. Absent means
    /// unconstrained beyond [`Liveness::vreg_class`].
    pub allowed_indices: HashMap<u32, BTreeSet<u16>>,
    /// Virtual registers referenced through classes that cannot both be honored
    /// (different files or views, or no register in common), with the pair.
    pub class_conflicts: HashMap<u32, (RegClassId, RegClassId)>,
    /// Every virtual register referenced in the analyzed region.
    pub vregs: BTreeSet<u32>,
    /// Virtual registers live on entry to each block (keyed by block).
    pub live_in: HashMap<BlockId, BTreeSet<u32>>,
}

impl Liveness {
    fn add_interference(&mut self, a: u32, b: u32) {
        if a != b {
            self.interference.insert((a.min(b), a.max(b)));
        }
    }

    pub fn interferes(&self, a: u32, b: u32) -> bool {
        a != b && self.interference.contains(&(a.min(b), a.max(b)))
    }

    fn forbid(&mut self, vreg: u32, phys: PhysReg) {
        self.forbidden.entry(vreg).or_default().insert(phys);
    }
}

fn ordered(a: u32, b: u32) -> (u32, u32) {
    (a.min(b), a.max(b))
}

/// Analyze liveness over `blocks` (in program order). The inter-block dataflow
/// follows every [`Terminator`] a block contains. A value defined in one block
/// and used in another is live across the edge between them, so the backward
/// fixpoint carries it into every block on the path — giving it the
/// interference edges that keep it from being clobbered.
pub fn analyze(context: &Context, blocks: &[BlockId]) -> Liveness {
    let mut result = Liveness::default();
    let mut value_classes: HashMap<ValueId, Option<RegClassId>> = HashMap::new();

    let block_infos = collect_block_infos(context, blocks, &mut result, &mut value_classes);
    let index: HashMap<BlockId, usize> = block_infos
        .iter()
        .enumerate()
        .map(|(i, b)| (b.block, i))
        .collect();
    let live_in = solve_live_sets(&block_infos, &index, blocks.first().copied());
    build_interference(&mut result, &block_infos, &index, &live_in);

    result
}

/// Per-block, per-op register info; discovers vreg classes along the way.
fn collect_block_infos(
    context: &Context,
    blocks: &[BlockId],
    result: &mut Liveness,
    value_classes: &mut HashMap<ValueId, Option<RegClassId>>,
) -> Vec<BlockInfo> {
    let mut block_infos: Vec<BlockInfo> = Vec::new();
    for &block_id in blocks {
        let block = context.get_block(block_id);
        let params: Vec<u32> = block
            .value_arguments()
            .iter()
            .map(|v| v.id().number())
            .collect();

        let ops = block
            .op_ids()
            .iter()
            .map(|&op_id| collect_op_info(context, op_id, result, value_classes))
            .collect();

        block_infos.push(BlockInfo {
            block: block_id,
            params,
            ops,
        });
    }

    // Block parameters were lowered to explicit copies before allocation, so a
    // parameter is a value only while some instruction still names it. One that
    // spilling has rewritten away carries nothing, and keeping it would leave
    // the allocator a candidate whose spilling can never relieve pressure.
    let referenced: BTreeSet<u32> = block_infos
        .iter()
        .flat_map(|info| info.ops.iter())
        .flat_map(|op| op.use_vregs.iter().chain(&op.def_vregs))
        .copied()
        .collect();
    for info in &mut block_infos {
        info.params.retain(|vreg| referenced.contains(vreg));
        result.vregs.extend(info.params.iter().copied());
    }
    block_infos
}

fn collect_op_info(
    context: &Context,
    op_id: OpId,
    result: &mut Liveness,
    value_classes: &mut HashMap<ValueId, Option<RegClassId>>,
) -> OpInfo {
    let op = context.get_op(op_id);
    let slots = crate::backend::reg_slots(&op);
    let regs = crate::analysis::defuse::op_regs_from(&op, &slots);
    let port_classes = slot_classes(&slots);

    let mut def_vregs = Vec::new();
    let mut use_vregs = Vec::new();
    let mut clobbers = Vec::new();
    let mut phys_uses = Vec::new();

    for value in regs.uses.iter().filter(|v| context.has_value(**v)) {
        let id = value.number();
        record_class(
            result,
            context,
            *value,
            slot_class(&port_classes, *value),
            value_classes,
        );
        result.vregs.insert(id);
        use_vregs.push(id);
    }
    for value in regs.defs.iter().filter(|v| context.has_value(**v)) {
        let id = value.number();
        record_class(
            result,
            context,
            *value,
            slot_class(&port_classes, *value),
            value_classes,
        );
        result.vregs.insert(id);
        def_vregs.push(id);
    }
    phys_uses.extend(regs.phys_uses.iter().copied());
    clobbers.extend(regs.phys_defs.iter().copied());

    OpInfo {
        def_vregs,
        use_vregs,
        clobbers,
        phys_uses,
        coalescable_copy: op
            .attr(crate::backend::prealloc::COALESCABLE_COPY_ATTR)
            .is_some(),
        successors: op
            .as_interface::<dyn Terminator>()
            .map(|term| term.successors())
            .unwrap_or_default(),
    }
}

/// Backward dataflow for live-in to a fixpoint. Every terminator joins its
/// successors' live-in where it stands, so a value defined later in the block
/// stays live along an earlier conditional edge.
fn solve_live_sets(
    block_infos: &[BlockInfo],
    index: &HashMap<BlockId, usize>,
    entry: Option<BlockId>,
) -> Vec<BTreeSet<u32>> {
    // Blocks reached by a control-flow edge. A non-entry block's parameters are
    // defined by its predecessors (each forwards them through the copies that
    // `lower_block_args` inserts before the branch), so they are live on entry to
    // the block and must flow back into every predecessor as live-out — otherwise
    // those copies would look dead and their registers could be reused. The entry
    // block's parameters are the function arguments: defined by the ABI, pinned by
    // pre-coloring, and never live-in.
    let has_pred: HashSet<BlockId> = block_infos
        .iter()
        .flat_map(|info| info.ops.iter())
        .flat_map(|op| op.successors.iter().copied())
        .collect();
    let mut live_in: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); block_infos.len()];

    let mut changed = true;
    while changed {
        changed = false;
        for (i, info) in block_infos.iter().enumerate().rev() {
            let mut live = HashSet::new();
            for op in info.ops.iter().rev() {
                join_successors(op, &live_in, index, &mut live);
                for &d in &op.def_vregs {
                    live.remove(&d);
                }
                live.extend(op.use_vregs.iter().copied());
            }
            let mut in_set: BTreeSet<u32> = live.into_iter().collect();
            if Some(info.block) != entry && has_pred.contains(&info.block) {
                in_set.extend(info.params.iter().copied());
            } else {
                for param in &info.params {
                    in_set.remove(param);
                }
            }
            if in_set != live_in[i] {
                live_in[i] = in_set;
                changed = true;
            }
        }
    }

    live_in
}

/// What is live right after `op`: whatever its successors read on entry.
fn join_successors(
    op: &OpInfo,
    live_in: &[BTreeSet<u32>],
    index: &HashMap<BlockId, usize>,
    live: &mut HashSet<u32>,
) {
    for succ in &op.successors {
        if let Some(&j) = index.get(succ) {
            live.extend(live_in[j].iter().copied());
        }
    }
}

/// Backward scan within each block to build the interference relation.
fn build_interference(
    result: &mut Liveness,
    block_infos: &[BlockInfo],
    index: &HashMap<BlockId, usize>,
    live_in: &[BTreeSet<u32>],
) {
    for (i, info) in block_infos.iter().enumerate() {
        result.live_in.insert(info.block, live_in[i].clone());

        let mut live: HashSet<u32> = HashSet::new();
        // Keep successor values live at block exit so fallthrough copies
        // retain their conservative interference and coalescing constraints.
        // The joins below additionally protect taken-edge values from later
        // definitions on the fallthrough path.
        for op in &info.ops {
            join_successors(op, live_in, index, &mut live);
        }
        // Physical registers read later in the block and not yet re-defined, so
        // still live across the current op. Seeded empty: fixed-register def/use
        // pairs (e.g. a shift count moved into `cl` right before the shift) are
        // emitted adjacent within one block by the lowerings, so no such range
        // crosses a block boundary.
        let mut live_phys: HashSet<PhysReg> = HashSet::new();

        for op in info.ops.iter().rev() {
            join_successors(op, live_in, index, &mut live);
            scan_op(result, op, &mut live, &mut live_phys);
        }

        // Block arguments are all simultaneously live at entry, so they pairwise
        // interfere (and with anything else live-in).
        let entry: Vec<u32> = info
            .params
            .iter()
            .copied()
            .chain(live.iter().copied())
            .collect::<BTreeSet<u32>>()
            .into_iter()
            .collect();
        for a in 0..entry.len() {
            for b in (a + 1)..entry.len() {
                result.interference.insert(ordered(entry[a], entry[b]));
            }
        }
    }
}

/// One step of the backward scan: record what `op` conflicts with, then move
/// the live sets past it.
fn scan_op(
    result: &mut Liveness,
    op: &OpInfo,
    live: &mut HashSet<u32>,
    live_phys: &mut HashSet<PhysReg>,
) {
    // A physical clobber conflicts with everything live across this op.
    for phys in &op.clobbers {
        for &l in live.iter() {
            result.forbid(l, *phys);
        }
    }
    // A physical register read later and still live across this op cannot
    // hold any vreg live here, nor a vreg this op defines: overlap is
    // resolved downstream by the same `phys_overlap` path as clobbers.
    for phys in live_phys.iter() {
        for &l in live.iter() {
            result.forbid(l, *phys);
        }
        for &d in &op.def_vregs {
            result.forbid(d, *phys);
        }
    }
    // Each defined vreg interferes with all currently-live vregs and with
    // the op's other defs. A coalescable copy is the exception: its
    // destination and source hold one value at the copy, so they may share a
    // register — any later divergence (a redefinition of either) adds its own
    // interference through the defining op.
    for &d in &op.def_vregs {
        for &l in live.iter() {
            if op.coalescable_copy && op.use_vregs.contains(&l) {
                continue;
            }
            result.add_interference(d, l);
        }
        for &d2 in &op.def_vregs {
            result.add_interference(d, d2);
        }
    }
    for &d in &op.def_vregs {
        live.remove(&d);
    }
    // A physical write ends the live range of that register (going backward).
    for phys in &op.clobbers {
        live_phys.remove(phys);
    }
    for &u in &op.use_vregs {
        live.insert(u);
    }
    // A physical read starts (going backward) a live range for that register.
    for phys in &op.phys_uses {
        live_phys.insert(*phys);
    }
}

/// The class each resolved register slot narrows its value to. A value read
/// through several slots must satisfy them all at once.
fn slot_classes(slots: &[crate::backend::SlotRef]) -> Vec<(ValueId, RegClassId)> {
    let mut classes = Vec::with_capacity(slots.len());
    for slot in slots {
        if let (crate::backend::RegSlot::Value(value), Some(class)) = (slot.slot, slot.port.class)
            && !classes.iter().any(|(seen, _)| *seen == value)
        {
            classes.push((value, class));
        }
    }
    classes
}

/// The class the slot reading `value` narrows it to, if any.
fn slot_class(classes: &[(ValueId, RegClassId)], value: ValueId) -> Option<RegClassId> {
    classes
        .iter()
        .find(|(slot, _)| *slot == value)
        .map(|(_, class)| *class)
}

/// Constrain `value` to the class its type names, narrowed by the class the
/// slot reading it encodes. A vreg referenced through several classes must
/// satisfy all of them at once, so the constraints intersect: it may only be
/// assigned a register every one of them encodes (an x86 value read by a REX-free
/// operand form is confined to that form's low registers even where it is also
/// copied through the full `GPR` class). Classes viewing different files or
/// different offsets of one file, or sharing no register, cannot both hold and are
/// reported instead of silently resolved to one of them.
fn record_class(
    result: &mut Liveness,
    context: &Context,
    value: ValueId,
    port_class: Option<RegClassId>,
    seen: &mut HashMap<ValueId, Option<RegClassId>>,
) {
    let id = value.number();
    // A value's own class is its type, which does not change while the scan
    // runs; reading it goes through the type interner, so read it once.
    let own = *seen
        .entry(value)
        .or_insert_with(|| value_class(context, value));
    for class in own.iter().chain(port_class.iter()) {
        record_one_class(result, id, *class);
    }
}

fn record_one_class(result: &mut Liveness, id: u32, class: RegClassId) {
    let Some(current) = result.vreg_class.get(&id).copied() else {
        result.vreg_class.insert(id, class);
        result
            .allowed_indices
            .insert(id, class.registers.iter().copied().collect());
        return;
    };
    // A register group named through a single-register slot (an RVV LMUL group
    // in a `VR` operand) allocates as the group: the wider class decides, and
    // the narrower one constrains nothing.
    if class.group_width != current.group_width && class.file() == current.file() {
        if class.group_width > current.group_width {
            result.vreg_class.insert(id, class);
            result
                .allowed_indices
                .insert(id, class.registers.iter().copied().collect());
        }
        return;
    }
    if !class.shares_view_with(current) {
        result.class_conflicts.entry(id).or_insert((current, class));
        return;
    }
    let allowed = result.allowed_indices.entry(id).or_default();
    allowed.retain(|index| class.contains(*index));
    if allowed.is_empty() {
        result.class_conflicts.entry(id).or_insert((current, class));
    } else if class.is_subclass_of(current) && !current.is_subclass_of(class) {
        result.vreg_class.insert(id, class);
    }
}
