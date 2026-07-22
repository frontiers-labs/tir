//! `vsetvli` insertion: materialize the vector-unit configuration demanded by
//! vector instructions.
//!
//! Instruction selection leaves each vector instruction carrying its demand as
//! attributes — `vl` (the AVL immediate or virtual register its behavior's
//! `VCSR::vl` read bound to) and `sew` (the element width its `VCFG::sew` read
//! bound to, from the vector type) — the same way virtual registers await
//! allocation and virtual branches await finalization. This pass walks each
//! block forward tracking the configuration the vector unit currently holds
//! and inserts a `vsetivli`/`vsetvli` only where the demand changes, so
//! consecutive instructions sharing a configuration share one configuration
//! instruction.

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::backend::{SymbolOp, VirtualCallOp, VirtualIndirectCallOp};
use tir::builtin::IntegerType;
use tir::{
    AnalysisManager, Context, OpId, OperationRef, Pass, PassError, PassTarget, PreservedAnalyses,
    Rewriter,
};

use crate::{AddImmOpBuilder, VSetIVliOp, VSetIVliOpBuilder, VSetVliOp, VSetVliOpBuilder};

/// The largest AVL `vsetivli`'s 5-bit unsigned immediate encodes.
const UIMM5_MAX: i64 = 31;
/// The largest AVL materializable with a single `addi rd, x0, imm`.
const SIMM12_MAX: i64 = 2047;

/// The minimum VLEN the V extension guarantees an application processor
/// (zvl128b); LMUL legalization sizes register groups against it.
const VLEN_MIN: i64 = 128;

/// The register-group multiplier a demand implies: the smallest LMUL whose
/// group holds `vl` elements of `sew` bits at the guaranteed minimum VLEN. A
/// register AVL (EVL-style, granted at run time) fits a single register by
/// construction.
fn lmul_for(avl: &AttributeValue, sew: i64) -> Result<i64, PassError> {
    let AttributeValue::Int(vl) = avl else {
        return Ok(1);
    };
    let bits = vl * sew;
    for lmul in [1, 2, 4, 8] {
        if bits <= lmul * VLEN_MIN {
            return Ok(lmul);
        }
    }
    Err(PassError::InvalidRuleSet(format!(
        "vector demand of {bits} bits exceeds LMUL=8 at VLEN>={VLEN_MIN}"
    )))
}

/// The register class holding one value of `total_bits` at the guaranteed
/// minimum VLEN: `VR` for a single register, else the LMUL group class.
pub(crate) fn vr_class_for_bits(
    total_bits: i64,
) -> Result<tir::backend::regalloc::RegClassId, PassError> {
    match lmul_for(&AttributeValue::Int(1), total_bits)? {
        1 => Ok(crate::RegClass::VR.id()),
        2 => Ok(crate::RegClass::VRM2.id()),
        4 => Ok(crate::RegClass::VRM4.id()),
        _ => Ok(crate::RegClass::VRM8.id()),
    }
}

/// Pack a `vtypei` immediate: tail-agnostic, mask-agnostic, the given element
/// width and group multiplier (`vma | vta | vsew << 3 | vlmul`).
pub(crate) fn vtypei_for(sew: i64, lmul: i64) -> Result<i64, PassError> {
    let vsew = match sew {
        8 => 0,
        16 => 1,
        32 => 2,
        64 => 3,
        _ => {
            return Err(PassError::InvalidRuleSet(format!(
                "unsupported element width {sew} for vtype"
            )));
        }
    };
    let vlmul = match lmul {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        _ => {
            return Err(PassError::InvalidRuleSet(format!(
                "unsupported group multiplier {lmul} for vtype"
            )));
        }
    };
    Ok((1 << 7) | (1 << 6) | (vsew << 3) | vlmul)
}

/// An AVL demand's identity: the immediate or the SSA value it binds. Register
/// classes are deliberately ignored — the demand attribute carries no class
/// while a vsetvli's operands do, and the virtual register id alone names the
/// value.
#[derive(Clone, Debug, PartialEq)]
enum ConfigKey {
    Imm(i64),
    Vreg(u32),
}

