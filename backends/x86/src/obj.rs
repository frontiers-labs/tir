//! x86 object-emission support: ELF parameters, the relocation used for direct
//! calls, the pre/post register-allocation lowerings that turn virtual ops into
//! real instructions, and the two-address fixup that makes the ALU encodings
//! compute `rs1 OP rs2`.

use tir::Operation;
use tir::attributes::AttributeValue;
use tir_be_common::binary::{
    BinaryWriter, ElfClass, InstructionPatcher, ObjectFormatInfo, RelocKind,
};

use crate::{
    JmpOpBuilder, JneOpBuilder, MovRI32OpBuilder, MovRI64OpBuilder, RetOpBuilder, Test32OpBuilder,
    Test64OpBuilder, VirtualBranchOp, VirtualBranchOpBuilder, VirtualCallOp, VirtualCondBranchOp,
    VirtualReturnOp, class_name, virt,
};

const EM_386: u16 = 3;
const EM_X86_64: u16 = 62;
// Both i386 and x86-64 number PC-relative-to-symbol as relocation type 2.
const R_386_PC32: u32 = 2;
const R_X86_64_PC32: u32 = 2;

pub(crate) fn object_format(bits: u32) -> ObjectFormatInfo {
    let (machine, class) = if bits == 32 {
        (EM_386, ElfClass::Elf32)
    } else {
        (EM_X86_64, ElfClass::Elf64)
    };
    // i386 and x86-64 share relocation number 2 for a PC-relative symbol.
    const _: () = assert!(R_386_PC32 == R_X86_64_PC32);
    ObjectFormatInfo {
        elf_machine: machine,
        elf_class: class,
        elf_flags: 0,
        reloc_for: |op| match op {
            // `call rel32`: the displacement is relative to the next instruction,
            // so the addend is -4 (the width of the displacement field).
            "call" => Some(RelocKind {
                r_type: R_X86_64_PC32,
                addend: -4,
            }),
            _ => None,
        },
        // x86 branch displacements are unscaled byte offsets.
        pc_rel_scale: |_| 0,
        // The relocation lands on the displacement, one opcode byte into `call`.
        reloc_field_offset: |op| if op == "call" { 1 } else { 0 },
    }
}

/// `jmp rel32` (`E9 disp32`): the displacement is relative to the next
/// instruction, five bytes past the start.
fn patch_jmp(bytes: &mut [u8], value: i64) -> Option<()> {
    let rel = i32::try_from(value - 5).ok()?;
    bytes.get_mut(1..5)?.copy_from_slice(&rel.to_le_bytes());
    Some(())
}

/// `jne rel32` (`0F 85 disp32`): a six-byte instruction.
fn patch_jne(bytes: &mut [u8], value: i64) -> Option<()> {
    let rel = i32::try_from(value - 6).ok()?;
    bytes.get_mut(2..6)?.copy_from_slice(&rel.to_le_bytes());
    Some(())
}

pub(crate) fn binary_writer() -> BinaryWriter {
    let mut patchers = crate::get_instruction_patchers();
    patchers.insert("jmp".to_string(), patch_jmp as InstructionPatcher);
    patchers.insert("jne".to_string(), patch_jne as InstructionPatcher);
    BinaryWriter::new(crate::get_instruction_encoders(), patchers)
}

/// Pre-RA: materialize a surviving `builtin.constant` into `mov $imm, rd`.
fn lower_constant(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    bits: u32,
) -> Result<bool, tir::PassError> {
    use tir::builtin::ConstantOp;

    let Some(constant) = op.as_op::<ConstantOp>() else {
        return Ok(false);
    };
    let value = tir_be_common::int_attr(constant.attributes(), "value").ok_or_else(|| {
        tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
    })?;
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&value) {
        return Err(tir::PassError::InvalidRuleSet(format!(
            "constant {value} does not fit a 32-bit mov immediate"
        )));
    }

    let class = class_name(bits);
    let dst = virt(constant.result().number(), class);
    let mov: Box<dyn Operation> = if bits == 32 {
        Box::new(
            MovRI32OpBuilder::new(context)
                .attr("rd", dst)
                .attr("imm", AttributeValue::Int(value))
                .build(),
        )
    } else {
        Box::new(
            MovRI64OpBuilder::new(context)
                .attr("rd", dst)
                .attr("imm", AttributeValue::Int(value))
                .build(),
        )
    };
    rewriter.replace_op(op, mov.as_ref())?;
    Ok(true)
}

