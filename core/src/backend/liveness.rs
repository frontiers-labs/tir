//! Liveness analysis over machine IR.
//!
//! After instruction selection, register operands live in op *attributes* as
//! [`RegisterAttr::Virtual`] (whose `id` is the SSA value number) or
//! [`RegisterAttr::Physical`], each tagged with an [`AttributeRole`] in the op's
//! `attribute_roles` table. A handful of ops (e.g. `return`/`vret`) still carry
//! their inputs as SSA `operands`/`results`; because a virtual register's `id`
//! equals the value number, both notations name the same register and are unified
//! here into a single `u32` virtual-register space.
//!
//! The analysis computes, per block, the standard backward live-in/live-out sets,
//! then replays a backward scan to derive the interference the register allocator
//! consumes: which virtual registers are simultaneously live (so must get distinct
//! physical registers) and which physical registers each virtual register is live
//! across (so must avoid — e.g. a call's caller-saved clobbers).

use std::collections::{BTreeSet, HashMap, HashSet};

use tir::attributes::{AttributeRole, AttributeValue, RegisterAttr};
use tir::{BlockId, Context, OpInstance};

/// A physical register: its class name and encoding index.
pub type PhysReg = (String, u16);

/// A register operand resolved from an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegRef {
    Virtual { id: u32, class: Option<String> },
    Physical { class: String, index: u16 },
}

/// The register operands of a single operation, split by direction. A
/// read-modify-write operand appears in both `defs` and `uses`.
#[derive(Clone, Debug, Default)]
pub struct OpRegs {
    pub defs: Vec<RegRef>,
    pub uses: Vec<RegRef>,
}

fn role_writes(role: AttributeRole) -> bool {
    matches!(
        role,
        AttributeRole::Def | AttributeRole::Clobber | AttributeRole::ReadWrite
    )
}

fn role_reads(role: AttributeRole) -> bool {
    matches!(role, AttributeRole::Use | AttributeRole::ReadWrite)
}

/// Resolve the register operands of one op from its SSA operands/results and its
/// register-valued attributes (consulting the op's `attribute_roles`).
pub fn op_regs(op: &OpInstance) -> OpRegs {
    let mut regs = OpRegs::default();

    // Builtin SSA ops (e.g. the block terminator) name registers positionally.
    for result in &op.results {
        regs.defs.push(RegRef::Virtual {
            id: result.number(),
            class: None,
        });
    }
    for operand in &op.operands {
        regs.uses.push(RegRef::Virtual {
            id: operand.number(),
            class: None,
        });
    }

    // Machine ops carry their register operands in attributes, with a def/use role.
    // An array of registers (e.g. a call's caller-saved clobber list) applies the
    // attribute's role to every element.
    for attr in &op.attributes {
        let attr_regs: Vec<&RegisterAttr> = match &attr.value {
            AttributeValue::Register(reg) => vec![reg],
            AttributeValue::Array(items) => items
                .iter()
                .filter_map(|item| match item {
                    AttributeValue::Register(reg) => Some(reg),
                    _ => None,
                })
                .collect(),
            _ => continue,
        };
        let role = op
            .attribute_roles
            .iter()
            .find(|(name, _)| *name == attr.name)
            .map(|(_, role)| *role)
            .unwrap_or(AttributeRole::None);

        for reg in attr_regs {
            let reg_ref = match reg {
                RegisterAttr::Virtual { id, class } => RegRef::Virtual {
                    id: *id,
                    class: class.clone(),
                },
                RegisterAttr::Physical { class, index } => RegRef::Physical {
                    class: class.clone(),
                    index: *index,
                },
            };
            if role_writes(role) {
                regs.defs.push(reg_ref.clone());
            }
            if role_reads(role) {
                regs.uses.push(reg_ref);
            }
        }
    }

    regs
}

/// Per-op register information cached for the backward scans.
struct OpInfo {
    /// Virtual registers written by this op.
    def_vregs: Vec<u32>,
    /// Virtual registers read by this op.
    use_vregs: Vec<u32>,
    /// Physical registers written/clobbered by this op.
    clobbers: Vec<PhysReg>,
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
    pub vreg_class: HashMap<u32, String>,
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
/// inter-block dataflow. Current functions are single-block, so `successors`
/// typically returns empty; the fixpoint loop generalizes once branch
/// terminators wire up the CFG.
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
                    RegRef::Physical { .. } => {}
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
                        clobbers.push((class.clone(), *index));
                    }
                }
            }

            ops.push(OpInfo {
                def_vregs,
                use_vregs,
                clobbers,
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
            // live_in = exposed_uses ∪ (live_out − defs)
            let mut in_set = info.exposed_uses.clone();
            for v in &out {
                if !info.defs.contains(v) {
                    in_set.insert(*v);
                }
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

        for op in info.ops.iter().rev() {
            // A physical clobber conflicts with everything live across this op.
            for phys in &op.clobbers {
                for &l in &live {
                    result.forbid(l, phys.clone());
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
            for &u in &op.use_vregs {
                live.insert(u);
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

fn record_class(result: &mut Liveness, id: u32, class: &Option<String>) {
    if let Some(class) = class {
        result.vreg_class.entry(id).or_insert_with(|| class.clone());
    }
}
