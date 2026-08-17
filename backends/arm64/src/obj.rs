//! AArch64 object-emission support: ELF format parameters, relocation
//! selection, and the lowerings that turn virtual control-flow ops into real
//! branch instructions around register allocation.

use tir::Operation;
use tir::attributes::AttributeValue;
use tir::backend::binary::{EM_AARCH64, ElfClass, ObjectFormatInfo, RelocKind};
use tir::backend::{VirtualBranchOp, VirtualCallOp, VirtualIndirectCallOp, VirtualReturnOp};

use crate::{
    AddressPCRelOpBuilder, BranchImmediateOpBuilder, BranchLinkOpBuilder, BranchLinkRegOpBuilder,
    ReturnOpBuilder, phys, virt,
};

const R_AARCH64_ADR_PREL_LO21: u32 = 274;
const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_ABS32: u32 = 258;
const R_AARCH64_TSTBR14: u32 = 279;
const R_AARCH64_CONDBR19: u32 = 280;
const R_AARCH64_JUMP26: u32 = 282;
const R_AARCH64_CALL26: u32 = 283;

pub(crate) fn object_format() -> ObjectFormatInfo {
    ObjectFormatInfo {
        elf_machine: EM_AARCH64,
        elf_class: ElfClass::Elf64,
        elf_flags: 0,
        absolute_reloc: |width| match width {
            4 => Some(R_AARCH64_ABS32),
            8 => Some(R_AARCH64_ABS64),
            _ => None,
        },
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

/// Pre-RA: materialize an `addr_of` symbol address as `adr rd, sym`. The
/// encoder leaves the immediate as a fixup emitted with R_AARCH64_ADR_PREL_LO21.
pub(crate) fn lower_addr_of(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::func::AddressOfOp;

    let Some(addr_of) = op.as_op::<AddressOfOp>() else {
        return Ok(false);
    };
    let adr = AddressPCRelOpBuilder::new(context)
        .attr(
            "rd",
            virt(addr_of.result().number(), crate::RegClass::GPR.id()),
        )
        .attr("imm", AttributeValue::Str(addr_of.sym_name().into()))
        .build();
    rewriter.replace_op(op, &adr)?;
    Ok(true)
}

fn block_attr(op: &dyn tir::Operation, name: &str) -> Result<tir::BlockId, tir::PassError> {
    match op.attr(name) {
        Some(AttributeValue::Block(block)) => Some(block),
        _ => None,
    }
    .ok_or_else(|| tir::PassError::InvalidRuleSet(format!("branch is missing its '{name}' target")))
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
            .attr("imm", AttributeValue::Str(callee.into()))
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
    match op.attr(name) {
        Some(AttributeValue::Str(s)) => Some(s.to_string()),
        _ => None,
    }
    .ok_or_else(|| tir::PassError::InvalidRuleSet(format!("call is missing its '{name}'")))
}

fn register_attr(op: &dyn tir::Operation, name: &str) -> Result<AttributeValue, tir::PassError> {
    match op.attr(name) {
        Some(value @ AttributeValue::Register(_)) => Some(value.clone()),
        _ => None,
    }
    .ok_or_else(|| tir::PassError::InvalidRuleSet(format!("call is missing its '{name}'")))
}