/// Pre-RA: `vcond_br cond, t, f` becomes `test cond, cond` + `jne t` + `jmp f`.
fn lower_vcond_br(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    bits: u32,
) -> Result<bool, tir::PassError> {
    let Some(cond_br) = op.as_op::<VirtualCondBranchOp>() else {
        return Ok(false);
    };
    let operands = cond_br.operands();
    let (Some(&condition), 1) = (operands.first(), operands.len()) else {
        return Err(tir::PassError::InvalidRuleSet(
            "block arguments on conditional branch edges are not supported by codegen yet"
                .to_string(),
        ));
    };
    let true_dest = block_attr(&cond_br, "true_dest")?;
    let false_dest = block_attr(&cond_br, "false_dest")?;

    let class = class_name(bits);
    let cond = virt(condition.number(), class);
    let test: Box<dyn Operation> = if bits == 32 {
        Box::new(
            Test32OpBuilder::new(context)
                .attr("rs1", cond.clone())
                .attr("rs2", cond)
                .build(),
        )
    } else {
        Box::new(
            Test64OpBuilder::new(context)
                .attr("rs1", cond.clone())
                .attr("rs2", cond)
                .build(),
        )
    };
    rewriter.insert_op_before(op, test.as_ref())?;
    let jne = JneOpBuilder::new(context)
        .attr("imm", AttributeValue::Block(true_dest))
        .build();
    rewriter.insert_op_before(op, &jne)?;

    let fallthrough = VirtualBranchOpBuilder::new(context)
        .attr("dest", AttributeValue::Block(false_dest))
        .build();
    rewriter.replace_op(op, &fallthrough)?;
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

fn string_attr(op: &dyn tir::Operation, name: &str) -> Result<String, tir::PassError> {
    op.attributes()
        .iter()
        .find_map(|attr| match (&attr.value, attr.name == name) {
            (AttributeValue::Str(s), true) => Some(s.clone()),
            _ => None,
        })
        .ok_or_else(|| tir::PassError::InvalidRuleSet(format!("call is missing its '{name}'")))
}

/// Post-RA: finalize the virtual control-flow and call ops to real instructions.
fn finalize_virtual_ops_impl(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    if op.as_op::<VirtualReturnOp>().is_some() {
        let ret = RetOpBuilder::new(context).build();
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
        let jump = JmpOpBuilder::new(context)
            .attr("imm", AttributeValue::Block(dest))
            .build();
        rewriter.replace_op(op, &jump)?;
        return Ok(true);
    }

    // `vcall callee` becomes `call callee`; the symbol survives as a fixup and is
    // emitted as a PC-relative relocation.
    if let Some(call) = op.as_op::<VirtualCallOp>() {
        let callee = string_attr(&call, "callee")?;
        let call_rel = crate::CallRelOpBuilder::new(context)
            .attr("imm", AttributeValue::Str(callee))
            .build();
        rewriter.replace_op(op, &call_rel)?;
        return Ok(true);
    }

    Ok(false)
}

// Width-specific entry points selected by the target.

pub(crate) fn lower_constant_32(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_constant(c, op, r, 32)
}

pub(crate) fn lower_constant_64(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_constant(c, op, r, 64)
}

pub(crate) fn lower_vcond_br_32(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_vcond_br(c, op, r, 32)
}

pub(crate) fn lower_vcond_br_64(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_vcond_br(c, op, r, 64)
}

pub(crate) fn finalize_virtual_ops(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    finalize_virtual_ops_impl(c, op, r)
}
