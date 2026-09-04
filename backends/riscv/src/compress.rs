//! Post-RA compression: rewrite base instructions into their 16-bit C forms
//! once registers and immediates are physical. Running after allocation keeps
//! the C extension's operand restrictions (tied destinations, the x8..x15
//! subset, small scaled immediates) out of the allocator's search space:
//! selection and allocation target the full ISA, and every instruction that
//! happens to satisfy a compressed form is narrowed here for free.
//!
//! PC-relative control flow (`jal`, conditional branches) is deliberately not
//! compressed: their targets are fixups patched at object emission, and the
//! binary writer has no branch relaxation, so a ±256B/±2KB compressed range
//! would turn a long branch into a hard error instead of a wider encoding.

use tir::Operation;
use tir::attributes::AttributeValue;
use tir::backend::{RegSlot, reg_slot};

use crate::{
    AddImmOp, AddImmWordOp, AddOp, AddWordOp, AndImmOp, AndOp, CAddImm4SpNOpBuilder,
    CAddImm16SpOpBuilder, CAddImmOpBuilder, CAddImmWordOpBuilder, CAddOpBuilder, CAddWordOpBuilder,
    CAndImmOpBuilder, CAndOpBuilder, CEnvBreakOpBuilder, CFLoadDoubleOpBuilder,
    CFLoadDoubleSpOpBuilder, CFLoadWordOpBuilder, CFLoadWordSpOpBuilder, CFStoreDoubleOpBuilder,
    CFStoreDoubleSpOpBuilder, CFStoreWordOpBuilder, CFStoreWordSpOpBuilder,
    CJumpAndLinkRegOpBuilder, CJumpRegOpBuilder, CLoadDoubleOpBuilder, CLoadDoubleSpOpBuilder,
    CLoadImmOpBuilder, CLoadUpperImmOpBuilder, CLoadWordOpBuilder, CLoadWordSpOpBuilder,
    CMoveOpBuilder, CNopOpBuilder, COrOpBuilder, CShiftLeftLogicalImmOpBuilder,
    CShiftRightArithmeticImmOpBuilder, CShiftRightLogicalImmOpBuilder, CStoreDoubleOpBuilder,
    CStoreDoubleSpOpBuilder, CStoreWordOpBuilder, CStoreWordSpOpBuilder, CSubOpBuilder,
    CSubWordOpBuilder, CXorOpBuilder, EnvBreakOp, FLoadDoubleOp, FLoadWordOp, FStoreDoubleOp,
    FStoreWordOp, JumpAndLinkRegOp, LoadDoubleWordOp, LoadUpperImmOp, LoadWordOp, OrOp,
    ShiftLeftLogicalImmOp, ShiftRightArithmeticImmOp, ShiftRightLogicalImmOp, StoreDoubleWordOp,
    StoreWordOp, SubOp, SubWordOp, XorOp, phys,
};
use tir::backend::VirtualReturnOp;

pub(crate) fn compress_rv32(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    compress(context, op, rewriter, 32)
}

pub(crate) fn compress_rv64(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    compress(context, op, rewriter, 64)
}

/// A register slot of an instruction, carried through to the compressed form
/// unchanged: the same value stays in the same register, so the function's
/// assignment still describes it.
fn slot(op: &dyn Operation, name: &str) -> Option<RegSlot> {
    reg_slot(op.handle(), name)
}

/// An integer immediate operand. Symbol/block operands (fixups) return None,
/// keeping their instruction uncompressed.
fn imm(op: &dyn Operation, name: &str) -> Option<i64> {
    match op.attr(name)? {
        AttributeValue::Int(value) => Some(value),
        _ => None,
    }
}

/// x8..x15 (f8..f15): the registers a 3-bit field reaches.
fn is_c_reg(index: u16) -> bool {
    (8..=15).contains(&index)
}

/// A load/store offset that fits a zero-extended immediate of `bits` bits
/// scaled by `scale`.
fn fits_uimm(value: i64, bits: u32, scale: i64) -> bool {
    value >= 0 && value % scale == 0 && value < (1 << bits)
}

fn fits_simm6(value: i64) -> bool {
    (-32..32).contains(&value)
}

/// The register an operand resolves to. Compression runs after allocation, so
/// every register slot names the register it was given.
fn reg(context: &tir::Context, inner: &dyn Operation, name: &str) -> Option<u16> {
    tir::backend::op_slot_register(context, inner.handle(), name).map(|(_, index)| index)
}

