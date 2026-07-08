//! AArch64 object-emission support: ELF format parameters, relocation
//! selection, and the lowerings that turn virtual control-flow ops into real
//! branch instructions around register allocation.

use tir::Operation;
use tir::attributes::AttributeValue;
use tir::backend::binary::{EM_AARCH64, ElfClass, ObjectFormatInfo, RelocKind};

use crate::{
    AddressPCRelOpBuilder, BranchImmediateOpBuilder, BranchLinkOpBuilder, BranchLinkRegOpBuilder,
    FMovImmediateDoubleOpBuilder, FMovImmediateSingleOpBuilder, MoveWideZeroOpBuilder,
    ReturnOpBuilder, VirtualBranchOp, VirtualCallOp, VirtualIndirectCallOp, VirtualReturnOp, phys,
    virt,
};

const R_AARCH64_ADR_PREL_LO21: u32 = 274;
const R_AARCH64_TSTBR14: u32 = 279;
const R_AARCH64_CONDBR19: u32 = 280;
const R_AARCH64_JUMP26: u32 = 282;
const R_AARCH64_CALL26: u32 = 283;

pub(crate) fn object_format() -> ObjectFormatInfo {
    ObjectFormatInfo {
        elf_machine: EM_AARCH64,
        elf_class: ElfClass::Elf64,
        elf_flags: 0,
        reloc_for: |op| match op {
            "adr" => Some(RelocKind {
                r_type: R_AARCH64_ADR_PREL_LO21,
                addend: 0,
                field_offset: 0,
            }),
            "bl" => Some(RelocKind {
                r_type: R_AARCH64_CALL26,
                addend: 0,
                field_offset: 0,
            }),
            "b" => Some(RelocKind {
                r_type: R_AARCH64_JUMP26,
                addend: 0,
                field_offset: 0,
            }),
            "b.eq" | "b.ne" | "b.lt" | "b.ge" | "b.lo" | "b.hs" | "b.gt" | "b.le" | "b.hi"
            | "b.ls" | "b.mi" | "b.pl" | "b.vs" | "b.vc" | "cbz" | "cbnz" => Some(RelocKind {
                r_type: R_AARCH64_CONDBR19,
                addend: 0,
                field_offset: 0,
            }),
            "tbz" | "tbnz" => Some(RelocKind {
                r_type: R_AARCH64_TSTBR14,
                addend: 0,
                field_offset: 0,
            }),
            _ => None,
        },
        // AArch64 branch immediates are word offsets; adr uses byte offsets.
        pc_rel_scale: |op| if op == "adr" { 0 } else { 2 },
        pc_rel_from_end: |_| false,
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
    let value = tir::backend::int_attr(constant.attributes(), "value").ok_or_else(|| {
        tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
    })?;
    if !(0..=0xFFFF).contains(&value) {
        return Err(tir::PassError::InvalidRuleSet(format!(
            "constant {value} does not fit movz #imm16; wide constant materialization is not implemented"
        )));
    }

    let movz = MoveWideZeroOpBuilder::new(context)
        .attr(
            "rd",
            virt(constant.result().number(), crate::RegClass::GPR.id()),
        )
        .attr("imm", AttributeValue::Int(value))
        .build();
    rewriter.replace_op(op, &movz)?;
    Ok(true)
}

pub(crate) fn lower_constantf(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::builtin::{ConstantFOp, FloatType};

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

    match width {
        Some(32) => {
            let bits = (value as f32).to_bits();
            let imm = find_vfp_imm32(bits).ok_or_else(|| {
                tir::PassError::InvalidRuleSet(format!(
                    "f32 constant {value} is not encodable as an arm64 fmov immediate"
                ))
            })?;
            let fmov = FMovImmediateSingleOpBuilder::new(context)
                .attr("fd", virt(result.number(), crate::RegClass::FPR32.id()))
                .attr("imm", AttributeValue::Int(i64::from(imm)))
                .build();
            rewriter.replace_op(op, &fmov)?;
        }
        Some(64) => {
            let bits = value.to_bits();
            let imm = find_vfp_imm64(bits).ok_or_else(|| {
                tir::PassError::InvalidRuleSet(format!(
                    "f64 constant {value} is not encodable as an arm64 fmov immediate"
                ))
            })?;
            let fmov = FMovImmediateDoubleOpBuilder::new(context)
                .attr("fd", virt(result.number(), crate::RegClass::FPR64.id()))
                .attr("imm", AttributeValue::Int(i64::from(imm)))
                .build();
            rewriter.replace_op(op, &fmov)?;
        }
        _ => {
            return Err(tir::PassError::InvalidRuleSet(
                "only f32/f64 constants are supported".to_string(),
            ));
        }
    }
    Ok(true)
}

fn find_vfp_imm32(bits: u32) -> Option<u8> {
    (0..=u8::MAX).find(|&imm| vfp_expand_imm32(imm) == bits)
}

fn find_vfp_imm64(bits: u64) -> Option<u8> {
    (0..=u8::MAX).find(|&imm| vfp_expand_imm64(imm) == bits)
}

fn vfp_expand_imm32(imm: u8) -> u32 {
    let sign = ((imm >> 7) & 1) as u32;
    let bit6 = ((imm >> 6) & 1) as u32;
    let imm54 = ((imm >> 4) & 0x3) as u32;
    let frac = (imm & 0xf) as u32;
    (sign << 31)
        | ((1 - bit6) << 30)
        | (((0u32.wrapping_sub(bit6)) & 0x1f) << 25)
        | (imm54 << 23)
        | (frac << 19)
}

fn vfp_expand_imm64(imm: u8) -> u64 {
    let sign = ((imm >> 7) & 1) as u64;
    let bit6 = ((imm >> 6) & 1) as u64;
    let imm54 = ((imm >> 4) & 0x3) as u64;
    let frac = (imm & 0xf) as u64;
    (sign << 63)
        | ((1 - bit6) << 62)
        | (((0u64.wrapping_sub(bit6)) & 0xff) << 54)
        | (imm54 << 52)
        | (frac << 48)
}

/// Pre-RA: materialize an `addr_of` symbol address as `adr rd, sym`. The
/// encoder leaves the immediate as a fixup emitted with R_AARCH64_ADR_PREL_LO21.
pub(crate) fn lower_addr_of(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::builtin::AddressOfOp;

    let Some(addr_of) = op.as_op::<AddressOfOp>() else {
        return Ok(false);
    };
    let adr = AddressPCRelOpBuilder::new(context)
        .attr(
            "rd",
            virt(addr_of.result().number(), crate::RegClass::GPR.id()),
        )
        .attr("imm", AttributeValue::Str(addr_of.sym_name()))
        .build();
    rewriter.replace_op(op, &adr)?;
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

/// Post-RA: `vret` becomes `ret x30`; `vbr` becomes `b dest`.
pub(crate) fn finalize_virtual_ops(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    if op.as_op::<VirtualReturnOp>().is_some() {
        let ret = ReturnOpBuilder::new(context)
            .attr("rn", phys(&(crate::RegClass::GPR.id(), 30)))
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
