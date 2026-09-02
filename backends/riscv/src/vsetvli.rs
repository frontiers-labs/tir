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

use tir::attributes::AttributeValue;
use tir::backend::{RegSlot, SymbolOp, VirtualCallOp, VirtualIndirectCallOp, reg_slot};
use tir::{
    AnalysisManager, Context, OpHandle, OperationRef, Pass, PassError, PassTarget, Rewriter,
    ValueId,
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
fn lmul_for(avl: &Demand, sew: i64) -> Result<i64, PassError> {
    let Demand::Imm(vl) = avl else {
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
    match lmul_for(&Demand::Imm(1), total_bits)? {
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

/// A vector-length demand, and the identity a configuration is keyed by: the
/// immediate the instruction was selected with, or the value its `VCSR::vl`
/// read bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Demand {
    Imm(i64),
    Value(ValueId),
}

/// The demand a register slot of `op` carries: an integer the slot was
/// materialized to, or the value it reads.
fn demand(op: &OpHandle, name: &str) -> Option<Demand> {
    match op.attr(name) {
        Some(AttributeValue::Int(v)) => Some(Demand::Imm(v)),
        Some(_) => None,
        None => match reg_slot(op, name)? {
            RegSlot::Value(value) => Some(Demand::Value(value)),
            RegSlot::Phys(_) => None,
        },
    }
}

/// The configuration the vector unit holds: the AVL demand keys the granted
/// `vl` satisfies, and the packed `vtypei` in effect.
struct ConfigState {
    keys: Vec<Demand>,
    vtypei: i64,
}

#[derive(Default)]
pub struct InsertVsetvliPass;

impl InsertVsetvliPass {
    /// Insert the configuration instruction(s) satisfying `demand` before
    /// `anchor`: `vsetivli` when the AVL is a 5-bit immediate, `vsetvli` when
    /// it is a register (materializing larger immediates through `addi`).
    fn insert_config(
        &self,
        context: &Context,
        rewriter: &mut Rewriter,
        anchor: &OperationRef,
        avl: Demand,
        vtypei: i64,
    ) -> Result<(), PassError> {
        let x0 = tir::backend::phys_attr((crate::RegClass::GPR.id(), 0));
        let vtypei = AttributeValue::Int(vtypei);
        match avl {
            Demand::Imm(v) if (0..=UIMM5_MAX).contains(&v) => {
                let op = VSetIVliOpBuilder::new(context)
                    .attr("rd", x0)
                    .attr("avl", AttributeValue::Int(v))
                    .attr("vtypei", vtypei)
                    .build();
                rewriter.insert_op_before(anchor, &op)
            }
            Demand::Imm(v) if (0..=SIMM12_MAX).contains(&v) => {
                let avl_reg = context.create_value(crate::gpr_ty(context), None).id();
                let li = AddImmOpBuilder::new(context)
                    .result_values(vec![avl_reg])
                    .attr("rs1", x0.clone())
                    .attr("imm", AttributeValue::Int(v))
                    .build();
                rewriter.insert_op_before(anchor, &li)?;
                let op = VSetVliOpBuilder::new(context)
                    .attr("rd", x0)
                    .avl(avl_reg)
                    .attr("vtypei", vtypei)
                    .build();
                rewriter.insert_op_before(anchor, &op)
            }
            Demand::Value(value) => {
                let op = VSetVliOpBuilder::new(context)
                    .attr("rd", x0)
                    .avl(value)
                    .attr("vtypei", vtypei)
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
    ) -> Result<(), PassError> {
        let Some(&region_id) = op.op().regions().first() else {
            return Ok(());
        };
        let blocks: Vec<_> = context
            .get_region(region_id)
            .iter(context.clone())
            .map(|b| b.id())
            .collect();

        for block_id in blocks {
            // Unknown at block entry (no cross-block propagation yet) and after
            // calls, which may reconfigure the unit.
            let mut state: Option<ConfigState> = None;
            for op_id in context.get_block(block_id).op_ids() {
                let body_op = context.get_op(op_id);
                let attr = |name: &str| body_op.attr(name);
                if body_op.is::<VSetVliOp>() || body_op.is::<VSetIVliOp>() {
                    // An existing configuration instruction (e.g. selected for a
                    // `vector.vector_len`) satisfies demands on its AVL, and —
                    // when its grant is live (`rd` a value) — demands on the
                    // granted count, since `vl` equals `rd`'s value.
                    state = match attr("vtypei") {
                        Some(AttributeValue::Int(vtypei)) => {
                            let mut keys: Vec<Demand> =
                                demand(&body_op, "avl").into_iter().collect();
                            if let Some(RegSlot::Value(value)) = reg_slot(&body_op, "rd") {
                                keys.push(Demand::Value(value));
                            }
                            Some(ConfigState { keys, vtypei })
                        }
                        _ => None,
                    };
                    continue;
                }
                if body_op.is::<VirtualCallOp>() || body_op.is::<VirtualIndirectCallOp>() {
                    state = None;
                    continue;
                }
                if body_op.attr("vl").is_none() && reg_slot(&body_op, "vl").is_none() {
                    continue;
                }
                let Some(key) = demand(&body_op, "vl") else {
                    return Err(PassError::InvalidRuleSet(format!(
                        "vector op '{}' has an unsupported vector-length demand",
                        body_op.name().as_str()
                    )));
                };
                let Some(AttributeValue::Int(sew)) = attr("sew") else {
                    return Err(PassError::InvalidRuleSet(format!(
                        "vector op '{}' demands a vector length but no element width",
                        body_op.name().as_str()
                    )));
                };
                // LMUL legalization: an op whose demanded elements exceed one
                // register works on a register group, so the values its `VR`
                // slots name move to the group class and the allocator assigns
                // aligned spans.
                let lmul = lmul_for(&key, sew)?;
                if lmul > 1 {
                    let group = match lmul {
                        2 => crate::RegClass::VRM2.id(),
                        4 => crate::RegClass::VRM4.id(),
                        _ => crate::RegClass::VRM8.id(),
                    };
                    let group_ty = tir::backend::RegClassType::new(context, group);
                    for slot in tir::backend::reg_slots(&body_op) {
                        if slot.port.class == Some(crate::RegClass::VR.id())
                            && let RegSlot::Value(value) = slot.slot
                        {
                            context.retype_value(value, group_ty);
                        }
                    }
                }
                let vtypei = vtypei_for(sew, lmul)?;
                if state
                    .as_ref()
                    .is_some_and(|s| s.vtypei == vtypei && s.keys.contains(&key))
                {
                    continue;
                }
                let anchor = OperationRef::new(context.get_op(op_id));
                self.insert_config(context, rewriter, &anchor, key, vtypei)?;
                state = Some(ConfigState {
                    keys: vec![key],
                    vtypei,
                });
            }
        }

        Ok(())
    }
}
