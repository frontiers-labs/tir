//! AArch64 object-emission support: ELF format parameters, relocation
//! selection, and the lowerings that turn virtual control-flow ops into real
//! branch instructions around register allocation.

use tir::Operation;
use tir::attributes::AttributeValue;
use tir_be_common::binary::{EM_AARCH64, ElfClass, ObjectFormatInfo, RelocKind};

use crate::{
    BranchEqOpBuilder, BranchGreaterEqOpBuilder, BranchImmediateOpBuilder, BranchLessThanOpBuilder,
    BranchLinkOpBuilder, BranchLinkRegOpBuilder, BranchNotEqOpBuilder, CompareOpBuilder,
    MoveWideZeroOpBuilder, ReturnOpBuilder, VirtualBranchOp, VirtualCallOp, VirtualCondBranchOp,
    VirtualIndirectCallOp, VirtualReturnOp, phys, virt,
};

const R_AARCH64_CONDBR19: u32 = 280;
const R_AARCH64_JUMP26: u32 = 282;
const R_AARCH64_CALL26: u32 = 283;

pub(crate) fn object_format() -> ObjectFormatInfo {
    ObjectFormatInfo {
        elf_machine: EM_AARCH64,
        elf_class: ElfClass::Elf64,
        elf_flags: 0,
        reloc_for: |op| match op {
            "bl" => Some(RelocKind {
                r_type: R_AARCH64_CALL26,
                addend: 0,
            }),
            "b" => Some(RelocKind {
                r_type: R_AARCH64_JUMP26,
                addend: 0,
            }),
            "b.eq" | "b.ne" | "b.lt" | "b.ge" | "b.lo" | "b.hs" => Some(RelocKind {
                r_type: R_AARCH64_CONDBR19,
                addend: 0,
            }),
            _ => None,
        },
        // AArch64 branch immediates are word offsets: byte delta >> 2.
        pc_rel_scale: |_| 2,
    }
}

/// Pre-RA: materialize a `constant` that survived instruction selection into
/// `movz rd, #imm` (only the unshifted 16-bit form exists so far).
pub(crate) fn lower_constant(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::builtin::ConstantOp;

    let Some(constant) = op.as_op::<ConstantOp>() else {
        return Ok(false);
    };
    let value = tir_be_common::int_attr(constant.attributes(), "value").ok_or_else(|| {
        tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
    })?;
    if !(0..=0xFFFF).contains(&value) {
        return Err(tir::PassError::InvalidRuleSet(format!(
            "constant {value} does not fit movz #imm16; wide constant materialization is not implemented"
        )));
    }

    let movz = MoveWideZeroOpBuilder::new(context)
        .attr("rd", virt(constant.result().number(), "GPR"))
        .attr("imm", AttributeValue::Int(value))
        .build();
    rewriter.replace_op(op, &movz)?;
    Ok(true)
}

/// Pre-RA: `vcond_br cond, t, f` becomes a `cmp` + conditional branch to `t` +
/// `b f`. Runs before register allocation so the condition register gets colored.
///
/// When `cond` is produced by a `cmpi`, the comparison is fused: the two compared
/// values feed the `cmp` directly and a flag-tested branch (`b.lt`/`b.ge`/`b.eq`/
/// `b.ne`, with operands swapped for the `>`/`<=` predicates AArch64 lacks)
/// replaces the boolean test, and the dead `cmpi` is erased. Otherwise the
/// condition register is compared against `xzr` with `b.ne`.
pub(crate) fn lower_vcond_br(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    let Some(cond_br) = op.as_op::<VirtualCondBranchOp>() else {
        return Ok(false);
    };
    // The only operand is the condition; any extra operands are values
    // forwarded to successor block arguments, which codegen cannot place yet.
    let operands = cond_br.operands();
    let (Some(&condition), 1) = (operands.first(), operands.len()) else {
        return Err(tir::PassError::InvalidRuleSet(
            "block arguments on conditional branch edges are not supported by codegen yet"
                .to_string(),
        ));
    };
    let true_dest = block_attr(&cond_br, "true_dest")?;
    let false_dest = block_attr(&cond_br, "false_dest")?;

    let fused = tir_be_common::cmpi_operands(context, condition);
    let (cmp, taken) = match &fused {
        Some((lhs, rhs, predicate)) => compare_branch(context, predicate, *lhs, *rhs, true_dest)?,
        None => {
            let cmp = CompareOpBuilder::new(context)
                .attr("rn", virt(condition.number(), "GPR"))
                .attr("rm", phys(&("GPR".to_string(), 31))) // xzr
                .build();
            let bne = BranchNotEqOpBuilder::new(context)
                .attr("imm", AttributeValue::Block(true_dest))
                .build();
            (
                Box::new(cmp) as Box<dyn tir::Operation>,
                Box::new(bne) as Box<dyn tir::Operation>,
            )
        }
    };
    rewriter.insert_op_before(op, cmp.as_ref())?;
    rewriter.insert_op_before(op, taken.as_ref())?;

    let fallthrough = crate::VirtualBranchOpBuilder::new(context)
        .attr("dest", AttributeValue::Block(false_dest))
        .build();
    rewriter.replace_op(op, &fallthrough)?;

    // The cmpi is dead once its only use (the branch) is gone; erase it last so
    // no operation transiently references a removed value.
    if fused.is_some() {
        tir_be_common::erase_defining_op(context, condition, rewriter)?;
    }
    Ok(true)
}

