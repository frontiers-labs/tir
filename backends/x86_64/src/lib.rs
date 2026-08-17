//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

const MODEL_CHECK_SOURCES: &[(&str, &str)] = &[
    ("main.tmdl", include_str!("../defs/main.tmdl")),
    ("base.tmdl", include_str!("../defs/base.tmdl")),
    ("arith_ext.tmdl", include_str!("../defs/arith_ext.tmdl")),
    ("conditional.tmdl", include_str!("../defs/conditional.tmdl")),
    ("memory_ext.tmdl", include_str!("../defs/memory_ext.tmdl")),
    ("atomics.tmdl", include_str!("../defs/atomics.tmdl")),
    ("ordering.tmdl", include_str!("../defs/ordering.tmdl")),
    ("float.tmdl", include_str!("../defs/float.tmdl")),
    ("perf.tmdl", include_str!("../defs/perf.tmdl")),
];

pub use isa::{Feature, get_isel_rules, register_info, register_views, register_widths};
pub use isa::{TargetConfig, X86_64Dialect};

mod isa {
    // Generated code: not everything is used by this asm-focused prototype.
    #![allow(dead_code, unused_variables, unused_mut, clippy::all)]

    use tir::Operation;
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::backend::{VirtualBranchOp, VirtualCallOp, VirtualIndirectCallOp, VirtualReturnOp};
    use tir::helpers::{dialect, operation};

    include!(concat!(env!("OUT_DIR"), "/x86_64.rs"));

