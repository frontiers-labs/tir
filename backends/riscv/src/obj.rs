//! RISC-V object-emission support: ELF format parameters, relocation
//! selection, and the lowerings that turn virtual control-flow ops into real
//! branch instructions around register allocation.

use tir::Operation;
use tir::attributes::AttributeValue;
use tir::backend::binary::{EM_RISCV, ElfClass, ObjectFormatInfo, RelocKind};

use crate::{
    JumpAndLinkOpBuilder, JumpAndLinkRegOpBuilder, VirtualBranchOp, VirtualCallOp,
    VirtualIndirectCallOp, VirtualReturnOp, phys, virt,
};

const R_RISCV_BRANCH: u32 = 16;
const R_RISCV_JAL: u32 = 17;
const R_RISCV_HI20: u32 = 26;
const R_RISCV_LO12_I: u32 = 27;
const R_RISCV_RVC_BRANCH: u32 = 44;
const R_RISCV_RVC_JUMP: u32 = 45;

const EF_RISCV_RVC: u32 = 0x1;
const EF_RISCV_FLOAT_ABI_DOUBLE: u32 = 0x4;

pub(crate) fn object_format(xlen: u32, features: &[crate::Feature]) -> ObjectFormatInfo {
    // e_flags declare the ABI the object was built for; linkers refuse to mix
    // float ABIs, so D-extension targets must claim lp64d/ilp32d.
    let mut elf_flags = 0;
    if features.contains(&crate::Feature::C) {
        elf_flags |= EF_RISCV_RVC;
    }
    if features.contains(&crate::Feature::D) {
        elf_flags |= EF_RISCV_FLOAT_ABI_DOUBLE;
    }
    ObjectFormatInfo {
        elf_machine: EM_RISCV,
        elf_class: if xlen == 64 {
            ElfClass::Elf64
        } else {
            ElfClass::Elf32
        },
        elf_flags,
        reloc_for: |op| match op {
            "jal" => Some(RelocKind {
                r_type: R_RISCV_JAL,
                addend: 0,
            }),
            "beq" | "bne" | "blt" | "bge" | "bltu" | "bgeu" => Some(RelocKind {
                r_type: R_RISCV_BRANCH,
                addend: 0,
            }),
            // Compressed control flow only reaches the encoder through
            // hand-written assembly; codegen never compresses fixup-carrying
            // instructions (no branch relaxation exists for their short
            // ranges).
            "c.j" | "c.jal" => Some(RelocKind {
                r_type: R_RISCV_RVC_JUMP,
                addend: 0,
            }),
            "c.beqz" | "c.bnez" => Some(RelocKind {
                r_type: R_RISCV_RVC_BRANCH,
                addend: 0,
            }),
            // Symbol-operand `lui`/`addi` only come from `lower_addr_of`'s
            // absolute-address pair; immediate forms never carry fixups.
            "lui" => Some(RelocKind {
                r_type: R_RISCV_HI20,
                addend: 0,
            }),
            "addi" => Some(RelocKind {
                r_type: R_RISCV_LO12_I,
                addend: 0,
            }),
            _ => None,
        },
        // RISC-V branch immediates are byte offsets (bit 0 implicit in the
        // encoding's scattering), so deltas are patched unscaled.
        pc_rel_scale: |_| 0,
        pc_rel_from_end: |_| false,
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
    let value = tir::backend::int_attr(constant.attributes(), "value").ok_or_else(|| {
        tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
    })?;
    let dest = virt(constant.result().number(), "GPR");

    let last = materialize_int(context, op, rewriter, dest, value, xlen)?;
    rewriter.replace_op(op, last.as_ref())?;
    Ok(true)
}

/// The instruction sequence materializing `value` into GPR `dest`: `addi rd,
/// x0, imm` when it fits 12 bits, else `lui` + `addiw` (`addi` on rv32). All
/// but the returned final instruction are inserted ahead of `op`.
fn materialize_int(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    dest: AttributeValue,
    value: i64,
    xlen: u32,
) -> Result<Box<dyn Operation>, tir::PassError> {
    if (-2048..2048).contains(&value) {
        let li = crate::AddImmOpBuilder::new(context)
            .attr("rd", dest)
            .attr("rs1", phys(&("GPR".to_string(), 0)))
            .attr("imm", AttributeValue::Int(value))
            .build();
        return Ok(Box::new(li));
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
        Ok(Box::new(add))
    } else {
        let add = crate::AddImmOpBuilder::new(context)
            .attr("rd", dest.clone())
            .attr("rs1", dest)
            .attr("imm", AttributeValue::Int(lo))
            .build();
        Ok(Box::new(add))
    }
}

/// Pre-RA: materialize an `addr_of` symbol address as the absolute
/// `lui rd, %hi(sym)` + `addi rd, rd, %lo(sym)` pair. Both instructions carry
/// the symbol as their immediate; the encoder turns that into R_RISCV_HI20 and
/// R_RISCV_LO12_I relocations (absolute addressing, so executables must link
/// non-PIE).
pub(crate) fn lower_addr_of(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::builtin::AddressOfOp;

    let Some(addr_of) = op.as_op::<AddressOfOp>() else {
        return Ok(false);
    };
    let sym = addr_of.sym_name();
    let dest = virt(addr_of.result().number(), "GPR");

    let lui = crate::LoadUpperImmOpBuilder::new(context)
        .attr("rd", dest.clone())
        .attr("imm", AttributeValue::Str(sym.clone()))
        .build();
    rewriter.insert_op_before(op, &lui)?;
    let addi = crate::AddImmOpBuilder::new(context)
        .attr("rd", dest.clone())
        .attr("rs1", dest)
        .attr("imm", AttributeValue::Str(sym))
        .build();
    rewriter.replace_op(op, &addi)?;
    Ok(true)
}

/// Pre-RA: materialize a `constantf` into its bit pattern in a scratch GPR
/// (the integer `li` sequence) followed by a bit-pattern move into the float
/// destination: `fmv.w.x` for f32, `fmv.d.x` for f64. f64 needs the whole
/// binary64 pattern in one integer register, so it is rv64-only (and shares
/// the integer path's 32-bit materialization limit).
pub(crate) fn lower_constantf_rv32(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_constantf(context, op, rewriter, 32)
}

pub(crate) fn lower_constantf_rv64(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_constantf(context, op, rewriter, 64)
}

fn lower_constantf(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    xlen: u32,
) -> Result<bool, tir::PassError> {
    use tir::builtin::{ConstantFOp, FloatType, IntegerType};

    let Some(constant) = op.as_op::<ConstantFOp>() else {
        return Ok(false);
    };
    let value = constant
        .attributes()
        .iter()
        .find_map(|attr| match (&attr.value, attr.name == "value") {
            (AttributeValue::F64(v), true) => Some(*v),
            _ => None,
        })
        .ok_or_else(|| {
            tir::PassError::InvalidRuleSet("constantf op without a float value".to_string())
        })?;
    let result = constant.result();
    let width = {
        let ty = context.get_type_data(context.get_value(result).ty());
        (ty.as_ref() as &dyn std::any::Any)
            .downcast_ref::<FloatType>()
            .map(FloatType::bit_width)
    };

    let scratch = context
        .create_value(IntegerType::new(context, xlen), None)
        .id()
        .number();
    let scratch_reg = virt(scratch, "GPR");

    let fmv: Box<dyn Operation> = match width {
        Some(32) => {
            // The attribute holds an f64; rounding to f32 yields the constant's
            // binary32 pattern, sign-extended for the integer materializer.
            let bits = (value as f32).to_bits() as i32 as i64;
            let li = materialize_int(context, op, rewriter, scratch_reg.clone(), bits, xlen)?;
            rewriter.insert_op_before(op, li.as_ref())?;
            Box::new(
                crate::FMvWXOpBuilder::new(context)
                    .attr("fd", virt(result.number(), "FPR32"))
                    .attr("rs1", scratch_reg)
                    .build(),
            )
        }
        Some(64) => {
            if xlen != 64 {
                return Err(tir::PassError::InvalidRuleSet(
                    "f64 constants are not supported on rv32 (fmv.d.x needs rv64)".to_string(),
                ));
            }
            let bits = value.to_bits() as i64;
            let li = materialize_int(context, op, rewriter, scratch_reg.clone(), bits, xlen)?;
            rewriter.insert_op_before(op, li.as_ref())?;
            Box::new(
                crate::FMvDXOpBuilder::new(context)
                    .attr("fd", virt(result.number(), "FPR64"))
                    .attr("rs1", scratch_reg)
                    .build(),
            )
        }
        _ => {
            return Err(tir::PassError::InvalidRuleSet(
                "only f32/f64 constants are supported".to_string(),
            ));
        }
    };
    rewriter.replace_op(op, fmv.as_ref())?;
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
