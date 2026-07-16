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
                field_offset: 0,
            }),
            "beq" | "bne" | "blt" | "bge" | "bltu" | "bgeu" => Some(RelocKind {
                r_type: R_RISCV_BRANCH,
                addend: 0,
                field_offset: 0,
            }),
            // Compressed control flow only reaches the encoder through
            // hand-written assembly; codegen never compresses fixup-carrying
            // instructions (no branch relaxation exists for their short
            // ranges).
            "c.j" | "c.jal" => Some(RelocKind {
                r_type: R_RISCV_RVC_JUMP,
                addend: 0,
                field_offset: 0,
            }),
            "c.beqz" | "c.bnez" => Some(RelocKind {
                r_type: R_RISCV_RVC_BRANCH,
                addend: 0,
                field_offset: 0,
            }),
            // Symbol-operand `lui`/`addi` only come from `lower_addr_of`'s
            // absolute-address pair; immediate forms never carry fixups.
            "lui" => Some(RelocKind {
                r_type: R_RISCV_HI20,
                addend: 0,
                field_offset: 0,
            }),
            "addi" => Some(RelocKind {
                r_type: R_RISCV_LO12_I,
                addend: 0,
                field_offset: 0,
            }),
            _ => None,
        },
        // RISC-V branch immediates are byte offsets (bit 0 implicit in the
        // encoding's scattering), so deltas are patched unscaled.
        pc_rel_scale: |_| 0,
        pc_rel_from_end: |_| false,
    }
}

/// Pre-RA: integer constants are selected by the e-graph materialize axioms,
/// so a live `constant` op reaching this hook is a selection bug — fail loudly,
/// never miscompile. rv32 keeps its specific diagnostic for values a 32-bit
/// target cannot represent (no rule could ever cover them).
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
    _context: &tir::Context,
    op: &tir::OperationRef,
    _rewriter: &mut tir::Rewriter,
    xlen: u32,
) -> Result<bool, tir::PassError> {
    use tir::builtin::ConstantOp;

    let Some(constant) = op.as_op::<ConstantOp>() else {
        return Ok(false);
    };
    let value = tir::backend::int_attr(constant.attributes(), "value").ok_or_else(|| {
        tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
    })?;

    if xlen == 32 && i32::try_from(value).is_err() {
        return Err(tir::PassError::InvalidRuleSet(format!(
            "constant {value} does not fit the selected 32-bit target"
        )));
    }
    Err(tir::PassError::InvalidRuleSet(format!(
        "constant {value} survived instruction selection; the materialize \
         axioms must cover every representable integer constant"
    )))
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
    let dest = virt(addr_of.result().number(), crate::RegClass::GPR.id());

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
            .attr("rd", phys(&(crate::RegClass::GPR.id(), 0)))
            .attr("rs1", phys(&(crate::RegClass::GPR.id(), 1)))
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
            .attr("rd", phys(&(crate::RegClass::GPR.id(), 0)))
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
            .attr(
                "rd",
                phys(&crate::default_abi().ra.expect("RISC-V ABI must define ra")),
            )
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
            .attr(
                "rd",
                phys(&crate::default_abi().ra.expect("RISC-V ABI must define ra")),
            )
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