fn compress(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    xlen: u32,
) -> Result<bool, tir::PassError> {
    match compressed_form(context, op, xlen) {
        Some(new_op) => rewriter.replace_op(op, new_op.as_ref()).map(|()| true),
        None => Ok(false),
    }
}

/// The 16-bit form of `op`, or None when no compressed encoding covers it.
fn compressed_form(
    context: &tir::Context,
    op: &tir::OperationRef,
    xlen: u32,
) -> Option<Box<dyn Operation>> {
    // The return sequence compresses to `c.jr ra` (finalize would otherwise
    // expand it to the full `jalr x0, x1, 0`).
    if op.as_op::<VirtualReturnOp>().is_some() {
        let jr = CJumpRegOpBuilder::new(context)
            .attr("rs1", phys(&(crate::RegClass::GPR.id(), 1)))
            .build();
        return Some(Box::new(jr));
    }

    compress_add_imm(context, op)
        .or_else(|| compress_add_imm_word(context, op, xlen))
        .or_else(|| compress_load_upper_imm(context, op))
        .or_else(|| compress_add(context, op))
        .or_else(|| compress_ca_alu(context, op, xlen))
        .or_else(|| compress_imm_alu(context, op, xlen))
        .or_else(|| compress_mem(context, op, xlen))
        .or_else(|| compress_jump_and_link_reg(context, op))
        .or_else(|| {
            op.as_op::<EnvBreakOp>()
                .map(|_| Box::new(CEnvBreakOpBuilder::new(context).build()) as Box<dyn Operation>)
        })
}

fn compress_add_imm(context: &tir::Context, op: &tir::OperationRef) -> Option<Box<dyn Operation>> {
    let inner = op.as_op::<AddImmOp>()?;
    let (rd, rs1, value) = (
        reg(context, &inner, "rd")?,
        reg(context, &inner, "rs1")?,
        imm(&inner, "imm")?,
    );
    let rd_slot = slot(&inner, "rd").expect("checked above");
    if value == 0 {
        if rd == 0 && rs1 == 0 {
            return Some(Box::new(CNopOpBuilder::new(context).build()));
        }
        if rd != 0 && rs1 != 0 {
            let mv = CMoveOpBuilder::new(context);
            let mv = tir::reg_use!(mv, rs2, slot(&inner, "rs1").expect("checked above"));
            let mv = tir::reg_def!(mv, rd, rd_slot);
            return Some(Box::new(mv.build()));
        }
        return None;
    }
    if rd == 2 && rs1 == 2 && value % 16 == 0 && (-512..512).contains(&value) {
        // `c.addi16sp` names the stack pointer implicitly, in both
        // directions; the slots say which register that is.
        let sp = phys(&(crate::RegClass::GPR.id(), 2));
        let addi16sp = CAddImm16SpOpBuilder::new(context)
            .attr("x2", sp.clone())
            .attr("x2_def", sp)
            .attr("imm", AttributeValue::Int(value))
            .build();
        return Some(Box::new(addi16sp));
    }
    let imm = AttributeValue::Int(value);
    if rd == rs1 && rd != 0 && fits_simm6(value) {
        let addi = CAddImmOpBuilder::new(context).attr("imm", imm);
        // `c.addi` is two-address: it reads the destination it writes.
        let addi = tir::reg_use!(addi, rd_tied, slot(&inner, "rs1").expect("checked above"));
        return Some(Box::new(tir::reg_def!(addi, rd, rd_slot).build()));
    }
    if rs1 == 0 && rd != 0 && fits_simm6(value) {
        let li = CLoadImmOpBuilder::new(context).attr("imm", imm);
        return Some(Box::new(tir::reg_def!(li, rd, rd_slot).build()));
    }
    if rs1 == 2 && is_c_reg(rd) && value > 0 && fits_uimm(value, 10, 4) {
        let addi4spn = CAddImm4SpNOpBuilder::new(context).attr("imm", imm);
        return Some(Box::new(tir::reg_def!(addi4spn, rd, rd_slot).build()));
    }
    None
}

fn compress_add_imm_word(
    context: &tir::Context,
    op: &tir::OperationRef,
    xlen: u32,
) -> Option<Box<dyn Operation>> {
    if xlen != 64 {
        return None;
    }
    let inner = op.as_op::<AddImmWordOp>()?;
    let (rd, rs1, value) = (
        reg(context, &inner, "rd")?,
        reg(context, &inner, "rs1")?,
        imm(&inner, "imm")?,
    );
    if rd == rs1 && rd != 0 && fits_simm6(value) {
        let addiw = CAddImmWordOpBuilder::new(context).attr("imm", AttributeValue::Int(value));
        let addiw = tir::reg_use!(addiw, rd_tied, slot(&inner, "rs1").expect("checked above"));
        let addiw = tir::reg_def!(addiw, rd, slot(&inner, "rd").expect("checked above"));
        return Some(Box::new(addiw.build()));
    }
    None
}