fn config_key(value: &AttributeValue) -> Option<ConfigKey> {
    match value {
        AttributeValue::Int(v) => Some(ConfigKey::Imm(*v)),
        AttributeValue::Register(RegisterAttr::Virtual { id, .. }) => Some(ConfigKey::Vreg(*id)),
        _ => None,
    }
}

/// The configuration the vector unit holds: the AVL demand keys the granted
/// `vl` satisfies, and the packed `vtypei` in effect.
struct ConfigState {
    keys: Vec<ConfigKey>,
    vtypei: i64,
}

pub struct InsertVsetvliPass {
    xlen: u32,
}

impl InsertVsetvliPass {
    pub fn new(xlen: u32) -> Self {
        Self { xlen }
    }

    /// Insert the configuration instruction(s) satisfying `demand` before
    /// `anchor`: `vsetivli` when the AVL is a 5-bit immediate, `vsetvli` when
    /// it is a register (materializing larger immediates through `addi`).
    fn insert_config(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        anchor: &OperationRef,
        avl: &AttributeValue,
        vtypei: i64,
    ) -> Result<(), PassError> {
        let x0 = AttributeValue::Register(RegisterAttr::Physical {
            class: crate::RegClass::GPR.id(),
            index: 0,
        });
        match avl {
            AttributeValue::Int(v) if (0..=UIMM5_MAX).contains(v) => {
                let op = VSetIVliOpBuilder::new(context)
                    .attr("rd", x0)
                    .attr("avl", AttributeValue::Int(*v))
                    .attr("vtypei", AttributeValue::Int(vtypei))
                    .build();
                rewriter.insert_op_before(anchor, &op)
            }
            AttributeValue::Int(v) if (0..=SIMM12_MAX).contains(v) => {
                let ty = IntegerType::new(context, self.xlen);
                let avl_reg = context.create_value(ty, None).id().number();
                let li = AddImmOpBuilder::new(context)
                    .attr(
                        "rd",
                        AttributeValue::Register(RegisterAttr::Virtual {
                            id: avl_reg,
                            class: Some(crate::RegClass::GPR.id()),
                        }),
                    )
                    .attr("rs1", x0.clone())
                    .attr("imm", AttributeValue::Int(*v))
                    .build();
                rewriter.insert_op_before(anchor, &li)?;
                let op = VSetVliOpBuilder::new(context)
                    .attr("rd", x0)
                    .attr(
                        "avl",
                        AttributeValue::Register(RegisterAttr::Virtual {
                            id: avl_reg,
                            class: Some(crate::RegClass::GPR.id()),
                        }),
                    )
                    .attr("vtypei", AttributeValue::Int(vtypei))
                    .build();
                rewriter.insert_op_before(anchor, &op)
            }
            AttributeValue::Register(RegisterAttr::Virtual { id, .. }) => {
                let op = VSetVliOpBuilder::new(context)
                    .attr("rd", x0)
                    .attr(
                        "avl",
                        AttributeValue::Register(RegisterAttr::Virtual {
                            id: *id,
                            class: Some(crate::RegClass::GPR.id()),
                        }),
                    )
                    .attr("vtypei", AttributeValue::Int(vtypei))
                    .build();
                rewriter.insert_op_before(anchor, &op)
            }
            other => Err(PassError::InvalidRuleSet(format!(
                "unsupported vector-length demand {other:?}"
            ))),
        }
    }
}

