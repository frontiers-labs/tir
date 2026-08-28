//! Liveness analysis over machine IR.
//!
//! Register operands are SSA operands and results, read through [`op_regs`]
//! (see [`crate::analysis::defuse`]); a virtual register is a value, named here
//! by its value number.
//!
//! The analysis computes, per block, the standard backward live-in/live-out sets,
//! then replays a backward scan to derive the interference the register allocator
//! consumes: which virtual registers are simultaneously live (so must get distinct
//! physical registers) and which physical registers each virtual register is live
//! across (so must avoid — e.g. a call's caller-saved clobbers).

use std::collections::{BTreeSet, HashMap, HashSet};

use tir::backend::regalloc::RegClassId;
use tir::{BlockId, Context, ValueId};

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
}

struct BlockInfo {
    block: BlockId,
    /// Block-argument value ids — defined at block entry.
    params: Vec<u32>,
    ops: Vec<OpInfo>,
    /// Upward-exposed uses: read before any def within the block.
    exposed_uses: BTreeSet<u32>,
    /// Every vreg defined somewhere in the block (params included).
    defs: BTreeSet<u32>,
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

/// Whether `value` is a register at all. A `!state` port names the memory an
/// instruction observes and leaves behind: it is an SSA edge like any other, but
/// it lives in no register, so allocation neither colors it nor interferes on it.
fn is_register(context: &Context, value: ValueId) -> bool {
    context.has_value(value)
        && context.get_value(value).ty() != tir::builtin::StateType::new(context)
}

/// Every virtual register some instruction in `blocks` names, as a use or a def.
fn referenced_vregs(context: &Context, blocks: &[BlockId]) -> BTreeSet<u32> {
    let mut referenced = BTreeSet::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let regs = op_regs(&context.get_op(op_id));
            for value in regs.uses.iter().chain(&regs.defs) {
                if is_register(context, *value) {
                    referenced.insert(value.number());
                }
            }
        }
    }
    referenced
}