fn compress_load_upper_imm(
    context: &tir::Context,
    op: &tir::OperationRef,
) -> Option<Box<dyn Operation>> {
    let inner = op.as_op::<LoadUpperImmOp>()?;
    let (rd, value) = (reg(context, &inner, "rd")?, imm(&inner, "imm")?);
    // The 20-bit operand may carry the value in unsigned form; the
    // compressed form holds its 6 low bits sign-extended.
    let value = ((value & 0xFFFFF) << 44) >> 44;
    if rd != 0 && rd != 2 && value != 0 && fits_simm6(value) {
        let lui = CLoadUpperImmOpBuilder::new(context).attr("imm", AttributeValue::Int(value));
        let lui = tir::reg_def!(lui, rd, slot(&inner, "rd").expect("checked above"));
        return Some(Box::new(lui.build()));
    }
    None
}

fn compress_add(context: &tir::Context, op: &tir::OperationRef) -> Option<Box<dyn Operation>> {
    let inner = op.as_op::<AddOp>()?;
    let (rd, rs1, rs2) = (
        reg(context, &inner, "rd")?,
        reg(context, &inner, "rs1")?,
        reg(context, &inner, "rs2")?,
    );
    if rd == 0 {
        return None;
    }
    let rd_slot = slot(&inner, "rd").expect("checked above");
    let src = if rd == rs1 && rs2 != 0 {
        Some("rs2")
    } else if rd == rs2 && rs1 != 0 {
        Some("rs1")
    } else {
        None
    };
    if let Some(src) = src {
        let add = CAddOpBuilder::new(context);
        // `c.add` is two-address: the destination is also the first source.
        let tied = if src == "rs2" { "rs1" } else { "rs2" };
        let add = tir::reg_use!(add, rd_tied, slot(&inner, tied).expect("checked above"));
        let add = tir::reg_use!(add, rs2, slot(&inner, src).expect("checked above"));
        return Some(Box::new(tir::reg_def!(add, rd, rd_slot).build()));
    }
    let src = if rs1 == 0 && rs2 != 0 {
        Some("rs2")
    } else if rs2 == 0 && rs1 != 0 {
        Some("rs1")
    } else {
        None
    };
    if let Some(src) = src {
        let mv = CMoveOpBuilder::new(context);
        let mv = tir::reg_use!(mv, rs2, slot(&inner, src).expect("checked above"));
        return Some(Box::new(tir::reg_def!(mv, rd, rd_slot).build()));
    }
    None
}

/// The CA-format two-address ALU ops over x8..x15. `sub`/`subw` are the
/// only non-commutative members.
macro_rules! ca_op {
    ($name:ident, $ty:ty, $builder:ident, $commutative:expr) => {
        fn $name(context: &tir::Context, op: &tir::OperationRef) -> Option<Box<dyn Operation>> {
            let inner = op.as_op::<$ty>()?;
            let (rd, rs1, rs2) = (
                reg(context, &inner, "rd")?,
                reg(context, &inner, "rs1")?,
                reg(context, &inner, "rs2")?,
            );
            let src = if rd == rs1 {
                Some("rs2")
            } else if $commutative && rd == rs2 {
                Some("rs1")
            } else {
                None
            };
            if is_c_reg(rd)
                && is_c_reg(rs1)
                && is_c_reg(rs2)
                && let Some(src) = src
            {
                let tied = if src == "rs2" { "rs1" } else { "rs2" };
                let new_op = $builder::new(context);
                let new_op =
                    tir::reg_use!(new_op, rd_tied, slot(&inner, tied).expect("checked above"));
                let new_op = tir::reg_use!(new_op, rs2, slot(&inner, src).expect("checked above"));
                let new_op = tir::reg_def!(new_op, rd, slot(&inner, "rd").expect("checked above"));
                return Some(Box::new(new_op.build()));
            }
            None
        }
    };
}
ca_op!(compress_sub, SubOp, CSubOpBuilder, false);
ca_op!(compress_xor, XorOp, CXorOpBuilder, true);
ca_op!(compress_or, OrOp, COrOpBuilder, true);
ca_op!(compress_and, AndOp, CAndOpBuilder, true);
ca_op!(compress_sub_word, SubWordOp, CSubWordOpBuilder, false);
ca_op!(compress_add_word, AddWordOp, CAddWordOpBuilder, true);