/// Build the `(cmp, conditional branch)` pair taken when `predicate` holds on
/// `(lhs, rhs)`. AArch64 has only `b.lt`/`b.ge`/`b.eq`/`b.ne` for signed tests,
/// so `>`/`<=` are realized by swapping the `cmp` operands.
#[allow(clippy::type_complexity)]
fn compare_branch(
    context: &tir::Context,
    predicate: &str,
    lhs: tir::ValueId,
    rhs: tir::ValueId,
    true_dest: tir::BlockId,
) -> Result<(Box<dyn tir::Operation>, Box<dyn tir::Operation>), tir::PassError> {
    let l = virt(lhs.number(), "GPR");
    let r = virt(rhs.number(), "GPR");
    let dest = AttributeValue::Block(true_dest);
    // (rn, rm) for the cmp, then the flag-tested branch.
    let (rn, rm, branch): (_, _, Box<dyn tir::Operation>) = match predicate {
        "slt" => (
            l,
            r,
            Box::new(
                BranchLessThanOpBuilder::new(context)
                    .attr("imm", dest)
                    .build(),
            ),
        ),
        "sge" => (
            l,
            r,
            Box::new(
                BranchGreaterEqOpBuilder::new(context)
                    .attr("imm", dest)
                    .build(),
            ),
        ),
        "sgt" => (
            r,
            l,
            Box::new(
                BranchLessThanOpBuilder::new(context)
                    .attr("imm", dest)
                    .build(),
            ),
        ),
        "sle" => (
            r,
            l,
            Box::new(
                BranchGreaterEqOpBuilder::new(context)
                    .attr("imm", dest)
                    .build(),
            ),
        ),
        "eq" => (
            l,
            r,
            Box::new(BranchEqOpBuilder::new(context).attr("imm", dest).build()),
        ),
        "ne" => (
            l,
            r,
            Box::new(BranchNotEqOpBuilder::new(context).attr("imm", dest).build()),
        ),
        other => {
            return Err(tir::PassError::InvalidRuleSet(format!(
                "unsupported cmpi predicate '{other}'"
            )));
        }
    };
    let cmp = CompareOpBuilder::new(context)
        .attr("rn", rn)
        .attr("rm", rm)
        .build();
    Ok((Box::new(cmp), branch))
}

fn block_attr(op: &dyn tir::Operation, name: &str) -> Result<tir::BlockId, tir::PassError> {
    op.attributes()
        .iter()
        .find_map(|attr| match (&attr.value, attr.name == name) {
            (AttributeValue::Block(block), true) => Some(*block),
            _ => None,
        })
        .ok_or_else(|| {
            tir::PassError::InvalidRuleSet(format!("branch is missing its '{name}' target"))
        })
}

/// Post-RA: `vret` becomes `ret x30`; `vbr` becomes `b dest`.
pub(crate) fn finalize_virtual_ops(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    if op.as_op::<VirtualReturnOp>().is_some() {
        let ret = ReturnOpBuilder::new(context)
            .attr("rn", phys(&("GPR".to_string(), 30)))
            .build();
        rewriter.replace_op(op, &ret)?;
        return Ok(true);
    }

    if let Some(br) = op.as_op::<VirtualBranchOp>() {
        if !br.operands().is_empty() {
            return Err(tir::PassError::InvalidRuleSet(
                "block arguments on branch edges are not supported by codegen yet".to_string(),
            ));
        }
        let dest = block_attr(&br, "dest")?;
        let jump = BranchImmediateOpBuilder::new(context)
            .attr("imm", AttributeValue::Block(dest))
            .build();
        rewriter.replace_op(op, &jump)?;
        return Ok(true);
    }

    // `vcall callee` becomes `bl callee`: the symbol operand survives into the
    // encoder as a fixup and is emitted as an R_AARCH64_CALL26 relocation, since
    // the callee's address is unknown until link time.
    if let Some(call) = op.as_op::<VirtualCallOp>() {
        let callee = string_attr(&call, "callee")?;
        let bl = BranchLinkOpBuilder::new(context)
            .attr("imm", AttributeValue::Str(callee))
            .build();
        rewriter.replace_op(op, &bl)?;
        return Ok(true);
    }

    // `vcall_indirect` becomes `blr target`; the target register was colored by
    // the allocator through the op's `callee_reg` attribute.
    if let Some(call) = op.as_op::<VirtualIndirectCallOp>() {
        let target = register_attr(&call, "callee_reg")?;
        let blr = BranchLinkRegOpBuilder::new(context)
            .attr("rn", target)
            .build();
        rewriter.replace_op(op, &blr)?;
        return Ok(true);
    }

    Ok(false)
}

fn string_attr(op: &dyn tir::Operation, name: &str) -> Result<String, tir::PassError> {
    op.attributes()
        .iter()
        .find_map(|attr| match (&attr.value, attr.name == name) {
            (AttributeValue::Str(s), true) => Some(s.clone()),
            _ => None,
        })
        .ok_or_else(|| tir::PassError::InvalidRuleSet(format!("call is missing its '{name}'")))
}

fn register_attr(op: &dyn tir::Operation, name: &str) -> Result<AttributeValue, tir::PassError> {
    op.attributes()
        .iter()
        .find_map(|attr| match (&attr.value, attr.name == name) {
            (value @ AttributeValue::Register(_), true) => Some(value.clone()),
            _ => None,
        })
        .ok_or_else(|| tir::PassError::InvalidRuleSet(format!("call is missing its '{name}'")))
}
