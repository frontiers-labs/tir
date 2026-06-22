//! RISC-V object-emission support: ELF format parameters, relocation
//! selection, and the lowerings that turn virtual control-flow ops into real
//! branch instructions around register allocation.

use tir::Operation;
use tir::attributes::AttributeValue;
use tir_be_common::binary::{EM_RISCV, ElfClass, ObjectFormatInfo, RelocKind};

use crate::{
    JumpAndLinkOpBuilder, JumpAndLinkRegOpBuilder, VirtualBranchOp, VirtualCallOp,
    VirtualIndirectCallOp, VirtualReturnOp, phys, virt,
};

const R_RISCV_BRANCH: u32 = 16;
const R_RISCV_JAL: u32 = 17;

pub(crate) fn object_format(xlen: u32) -> ObjectFormatInfo {
    ObjectFormatInfo {
        elf_machine: EM_RISCV,
        elf_class: if xlen == 64 {
            ElfClass::Elf64
        } else {
            ElfClass::Elf32
        },
        elf_flags: 0,
        reloc_for: |op| match op {
            "jal" => Some(RelocKind {
                r_type: R_RISCV_JAL,
                addend: 0,
            }),
            "beq" | "bne" | "blt" | "bge" | "bltu" | "bgeu" => Some(RelocKind {
                r_type: R_RISCV_BRANCH,
                addend: 0,
            }),
            _ => None,
        },
        // RISC-V branch immediates are byte offsets (bit 0 implicit in the
        // encoding's scattering), so deltas are patched unscaled.
        pc_rel_scale: |_| 0,
    }
}

/// Pre-RA: materialize a `constant` that survived instruction selection
/// (i.e. one no instruction folded as an immediate) into `addi rd, x0, imm`,
/// or `lui`+`addiw` (`addi` on rv32) when it does not fit 12 bits.
pub(crate) fn lower_constant_rv32(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_constant(context, op, rewriter, 32)
}

pub(crate) fn lower_constant_rv64(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_constant(context, op, rewriter, 64)
}

fn lower_constant(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    xlen: u32,
) -> Result<bool, tir::PassError> {
    use tir::builtin::ConstantOp;

    let Some(constant) = op.as_op::<ConstantOp>() else {
        return Ok(false);
    };
    let value = tir_be_common::int_attr(constant.attributes(), "value").ok_or_else(|| {
        tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
    })?;
    let dest = virt(constant.result().number(), "GPR");

    if (-2048..2048).contains(&value) {
        let li = crate::AddImmOpBuilder::new(context)
            .attr("rd", dest)
            .attr("rs1", phys(&("GPR".to_string(), 0)))
            .attr("imm", AttributeValue::Int(value))
            .build();
        rewriter.replace_op(op, &li)?;
        return Ok(true);
    }

    if i32::try_from(value).is_err() {
        return Err(tir::PassError::InvalidRuleSet(format!(
            "constant {value} does not fit 32 bits; wide constant materialization is not implemented"
        )));
    }

    // Split into a sign-adjusted upper-20/lower-12 pair: `lui` then `addiw`
    // (`addi` on rv32) reconstruct the 32-bit value.
    let hi = ((value + 0x800) >> 12) & 0xFFFFF;
    let lo = value - (((value + 0x800) >> 12) << 12);
    let lui = crate::LoadUpperImmOpBuilder::new(context)
        .attr("rd", dest.clone())
        .attr("imm", AttributeValue::Int(hi))
        .build();
    rewriter.insert_op_before(op, &lui)?;
    if xlen == 64 {
        let add = crate::AddImmWordOpBuilder::new(context)
            .attr("rd", dest.clone())
            .attr("rs1", dest)
            .attr("imm", AttributeValue::Int(lo))
            .build();
        rewriter.replace_op(op, &add)?;
    } else {
        let add = crate::AddImmOpBuilder::new(context)
            .attr("rd", dest.clone())
            .attr("rs1", dest)
            .attr("imm", AttributeValue::Int(lo))
            .build();
        rewriter.replace_op(op, &add)?;
    }
    Ok(true)
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

/// Post-RA: `vret` becomes `jalr x0, x1, 0`; `vbr` becomes `jal x0, dest`.
pub(crate) fn finalize_virtual_ops(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    if op.as_op::<VirtualReturnOp>().is_some() {
        let ret = JumpAndLinkRegOpBuilder::new(context)
            .attr("rd", phys(&("GPR".to_string(), 0)))
            .attr("rs1", phys(&("GPR".to_string(), 1)))
            .attr("imm", AttributeValue::Int(0))
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
        let jump = JumpAndLinkOpBuilder::new(context)
            .attr("rd", phys(&("GPR".to_string(), 0)))
            .attr("imm", AttributeValue::Block(dest))
            .build();
        rewriter.replace_op(op, &jump)?;
        return Ok(true);
    }

    // `vcall callee` becomes `jal ra, callee`: the symbol operand survives into
    // the encoder as a fixup and is emitted as an R_RISCV_JAL relocation, since
    // the callee's address is unknown until link time.
    if let Some(call) = op.as_op::<VirtualCallOp>() {
        let callee = string_attr(&call, "callee")?;
        let jal = JumpAndLinkOpBuilder::new(context)
            .attr("rd", phys(&("GPR".to_string(), crate::RA)))
            .attr("imm", AttributeValue::Str(callee))
            .build();
        rewriter.replace_op(op, &jal)?;
        return Ok(true);
    }

    // `vcall_indirect` becomes `jalr ra, target, 0`; the target register was
    // colored by the allocator through the op's `callee_reg` attribute.
    if let Some(call) = op.as_op::<VirtualIndirectCallOp>() {
        let target = register_attr(&call, "callee_reg")?;
        let jalr = JumpAndLinkRegOpBuilder::new(context)
            .attr("rd", phys(&("GPR".to_string(), crate::RA)))
            .attr("rs1", target)
            .attr("imm", AttributeValue::Int(0))
            .build();
        rewriter.replace_op(op, &jalr)?;
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