fn compress_ca_alu(
    context: &tir::Context,
    op: &tir::OperationRef,
    xlen: u32,
) -> Option<Box<dyn Operation>> {
    let rv64 = xlen == 64;
    compress_sub(context, op)
        .or_else(|| compress_xor(context, op))
        .or_else(|| compress_or(context, op))
        .or_else(|| compress_and(context, op))
        .or_else(|| rv64.then(|| compress_sub_word(context, op)).flatten())
        .or_else(|| rv64.then(|| compress_add_word(context, op)).flatten())
}

/// Shift/and immediates.
macro_rules! imm_alu {
    ($name:ident, $ty:ty, $builder:ident, $rd_ok:expr, $imm_ok:expr) => {
        fn $name(
            context: &tir::Context,
            op: &tir::OperationRef,
            xlen: u32,
        ) -> Option<Box<dyn Operation>> {
            let inner = op.as_op::<$ty>()?;
            let (rd, rs1, value) = (
                reg(context, &inner, "rd")?,
                reg(context, &inner, "rs1")?,
                imm(&inner, "imm")?,
            );
            #[allow(clippy::redundant_closure_call)]
            if rd == rs1 && ($rd_ok)(rd) && ($imm_ok)(value, xlen) {
                let new_op = $builder::new(context).attr("imm", AttributeValue::Int(value));
                let new_op =
                    tir::reg_use!(new_op, rd_tied, slot(&inner, "rs1").expect("checked above"));
                let new_op = tir::reg_def!(new_op, rd, slot(&inner, "rd").expect("checked above"));
                return Some(Box::new(new_op.build()));
            }
            None
        }
    };
}
fn shamt_ok(value: i64, xlen: u32) -> bool {
    value > 0 && value < xlen as i64
}
imm_alu!(
    compress_slli,
    ShiftLeftLogicalImmOp,
    CShiftLeftLogicalImmOpBuilder,
    |rd| rd != 0,
    shamt_ok
);
imm_alu!(
    compress_srli,
    ShiftRightLogicalImmOp,
    CShiftRightLogicalImmOpBuilder,
    is_c_reg,
    shamt_ok
);
imm_alu!(
    compress_srai,
    ShiftRightArithmeticImmOp,
    CShiftRightArithmeticImmOpBuilder,
    is_c_reg,
    shamt_ok
);
imm_alu!(
    compress_andi,
    AndImmOp,
    CAndImmOpBuilder,
    is_c_reg,
    |value, _| fits_simm6(value)
);

fn compress_imm_alu(
    context: &tir::Context,
    op: &tir::OperationRef,
    xlen: u32,
) -> Option<Box<dyn Operation>> {
    compress_slli(context, op, xlen)
        .or_else(|| compress_srli(context, op, xlen))
        .or_else(|| compress_srai(context, op, xlen))
        .or_else(|| compress_andi(context, op, xlen))
}