    dialect! {
        X86_64Dialect {
            name: "x86_64",
            operation_file: concat!(env!("OUT_DIR"), "/x86_64_ops.rs"),
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

    impl X86_64Dialect {
        pub fn get_asm_printer(&self) -> tir::backend::AsmPrinter {
            tir::backend::AsmPrinter::new(get_instruction_printers())
        }
    }

    fn virt(value: u32, class: tir::backend::regalloc::RegClassId) -> AttributeValue {
        AttributeValue::Register(RegisterAttr::Virtual {
            id: value,
            class: Some(class),
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
        let temp = context
            .create_value(tir::builtin::IntegerType::new(context, 64), None)
            .id();
        let materialize = MovAbsOpBuilder::new(context)
            .attr("dst", virt(temp.number(), RegClass::GPR.id()))
            .attr("imm", AttributeValue::Int(bits))
            .build();
        let move_bits = MovqXmmGprOpBuilder::new(context)
            .attr(
                "dst",
                virt(constant.result().number(), RegClass::XMMzx.id()),
            )
            .attr("src", virt(temp.number(), RegClass::GPR.id()))
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
                    .attr("dst", virt(condition.number(), RegClass::GPR32.id()))
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
        let dst = virt(constant.result().number(), RegClass::GPR.id());
        if i32::try_from(value).is_err() {
            let movabs = MovAbsOpBuilder::new(context)
                .attr("dst", dst)
                .attr("imm", AttributeValue::Int(value))
                .build();
            rewriter.replace_op(op, &movabs)?;
            return Ok(true);
        }

        let mov = MovImmOpBuilder::new(context)
            .attr("dst", dst)
            .attr("imm", AttributeValue::Int(value))
            .build();
        rewriter.replace_op(op, &mov)?;
        Ok(true)
    }

    /// Pre-RA: materialize an `addr_of` symbol address as `lea rd, [rip + sym]`.
    /// The encoder leaves the disp32 as a fixup emitted with R_X86_64_PC32.
    fn lower_addr_of(
        context: &tir::Context,
        op: &tir::OperationRef,
        rewriter: &mut tir::Rewriter,
    ) -> Result<bool, tir::PassError> {
        use tir::func::AddressOfOp;

        let Some(addr_of) = op.as_op::<AddressOfOp>() else {
            return Ok(false);
        };
        let lea = LeaRipOpBuilder::new(context)
            .attr("dst", virt(addr_of.result().number(), RegClass::GPR.id()))
            .attr("imm", AttributeValue::Str(addr_of.sym_name().into()))
            .build();
        rewriter.replace_op(op, &lea)?;
        Ok(true)
    }

    /// A register-to-register `mov dst, src`.
    fn mv(context: &tir::Context, dst: AttributeValue, src: AttributeValue) -> Box<dyn Operation> {
        Box::new(
            MovOpBuilder::new(context)
                .attr("dst", dst)
                .attr("src", src)
                .build(),
        )
    }

    fn abi_copy(
        context: &tir::Context,
        dst: AttributeValue,
        src: AttributeValue,
    ) -> Box<dyn Operation> {
        let AttributeValue::Register(register) = &dst else {
            unreachable!("ABI copies target a register")
        };
        match register.class().unwrap().name() {
            "GPR" => mv(context, dst, src),
            "XMM" => Box::new(
                MovsdOpBuilder::new(context)
                    .attr("dst", dst)
                    .attr("src", src)
                    .build(),
            ),
            other => unreachable!("unknown x86-64 ABI register class {other}"),
        }
    }

    struct X86CallEmitter;

    impl tir::backend::call_lowering::CallEmitter for X86CallEmitter {
        fn copy(
            &self,
            context: &tir::Context,
            dst: AttributeValue,
            src: AttributeValue,
        ) -> Box<dyn Operation> {
            abi_copy(context, dst, src)
        }

        fn stack_arg_store(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            value: AttributeValue,
            offset: i64,
        ) -> Result<Box<dyn Operation>, tir::PassError> {
            let AttributeValue::Register(RegisterAttr::Virtual {
                class: Some(class), ..
            }) = &value
            else {
                unreachable!("call lowering stores a typed virtual register")
            };
            let base = phys(abi.sp.0, abi.sp.1);
            let offset = AttributeValue::Int(offset);
            match class.name() {
                "GPR" => Ok(Box::new(
                    MovStoreDispOpBuilder::new(context)
                        .attr("base", base)
                        .attr("imm", offset)
                        .attr("src", value)
                        .build(),
                )),
                "XMM" => Ok(Box::new(
                    MovsdStoreDispOpBuilder::new(context)
                        .attr("base", base)
                        .attr("imm", offset)
                        .attr("src", value)
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
        fn base_index(op: &dyn Operation) -> Option<u16> {
            reg_index(op, "base")
        }
        fn attr(op: &dyn Operation, name: &str) -> AttributeValue {
            op.attr(name).expect("memory op operand attribute present")
        }
        // The allocated index of a physical register operand, or None if the
        // operand is still virtual (canonicalization runs post-RA, so a virtual
        // operand simply means "not this op" and is left unchanged).
        fn reg_index(op: &dyn Operation, name: &str) -> Option<u16> {
            match op.attr(name)? {
                AttributeValue::Register(RegisterAttr::Physical { index, .. }) => Some(index),
                _ => None,
            }
        }
        fn reg_class(op: &dyn Operation, name: &str) -> Option<tir::backend::regalloc::RegClassId> {
            match op.attr(name)? {
                AttributeValue::Register(RegisterAttr::Physical { class, .. }) => Some(class),
                _ => None,
            }
        }
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

        macro_rules! escape {
            ($Op:ty, $Sib:ident, $Rbp:ident, [$($a:literal),*]) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    let Some(idx) = base_index(&inner) else { return Ok(false); };
                    return match idx {
                        4 | 12 => replace(
                            rewriter,
                            Box::new($Sib::new(context)$(.attr($a, attr(&inner, $a)))*.build()),
                        ),
                        5 | 13 => replace(
                            rewriter,
                            Box::new($Rbp::new(context)$(.attr($a, attr(&inner, $a)))*.build()),
                        ),
                        _ => Ok(false),
                    };
                }
            };
            (sib $Op:ty, $Sib:ident, [$($a:literal),*]) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    let Some(idx) = base_index(&inner) else { return Ok(false); };
                    if matches!(idx, 4 | 12) {
                        return replace(
                            rewriter,
                            Box::new($Sib::new(context)$(.attr($a, attr(&inner, $a)))*.build()),
                        );
                    }
                    return Ok(false);
                }
            };
        }
        macro_rules! escape_norex {
            ($Op:ty, $Sib:ident, $Rbp:ident, $Norex:ident, $reg:literal, $limit:expr, [$($a:literal),*]) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    let Some(base) = base_index(&inner) else {
                        return Ok(false);
                    };
                    let replacement: Option<Box<dyn Operation>> = match base {
                        4 | 12 => Some(Box::new(
                            $Sib::new(context)$(.attr($a, attr(&inner, $a)))*.build(),
                        )),
                        5 | 13 => Some(Box::new(
                            $Rbp::new(context)$(.attr($a, attr(&inner, $a)))*.build(),
                        )),
                        _ if base < 8
                            && matches!(reg_index(&inner, $reg), Some(reg) if reg < $limit) =>
                        {
                            Some(Box::new(
                                $Norex::new(context)$(.attr($a, attr(&inner, $a)))*.build(),
                            ))
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

        escape!(
            MovLoadOp,
            MovLoadSibOpBuilder,
            MovLoadRbpOpBuilder,
            ["dst", "base"]
        );
        escape!(
            MovStoreOp,
            MovStoreSibOpBuilder,
            MovStoreRbpOpBuilder,
            ["base", "src"]
        );
        escape!(
            Movzx8LoadOp,
            Movzx8LoadSibOpBuilder,
            Movzx8LoadRbpOpBuilder,
            ["dst", "base"]
        );
        escape!(
            Movzx16LoadOp,
            Movzx16LoadSibOpBuilder,
            Movzx16LoadRbpOpBuilder,
            ["dst", "base"]
        );
        escape!(
            Movsx8LoadOp,
            Movsx8LoadSibOpBuilder,
            Movsx8LoadRbpOpBuilder,
            ["dst", "base"]
        );
        escape!(
            Movsx16LoadOp,
            Movsx16LoadSibOpBuilder,
            Movsx16LoadRbpOpBuilder,
            ["dst", "base"]
        );
        escape!(
            MovsxdLoadOp,
            MovsxdLoadSibOpBuilder,
            MovsxdLoadRbpOpBuilder,
            ["dst", "base"]
        );
        escape_norex!(
            Mov32LoadOp,
            Mov32LoadSibOpBuilder,
            Mov32LoadRbpOpBuilder,
            Mov32LoadNorexOpBuilder,
            "dst",
            8,
            ["dst", "base"]
        );
        escape_norex!(
            Mov32StoreOp,
            Mov32StoreSibOpBuilder,
            Mov32StoreRbpOpBuilder,
            Mov32StoreNorexOpBuilder,
            "src",
            8,
            ["base", "src"]
        );
        escape_norex!(
            Mov16StoreOp,
            Mov16StoreSibOpBuilder,
            Mov16StoreRbpOpBuilder,
            Mov16StoreNorexOpBuilder,
            "src",
            8,
            ["base", "src"]
        );
        escape_norex!(
            Mov8StoreOp,
            Mov8StoreSibOpBuilder,
            Mov8StoreRbpOpBuilder,
            Mov8StoreNorexOpBuilder,
            "src",
            4,
            ["base", "src"]
        );
        escape!(sib MovLoadDispOp, MovLoadDispSibOpBuilder, ["dst", "base", "imm"]);
        escape!(sib MovStoreDispOp, MovStoreDispSibOpBuilder, ["base", "imm", "src"]);
        escape!(sib Mov32LoadDispOp, Mov32LoadDispSibOpBuilder, ["dst", "base", "imm"]);
        escape!(sib Mov32StoreDispOp, Mov32StoreDispSibOpBuilder, ["base", "imm", "src"]);
        escape!(sib Mov16LoadDispOp, Mov16LoadDispSibOpBuilder, ["dst", "base", "imm"]);
        escape!(sib Mov16StoreDispOp, Mov16StoreDispSibOpBuilder, ["base", "imm", "src"]);
        escape!(sib Mov8LoadDispOp, Mov8LoadDispSibOpBuilder, ["dst", "base", "imm"]);
        escape!(sib Mov8StoreDispOp, Mov8StoreDispSibOpBuilder, ["base", "imm", "src"]);
        escape!(sib MovssLoadDispOp, MovssLoadDispSibOpBuilder, ["dst", "base", "imm"]);
        escape!(sib MovssStoreDispOp, MovssStoreDispSibOpBuilder, ["base", "imm", "src"]);
        escape!(sib MovsdLoadDispOp, MovsdLoadDispSibOpBuilder, ["dst", "base", "imm"]);
        escape!(sib MovsdStoreDispOp, MovsdStoreDispSibOpBuilder, ["base", "imm", "src"]);
        escape!(sib MovssLoadDispNorexOp, MovssLoadDispSibNorexOpBuilder, ["dst", "base", "imm"]);
        escape!(sib MovssStoreDispNorexOp, MovssStoreDispSibNorexOpBuilder, ["base", "imm", "src"]);
        escape!(sib MovsdLoadDispNorexOp, MovsdLoadDispSibNorexOpBuilder, ["dst", "base", "imm"]);
        escape!(sib MovsdStoreDispNorexOp, MovsdStoreDispSibNorexOpBuilder, ["base", "imm", "src"]);

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
            && reg_class(&inner, "dst") == Some(RegClass::GPR8.id())
        {
            return replace(
                rewriter,
                Box::new(
                    Test8OpBuilder::new(context)
                        .attr("dst", attr(&inner, "dst"))
                        .attr("src", attr(&inner, "src"))
                        .build(),
                ),
            );
        }

        // Register/register: `op → op_norex` when both operands are low.
        macro_rules! rr_norex {
            ($Op:ty, $Norex:ident, $t:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match (reg_index(&inner, "dst"), reg_index(&inner, "src")) {
                        (Some(d), Some(s)) if d < $t && s < $t => replace(
                            rewriter,
                            Box::new(
                                $Norex::new(context)
                                    .attr("dst", attr(&inner, "dst"))
                                    .attr("src", attr(&inner, "src"))
                                    .build(),
                            ),
                        ),
                        _ => Ok(false),
                    };
                }
            };
        }
        macro_rules! rr_named_norex {
            ($Op:ty, $Norex:ident, $dst:literal, $src:literal, $t:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match (reg_index(&inner, $dst), reg_index(&inner, $src)) {
                        (Some(d), Some(s)) if d < $t && s < $t => replace(
                            rewriter,
                            Box::new(
                                $Norex::new(context)
                                    .attr($dst, attr(&inner, $dst))
                                    .attr($src, attr(&inner, $src))
                                    .build(),
                            ),
                        ),
                        _ => Ok(false),
                    };
                }
            };
        }
        macro_rules! rr_imm_norex {
            ($Op:ty, $Norex:ident, $t:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match (reg_index(&inner, "dst"), reg_index(&inner, "src")) {
                        (Some(d), Some(s)) if d < $t && s < $t => replace(
                            rewriter,
                            Box::new(
                                $Norex::new(context)
                                    .attr("dst", attr(&inner, "dst"))
                                    .attr("src", attr(&inner, "src"))
                                    .attr("imm", attr(&inner, "imm"))
                                    .build(),
                            ),
                        ),
                        _ => Ok(false),
                    };
                }
            };
        }
        macro_rules! mem_norex {
            ($Op:ty, $Norex:ident, $reg:literal, [$($a:literal),*]) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match (reg_index(&inner, $reg), reg_index(&inner, "base")) {
                        (Some(r), Some(b)) if r < LO && b < LO => {
                            let builder = $Norex::new(context);
                            $( let builder = builder.attr($a, attr(&inner, $a)); )*
                            replace(rewriter, Box::new(builder.build()))
                        }
                        _ => Ok(false),
                    };
                }
            };
        }
        // Single register + immediate: `op → op_norex` when the register is low.
        macro_rules! ri_norex {
            ($Op:ty, $Norex:ident, $t:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match reg_index(&inner, "dst") {
                        Some(d) if d < $t => replace(
                            rewriter,
                            Box::new(
                                $Norex::new(context)
                                    .attr("dst", attr(&inner, "dst"))
                                    .attr("imm", attr(&inner, "imm"))
                                    .build(),
                            ),
                        ),
                        _ => Ok(false),
                    };
                }
            };
        }
        // Group-1 32/16-bit immediate: pick imm8/imm8-norex/imm32-norex.
        macro_rules! g1_imm {
            ($Op:ty, $Imm8:ident, $Imm8N:ident, $Imm32N:ident) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    let low = matches!(reg_index(&inner, "dst"), Some(d) if d < LO);
                    let small = matches!(imm_int(&inner, "imm"), Some(v) if (-128..=127).contains(&v));
                    let d = attr(&inner, "dst");
                    let i = attr(&inner, "imm");
                    let new: Box<dyn Operation> = match (small, low) {
                        (true, true) => {
                            Box::new($Imm8N::new(context).attr("dst", d).attr("imm", i).build())
                        }
                        (true, false) => {
                            Box::new($Imm8::new(context).attr("dst", d).attr("imm", i).build())
                        }
                        (false, true) => {
                            Box::new($Imm32N::new(context).attr("dst", d).attr("imm", i).build())
                        }
                        (false, false) => return Ok(false),
                    };
                    return replace(rewriter, new);
                }
            };
        }
        // Group-1 64-bit immediate: only the 0x83 imm8 fold (REX.W stays).
        macro_rules! g1_imm64 {
            ($Op:ty, $Imm8:ident) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match imm_int(&inner, "imm") {
                        Some(v) if (-128..=127).contains(&v) => replace(
                            rewriter,
                            Box::new(
                                $Imm8::new(context)
                                    .attr("dst", attr(&inner, "dst"))
                                    .attr("imm", attr(&inner, "imm"))
                                    .build(),
                            ),
                        ),
                        _ => Ok(false),
                    };
                }
            };
        }
        // A single register operand named `$n` (setcc dst, push/pop reg, indirect
        // jmp/call target): `op → op_norex` when that register is low.
        macro_rules! reg1_norex {
            ($Op:ty, $Norex:ident, $n:literal, $t:expr) => {
                if let Some(inner) = op.as_op::<$Op>() {
                    return match reg_index(&inner, $n) {
                        Some(d) if d < $t => replace(
                            rewriter,
                            Box::new($Norex::new(context).attr($n, attr(&inner, $n)).build()),
                        ),
                        _ => Ok(false),
                    };
                }
            };
        }

        rr_norex!(Add32Op, Add32NorexOpBuilder, LO);
        rr_norex!(Sub32Op, Sub32NorexOpBuilder, LO);
        rr_norex!(And32Op, And32NorexOpBuilder, LO);
        rr_norex!(Or32Op, Or32NorexOpBuilder, LO);
        rr_norex!(Xor32Op, Xor32NorexOpBuilder, LO);
        rr_norex!(Mov32Op, Mov32NorexOpBuilder, LO);
        rr_norex!(Imul32Op, Imul32NorexOpBuilder, LO);
        rr_imm_norex!(ImulImm32Op, ImulImm32NorexOpBuilder, LO);
        rr_norex!(Add16Op, Add16NorexOpBuilder, LO);
        rr_norex!(Sub16Op, Sub16NorexOpBuilder, LO);
        rr_norex!(And16Op, And16NorexOpBuilder, LO);
        rr_norex!(Or16Op, Or16NorexOpBuilder, LO);
        rr_norex!(Xor16Op, Xor16NorexOpBuilder, LO);
        rr_norex!(Mov16Op, Mov16NorexOpBuilder, LO);
        rr_norex!(Add8Op, Add8NorexOpBuilder, B);
        rr_norex!(Sub8Op, Sub8NorexOpBuilder, B);
        rr_norex!(And8Op, And8NorexOpBuilder, B);
        rr_norex!(Or8Op, Or8NorexOpBuilder, B);
        rr_norex!(Xor8Op, Xor8NorexOpBuilder, B);
        rr_norex!(Mov8Op, Mov8NorexOpBuilder, B);

        g1_imm!(
            AddImm32Op,
            AddImm8s32OpBuilder,
            AddImm8s32NorexOpBuilder,
            AddImm32NorexOpBuilder
        );
        g1_imm!(
            OrImm32Op,
            OrImm8s32OpBuilder,
            OrImm8s32NorexOpBuilder,
            OrImm32NorexOpBuilder
        );
        g1_imm!(
            AndImm32Op,
            AndImm8s32OpBuilder,
            AndImm8s32NorexOpBuilder,
            AndImm32NorexOpBuilder
        );
        g1_imm!(
            XorImm32Op,
            XorImm8s32OpBuilder,
            XorImm8s32NorexOpBuilder,
            XorImm32NorexOpBuilder
        );
        g1_imm!(
            SubImm32Op,
            SubImm8s32OpBuilder,
            SubImm8s32NorexOpBuilder,
            SubImm32NorexOpBuilder
        );
        g1_imm!(
            CmpImm32Op,
            CmpImm8s32OpBuilder,
            CmpImm8s32NorexOpBuilder,
            CmpImm32NorexOpBuilder
        );
        g1_imm!(
            AddImm16Op,
            AddImm8s16OpBuilder,
            AddImm8s16NorexOpBuilder,
            AddImm16NorexOpBuilder
        );
        g1_imm!(
            OrImm16Op,
            OrImm8s16OpBuilder,
            OrImm8s16NorexOpBuilder,
            OrImm16NorexOpBuilder
        );
        g1_imm!(
            AndImm16Op,
            AndImm8s16OpBuilder,
            AndImm8s16NorexOpBuilder,
            AndImm16NorexOpBuilder
        );
        g1_imm!(
            XorImm16Op,
            XorImm8s16OpBuilder,
            XorImm8s16NorexOpBuilder,
            XorImm16NorexOpBuilder
        );

        g1_imm64!(AddImmOp, AddImm8sOpBuilder);
        g1_imm64!(OrImmOp, OrImm8sOpBuilder);
        g1_imm64!(AndImmOp, AndImm8sOpBuilder);
        g1_imm64!(XorImmOp, XorImm8sOpBuilder);
        g1_imm64!(SubImmOp, SubImm8sOpBuilder);
        g1_imm64!(CmpImmOp, CmpImm8sOpBuilder);

        // mov/test immediates: no 0x83 form, only the REX-free downgrade.
        // Direct isel picks of the 0x83 imm8 short forms still need the
        // REX-free downgrade.
        ri_norex!(AddImm8s32Op, AddImm8s32NorexOpBuilder, LO);
        ri_norex!(OrImm8s32Op, OrImm8s32NorexOpBuilder, LO);
        ri_norex!(AndImm8s32Op, AndImm8s32NorexOpBuilder, LO);
        ri_norex!(XorImm8s32Op, XorImm8s32NorexOpBuilder, LO);
        ri_norex!(SubImm8s32Op, SubImm8s32NorexOpBuilder, LO);
        ri_norex!(CmpImm8s32Op, CmpImm8s32NorexOpBuilder, LO);

        ri_norex!(MovImm32Op, MovImm32NorexOpBuilder, LO);
        ri_norex!(TestImm32Op, TestImm32NorexOpBuilder, LO);
        ri_norex!(MovImm16Op, MovImm16NorexOpBuilder, LO);
        // 8-bit group-1 + mov immediates: REX-free when in al/cl/dl/bl.
        ri_norex!(AddImm8Op, AddImm8NorexOpBuilder, B);
        ri_norex!(OrImm8Op, OrImm8NorexOpBuilder, B);
        ri_norex!(AndImm8Op, AndImm8NorexOpBuilder, B);
        ri_norex!(XorImm8Op, XorImm8NorexOpBuilder, B);
        ri_norex!(MovImm8Op, MovImm8NorexOpBuilder, B);

        ri_norex!(ShlImm32Op, ShlImm32NorexOpBuilder, LO);
        ri_norex!(ShrImm32Op, ShrImm32NorexOpBuilder, LO);
        ri_norex!(SarImm32Op, SarImm32NorexOpBuilder, LO);
        ri_norex!(ShlImm16Op, ShlImm16NorexOpBuilder, LO);
        ri_norex!(ShrImm16Op, ShrImm16NorexOpBuilder, LO);
        ri_norex!(SarImm16Op, SarImm16NorexOpBuilder, LO);
        ri_norex!(ShlImm8Op, ShlImm8NorexOpBuilder, B);
        ri_norex!(ShrImm8Op, ShrImm8NorexOpBuilder, B);
        ri_norex!(SarImm8Op, SarImm8NorexOpBuilder, B);

        reg1_norex!(SetEqOp, SetEqNorexOpBuilder, "dst", B);
        reg1_norex!(SetParityOp, SetParityNorexOpBuilder, "dst", B);
        reg1_norex!(SetNoParityOp, SetNoParityNorexOpBuilder, "dst", B);
        reg1_norex!(SetNotEqOp, SetNotEqNorexOpBuilder, "dst", B);
        reg1_norex!(SetLessOp, SetLessNorexOpBuilder, "dst", B);
        reg1_norex!(SetGreaterEqOp, SetGreaterEqNorexOpBuilder, "dst", B);
        reg1_norex!(SetLessEqOp, SetLessEqNorexOpBuilder, "dst", B);
        reg1_norex!(SetGreaterOp, SetGreaterNorexOpBuilder, "dst", B);
        reg1_norex!(SetBelowOp, SetBelowNorexOpBuilder, "dst", B);
        reg1_norex!(SetAboveEqOp, SetAboveEqNorexOpBuilder, "dst", B);
        reg1_norex!(SetBelowEqOp, SetBelowEqNorexOpBuilder, "dst", B);
        reg1_norex!(SetAboveOp, SetAboveNorexOpBuilder, "dst", B);

        reg1_norex!(PushOp, PushNorexOpBuilder, "reg", LO);
        reg1_norex!(PopOp, PopNorexOpBuilder, "reg", LO);
        // The indirect jmp/call forms are not produced before this pass: `jmp
        // *reg` reaches its `_norex` form through the assembler, and the codegen
        // indirect call is materialized REX-free directly in `finalize_virtual_ops`
        // (it is created there, after this pass would have run).

        // cmp/test reg-reg, neg/not, by-cl shifts and rotate immediates: the
        // 32-bit width is the only one with a generic (so the only one reachable
        // here); the 16/8-bit `_norex` forms exist for the assembler only.
        rr_norex!(Cmp32Op, Cmp32NorexOpBuilder, LO);
        rr_norex!(Test32Op, Test32NorexOpBuilder, LO);
        reg1_norex!(Neg32Op, Neg32NorexOpBuilder, "dst", LO);
        reg1_norex!(Not32Op, Not32NorexOpBuilder, "dst", LO);
        reg1_norex!(SignedDivide32Op, SignedDivide32NorexOpBuilder, "dst", LO);
        reg1_norex!(
            UnsignedDivide32Op,
            UnsignedDivide32NorexOpBuilder,
            "dst",
            LO
        );
        reg1_norex!(ShlCl32Op, ShlCl32NorexOpBuilder, "dst", LO);
        reg1_norex!(ShrCl32Op, ShrCl32NorexOpBuilder, "dst", LO);
        reg1_norex!(SarCl32Op, SarCl32NorexOpBuilder, "dst", LO);
        ri_norex!(RolImm32Op, RolImm32NorexOpBuilder, LO);
        ri_norex!(RorImm32Op, RorImm32NorexOpBuilder, LO);

        // Low-xmm SSE: drop the empty REX when both xmm operands are xmm0..xmm7.
        rr_norex!(AddssOp, AddssNorexOpBuilder, LO);
        rr_norex!(SubssOp, SubssNorexOpBuilder, LO);
        rr_norex!(MulssOp, MulssNorexOpBuilder, LO);
        rr_norex!(DivssOp, DivssNorexOpBuilder, LO);
        rr_norex!(MovssOp, MovssNorexOpBuilder, LO);
        rr_norex!(AddsdOp, AddsdNorexOpBuilder, LO);
        rr_norex!(SubsdOp, SubsdNorexOpBuilder, LO);
        rr_norex!(MulsdOp, MulsdNorexOpBuilder, LO);
        rr_norex!(DivsdOp, DivsdNorexOpBuilder, LO);
        rr_norex!(MovsdOp, MovsdNorexOpBuilder, LO);
        rr_norex!(Cvtsi2ss32Op, Cvtsi2ss32NorexOpBuilder, LO);
        rr_norex!(Cvttss2si32Op, Cvttss2si32NorexOpBuilder, LO);
        rr_norex!(Cvtsi2sd32Op, Cvtsi2sd32NorexOpBuilder, LO);
        rr_norex!(Cvttsd2si32Op, Cvttsd2si32NorexOpBuilder, LO);
        rr_norex!(MovdXmmGpr32Op, MovdXmmGpr32NorexOpBuilder, LO);
        rr_norex!(MovdGpr32XmmOp, MovdGpr32XmmNorexOpBuilder, LO);
        rr_named_norex!(UcomissOp, UcomissNorexOpBuilder, "lhs", "rhs", LO);
        rr_named_norex!(UcomisdOp, UcomisdNorexOpBuilder, "lhs", "rhs", LO);
        mem_norex!(
            MovssLoadDispOp,
            MovssLoadDispNorexOpBuilder,
            "dst",
            ["dst", "base", "imm"]
        );
        mem_norex!(
            MovssStoreDispOp,
            MovssStoreDispNorexOpBuilder,
            "src",
            ["base", "imm", "src"]
        );
        mem_norex!(
            MovssLoadDispSibOp,
            MovssLoadDispSibNorexOpBuilder,
            "dst",
            ["dst", "base", "imm"]
        );
        mem_norex!(
            MovssStoreDispSibOp,
            MovssStoreDispSibNorexOpBuilder,
            "src",
            ["base", "imm", "src"]
        );
        mem_norex!(
            MovsdLoadDispOp,
            MovsdLoadDispNorexOpBuilder,
            "dst",
            ["dst", "base", "imm"]
        );
        mem_norex!(
            MovsdStoreDispOp,
            MovsdStoreDispNorexOpBuilder,
            "src",
            ["base", "imm", "src"]
        );
        mem_norex!(
            MovsdLoadDispSibOp,
            MovsdLoadDispSibNorexOpBuilder,
            "dst",
            ["dst", "base", "imm"]
        );
        mem_norex!(
            MovsdStoreDispSibOp,
            MovsdStoreDispSibNorexOpBuilder,
            "src",
            ["base", "imm", "src"]
        );

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
            rewriter.replace_op(op, &real)?;
            return Ok(true);
        }

        // `vcall_indirect` becomes `call *target`; the target register was colored
        // by the allocator through the op's `callee_reg` attribute.
        if let Some(call) = op.as_op::<VirtualIndirectCallOp>() {
            let target = match call.attr("callee_reg") {
                Some(value @ AttributeValue::Register(_)) => Some(value.clone()),
                _ => None,
            }
            .ok_or_else(|| {
                tir::PassError::InvalidRuleSet(
                    "vcall_indirect is missing its 'callee_reg'".to_string(),
                )
            })?;
            // `call *reg` needs no REX when the target is rax..rdi; emit the
            // REX-free form directly (this op is created after
            // `canonicalize_encodings` would run).
            let low = matches!(
                &target,
                AttributeValue::Register(RegisterAttr::Physical { index, .. }) if *index < 8
            );
            let real: Box<dyn Operation> = if low {
                Box::new(
                    CallIndirectNorexOpBuilder::new(context)
                        .attr("target", target)
                        .build(),
                )
            } else {
                Box::new(
                    CallIndirectOpBuilder::new(context)
                        .attr("target", target)
                        .build(),
                )
            };
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
            value: u32,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Box<dyn Operation> {
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => Box::new(
                    MovStoreDispOpBuilder::new(context)
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .attr("src", virt(value, class))
                        .build(),
                ),
                Some(MoveKind::Gpr32) => Box::new(
                    Mov32StoreDispOpBuilder::new(context)
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .attr("src", virt(value, class))
                        .build(),
                ),
                Some(MoveKind::Gpr16) => Box::new(
                    Mov16StoreDispOpBuilder::new(context)
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .attr("src", virt(value, class))
                        .build(),
                ),
                Some(MoveKind::Gpr8) => Box::new(
                    Mov8StoreDispOpBuilder::new(context)
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .attr("src", virt(value, class))
                        .build(),
                ),
                Some(MoveKind::Xmm64) => Box::new(
                    MovsdStoreDispOpBuilder::new(context)
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .attr("src", virt(value, class))
                        .build(),
                ),
                Some(MoveKind::Xmm32) => Box::new(
                    MovssStoreDispOpBuilder::new(context)
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .attr("src", virt(value, class))
                        .build(),
                ),
                _ => unimplemented!("x86-64 spilling for {} is not implemented", class.name()),
            }
        }

        fn emit_spill_reload(
            &self,
            context: &tir::Context,
            value: u32,
            class: tir::backend::regalloc::RegClassId,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Box<dyn Operation> {
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => Box::new(
                    MovLoadDispOpBuilder::new(context)
                        .attr("dst", virt(value, class))
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ),
                Some(MoveKind::Gpr32) => Box::new(
                    Mov32LoadDispOpBuilder::new(context)
                        .attr("dst", virt(value, class))
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ),
                Some(MoveKind::Gpr16) => Box::new(
                    Mov16LoadDispOpBuilder::new(context)
                        .attr("dst", virt(value, class))
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ),
                Some(MoveKind::Gpr8) => Box::new(
                    Mov8LoadDispOpBuilder::new(context)
                        .attr("dst", virt(value, class))
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ),
                Some(MoveKind::Xmm64) => Box::new(
                    MovsdLoadDispOpBuilder::new(context)
                        .attr("dst", virt(value, class))
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ),
                Some(MoveKind::Xmm32) => Box::new(
                    MovssLoadDispOpBuilder::new(context)
                        .attr("dst", virt(value, class))
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ),
                _ => unimplemented!("x86-64 spilling for {} is not implemented", class.name()),
            }
        }

        fn emit_copy(
            &self,
            context: &tir::Context,
            class: tir::backend::regalloc::RegClassId,
            dst: u32,
            src: u32,
        ) -> Box<dyn Operation> {
            let virt = |id: u32| {
                AttributeValue::Register(RegisterAttr::Virtual {
                    id,
                    class: Some(class),
                })
            };
            match self.move_kind(class) {
                Some(MoveKind::Gpr64) => Box::new(
                    MovOpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                Some(MoveKind::Gpr32) => Box::new(
                    Mov32OpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                Some(MoveKind::Gpr16) => Box::new(
                    Mov16OpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                Some(MoveKind::Gpr8) => Box::new(
                    Mov8OpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                Some(MoveKind::Gpr8High) => Box::new(
                    Mov8HOpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                Some(MoveKind::Xmm64) => Box::new(
                    MovsdOpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                Some(MoveKind::Xmm32) => Box::new(
                    MovssOpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
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
            dst: u32,
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
            let dst = virt(dst, class);
            let mut ops = vec![mv(context, dst.clone(), phys(frame.0, frame.1))];
            if offset != 0 {
                ops.push(Box::new(
                    AddImmOpBuilder::new(context)
                        .attr("dst", dst)
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ));
            }
            Ok(ops)
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
    }

    impl TargetConfig {
        /// Parse an x86-64 architecture name.
        pub fn parse(
            march: &str,
            _mcpu: Option<&str>,
            mattr: Option<&str>,
        ) -> Result<Self, String> {
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
            Ok(Self { features })
        }

        /// The enabled ISA set.
        pub fn features(&self) -> &[Feature] {
            &self.features
        }
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
            vec![lower_float_constant, lower_constant, lower_addr_of]
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

        fn asm_printer(&self, context: &tir::Context) -> tir::backend::AsmPrinter {
            context
                .find_dialect::<X86_64Dialect>()
                .expect("x86_64 dialect must be registered before building an asm printer")
                .get_asm_printer()
        }

        fn machine_model(&self, name: &str) -> Option<tir::backend::sched::MachineModel> {
            machine_model(name, self.config.features())
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

        fn binary_writer(
            &self,
            _context: &tir::Context,
        ) -> Option<tir::backend::binary::BinaryWriter> {
            Some(tir::backend::binary::BinaryWriter::new(
                get_instruction_encoders(),
                get_instruction_patchers(),
            ))
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

    #[cfg(test)]
    mod canonicalize_tests {
        use super::*;
        use tir::backend::lower::OpLoweringPass;
        use tir::builtin::{ModuleEndOpBuilder, ModuleOpBuilder};
        use tir::{IRFormatter, PassManager};

        /// Rewrite a `mov_load` whose base is the physical register `base_index`
        /// and return the printed IR of the (single) resulting op.
        fn rewrite_load(base_index: u16) -> String {
            let context = tir::Context::with_default_dialects();
            context.register_dialect::<X86_64Dialect>();
            let module = ModuleOpBuilder::new(&context).build();
            let b = module.body();
            b.append_op(
                MovLoadOpBuilder::new(&context)
                    .attr("dst", phys(RegClass::GPR.id(), 0))
                    .attr("base", phys(RegClass::GPR.id(), base_index))
                    .build(),
            );
            b.append_op(ModuleEndOpBuilder::new(&context).build());

            let mut pm = PassManager::new();
            pm.add_pass(OpLoweringPass::new(
                "canonicalize-encodings",
                vec![canonicalize_encodings],
            ));
            pm.run(&context, context.get_op(module.id()))
                .expect("canonicalize pass runs");

            let mut buf = String::new();
            let mut fmt = IRFormatter::new(&mut buf);
            module.print(&mut fmt).expect("print module");
            buf
        }

        #[test]
        fn rsp_and_r12_bases_take_the_sib_form() {
            assert!(rewrite_load(4).contains("mov_load_sib"));
            assert!(rewrite_load(12).contains("mov_load_sib"));
        }

        #[test]
        fn rbp_and_r13_bases_take_the_rbp_form() {
            assert!(rewrite_load(5).contains("mov_load_rbp"));
            assert!(rewrite_load(13).contains("mov_load_rbp"));
        }

        #[test]
        fn ordinary_bases_are_left_generic() {
            let ir = rewrite_load(3);
            assert!(ir.contains("mov_load"));
            assert!(!ir.contains("mov_load_sib"));
            assert!(!ir.contains("mov_load_rbp"));
        }

        /// Run `canonicalize_encodings` over a single op and return the printed IR.
        macro_rules! canon {
            ($build:expr) => {{
                let context = tir::Context::with_default_dialects();
                context.register_dialect::<X86_64Dialect>();
                let module = ModuleOpBuilder::new(&context).build();
                let b = module.body();
                b.append_op($build(&context));
                b.append_op(ModuleEndOpBuilder::new(&context).build());
                let mut pm = PassManager::new();
                pm.add_pass(OpLoweringPass::new("c", vec![canonicalize_encodings]));
                pm.run(&context, context.get_op(module.id()))
                    .expect("pass runs");
                let mut buf = String::new();
                module
                    .print(&mut IRFormatter::new(&mut buf))
                    .expect("print module");
                buf
            }};
        }

        fn g32(index: u16) -> AttributeValue {
            phys(RegClass::GPR32.id(), index)
        }

        #[test]
        fn rr_low_becomes_norex_high_stays() {
            let low = canon!(|c| Add32OpBuilder::new(c)
                .attr("dst", g32(0))
                .attr("src", g32(1))
                .build());
            assert!(low.contains("add32_norex"));
            let high = canon!(|c| Add32OpBuilder::new(c)
                .attr("dst", g32(8))
                .attr("src", g32(1))
                .build());
            assert!(high.contains("x86_64.add32 "));
            assert!(!high.contains("norex"));
        }

        #[test]
        fn scalar_single_low_becomes_norex_high_stays() {
            let low = canon!(|c| AddssOpBuilder::new(c)
                .attr("dst", phys(RegClass::XMM32.id(), 0))
                .attr("src", phys(RegClass::XMM32.id(), 1))
                .build());
            assert!(low.contains("addss_norex"));
            let high = canon!(|c| AddssOpBuilder::new(c)
                .attr("dst", phys(RegClass::XMM32.id(), 8))
                .attr("src", phys(RegClass::XMM32.id(), 1))
                .build());
            assert!(high.contains("x86_64.addss "));
            assert!(!high.contains("norex"));
        }

        #[test]
        fn scalar_compare_and_conversion_low_become_norex() {
            let compare = canon!(|c| UcomissOpBuilder::new(c)
                .attr("lhs", phys(RegClass::XMM32.id(), 0))
                .attr("rhs", phys(RegClass::XMM32.id(), 1))
                .build());
            assert!(compare.contains("ucomiss_norex"));

            let conversion = canon!(|c| Cvtsi2ss32OpBuilder::new(c)
                .attr("dst", phys(RegClass::XMM32.id(), 0))
                .attr("src", g32(0))
                .build());
            assert!(conversion.contains("cvtsi2ss32_norex"));
        }

        #[test]
        fn legacy_integer_parsers_belong_to_x86() {
            let (parsers, _) = get_instruction_parsers(&[Feature::X86]);
            assert!(parsers.contains_key("add"));
            assert!(parsers.contains_key("setne"));
            assert!(!parsers.contains_key("addsd"));

            let context = tir::Context::with_default_dialects();
            let parser = tir::backend::AsmParser::new(parsers);
            for instruction in [
                "add eax, ebx",
                "mov ah, bh",
                "mov edi, 42",
                "mov eax, [rbx]",
                "mov [rbx], ecx",
                "mov [rbx], cx",
                "mov [rbx], cl",
                "movzx eax, bl",
                "movsx eax, cx",
                "movzx eax, byte ptr [rbx]",
                "movsx eax, word ptr [rbx]",
                "imul eax, ebx",
                "idiv ebx",
                "div ebx",
                "cdq",
                "shl eax",
                "neg ax",
                "jmp 4",
                "je 4",
                "call 4",
                "ret",
                "jmp *rax",
                "call *rax",
                "push rax",
                "pop rax",
            ] {
                assert!(
                    parser.parse_asm(&context, instruction).is_ok(),
                    "X86 rejected {instruction}"
                );
            }
            for instruction in ["add rax, rbx", "add r8d, eax", "addsd xmm0, xmm1"] {
                assert!(
                    parser.parse_asm(&context, instruction).is_err(),
                    "X86 accepted {instruction}"
                );
            }
        }

        #[test]
        fn extended_xmm_registers_require_long_mode() {
            let context = tir::Context::with_default_dialects();
            let (sse, _) = get_instruction_parsers(&[Feature::X86, Feature::SSE]);
            let sse = tir::backend::AsmParser::new(sse);
            assert!(sse.parse_asm(&context, "addss xmm0, xmm1").is_ok());
            assert!(sse.parse_asm(&context, "addss xmm8, xmm1").is_err());
            assert!(sse.parse_asm(&context, "addsd xmm0, xmm1").is_err());
            assert!(sse.parse_asm(&context, "ucomiss xmm0, xmm1").is_ok());
            assert!(sse.parse_asm(&context, "ucomiss xmm8, xmm1").is_err());
            assert!(sse.parse_asm(&context, "cvtsi2ss xmm0, eax").is_ok());
            assert!(sse.parse_asm(&context, "cvtsi2ss xmm8, eax").is_err());
            assert!(sse.parse_asm(&context, "cvtsi2ss xmm0, r8d").is_err());
            assert!(sse.parse_asm(&context, "cvttss2si eax, xmm0").is_ok());
            assert!(sse.parse_asm(&context, "cvttss2si r8d, xmm0").is_err());
            assert!(sse.parse_asm(&context, "cvttss2si eax, xmm8").is_err());
            assert!(sse.parse_asm(&context, "movss xmm0, [rax + 8]").is_ok());
            assert!(sse.parse_asm(&context, "movss xmm8, [rax + 8]").is_err());
            assert!(sse.parse_asm(&context, "movss xmm0, [r8 + 8]").is_err());

            let (sse2, _) = get_instruction_parsers(&[Feature::X86, Feature::SSE, Feature::SSE2]);
            let sse2 = tir::backend::AsmParser::new(sse2);
            assert!(sse2.parse_asm(&context, "addsd xmm0, xmm1").is_ok());
            assert!(sse2.parse_asm(&context, "addsd xmm8, xmm1").is_err());
            assert!(sse2.parse_asm(&context, "ucomisd xmm0, xmm1").is_ok());
            assert!(sse2.parse_asm(&context, "ucomisd xmm8, xmm1").is_err());
            assert!(sse2.parse_asm(&context, "cvtsi2sd xmm0, eax").is_ok());
            assert!(sse2.parse_asm(&context, "cvtsi2sd xmm8, eax").is_err());
            assert!(sse2.parse_asm(&context, "cvtsi2sd xmm0, r8d").is_err());
            assert!(sse2.parse_asm(&context, "cvttsd2si eax, xmm0").is_ok());
            assert!(sse2.parse_asm(&context, "cvttsd2si r8d, xmm0").is_err());
            assert!(sse2.parse_asm(&context, "cvttsd2si eax, xmm8").is_err());
            assert!(sse2.parse_asm(&context, "movd xmm0, eax").is_ok());
            assert!(sse2.parse_asm(&context, "movd eax, xmm0").is_ok());
            assert!(sse2.parse_asm(&context, "movd xmm8, eax").is_err());
            assert!(sse2.parse_asm(&context, "movd xmm0, r8d").is_err());
            assert!(sse2.parse_asm(&context, "movd r8d, xmm0").is_err());
            assert!(sse2.parse_asm(&context, "movq rax, xmm0").is_err());
            assert!(sse2.parse_asm(&context, "cvtsi2sd xmm0, rax").is_err());
            assert!(sse2.parse_asm(&context, "cvttsd2si rax, xmm0").is_err());
            assert!(sse2.parse_asm(&context, "movsd xmm0, [rax + 8]").is_ok());
            assert!(sse2.parse_asm(&context, "movsd xmm8, [rax + 8]").is_err());
            assert!(sse2.parse_asm(&context, "movsd xmm0, [r8 + 8]").is_err());

            let (long_mode, _) = get_instruction_parsers(&[
                Feature::X86,
                Feature::SSE,
                Feature::SSE2,
                Feature::X86_64,
            ]);
            let long_mode = tir::backend::AsmParser::new(long_mode);
            assert!(long_mode.parse_asm(&context, "addss xmm8, xmm1").is_ok());
            assert!(long_mode.parse_asm(&context, "addsd xmm8, xmm1").is_ok());
            assert!(long_mode.parse_asm(&context, "ucomiss xmm8, xmm1").is_ok());
            assert!(long_mode.parse_asm(&context, "ucomisd xmm8, xmm1").is_ok());
            assert!(long_mode.parse_asm(&context, "cvtsi2ss xmm8, r8d").is_ok());
            assert!(long_mode.parse_asm(&context, "cvtsi2sd xmm8, r8d").is_ok());
            assert!(long_mode.parse_asm(&context, "cvttss2si r8d, xmm8").is_ok());
            assert!(long_mode.parse_asm(&context, "cvttsd2si r8d, xmm8").is_ok());
            assert!(long_mode.parse_asm(&context, "movd xmm8, r8d").is_ok());
            assert!(long_mode.parse_asm(&context, "movd r8d, xmm8").is_ok());
            assert!(long_mode.parse_asm(&context, "movq rax, xmm0").is_ok());
            assert!(long_mode.parse_asm(&context, "cvtsi2sd xmm0, rax").is_ok());
            assert!(long_mode.parse_asm(&context, "cvttsd2si rax, xmm0").is_ok());
            assert!(
                long_mode
                    .parse_asm(&context, "movss xmm8, [r8 + 8]")
                    .is_ok()
            );
            assert!(
                long_mode
                    .parse_asm(&context, "movsd xmm8, [r8 + 8]")
                    .is_ok()
            );
            assert_eq!(isa_params(&[Feature::X86]), vec![("XLEN", 32)]);
            assert_eq!(
                isa_params(&[Feature::X86, Feature::X86_64]),
                vec![("XLEN", 64)]
            );
        }

        #[test]
        fn atomic_register_extensions_require_long_mode() {
            let context = tir::Context::with_default_dialects();
            let (x86, _) = get_instruction_parsers(&[Feature::X86]);
            assert!(x86.contains_key("xchg"));
            assert!(x86.contains_key("lock"));
            let x86 = tir::backend::AsmParser::new(x86);
            for instruction in [
                "xchg [rax + 8], ebx",
                "lock xadd [rax + 8], ebx",
                "lock xor [rax + 8], ebx",
                "lock and [rax + 8], ebx",
                "lock or [rax + 8], ebx",
                "xchg [rsp + 8], ebx",
            ] {
                assert!(
                    x86.parse_asm(&context, instruction).is_ok(),
                    "X86 rejected {instruction}"
                );
            }
            for instruction in [
                "xchg [rax + 8], rbx",
                "xchg [r8 + 8], ebx",
                "lock xadd [rax + 8], r8d",
                "lock xor [rax + 8], r8d",
            ] {
                assert!(
                    x86.parse_asm(&context, instruction).is_err(),
                    "X86 accepted {instruction}"
                );
            }

            let (long_mode, _) = get_instruction_parsers(&[
                Feature::X86,
                Feature::SSE,
                Feature::SSE2,
                Feature::X86_64,
            ]);
            let long_mode = tir::backend::AsmParser::new(long_mode);
            for instruction in [
                "xchg [rax + 8], rbx",
                "xchg [r12 + 8], r9",
                "lock xadd [r12 + 8], r9",
                "lock xor [r12 + 8], r9",
            ] {
                assert!(
                    long_mode.parse_asm(&context, instruction).is_ok(),
                    "X86_64 rejected {instruction}"
                );
            }
        }

        #[test]
        fn memory_ordering_instructions_follow_sse_levels() {
            let context = tir::Context::with_default_dialects();

            let (x86, _) = get_instruction_parsers(&[Feature::X86]);
            let x86 = tir::backend::AsmParser::new(x86);
            for instruction in ["sfence", "lfence", "mfence", "pause"] {
                assert!(x86.parse_asm(&context, instruction).is_err());
            }

            let (sse, _) = get_instruction_parsers(&[Feature::X86, Feature::SSE]);
            let sse = tir::backend::AsmParser::new(sse);
            assert!(sse.parse_asm(&context, "sfence").is_ok());
            for instruction in ["lfence", "mfence", "pause"] {
                assert!(sse.parse_asm(&context, instruction).is_err());
            }

            let (sse2, _) = get_instruction_parsers(&[Feature::X86, Feature::SSE, Feature::SSE2]);
            let sse2 = tir::backend::AsmParser::new(sse2);
            for instruction in ["sfence", "lfence", "mfence", "pause"] {
                assert!(sse2.parse_asm(&context, instruction).is_ok());
            }
        }

        // A REX-free subclass is a view over the GPR file, so copying one is the
        // file's move at the subclass's width, not an unknown class.
        #[test]
        fn subclass_copies_use_the_file_move_at_the_class_width() {
            let context = tir::Context::with_default_dialects();
            let target = X86RegAlloc::new(&[Feature::X86, Feature::X86_64]);
            let copy = |class| {
                let op = tir::backend::regalloc::TargetRegAlloc::emit_copy(
                    &target, &context, class, 0, 1,
                );
                context.get_op(op.id()).name().as_str().to_string()
            };

            assert_eq!(copy(RegClass::GPR32low.id()), "mov32");
            assert_eq!(copy(RegClass::GPR16low.id()), "mov16");
            assert_eq!(copy(RegClass::GPR8low.id()), "mov8");
            assert_eq!(copy(RegClass::GPRlow.id()), "mov");
            assert_eq!(copy(RegClass::XMMlow.id()), "movsd");
            assert_eq!(copy(RegClass::XMM32low.id()), "movss");
        }

        #[test]
        fn scalar_single_spills_use_movss() {
            let context = tir::Context::with_default_dialects();
            let target = X86RegAlloc::new(&[Feature::X86, Feature::X86_64]);
            let class = RegClass::XMM32.id();
            let frame = (RegClass::GPR.id(), 4);
            let store = tir::backend::regalloc::TargetRegAlloc::emit_spill_store(
                &target, &context, 0, class, &frame, 8,
            );
            let reload = tir::backend::regalloc::TargetRegAlloc::emit_spill_reload(
                &target, &context, 0, class, &frame, 8,
            );

            assert_eq!(
                context.get_op(store.id()).name().as_str(),
                "movss_store_disp"
            );
            assert_eq!(
                context.get_op(reload.id()).name().as_str(),
                "movss_load_disp"
            );
        }

        #[test]
        fn byte_forms_use_the_al_cl_dl_bl_threshold() {
            let al = canon!(|c| Add8OpBuilder::new(c)
                .attr("dst", phys(RegClass::GPR8.id(), 3))
                .attr("src", phys(RegClass::GPR8.id(), 0))
                .build());
            assert!(al.contains("add8_norex"));
            // spl (index 4) requires an empty REX, so it stays on the REX form.
            let spl = canon!(|c| Add8OpBuilder::new(c)
                .attr("dst", phys(RegClass::GPR8.id(), 4))
                .attr("src", phys(RegClass::GPR8.id(), 0))
                .build());
            assert!(spl.contains("x86_64.add8 "));
            assert!(!spl.contains("norex"));
        }

        #[test]
        fn group1_imm_selects_imm8_norex_imm32_by_range_and_register() {
            let mk = |imm: i64, reg: u16| {
                canon!(|c: &tir::Context| AddImm32OpBuilder::new(c)
                    .attr("dst", g32(reg))
                    .attr("imm", AttributeValue::Int(imm))
                    .build())
            };
            assert!(mk(42, 1).contains("add_imm8s32_norex"));
            assert!(mk(42, 9).contains("add_imm8s32")); // high reg keeps REX
            assert!(mk(300, 1).contains("add_imm32_norex")); // out of i8 range
            assert!(mk(300, 9).contains("x86_64.add_imm32 ")); // neither applies
        }

        #[test]
        fn group1_imm64_folds_only_the_imm8() {
            let small = canon!(|c| AddImmOpBuilder::new(c)
                .attr("dst", phys(RegClass::GPR.id(), 1))
                .attr("imm", AttributeValue::Int(42))
                .build());
            assert!(small.contains("add_imm8s"));
            let big = canon!(|c| AddImmOpBuilder::new(c)
                .attr("dst", phys(RegClass::GPR.id(), 1))
                .attr("imm", AttributeValue::Int(300))
                .build());
            assert!(big.contains("x86_64.add_imm "));
        }

        #[test]
        fn legacy_multiply_and_divide_drop_empty_rex() {
            let multiply = canon!(|c| Imul32OpBuilder::new(c)
                .attr("dst", g32(0))
                .attr("src", g32(3))
                .build());
            assert!(multiply.contains("imul32_norex"));

            let multiply_immediate = canon!(|c| ImulImm32OpBuilder::new(c)
                .attr("dst", g32(0))
                .attr("src", g32(3))
                .attr("imm", AttributeValue::Int(7))
                .build());
            assert!(multiply_immediate.contains("imul_imm32_norex"));

            let divide = canon!(|c| SignedDivide32OpBuilder::new(c).attr("dst", g32(3)).build());
            assert!(divide.contains("signed_divide32_norex"));
        }

        #[test]
        fn legacy_memory_drops_empty_rex() {
            let load = canon!(|c| Mov32LoadOpBuilder::new(c)
                .attr("dst", g32(0))
                .attr("base", phys(RegClass::GPR.id(), 3))
                .build());
            assert!(load.contains("mov_load32_norex"));

            let store = canon!(|c| Mov32StoreOpBuilder::new(c)
                .attr("base", phys(RegClass::GPR.id(), 3))
                .attr("src", g32(1))
                .build());
            assert!(store.contains("mov_store32_norex"));

            let store16 = canon!(|c| Mov16StoreOpBuilder::new(c)
                .attr("base", phys(RegClass::GPR.id(), 3))
                .attr("src", phys(RegClass::GPR16.id(), 1))
                .build());
            assert!(store16.contains("mov_store16_norex"));

            let store8 = canon!(|c| Mov8StoreOpBuilder::new(c)
                .attr("base", phys(RegClass::GPR.id(), 3))
                .attr("src", phys(RegClass::GPR8.id(), 1))
                .build());
            assert!(store8.contains("mov_store8_norex"));
        }

        #[test]
        fn singletons_drop_rex_when_low() {
            let push = canon!(|c| PushOpBuilder::new(c)
                .attr("reg", phys(RegClass::GPR.id(), 3))
                .build());
            assert!(push.contains("push_norex"));
            let push_hi = canon!(|c| PushOpBuilder::new(c)
                .attr("reg", phys(RegClass::GPR.id(), 12))
                .build());
            assert!(push_hi.contains("x86_64.push ") && !push_hi.contains("norex"));
            let sete = canon!(|c| SetEqOpBuilder::new(c)
                .attr("dst", phys(RegClass::GPR8.id(), 0))
                .build());
            assert!(sete.contains("sete_norex"));
            let shl = canon!(|c| ShlImm32OpBuilder::new(c)
                .attr("dst", g32(0))
                .attr("imm", AttributeValue::Int(3))
                .build());
            assert!(shl.contains("shl32_norex"));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn guarded_relaxations_hold_for_all_rules() {
        let context = tir::Context::with_default_dialects();
        let config = crate::TargetConfig::parse("x86_64", None, None).unwrap();
        let rules = crate::get_isel_rules(&context, config.features());
        tir::backend::isel::prove_guarded_relaxations(&rules).unwrap();
    }

    #[test]
    fn tiger_lake_uses_measured_ooo_window() {
        assert_eq!(crate::isa::tiger_lake_model().buffer("rob"), Some(230));
    }

    #[test]
    fn x86_64_target_enables_required_features() {
        let config = crate::TargetConfig::parse("x86_64", None, None).unwrap();
        assert_eq!(
            config.features(),
            &[
                crate::Feature::X86,
                crate::Feature::X86_64,
                crate::Feature::SSE,
                crate::Feature::SSE2,
            ]
        );
        assert!(crate::TargetConfig::parse("x86", None, None).is_err());
    }

    #[test]
    fn generated_abi_matches_sysv_register_convention() {
        let abi = crate::isa::default_abi();
        let int_args = abi
            .args
            .iter()
            .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Int)
            .unwrap();
        let int_rets = abi
            .rets
            .iter()
            .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Int)
            .unwrap();
        let float_args = abi
            .args
            .iter()
            .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Float)
            .unwrap();
        let float_rets = abi
            .rets
            .iter()
            .find(|sequence| sequence.kind == tir::backend::abi::ValueKind::Float)
            .unwrap();

        assert_eq!(abi.name, "sysv");
        assert_eq!(abi.sp, (int_args.regs[0].0, 4));
        assert_eq!(abi.ra, None);
        assert_eq!(abi.fp, Some((int_args.regs[0].0, 5)));
        assert_eq!(abi.stack.align, 16);
        assert_eq!(abi.stack.slot_size, 8);
        assert_eq!(abi.stack.save_style, tir::backend::abi::SaveStyle::PushPop);
        assert_eq!(
            int_args
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            vec![7, 6, 2, 1, 8, 9]
        );
        assert_eq!(
            int_rets
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(
            float_args
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            (0..=7).collect::<Vec<_>>()
        );
        assert_eq!(
            float_rets
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            abi.callee_saved
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            vec![3, 5, 12, 13, 14, 15]
        );
    }
}
