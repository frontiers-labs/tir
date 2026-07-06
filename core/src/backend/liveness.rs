//! Liveness analysis over machine IR.
//!
//! Register operands are resolved through [`op_regs`] (see
//! [`crate::analysis::defuse`]), which unifies SSA `operands`/`results` and
//! register-valued attributes into a single `u32` virtual-register space.
//!
//! The analysis computes, per block, the standard backward live-in/live-out sets,
//! then replays a backward scan to derive the interference the register allocator
//! consumes: which virtual registers are simultaneously live (so must get distinct
//! physical registers) and which physical registers each virtual register is live
//! across (so must avoid — e.g. a call's caller-saved clobbers).

use std::collections::{BTreeSet, HashMap, HashSet};

use tir::backend::regalloc::RegClassId;
use tir::{BlockId, Context};

pub use crate::analysis::defuse::{OpRegs, RegRef, op_regs};

/// A physical register: its class handle and encoding index.
pub type PhysReg = (RegClassId, u16);

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
    /// The register class discovered for each virtual register from its operands.
    pub vreg_class: HashMap<u32, RegClassId>,
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

    // 1. Gather per-block, per-op register info; discover vreg classes.
    let mut block_infos: Vec<BlockInfo> = Vec::new();
    for &block_id in blocks {
        let block = context.get_block(block_id);
        let params: Vec<u32> = block.arguments().iter().map(|v| v.id().number()).collect();

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
            let regs = op_regs(&op);

            let mut def_vregs = Vec::new();
            let mut use_vregs = Vec::new();
            let mut clobbers = Vec::new();
            let mut phys_uses = Vec::new();

            for r in &regs.uses {
                match r {
                    RegRef::Virtual { id, class } => {
                        record_class(&mut result, *id, class);
                        result.vregs.insert(*id);
                        use_vregs.push(*id);
                        if !defined.contains(id) {
                            exposed_uses.insert(*id);
                        }
                    }
                    RegRef::Physical { class, index } => {
                        phys_uses.push((*class, *index));
                    }
                }
            }
            for r in &regs.defs {
                match r {
                    RegRef::Virtual { id, class } => {
                        record_class(&mut result, *id, class);
                        result.vregs.insert(*id);
                        def_vregs.push(*id);
                        defined.insert(*id);
                        block_defs.insert(*id);
                    }
                    RegRef::Physical { class, index } => {
                        clobbers.push((*class, *index));
                    }
                }
            }

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

fn record_class(result: &mut Liveness, id: u32, class: &Option<RegClassId>) {
    if let Some(class) = class {
        result.vreg_class.entry(id).or_insert(*class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tir::backend::regalloc::RegClassInfo;
    use tir::builtin::{IntegerType, ops};
    use tir::{Block, IRBuilder, TypeId, ValueId};

    static R_CLASS: RegClassInfo = RegClassInfo {
        name: "R",
        file: "R",
        allocation_order: &[0],
        caller_saved: &[0],
        callee_saved: &[],
        arguments: &[],
        return_values: &[],
        reserved: &[],
        group_width: 1,
    };

    fn r() -> RegClassId {
        RegClassId::new(&R_CLASS)
    }

    // `addi %a, %b` whose fresh result names a new virtual register (a def), with
    // its two operands read as uses — enough for liveness, which resolves builtin
    // SSA ops positionally.
    fn addi(context: &Context, block: &Arc<Block>, a: ValueId, b: ValueId, ty: TypeId) -> ValueId {
        let mut builder = IRBuilder::new(block.clone());
        builder
            .insert(ops::addi(context, a, b, ty).build())
            .result()
    }

    // Two defs in the entry block where the first is used only in a successor
    // block: the two entry defs interfere iff the successor edge is wired, because
    // that is what keeps the first value live across the second's def. With the
    // edge dropped (the old `|_| Vec::new()`), the first value looks dead at its
    // def and the allocator is free to reuse its register — the miscompile.
    #[test]
    fn cross_block_def_interferes_only_with_wired_successors() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None);
        let a_id = a.id();
        let entry = context.create_block(vec![a]);
        let succ = context.create_block(vec![]);

        // `v` is used only in the successor (so it is live across the edge); `w`
        // is defined after `v` and dies inside the entry block (consumed by `u`).
        // Their interference therefore hinges entirely on `v` being live-out.
        let v = addi(&context, &entry, a_id, a_id, ty);
        let w = addi(&context, &entry, a_id, a_id, ty);
        addi(&context, &entry, w, w, ty);
        addi(&context, &succ, v, a_id, ty);

        let blocks = [entry.id(), succ.id()];
        let with_edge = analyze(&context, &blocks, |blk| {
            if blk == entry.id() {
                vec![succ.id()]
            } else {
                vec![]
            }
        });
        assert!(
            with_edge.interferes(v.number(), w.number()),
            "a value live across a later def must interfere with it",
        );
        assert!(
            with_edge.live_in[&succ.id()].contains(&v.number()),
            "the cross-block value is live into its using block",
        );

        let no_edge = analyze(&context, &blocks, |_| Vec::new());
        assert!(
            !no_edge.interferes(v.number(), w.number()),
            "without the CFG edge the bug hides the interference (regression guard)",
        );
    }

    // Diamond: entry defines a value used only at the merge, so it is live-through
    // both arms and must interfere with every def on either arm.
    #[test]
    fn diamond_live_through_interferes_on_both_arms() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None);
        let a_id = a.id();
        let entry = context.create_block(vec![a]);
        let left = context.create_block(vec![]);
        let right = context.create_block(vec![]);
        let merge = context.create_block(vec![]);

        let v = addi(&context, &entry, a_id, a_id, ty);
        let la = addi(&context, &left, a_id, a_id, ty);
        let ra = addi(&context, &right, a_id, a_id, ty);
        addi(&context, &merge, v, a_id, ty);

        let blocks = [entry.id(), left.id(), right.id(), merge.id()];
        let liveness = analyze(&context, &blocks, |blk| {
            if blk == entry.id() {
                vec![left.id(), right.id()]
            } else if blk == left.id() || blk == right.id() {
                vec![merge.id()]
            } else {
                vec![]
            }
        });

        assert!(liveness.live_in[&left.id()].contains(&v.number()));
        assert!(liveness.live_in[&right.id()].contains(&v.number()));
        assert!(
            liveness.interferes(v.number(), la.number()),
            "live-through value must interfere with the left arm's def",
        );
        assert!(
            liveness.interferes(v.number(), ra.number()),
            "live-through value must interfere with the right arm's def",
        );
    }

    // Append an op that reads (`is_def == false`) or writes (`is_def == true`) the
    // physical register `class[index]` via a role-tagged register attribute.
    fn phys_op(context: &Context, block: &Arc<Block>, class: RegClassId, index: u16, is_def: bool) {
        use tir::OpInstance;
        use tir::attributes::{AttributeRole, AttributeValue, NamedAttribute, RegisterAttr};

        static DEF_ROLES: &[(&str, AttributeRole)] = &[("r", AttributeRole::Def)];
        static USE_ROLES: &[(&str, AttributeRole)] = &[("r", AttributeRole::Use)];

        let instance = OpInstance {
            id: Default::default(),
            name: "test.phys",
            dialect: "test",
            context: context.as_context_ref(),
            operands: Vec::new(),
            results: Vec::new(),
            regions: Vec::new(),
            attributes: vec![NamedAttribute::new(
                "r",
                AttributeValue::Register(RegisterAttr::Physical { class, index }),
            )],
            attribute_roles: if is_def { DEF_ROLES } else { USE_ROLES },
        };
        let op = context.add_operation(instance);
        block.insert(block.len(), op.id);
    }

    // A fixed-register read protocol: `def P; def v1; use P; use v1`. `v1` is live
    // across the read of the physical register `P`, so it must not be colored `P` —
    // otherwise the allocator could park it in `P` between `P`'s def and this read.
    #[test]
    fn physical_read_forbids_live_vreg() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None);
        let a_id = a.id();
        let block = context.create_block(vec![a]);

        phys_op(&context, &block, r(), 0, true); // def P
        let v1 = addi(&context, &block, a_id, a_id, ty); // def v1 (live across the read)
        phys_op(&context, &block, r(), 0, false); // use P
        addi(&context, &block, v1, a_id, ty); // use v1

        let liveness = analyze(&context, &[block.id()], |_| Vec::new());

        assert!(
            liveness.forbidden[&v1.number()].contains(&(r(), 0)),
            "a vreg live across a physical-register read must be forbidden from it",
        );
    }

    // A back edge (a loop): the fixpoint must converge, and a value defined in the
    // header and read inside the body stays live around the edge.
    #[test]
    fn loop_back_edge_converges() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None);
        let a_id = a.id();
        let header = context.create_block(vec![a]);
        let body = context.create_block(vec![]);

        let carried = addi(&context, &header, a_id, a_id, ty);
        addi(&context, &body, carried, a_id, ty);

        // header -> body -> header (back edge).
        let blocks = [header.id(), body.id()];
        let liveness = analyze(&context, &blocks, |blk| {
            if blk == header.id() {
                vec![body.id()]
            } else {
                vec![header.id()]
            }
        });

        assert!(
            liveness.live_in[&body.id()].contains(&carried.number()),
            "the header-defined value is live into the loop body",
        );
    }
}