/// Loads and stores: sp-relative forms take the full register set and a
/// wider offset; the general forms need both registers in x8..x15.
macro_rules! mem_op {
    ($name:ident, $ty:ty, $dir:ident, $data:ident, $scale:literal, $sp_bits:literal,
     $c_bits:literal, $sp_builder:ident, $c_builder:ident, $data_ok:expr, $sp_data_ok:expr) => {
        fn $name(context: &tir::Context, op: &tir::OperationRef) -> Option<Box<dyn Operation>> {
            let inner = op.as_op::<$ty>()?;
            let (data, rs1, value) = (
                reg(context, &inner, stringify!($data))?,
                reg(context, &inner, "rs1")?,
                imm(&inner, "imm")?,
            );
            let data_slot = slot(&inner, stringify!($data)).expect("checked above");
            #[allow(clippy::redundant_closure_call)]
            if rs1 == 2 && ($sp_data_ok)(data) && fits_uimm(value, $sp_bits, $scale) {
                // The sp-relative forms name the stack pointer implicitly;
                // the slot says which register that is.
                let new_op = $sp_builder::new(context)
                    .attr("x2", phys(&(crate::RegClass::GPR.id(), 2)))
                    .attr("imm", AttributeValue::Int(value));
                let new_op = tir::$dir!(new_op, $data, data_slot);
                return Some(Box::new(new_op.build()));
            }
            #[allow(clippy::redundant_closure_call)]
            if ($data_ok)(data) && is_c_reg(rs1) && fits_uimm(value, $c_bits, $scale) {
                let new_op = $c_builder::new(context).attr("imm", AttributeValue::Int(value));
                let new_op =
                    tir::reg_use!(new_op, rs1, slot(&inner, "rs1").expect("checked above"));
                let new_op = tir::$dir!(new_op, $data, data_slot);
                return Some(Box::new(new_op.build()));
            }
            None
        }
    };
}
fn any_reg(_: u16) -> bool {
    true
}
fn not_zero(index: u16) -> bool {
    index != 0
}
mem_op!(
    compress_load_word,
    LoadWordOp,
    reg_def,
    rd,
    4,
    8,
    7,
    CLoadWordSpOpBuilder,
    CLoadWordOpBuilder,
    is_c_reg,
    not_zero
);
mem_op!(
    compress_store_word,
    StoreWordOp,
    reg_use,
    rs2,
    4,
    8,
    7,
    CStoreWordSpOpBuilder,
    CStoreWordOpBuilder,
    is_c_reg,
    any_reg
);
mem_op!(
    compress_load_double,
    LoadDoubleWordOp,
    reg_def,
    rd,
    8,
    9,
    8,
    CLoadDoubleSpOpBuilder,
    CLoadDoubleOpBuilder,
    is_c_reg,
    not_zero
);
mem_op!(
    compress_store_double,
    StoreDoubleWordOp,
    reg_use,
    rs2,
    8,
    9,
    8,
    CStoreDoubleSpOpBuilder,
    CStoreDoubleOpBuilder,
    is_c_reg,
    any_reg
);
// Float loads/stores: an fld/fsw op in the stream implies its base
// extension (D/F) is enabled, so C's presence completes the Zcd/Zcf
// conjunction. The word forms are rv32-only.
mem_op!(
    compress_fload_double,
    FLoadDoubleOp,
    reg_def,
    fd,
    8,
    9,
    8,
    CFLoadDoubleSpOpBuilder,
    CFLoadDoubleOpBuilder,
    is_c_reg,
    any_reg
);
mem_op!(
    compress_fstore_double,
    FStoreDoubleOp,
    reg_use,
    fs2,
    8,
    9,
    8,
    CFStoreDoubleSpOpBuilder,
    CFStoreDoubleOpBuilder,
    is_c_reg,
    any_reg
);
mem_op!(
    compress_fload_word,
    FLoadWordOp,
    reg_def,
    fd,
    4,
    8,
    7,
    CFLoadWordSpOpBuilder,
    CFLoadWordOpBuilder,
    is_c_reg,
    any_reg
);
mem_op!(
    compress_fstore_word,
    FStoreWordOp,
    reg_use,
    fs2,
    4,
    8,
    7,
    CFStoreWordSpOpBuilder,
    CFStoreWordOpBuilder,
    is_c_reg,
    any_reg
);

fn compress_mem(
    context: &tir::Context,
    op: &tir::OperationRef,
    xlen: u32,
) -> Option<Box<dyn Operation>> {
    let rv64 = xlen == 64;
    let rv32 = xlen == 32;
    compress_load_word(context, op)
        .or_else(|| compress_store_word(context, op))
        .or_else(|| rv64.then(|| compress_load_double(context, op)).flatten())
        .or_else(|| rv64.then(|| compress_store_double(context, op)).flatten())
        .or_else(|| compress_fload_double(context, op))
        .or_else(|| compress_fstore_double(context, op))
        .or_else(|| rv32.then(|| compress_fload_word(context, op)).flatten())
        .or_else(|| rv32.then(|| compress_fstore_word(context, op)).flatten())
}

fn compress_jump_and_link_reg(
    context: &tir::Context,
    op: &tir::OperationRef,
) -> Option<Box<dyn Operation>> {
    let inner = op.as_op::<JumpAndLinkRegOp>()?;
    let (rd, rs1, value) = (
        reg(context, &inner, "rd")?,
        reg(context, &inner, "rs1")?,
        imm(&inner, "imm")?,
    );
    if value != 0 || rs1 == 0 {
        return None;
    }
    let rs1_slot = slot(&inner, "rs1").expect("checked above");
    if rd == 0 {
        let jr = CJumpRegOpBuilder::new(context);
        return Some(Box::new(tir::reg_use!(jr, rs1, rs1_slot).build()));
    }
    if rd == 1 {
        let jalr = CJumpAndLinkRegOpBuilder::new(context);
        return Some(Box::new(tir::reg_use!(jalr, rs1, rs1_slot).build()));
    }
    None
}
