//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

const MODEL_CHECK_SOURCES: &[(&str, &str)] = &[
    ("main.tmdl", include_str!("../defs/main.tmdl")),
    ("encoding.tmdl", include_str!("../defs/encoding.tmdl")),
    ("base.tmdl", include_str!("../defs/base.tmdl")),
    ("arith_ext.tmdl", include_str!("../defs/arith_ext.tmdl")),
    ("conditional.tmdl", include_str!("../defs/conditional.tmdl")),
    ("memory_ext.tmdl", include_str!("../defs/memory_ext.tmdl")),
    ("atomics.tmdl", include_str!("../defs/atomics.tmdl")),
    ("ordering.tmdl", include_str!("../defs/ordering.tmdl")),
    ("float.tmdl", include_str!("../defs/float.tmdl")),
    ("perf.tmdl", include_str!("../defs/perf.tmdl")),
];

pub use isa::{
    Feature, get_isel_rules, instruction_infos, register_info, register_views, register_widths,
};
pub use isa::{TargetConfig, X86_64Dialect};

mod isa {
    // Generated code: not everything is used by this asm-focused prototype.
    #![allow(dead_code, unused_variables, unused_mut, clippy::all)]

    use tir::Operation;
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::backend::{RegSlot, fresh_reg};
    use tir::backend::{VirtualBranchOp, VirtualCallOp, VirtualIndirectCallOp, VirtualReturnOp};
    use tir::helpers::{dialect, operation};

    include!(concat!(env!("OUT_DIR"), "/x86_64.rs"));

    dialect! {
        X86_64Dialect {
            name: "x86_64",
            operation_file: concat!(env!("OUT_DIR"), "/x86_64_ops.rs"),
            type_parsers: reg_class_type_parsers(),
        }
    }