impl Pass for InsertVsetvliPass {
    fn name(&self) -> &'static str {
        "riscv-insert-vsetvli"
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
    ) -> Result<PreservedAnalyses, PassError> {
        let Some(&region_id) = op.op().regions.first() else {
            return Ok(PreservedAnalyses::all());
        };
        let blocks: Vec<_> = context
            .get_region(region_id)
            .iter(context.clone())
            .map(|b| b.id())
            .collect();

        let mut changed = false;
        for block_id in blocks {
            // Unknown at block entry (no cross-block propagation yet) and after
            // calls, which may reconfigure the unit.
            let mut state: Option<ConfigState> = None;
            for op_id in context.get_block(block_id).op_ids() {
                let body_op = context.get_op(op_id);
                let attr = |name: &str| {
                    body_op
                        .attributes
                        .iter()
                        .find(|a| a.name == name)
                        .map(|a| a.value.clone())
                };
                if body_op.is::<VSetVliOp>() || body_op.is::<VSetIVliOp>() {
                    // An existing configuration instruction (e.g. selected for a
                    // `vector.vector_len`) satisfies demands on its AVL, and —
                    // when its grant is live (`rd` a virtual register) — demands
                    // on the granted count, since `vl` equals `rd`'s value.
                    state = attr("vtypei")
                        .as_ref()
                        .and_then(config_key)
                        .and_then(|vtypei_key| match vtypei_key {
                            ConfigKey::Imm(vtypei) => {
                                let mut keys: Vec<ConfigKey> = attr("avl")
                                    .as_ref()
                                    .and_then(config_key)
                                    .into_iter()
                                    .collect();
                                if let Some(AttributeValue::Register(RegisterAttr::Virtual {
                                    id,
                                    ..
                                })) = attr("rd")
                                {
                                    keys.push(ConfigKey::Vreg(id));
                                }
                                Some(ConfigState { keys, vtypei })
                            }
                            ConfigKey::Vreg(_) => None,
                        });
                    continue;
                }
                if body_op.is::<VirtualCallOp>() || body_op.is::<VirtualIndirectCallOp>() {
                    state = None;
                    continue;
                }
                let Some(avl) = attr("vl") else {
                    continue;
                };
                let Some(key) = config_key(&avl) else {
                    return Err(PassError::InvalidRuleSet(format!(
                        "unsupported vector-length demand {avl:?}"
                    )));
                };
                let Some(AttributeValue::Int(sew)) = attr("sew") else {
                    return Err(PassError::InvalidRuleSet(format!(
                        "vector op '{}' demands a vector length but no element width",
                        body_op.name().as_str()
                    )));
                };
                // LMUL legalization: an op whose demanded elements exceed one
                // register works on a register group, so its `VR` operands move
                // to the group class and the allocator assigns aligned spans.
                let lmul = lmul_for(&avl, sew)?;
                if lmul > 1 {
                    let group = match lmul {
                        2 => crate::RegClass::VRM2.id(),
                        4 => crate::RegClass::VRM4.id(),
                        _ => crate::RegClass::VRM8.id(),
                    };
                    let mut attrs = body_op.attributes.clone();
                    let mut rewrote = false;
                    for a in &mut attrs {
                        if let AttributeValue::Register(RegisterAttr::Virtual {
                            class: Some(c),
                            ..
                        }) = &mut a.value
                            && *c == crate::RegClass::VR.id()
                        {
                            *c = group;
                            rewrote = true;
                        }
                    }
                    if rewrote {
                        context.set_op_attributes(op_id, attrs);
                        changed = true;
                    }
                }
                let vtypei = vtypei_for(sew, lmul)?;
                if state
                    .as_ref()
                    .is_some_and(|s| s.vtypei == vtypei && s.keys.contains(&key))
                {
                    continue;
                }
                let anchor = op_ref_in(context, block_id, op_id);
                self.insert_config(context, rewriter, &anchor, &avl, vtypei)?;
                state = Some(ConfigState {
                    keys: vec![key],
                    vtypei,
                });
                changed = true;
            }
        }

        if changed {
            Ok(PreservedAnalyses::none())
        } else {
            Ok(PreservedAnalyses::all())
        }
    }
}

fn op_ref_in(context: &Context, block_id: tir::BlockId, op_id: OpId) -> OperationRef {
    OperationRef::new(
        context.get_op(op_id),
        Some(context.get_block(block_id)),
        None,
    )
}