/// Analyze liveness over `blocks` (in program order), using `successors` for the
/// inter-block dataflow: `successors(b)` returns the control-flow successor blocks
/// of `b`. A value defined in one block and used in another is live across the
/// edge between them, so the backward fixpoint carries it into every block on the
/// path — giving it the interference edges that keep it from being clobbered.
pub fn analyze(
    context: &Context,
    blocks: &[BlockId],
    successors: impl Fn(BlockId) -> Vec<BlockId>,
) -> Liveness {
    let mut result = Liveness::default();
    let referenced = referenced_vregs(context, blocks);
    let mut value_classes: HashMap<ValueId, Option<RegClassId>> = HashMap::new();

    // 1. Gather per-block, per-op register info; discover vreg classes.
    let mut block_infos: Vec<BlockInfo> = Vec::new();
    for &block_id in blocks {
        let block = context.get_block(block_id);
        // Block parameters were lowered to explicit copies before allocation, so a
        // parameter is a value only while some instruction still names it. One that
        // spilling has rewritten away carries nothing, and keeping it would leave
        // the allocator a candidate whose spilling can never relieve pressure.
        let params: Vec<u32> = block
            .arguments()
            .iter()
            .map(|v| v.id().number())
            .filter(|vreg| referenced.contains(vreg))
            .collect();

        let mut ops = Vec::new();
        let mut exposed_uses = BTreeSet::new();
        let mut defined = BTreeSet::new();
        let mut block_defs: BTreeSet<u32> = params.iter().copied().collect();

        for &param in &params {
            result.vregs.insert(param);
            defined.insert(param);
        }

        for op_id in block.op_ids() {
            let op = context.get_op(op_id);
            let slots = crate::backend::reg_slots(&op);
            let regs = crate::analysis::defuse::op_regs_from(&op, &slots);
            let port_classes = slot_classes(&slots);

            let mut def_vregs = Vec::new();
            let mut use_vregs = Vec::new();
            let mut clobbers = Vec::new();
            let mut phys_uses = Vec::new();

            for value in regs.uses.iter().filter(|v| is_register(context, **v)) {
                let id = value.number();
                record_class(
                    &mut result,
                    context,
                    *value,
                    slot_class(&port_classes, *value),
                    &mut value_classes,
                );
                result.vregs.insert(id);
                use_vregs.push(id);
                if !defined.contains(&id) {
                    exposed_uses.insert(id);
                }
            }
            for value in regs.defs.iter().filter(|v| is_register(context, **v)) {
                let id = value.number();
                record_class(
                    &mut result,
                    context,
                    *value,
                    slot_class(&port_classes, *value),
                    &mut value_classes,
                );
                result.vregs.insert(id);
                def_vregs.push(id);
                defined.insert(id);
                block_defs.insert(id);
            }
            phys_uses.extend(regs.phys_uses.iter().copied());
            clobbers.extend(regs.phys_defs.iter().copied());

            ops.push(OpInfo {
                def_vregs,
                use_vregs,
                clobbers,
                phys_uses,
            });
        }

        block_infos.push(BlockInfo {
            block: block_id,
            params,
            ops,
            exposed_uses,
            defs: block_defs,
        });
    }

    // 2. Backward dataflow for live-in / live-out to a fixpoint.
    let index: HashMap<BlockId, usize> = block_infos
        .iter()
        .enumerate()
        .map(|(i, b)| (b.block, i))
        .collect();

    // Blocks reached by a control-flow edge. A non-entry block's parameters are
    // defined by its predecessors (each forwards them through the copies that
    // `lower_block_args` inserts before the branch), so they are live on entry to
    // the block and must flow back into every predecessor as live-out — otherwise
    // those copies would look dead and their registers could be reused. The entry
    // block's parameters are the function arguments: defined by the ABI, pinned by
    // pre-coloring, and never live-in.
    let entry = blocks.first().copied();
    let mut has_pred: HashSet<BlockId> = HashSet::new();
    for &block_id in blocks {
        for succ in successors(block_id) {
            has_pred.insert(succ);
        }
    }
    let mut live_in: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); block_infos.len()];
    let mut live_out: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); block_infos.len()];

    let mut changed = true;
    while changed {
        changed = false;
        for (i, info) in block_infos.iter().enumerate().rev() {
            let mut out = BTreeSet::new();
            for succ in successors(info.block) {
                if let Some(&j) = index.get(&succ) {
                    out.extend(live_in[j].iter().copied());
                }
            }
            // live_in = params ∪ exposed_uses ∪ (live_out − defs), where params
            // contribute only for a non-entry block reached by an edge.
            let mut in_set = info.exposed_uses.clone();
            for v in &out {
                if !info.defs.contains(v) {
                    in_set.insert(*v);
                }
            }
            if Some(info.block) != entry && has_pred.contains(&info.block) {
                in_set.extend(info.params.iter().copied());
            }
            if out != live_out[i] {
                live_out[i] = out;
                changed = true;
            }
            if in_set != live_in[i] {
                live_in[i] = in_set;
                changed = true;
            }
        }
    }

    // 3. Backward scan within each block to build the interference relation.
    for (i, info) in block_infos.iter().enumerate() {
        result.live_in.insert(info.block, live_in[i].clone());

        let mut live: HashSet<u32> = live_out[i].iter().copied().collect();
        // Physical registers read later in the block and not yet re-defined, so
        // still live across the current op. Seeded empty: fixed-register def/use
        // pairs (e.g. a shift count moved into `cl` right before the shift) are
        // emitted adjacent within one block by the lowerings, so no such range
        // crosses a block boundary.
        let mut live_phys: HashSet<PhysReg> = HashSet::new();

        for op in info.ops.iter().rev() {
            // A physical clobber conflicts with everything live across this op.
            for phys in &op.clobbers {
                for &l in &live {
                    result.forbid(l, *phys);
                }
            }
            // A physical register read later and still live across this op cannot
            // hold any vreg live here, nor a vreg this op defines: overlap is
            // resolved downstream by the same `phys_overlap` path as clobbers.
            for phys in &live_phys {
                for &l in &live {
                    result.forbid(l, *phys);
                }
                for &d in &op.def_vregs {
                    result.forbid(d, *phys);
                }
            }
            // Each defined vreg interferes with all currently-live vregs and with
            // the op's other defs.
            for &d in &op.def_vregs {
                for &l in &live {
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

    result
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