    fn lower_func_and_return_to_asm_symbol(
        context: &tir::Context,
        op: &tir::OperationRef,
        rewriter: &mut tir::Rewriter,
    ) -> Result<bool, tir::PassError> {
        tir::backend::lower::lower_function_and_return(context, op, rewriter, |ty| {
            let ty = context.get_type_data(ty);
            Ok(
                if (ty.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<tir::builtin::FloatType>()
                    .is_some()
                {
                    RegClass::XMM.id()
                } else {
                    RegClass::GPR.id()
                },
            )
        })
    }

    /// Pre-RA: materialize a `constantf` that survived instruction selection
    /// into `movabs r64, bits` + `movq xmm, r64`.
    fn lower_float_constant(
        context: &tir::Context,
        op: &tir::OperationRef,
        rewriter: &mut tir::Rewriter,
    ) -> Result<bool, tir::PassError> {
        use tir::builtin::ConstantFOp;

        let Some(constant) = op.as_op::<ConstantFOp>() else {
            return Ok(false);
        };
        let Some(bits) = tir::backend::f64_constant_bits(context, &constant) else {
            return Ok(false);
        };
        let temp = fresh_reg(context, RegClass::GPR.id());
        let materialize = MovAbsOpBuilder::new(context)
            .result_values(vec![temp])
            .attr("imm", AttributeValue::Int(bits))
            .build();
        let move_bits = MovqXmmGprOpBuilder::new(context)
            .result_types(vec![tir::backend::RegClassType::new(
                context,
                RegClass::XMMzx.id(),
            )])
            .src(temp)
            .build();
        rewriter.insert_op_before(op, &materialize)?;
        rewriter.replace_op(op, &move_bits)?;
        Ok(true)
    }

    /// Emit the branch-if-nonzero fallback for a condition no branch rule
    /// fused: `test cond, 1` + `jne dest`. The condition is a width-1 value, so
    /// only bit 0 of its register is defined — a whole-register `test` would
    /// branch on undefined bits.
    fn emit_branch_nonzero(
        context: &tir::Context,
        condition: tir::ValueId,
        dest: tir::BlockId,
    ) -> Vec<Box<dyn Operation>> {
        vec![
            Box::new(
                TestImm32OpBuilder::new(context)
                    .dst(condition)
                    .attr("imm", AttributeValue::Int(1))
                    .build(),
            ),
            Box::new(
                JumpNotEqOpBuilder::new(context)
                    .attr("imm", AttributeValue::Block(dest))
                    .build(),
            ),
        ]
    }

    /// Pre-RA: materialize a `constant` that survived instruction selection
    /// (one no instruction folded as an immediate) into `mov rd, imm32`.
    fn lower_constant(
        context: &tir::Context,
        op: &tir::OperationRef,
        rewriter: &mut tir::Rewriter,
    ) -> Result<bool, tir::PassError> {
        use tir::builtin::ConstantOp;

        let Some(constant) = op.as_op::<ConstantOp>() else {
            return Ok(false);
        };
        let value = tir::backend::int_attr(&constant, "value").ok_or_else(|| {
            tir::PassError::InvalidRuleSet("constant op without an integer value".to_string())
        })?;
        let dst = vec![tir::backend::RegClassType::new(context, RegClass::GPR.id())];
        if i32::try_from(value).is_err() {
            let movabs = MovAbsOpBuilder::new(context)
                .result_types(dst)
                .attr("imm", AttributeValue::Int(value))
                .build();
            rewriter.replace_op(op, &movabs)?;
            return Ok(true);
        }

        let mov = MovImmOpBuilder::new(context)
            .result_types(dst)
            .attr("imm", AttributeValue::Int(value))
            .build();
        rewriter.replace_op(op, &mov)?;
        Ok(true)
    }

    /// Pre-RA: materialize a `sym_addr` symbol address as `lea rd, [rip + sym]`.
    /// The encoder leaves the disp32 as a fixup emitted with R_X86_64_PC32.
    fn lower_sym_addr(
        context: &tir::Context,
        op: &tir::OperationRef,
        rewriter: &mut tir::Rewriter,
    ) -> Result<bool, tir::PassError> {
        use tir::builtin::SymAddrOp;

        let Some(addr_of) = op.as_op::<SymAddrOp>() else {
            return Ok(false);
        };
        let lea = LeaRipOpBuilder::new(context)
            .result_types(vec![tir::backend::RegClassType::new(
                context,
                RegClass::GPR.id(),
            )])
            .attr("imm", AttributeValue::Str(addr_of.sym_name().into()))
            .build();
        rewriter.replace_op(op, &lea)?;
        Ok(true)
    }

    /// A register-to-register `mov dst, src`.
    fn mv(context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
        let builder = MovOpBuilder::new(context);
        let builder = tir::reg_use!(builder, src, src);
        Box::new(tir::reg_def!(builder, dst, dst).build())
    }

    fn abi_copy(context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
        let class = tir::backend::slot_class(context, dst).expect("ABI copies target a register");
        match class.name() {
            "GPR" => mv(context, dst, src),
            "XMM" => {
                let builder = MovsdOpBuilder::new(context);
                let builder = tir::reg_use!(builder, src, src);
                Box::new(tir::reg_def!(builder, dst, dst).build())
            }
            other => unreachable!("unknown x86-64 ABI register class {other}"),
        }
    }

    struct X86CallEmitter;

    impl tir::backend::call_lowering::CallEmitter for X86CallEmitter {
        fn copy(&self, context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
            abi_copy(context, dst, src)
        }

        fn stack_arg_store(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            value: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            offset: i64,
        ) -> Result<Box<dyn Operation>, tir::PassError> {
            let base = phys(abi.sp.0, abi.sp.1);
            let offset = AttributeValue::Int(offset);
            match class.name() {
                "GPR" => Ok(Box::new(
                    MovStoreDispOpBuilder::new(context)
                        .attr("base", base)
                        .attr("imm", offset)
                        .src(value)
                        .build(),
                )),
                "XMM" => Ok(Box::new(
                    MovsdStoreDispOpBuilder::new(context)
                        .attr("base", base)
                        .attr("imm", offset)
                        .src(value)
                        .build(),
                )),
                other => Err(tir::PassError::InvalidRuleSet(format!(
                    "x86-64 stack arguments for register class {other} are not supported"
                ))),
            }
        }

        fn call_prefix(
            &self,
            context: &tir::Context,
            _abi: &tir::backend::abi::AbiInfo,
            _outgoing_size: u32,
            vector_register_args: u8,
        ) -> Vec<Box<dyn Operation>> {
            vec![Box::new(
                MovImm32OpBuilder::new(context)
                    .attr("dst", phys(RegClass::GPR32.id(), 0))
                    .attr("imm", AttributeValue::Int(i64::from(vector_register_args)))
                    .build(),
            )]
        }

        fn call_suffix(
            &self,
            _context: &tir::Context,
            _abi: &tir::backend::abi::AbiInfo,
            _outgoing_size: u32,
        ) -> Vec<Box<dyn Operation>> {
            Vec::new()
        }
    }

    /// Post-RA: a memory operand whose allocated base is rsp/r12 (ModR/M rm=100)
    /// or rbp/r13 with mod=00 (rm=101) needs an escape the generic encoding
    /// omits — a SIB byte for the former, a mod=01 zero disp8 for the latter —
    /// or the byte stream desyncs. Rewrite each affected op to its `_sib`/`_rbp`
    /// variant now that the base is physical. The disp (mod=10) forms only need
    /// the SIB variant; rbp/r13 are already legal there.
    fn canonicalize_encodings(
        context: &tir::Context,
        op: &tir::OperationRef,
        rewriter: &mut tir::Rewriter,
    ) -> Result<bool, tir::PassError> {
        // Post-RA the register slots still hold values; where each landed is
        // read from the function's assignment.
        let register = |inner: &dyn Operation, name: &str| {
            tir::backend::op_slot_register(context, inner.handle(), name)
        };
        let base_index = |inner: &dyn Operation| register(inner, "base").map(|(_, index)| index);
        let reg_index = |inner: &dyn Operation, name: &str| register(inner, name).map(|(_, i)| i);
        // An immediate operand's integer value, or None for a symbol reference
        // (a relocation that cannot fold to the sign-extended imm8 form).
        fn imm_int(op: &dyn Operation, name: &str) -> Option<i64> {
            match op.attr(name)? {
                AttributeValue::Int(v) => Some(v),
                _ => None,
            }
        }
        let replace = |rewriter: &mut tir::Rewriter, new_op: Box<dyn Operation>| {
            rewriter.replace_op(op, new_op.as_ref()).map(|()| true)
        };
        // Every variant below is the same instruction in another encoding, so it
        // keeps the original's operands, results and immediates.
        macro_rules! reencode {
            ($Target:ty, $inner:expr) => {
                tir::backend::reencode_as::<$Target>(context, $inner.handle())
            };
        }

        macro_rules! escape {
            ($Op:ty, $Sib:ty, $Rbp:ty) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    let Some(idx) = base_index(&inner) else {
                        return Ok(false);
                    };
                    return match idx {
                        4 | 12 => replace(rewriter, reencode!($Sib, inner)),
                        5 | 13 => replace(rewriter, reencode!($Rbp, inner)),
                        _ => Ok(false),
                    };
                }
            };
            (sib $Op:ty, $Sib:ty) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    let Some(idx) = base_index(&inner) else {
                        return Ok(false);
                    };
                    if matches!(idx, 4 | 12) {
                        return replace(rewriter, reencode!($Sib, inner));
                    }
                    return Ok(false);
                }
            };
        }
        macro_rules! escape_norex {
            ($Op:ty, $Sib:ty, $Rbp:ty, $Norex:ty, $reg:literal, $limit:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    let Some(base) = base_index(&inner) else {
                        return Ok(false);
                    };
                    let replacement: Option<Box<dyn Operation>> = match base {
                        4 | 12 => Some(reencode!($Sib, inner)),
                        5 | 13 => Some(reencode!($Rbp, inner)),
                        _ if base < 8
                            && matches!(reg_index(&inner, $reg), Some(reg) if reg < $limit) =>
                        {
                            Some(reencode!($Norex, inner))
                        }
                        _ => None,
                    };
                    return match replacement {
                        Some(replacement) => replace(rewriter, replacement),
                        None => Ok(false),
                    };
                }
            };
        }

        escape!(MovLoadOp, MovLoadSibOp, MovLoadRbpOp);
        escape!(MovStoreOp, MovStoreSibOp, MovStoreRbpOp);
        escape!(Movzx8LoadOp, Movzx8LoadSibOp, Movzx8LoadRbpOp);
        escape!(Movzx16LoadOp, Movzx16LoadSibOp, Movzx16LoadRbpOp);
        escape!(Movsx8LoadOp, Movsx8LoadSibOp, Movsx8LoadRbpOp);
        escape!(Movsx16LoadOp, Movsx16LoadSibOp, Movsx16LoadRbpOp);
        escape!(MovsxdLoadOp, MovsxdLoadSibOp, MovsxdLoadRbpOp);
        escape_norex!(
            Mov32LoadOp,
            Mov32LoadSibOp,
            Mov32LoadRbpOp,
            Mov32LoadNorexOp,
            "dst",
            8
        );
        escape_norex!(
            Mov32StoreOp,
            Mov32StoreSibOp,
            Mov32StoreRbpOp,
            Mov32StoreNorexOp,
            "src",
            8
        );
        escape_norex!(
            Mov16StoreOp,
            Mov16StoreSibOp,
            Mov16StoreRbpOp,
            Mov16StoreNorexOp,
            "src",
            8
        );
        escape_norex!(
            Mov8StoreOp,
            Mov8StoreSibOp,
            Mov8StoreRbpOp,
            Mov8StoreNorexOp,
            "src",
            4
        );
        escape!(sib LeaBaseDisp32Op, LeaBaseDisp32SibOp);
        escape!(sib MovLoadDispOp, MovLoadDispSibOp);
        escape!(sib MovStoreDispOp, MovStoreDispSibOp);
        escape!(sib Mov32LoadDispOp, Mov32LoadDispSibOp);
        escape!(sib Mov32StoreDispOp, Mov32StoreDispSibOp);
        escape!(sib Mov16LoadDispOp, Mov16LoadDispSibOp);
        escape!(sib Mov16StoreDispOp, Mov16StoreDispSibOp);
        escape!(sib Mov8LoadDispOp, Mov8LoadDispSibOp);
        escape!(sib Mov8StoreDispOp, Mov8StoreDispSibOp);
        escape!(sib MovssLoadDispOp, MovssLoadDispSibOp);
        escape!(sib MovssStoreDispOp, MovssStoreDispSibOp);
        escape!(sib MovsdLoadDispOp, MovsdLoadDispSibOp);
        escape!(sib MovsdStoreDispOp, MovsdStoreDispSibOp);
        escape!(sib MovssLoadDispNorexOp, MovssLoadDispSibNorexOp);
        escape!(sib MovssStoreDispNorexOp, MovssStoreDispSibNorexOp);
        escape!(sib MovsdLoadDispNorexOp, MovsdLoadDispSibNorexOp);
        escape!(sib MovsdStoreDispNorexOp, MovsdStoreDispSibNorexOp);

        // REX-free canonicalization: drop the REX byte GNU as omits when every
        // register index is low (< 8, or < 4 for the 8-bit forms that must avoid
        // the spl/bpl/sil/dil encodings), and fold group-1 immediates that fit a
        // sign-extended i8 into the 0x83 short form. Each op maps to exactly one
        // behavior-free variant, so selection is unaffected.
        const LO: u16 = 8;
        const B: u16 = 4;

        // A boolean materialized by setcc defines one byte. Preserve that width
        // when a generic test consumes it, so stale upper register bits are ignored.
        if let Some(inner) = op.as_op::<TestOp>()
            && register(&inner, "dst").map(|(class, _)| class) == Some(RegClass::GPR8.id())
        {
            return replace(rewriter, reencode!(Test8Op, inner));
        }

        // Register/register: `op → op_norex` when both operands are low.
        macro_rules! rr_norex {
            ($Op:ty, $Norex:ty, $t:expr) => {
                rr_named_norex!($Op, $Norex, "dst", "src", $t)
            };
        }
        macro_rules! rr_named_norex {
            ($Op:ty, $Norex:ty, $dst:literal, $src:literal, $t:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match (reg_index(&inner, $dst), reg_index(&inner, $src)) {
                        (Some(d), Some(s)) if d < $t && s < $t => {
                            replace(rewriter, reencode!($Norex, inner))
                        }
                        _ => Ok(false),
                    };
                }
            };
        }
        macro_rules! mem_norex {
            ($Op:ty, $Norex:ty, $reg:literal) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match (reg_index(&inner, $reg), base_index(&inner)) {
                        (Some(r), Some(b)) if r < LO && b < LO => {
                            replace(rewriter, reencode!($Norex, inner))
                        }
                        _ => Ok(false),
                    };
                }
            };
        }
        // Single register (+ immediate): `op → op_norex` when the register is low.
        macro_rules! reg1_norex {
            ($Op:ty, $Norex:ty, $n:literal, $t:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match reg_index(&inner, $n) {
                        Some(d) if d < $t => replace(rewriter, reencode!($Norex, inner)),
                        _ => Ok(false),
                    };
                }
            };
        }
        macro_rules! ri_norex {
            ($Op:ty, $Norex:ty, $t:expr) => {
                reg1_norex!($Op, $Norex, "dst", $t)
            };
        }
        rr_norex!(Imul32Op, Imul32NorexOp, LO);
        rr_norex!(ImulImm32Op, ImulImm32NorexOp, LO);

        ri_norex!(ShlImm32Op, ShlImm32NorexOp, LO);
        ri_norex!(ShrImm32Op, ShrImm32NorexOp, LO);
        ri_norex!(SarImm32Op, SarImm32NorexOp, LO);
        ri_norex!(ShlImm16Op, ShlImm16NorexOp, LO);
        ri_norex!(ShrImm16Op, ShrImm16NorexOp, LO);
        ri_norex!(SarImm16Op, SarImm16NorexOp, LO);
        ri_norex!(ShlImm8Op, ShlImm8NorexOp, B);
        ri_norex!(ShrImm8Op, ShrImm8NorexOp, B);
        ri_norex!(SarImm8Op, SarImm8NorexOp, B);

        reg1_norex!(SetEqOp, SetEqNorexOp, "dst", B);
        reg1_norex!(SetParityOp, SetParityNorexOp, "dst", B);
        reg1_norex!(SetNoParityOp, SetNoParityNorexOp, "dst", B);
        reg1_norex!(SetNotEqOp, SetNotEqNorexOp, "dst", B);
        reg1_norex!(SetLessOp, SetLessNorexOp, "dst", B);
        reg1_norex!(SetGreaterEqOp, SetGreaterEqNorexOp, "dst", B);
        reg1_norex!(SetLessEqOp, SetLessEqNorexOp, "dst", B);
        reg1_norex!(SetGreaterOp, SetGreaterNorexOp, "dst", B);
        reg1_norex!(SetBelowOp, SetBelowNorexOp, "dst", B);
        reg1_norex!(SetAboveEqOp, SetAboveEqNorexOp, "dst", B);
        reg1_norex!(SetBelowEqOp, SetBelowEqNorexOp, "dst", B);
        reg1_norex!(SetAboveOp, SetAboveNorexOp, "dst", B);

        reg1_norex!(PushOp, PushNorexOp, "reg", LO);
        reg1_norex!(PopOp, PopNorexOp, "reg", LO);
        // The indirect jmp/call forms are not produced before this pass: `jmp
        // *reg` reaches its `_norex` form through the assembler, and the codegen
        // indirect call is materialized REX-free directly in `finalize_virtual_ops`
        // (it is created there, after this pass would have run).

        // cmp/test reg-reg, neg/not, by-cl shifts and rotate immediates: the
        // 32-bit width is the only one with a generic (so the only one reachable
        // here); the 16/8-bit `_norex` forms exist for the assembler only.
        reg1_norex!(Neg32Op, Neg32NorexOp, "dst", LO);
        reg1_norex!(Not32Op, Not32NorexOp, "dst", LO);
        reg1_norex!(SignedDivide32Op, SignedDivide32NorexOp, "dst", LO);
        reg1_norex!(UnsignedDivide32Op, UnsignedDivide32NorexOp, "dst", LO);
        reg1_norex!(ShlCl32Op, ShlCl32NorexOp, "dst", LO);
        reg1_norex!(ShrCl32Op, ShrCl32NorexOp, "dst", LO);
        reg1_norex!(SarCl32Op, SarCl32NorexOp, "dst", LO);
        ri_norex!(RolImm32Op, RolImm32NorexOp, LO);
        ri_norex!(RorImm32Op, RorImm32NorexOp, LO);

        // Low-xmm SSE: drop the empty REX when both xmm operands are xmm0..xmm7.
        rr_norex!(AddssOp, AddssNorexOp, LO);
        rr_norex!(SubssOp, SubssNorexOp, LO);
        rr_norex!(MulssOp, MulssNorexOp, LO);
        rr_norex!(DivssOp, DivssNorexOp, LO);
        rr_norex!(MovssOp, MovssNorexOp, LO);
        rr_norex!(AddsdOp, AddsdNorexOp, LO);
        rr_norex!(SubsdOp, SubsdNorexOp, LO);
        rr_norex!(MulsdOp, MulsdNorexOp, LO);
        rr_norex!(DivsdOp, DivsdNorexOp, LO);
        rr_norex!(MovsdOp, MovsdNorexOp, LO);
        rr_norex!(Cvtsi2ss32Op, Cvtsi2ss32NorexOp, LO);
        rr_norex!(Cvttss2si32Op, Cvttss2si32NorexOp, LO);
        rr_norex!(Cvtsi2sd32Op, Cvtsi2sd32NorexOp, LO);
        rr_norex!(Cvttsd2si32Op, Cvttsd2si32NorexOp, LO);
        rr_norex!(MovdXmmGpr32Op, MovdXmmGpr32NorexOp, LO);
        rr_norex!(MovdGpr32XmmOp, MovdGpr32XmmNorexOp, LO);
        rr_named_norex!(UcomissOp, UcomissNorexOp, "lhs", "rhs", LO);
        rr_named_norex!(UcomisdOp, UcomisdNorexOp, "lhs", "rhs", LO);
        mem_norex!(MovssLoadDispOp, MovssLoadDispNorexOp, "dst");
        mem_norex!(MovssStoreDispOp, MovssStoreDispNorexOp, "src");
        mem_norex!(MovssLoadDispSibOp, MovssLoadDispSibNorexOp, "dst");
        mem_norex!(MovssStoreDispSibOp, MovssStoreDispSibNorexOp, "src");
        mem_norex!(MovsdLoadDispOp, MovsdLoadDispNorexOp, "dst");
        mem_norex!(MovsdStoreDispOp, MovsdStoreDispNorexOp, "src");
        mem_norex!(MovsdLoadDispSibOp, MovsdLoadDispSibNorexOp, "dst");
        mem_norex!(MovsdStoreDispSibOp, MovsdStoreDispSibNorexOp, "src");

        Ok(false)
    }

    /// Post-RA: `vret` becomes `ret`; `vbr` becomes `jmp dest`.
    fn finalize_virtual_ops(
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
            let dest = match br.attr("dest") {
                Some(AttributeValue::Block(block)) => Some(block),
                _ => None,
            }
            .ok_or_else(|| {
                tir::PassError::InvalidRuleSet("branch is missing its 'dest' target".to_string())
            })?;
            let jump = JmpOpBuilder::new(context)
                .attr("imm", AttributeValue::Block(dest))
                .build();
            rewriter.replace_op(op, &jump)?;
            return Ok(true);
        }

        // `vcall callee` becomes `call callee`: the symbol operand survives into
        // the encoder as a fixup, emitted as an R_X86_64_PLT32 relocation since the
        // callee's address is unknown until link time.
        if let Some(call) = op.as_op::<VirtualCallOp>() {
            let callee = match call.attr("callee") {
                Some(AttributeValue::Str(s)) => Some(s.clone()),
                _ => None,
            }
            .ok_or_else(|| {
                tir::PassError::InvalidRuleSet("vcall is missing its 'callee'".to_string())
            })?;
            let real = CallOpBuilder::new(context)
                .attr("imm", AttributeValue::Str(callee))
                .build();
            tir::backend::forward_state(context, op.op(), &real);
            rewriter.replace_op(op, &real)?;
            return Ok(true);
        }

        // `vcall_indirect` becomes `call *target`; the target register is the one
        // the allocator gave the call's callee operand.
        if let Some(call) = op.as_op::<VirtualIndirectCallOp>() {
            let target = call.operands().first().copied().ok_or_else(|| {
                tir::PassError::InvalidRuleSet("indirect call has no callee register".to_string())
            })?;
            // `call *reg` needs no REX when the target is rax..rdi; emit the
            // REX-free form directly (this op is created after
            // `canonicalize_encodings` would run).
            let low = matches!(
                tir::backend::assigned_register(context, op.op(), target),
                Some((_, index)) if index < 8
            );
            let real: Box<dyn Operation> = if low {
                Box::new(
                    CallIndirectNorexOpBuilder::new(context)
                        .target(target)
                        .build(),
                )
            } else {
                Box::new(CallIndirectOpBuilder::new(context).target(target).build())
            };
            tir::backend::forward_state(context, op.op(), real.as_ref());
            rewriter.replace_op(op, real.as_ref())?;
            return Ok(true);
        }

        Ok(false)
    }

    /// The x86-64 stack pointer (`rsp`, GPR index 4).
    fn phys(class: tir::backend::regalloc::RegClassId, index: u16) -> AttributeValue {
        AttributeValue::Register(RegisterAttr::Physical { class, index })
    }

    /// The move family a register class is copied and spilled with. A class is a
    /// view over a register file, so the family follows from that view — the file
    /// it draws from, the width of the view and where the view starts — and never
    /// from the class name: `GPR32` and the REX-free `GPR32low` are the same
    /// 32-bit view of the GPR file and move alike.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum MoveKind {
        Gpr64,
        Gpr32,
        Gpr16,
        Gpr8,
        Gpr8High,
        Xmm64,
        Xmm32,
    }

    /// Register allocation target. Frame adjustment is `add rsp, ±size`; GPR
    /// spills use the displacement-based `mov` memory forms.
    struct X86RegAlloc {
        /// Architectural width per register class under the enabled features
        /// (`GPR` is `XLEN` wide, so this is not a compile-time constant).
        widths: Vec<(&'static str, u32)>,
    }

    impl X86RegAlloc {
        fn new(features: &[Feature]) -> Self {
            Self {
                widths: register_widths(features),
            }
        }

        fn move_kind(&self, class: tir::backend::regalloc::RegClassId) -> Option<MoveKind> {
            let width = self
                .widths
                .iter()
                .find(|(name, _)| *name == class.name())
                .map(|(_, width)| *width)?;
            match (class.file(), width, class.view.bit_offset) {
                ("GPR", 64, 0) => Some(MoveKind::Gpr64),
                ("GPR", 32, 0) => Some(MoveKind::Gpr32),
                ("GPR", 16, 0) => Some(MoveKind::Gpr16),
                ("GPR", 8, 0) => Some(MoveKind::Gpr8),
                ("GPR", 8, 8) => Some(MoveKind::Gpr8High),
                ("XMM128", 64, 0) => Some(MoveKind::Xmm64),
                ("XMM128", 32, 0) => Some(MoveKind::Xmm32),
                _ => None,
            }
        }
    }

    impl tir::backend::regalloc::TargetRegAlloc for X86RegAlloc {
        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            register_info()
        }

        fn emit_spill_store(
            &self,
            context: &tir::Context,
            value: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Box<dyn Operation> {
            macro_rules! store {
                ($Builder:ident) => {
                    Box::new(
                        $Builder::new(context)
                            .attr("base", phys(frame.0, frame.1))
                            .attr("imm", AttributeValue::Int(offset))
                            .src(value)
                            .build(),
                    )
                };
            }
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => store!(MovStoreDispOpBuilder),
                Some(MoveKind::Gpr32) => store!(Mov32StoreDispOpBuilder),
                Some(MoveKind::Gpr16) => store!(Mov16StoreDispOpBuilder),
                Some(MoveKind::Gpr8) => store!(Mov8StoreDispOpBuilder),
                Some(MoveKind::Xmm64) => store!(MovsdStoreDispOpBuilder),
                Some(MoveKind::Xmm32) => store!(MovssStoreDispOpBuilder),
                _ => unimplemented!("x86-64 spilling for {} is not implemented", class.name()),
            }
        }

        fn emit_spill_reload(
            &self,
            context: &tir::Context,
            value: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Box<dyn Operation> {
            macro_rules! load {
                ($Builder:ident) => {
                    Box::new(
                        $Builder::new(context)
                            .result_values(vec![value])
                            .attr("base", phys(frame.0, frame.1))
                            .attr("imm", AttributeValue::Int(offset))
                            .build(),
                    )
                };
            }
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => load!(MovLoadDispOpBuilder),
                Some(MoveKind::Gpr32) => load!(Mov32LoadDispOpBuilder),
                Some(MoveKind::Gpr16) => load!(Mov16LoadDispOpBuilder),
                Some(MoveKind::Gpr8) => load!(Mov8LoadDispOpBuilder),
                Some(MoveKind::Xmm64) => load!(MovsdLoadDispOpBuilder),
                Some(MoveKind::Xmm32) => load!(MovssLoadDispOpBuilder),
                _ => unimplemented!("x86-64 spilling for {} is not implemented", class.name()),
            }
        }

        fn emit_copy(
            &self,
            context: &tir::Context,
            class: tir::backend::regalloc::RegClassId,
            dst: RegSlot,
            src: RegSlot,
        ) -> Box<dyn Operation> {
            macro_rules! move_op {
                ($Builder:ident) => {{
                    let builder = $Builder::new(context);
                    let builder = tir::reg_use!(builder, src, src);
                    Box::new(tir::reg_def!(builder, dst, dst).build())
                }};
            }
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => move_op!(MovOpBuilder),
                Some(MoveKind::Gpr32) => move_op!(Mov32OpBuilder),
                Some(MoveKind::Gpr16) => move_op!(Mov16OpBuilder),
                Some(MoveKind::Gpr8) => move_op!(Mov8OpBuilder),
                Some(MoveKind::Gpr8High) => move_op!(Mov8HOpBuilder),
                Some(MoveKind::Xmm64) => move_op!(MovsdOpBuilder),
                Some(MoveKind::Xmm32) => move_op!(MovssOpBuilder),
                None => unreachable!("unknown x86-64 register class {}", class.name()),
            }
        }

        fn emit_prologue(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            size: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            let mut ops: Vec<Box<dyn Operation>> = Vec::new();
            for ((class, index), _) in saves {
                ops.push(Box::new(
                    PushOpBuilder::new(context)
                        .attr("reg", phys(*class, *index))
                        .build(),
                ));
            }
            if size > 0 {
                ops.push(adjust_rsp(context, abi, -(size as i64)));
            }
            ops
        }

        fn emit_epilogue(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            size: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            let mut ops: Vec<Box<dyn Operation>> = Vec::new();
            if size > 0 {
                ops.push(adjust_rsp(context, abi, size as i64));
            }
            for ((class, index), _) in saves.iter().rev() {
                ops.push(Box::new(
                    PopOpBuilder::new(context)
                        .attr("reg", phys(*class, *index))
                        .build(),
                ));
            }
            ops
        }

        fn emit_frame_address(
            &self,
            context: &tir::Context,
            dst: tir::ValueId,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Result<Vec<Box<dyn Operation>>, tir::PassError> {
            if self.move_kind(class) != Some(MoveKind::Gpr64) {
                return Err(tir::PassError::InvalidRuleSet(format!(
                    "x86-64 stack allocation addresses for register class {} are not supported",
                    class.name()
                )));
            }
            // `lea`, not `mov` plus `add`: a frame address is materialized
            // wherever the value is read, and `add` would leave the flags of
            // whatever compare stands there destroyed. `lea` computes the same
            // address in one instruction and writes no flags.
            if offset == 0 {
                return Ok(vec![mv(
                    context,
                    RegSlot::Value(dst),
                    RegSlot::Phys((frame.0, frame.1)),
                )]);
            }
            Ok(vec![Box::new(
                LeaBaseDisp32OpBuilder::new(context)
                    .result_values(vec![dst])
                    .attr("base", phys(frame.0, frame.1))
                    .attr("imm", AttributeValue::Int(offset))
                    .build(),
            )])
        }
    }

    fn adjust_rsp(
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        amount: i64,
    ) -> Box<dyn Operation> {
        Box::new(
            AddImmOpBuilder::new(context)
                .attr("dst", phys(abi.sp.0, abi.sp.1))
                .attr("dst_tied", phys(abi.sp.0, abi.sp.1))
                .attr("imm", AttributeValue::Int(amount))
                .build(),
        )
    }

    // R_X86_64_PC32 = 2, R_X86_64_PLT32 = 4. Both scatter `S + A - P` into a
    // 4-byte pc-relative field; the addend is -4 because `P` addresses the field
    // start while the displacement is measured from the instruction's end.
    const R_X86_64_PC32: u32 = 2;
    const R_X86_64_PLT32: u32 = 4;
    const R_X86_64_64: u32 = 1;

    /// The mnemonic an op name encodes. The base-ISA (`_legacy`) forms of the
    /// pc-relative branches share their 64-bit counterpart's encoding, so they
    /// relocate and measure their displacement identically.
    fn branch_mnemonic(op: &str) -> &str {
        op.strip_suffix("_legacy").unwrap_or(op)
    }

    fn object_format() -> tir::backend::binary::ObjectFormatInfo {
        use tir::backend::binary::{EM_X86_64, ElfClass, ObjectFormatInfo, RelocKind};
        ObjectFormatInfo {
            elf_machine: EM_X86_64,
            elf_class: ElfClass::Elf64,
            elf_flags: 0,
            absolute_reloc: |width| (width == 8).then_some(R_X86_64_64),
            reloc_for: |op| match branch_mnemonic(op) {
                // `call rel32`: the disp32 follows the 1-byte opcode.
                "call" => Some(RelocKind {
                    r_type: R_X86_64_PLT32,
                    addend: -4,
                    field_offset: 1,
                }),
                // `lea r64, [rip + disp32]`: the disp32 follows REX, opcode, ModR/M.
                "lea" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 3,
                }),
                // `jmp rel32` (E9 + disp32): the disp32 follows the 1-byte opcode.
                "jmp" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 1,
                }),
                // `jcc rel32` (0F 8x + disp32): the disp32 follows the 2-byte opcode.
                "je" | "jne" | "jl" | "jge" | "jb" | "jae" | "jle" | "jg" | "jbe" | "ja" | "js"
                | "jns" | "jo" | "jno" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 2,
                }),
                _ => None,
            },
            pc_rel_scale: |_| 0,
            // rel32 displacements are measured from the end of the instruction
            // (RIP points past the branch when the displacement applies).
            pc_rel_from_end: |op| {
                matches!(
                    branch_mnemonic(op),
                    "jmp"
                        | "je"
                        | "jne"
                        | "jl"
                        | "jge"
                        | "jb"
                        | "jae"
                        | "jle"
                        | "jg"
                        | "jbe"
                        | "ja"
                        | "js"
                        | "jns"
                        | "jo"
                        | "jno"
                        | "call"
                )
            },
        }
    }

    /// Parsed x86-64 target selection.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TargetConfig {
        features: Vec<Feature>,
        machine: Option<String>,
    }

    impl TargetConfig {
        /// Parse an x86-64 architecture name.
        pub fn parse(march: &str, mcpu: Option<&str>, mattr: Option<&str>) -> Result<Self, String> {
            match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "x86_64" | "amd64" | "x64" => {}
                other => return Err(format!("unknown x86-64 architecture '{other}'")),
            }
            let mut features = vec![Feature::X86, Feature::X86_64, Feature::SSE, Feature::SSE2];
            if let Some(mattr) = mattr {
                apply_mattr(&mut features, mattr)?;
            }
            validate_features(&features)?;
            if !features.contains(&Feature::X86_64) {
                return Err("--mattr must not disable the base ISA 'X86_64'".to_string());
            }
            let machine = match mcpu {
                None => None,
                Some(mcpu) => resolve_machine(mcpu, &features)?,
            };
            Ok(Self { features, machine })
        }

        /// The enabled ISA set.
        pub fn features(&self) -> &[Feature] {
            &self.features
        }
    }

    /// Resolve `--mcpu` to the machine model the compiler and the instrument
    /// both schedule against. `generic` selects no machine.
    fn resolve_machine(mcpu: &str, features: &[Feature]) -> Result<Option<String>, String> {
        let name = mcpu.trim().to_ascii_lowercase();
        if name == "generic" {
            return Ok(None);
        }
        if machine_model(&name, features).is_some() {
            return Ok(Some(name));
        }
        Err(format!(
            "unknown x86-64 cpu '{mcpu}' (expected 'generic' or one of: {})",
            machines(Feature::ALL).join(", ")
        ))
    }

    fn apply_mattr(features: &mut Vec<Feature>, mattr: &str) -> Result<(), String> {
        for item in mattr.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (add, name) = if let Some(name) = item.strip_prefix('+') {
                (true, name)
            } else if let Some(name) = item.strip_prefix('-') {
                (false, name)
            } else {
                return Err(format!(
                    "invalid --mattr entry '{item}' (expected '+feature' or '-feature')"
                ));
            };
            let feature = Feature::from_name(&name.to_ascii_lowercase().replace('-', "_"))
                .ok_or_else(|| format!("unknown x86-64 feature '{name}' in --mattr"))?;
            if add && !features.contains(&feature) {
                features.push(feature);
            } else if !add {
                features.retain(|f| *f != feature);
            }
        }
        Ok(())
    }

    struct X86Target {
        config: TargetConfig,
        selected_abi: &'static tir::backend::abi::AbiInfo,
    }

    impl tir::backend::TargetMachine for X86Target {
        fn name(&self) -> &'static str {
            "x86_64"
        }

        fn model_check_target(&self) -> Option<tir::backend::ModelCheckTarget> {
            Some(tir::backend::ModelCheckTarget {
                isa: "X86_64",
                features: self.config.features.iter().map(Feature::name).collect(),
                sources: super::MODEL_CHECK_SOURCES,
            })
        }

        fn register_dialects(&self, context: &tir::Context) {
            context.register_dialect::<tir::backend::AsmDialect>();
            context.register_dialect::<X86_64Dialect>();
            context.register_reg_classes(register_info().classes);
        }

        fn data_layout(&self) -> Option<tir::attributes::AttributeValue> {
            Some(tir::data_layout_spec(
                tir::Endianness::Little,
                self.abi().stack.align * 8,
                &[
                    ("i1", 8, 8),
                    ("i8", 8, 8),
                    ("i16", 16, 16),
                    ("i32", 32, 32),
                    ("i64", 64, 64),
                    ("f32", 32, 32),
                    ("f64", 64, 64),
                    ("p", 64, 64),
                ],
            ))
        }

        fn target_env(&self) -> Option<tir::attributes::AttributeValue> {
            let features: Vec<String> = self
                .config
                .features
                .iter()
                .map(|feature| feature.name().to_ascii_lowercase())
                .collect();
            Some(tir::target_env_spec(self.name(), &features))
        }

        fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
            tir::backend::isel::InstructionSelectPass::new(get_isel_rules(
                context,
                self.config.features(),
            ))
            .with_axioms(include_str!("isel.axioms"))
            .with_branch_emitters(tir::backend::isel::BranchEmitters {
                uncond: tir::backend::emit_uncond_branch,
                cond_nonzero: emit_branch_nonzero,
            })
            .with_op_lowering(lower_func_and_return_to_asm_symbol)
            .with_call_lowering(self.abi(), Box::new(X86CallEmitter))
            .with_data_layout(self.data_layout())
        }

        fn regalloc_target(&self) -> Box<dyn tir::backend::regalloc::TargetRegAlloc> {
            Box::new(X86RegAlloc::new(self.config.features()))
        }

        fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![lower_float_constant, lower_constant, lower_sym_addr]
        }

        fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![canonicalize_encodings, finalize_virtual_ops]
        }

        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            register_info()
        }

        fn abis(&self) -> &'static [tir::backend::abi::AbiInfo] {
            x86_64_abis()
        }

        fn abi(&self) -> &'static tir::backend::abi::AbiInfo {
            self.selected_abi
        }

        fn asm_parser(&self, _context: &tir::Context) -> tir::backend::AsmParser {
            let (parsers, disabled) = get_instruction_parsers(self.config.features());
            tir::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
        }

        fn machine_model(&self, name: &str) -> Option<tir::backend::sched::MachineModel> {
            machine_model(name, self.config.features())
        }

        fn default_machine(&self) -> Option<&str> {
            self.config.machine.as_deref()
        }

        fn machines(&self) -> Vec<&'static str> {
            machines(self.config.features())
        }

        fn isa_params(&self) -> Vec<(&'static str, i64)> {
            isa_params(self.config.features())
        }

        fn register_widths(&self) -> Vec<(&'static str, u32)> {
            register_widths(self.config.features())
        }

        fn register_views(&self) -> Vec<(&'static str, tir::backend::regalloc::RegisterView)> {
            register_views(self.config.features())
        }

        fn register_name(&self, class: &str, index: u16, prefer_abi: bool) -> Option<String> {
            register_name(class, index, prefer_abi)
        }

        fn object_format(&self) -> Option<tir::backend::binary::ObjectFormatInfo> {
            Some(object_format())
        }
    }

    fn select_x86_64(
        march: &str,
        mcpu: Option<&str>,
        mattr: Option<&str>,
        mabi: Option<&str>,
    ) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
        match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "x86_64" | "amd64" | "x64" => {
                let config = TargetConfig::parse(march, mcpu, mattr)?;
                let selected_abi = match mabi {
                    Some(name) => x86_64_abi_by_name(name).ok_or_else(|| {
                        format!(
                            "unknown ABI '{name}' for x86_64 (available: {})",
                            x86_64_abis()
                                .iter()
                                .map(|abi| abi.name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
                    None => x86_64_default_abi(),
                };
                Ok(Some(Box::new(X86Target {
                    config,
                    selected_abi,
                })))
            }
            _ => Ok(None),
        }
    }

    tir::register_target!(select_x86_64, ["x86_64"]);

    fn x86_64_abis() -> &'static [tir::backend::abi::AbiInfo] {
        static ABIS: std::sync::OnceLock<Vec<tir::backend::abi::AbiInfo>> =
            std::sync::OnceLock::new();
        ABIS.get_or_init(|| {
            abis()
                .iter()
                .map(|abi| tir::backend::abi::AbiInfo {
                    indirect_result: Some((RegClass::GPR.id(), 7)),
                    argument_group_policy: Some(tir::backend::abi::ArgumentGroupPolicy {
                        register_limit: Some(2),
                        rollback: tir::backend::abi::GroupRollback::Preserve,
                    }),
                    ..*abi
                })
                .collect()
        })
    }

    fn x86_64_default_abi() -> &'static tir::backend::abi::AbiInfo {
        &x86_64_abis()[0]
    }

    fn x86_64_abi_by_name(name: &str) -> Option<&'static tir::backend::abi::AbiInfo> {
        x86_64_abis().iter().find(|abi| abi.name == name)
    }
}
