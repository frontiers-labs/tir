//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

const MODEL_CHECK_SOURCES: &[(&str, &str)] = &[
    ("main.tmdl", include_str!("../defs/main.tmdl")),
    ("base.tmdl", include_str!("../defs/base.tmdl")),
    ("arith_ext.tmdl", include_str!("../defs/arith_ext.tmdl")),
    ("conditional.tmdl", include_str!("../defs/conditional.tmdl")),
    ("memory_ext.tmdl", include_str!("../defs/memory_ext.tmdl")),
    ("float.tmdl", include_str!("../defs/float.tmdl")),
];

pub use isa::{Feature, get_isel_rules, register_info, register_views, register_widths};
pub use isa::{TargetConfig, X86_64Dialect};

mod isa {
    // Generated code: not everything is used by this asm-focused prototype.
    #![allow(dead_code, unused_variables, unused_mut, clippy::all)]

    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::helpers::{dialect, operation};
    use tir::{Any, Operation};

    include!(concat!(env!("OUT_DIR"), "/x86_64.rs"));

    // Virtual return: the lowered form of `builtin.return`, deferring the actual
    // `ret` sequence. Register allocation matches it by name to pin its operand
    // to the return register.
    operation! {
        VirtualReturnOp {
            name: "vret",
            dialect: "x86_64",
            operands: [value],
            interfaces: [tir::Terminator],
        }
    }

    impl tir::Terminator for VirtualReturnOp {}

    // Virtual unconditional branch: emitted by selection for `builtin.br` and
    // conditional-branch fallthroughs, finalized to `jmp` after register
    // allocation (its block arguments must be colored first).
    operation! {
        VirtualBranchOp {
            name: "vbr",
            dialect: "x86_64",
            format: "custom",
            operands: O {
                dest_args: "*Any",
            },
            attributes: A {
                dest: "Block",
            },
            interfaces: [tir::Terminator],
        }
    }

    impl tir::Terminator for VirtualBranchOp {
        fn successors(&self) -> Vec<tir::BlockId> {
            tir::backend::branch_successors(self)
        }
    }

    // Virtual call ops: the lowered form of `builtin.call`/`builtin.indirect_call`.
    // Arguments and results travel through the ABI registers via copies emitted by
    // `lower_calls`; the ops only carry the callee (a symbol relocated at link time,
    // or an already-colored register) plus the caller-saved clobber list, deferring
    // the actual `call` encoding to a post-RA pass.
    operation! {
        VirtualCallOp {
            name: "vcall",
            dialect: "x86_64",
            attributes: A {
                callee: "Str",
            },
            roles: R {
                clobbers: Clobber,
            },
        }
    }

    operation! {
        VirtualIndirectCallOp {
            name: "vcall_indirect",
            dialect: "x86_64",
            attributes: A {
                callee_reg: "Register",
            },
            roles: R {
                callee_reg: Use,
                clobbers: Clobber,
            },
        }
    }

    impl VirtualBranchOp {
        fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
            tir::backend::print_branch(fmt, self, "x86_64.vbr")
        }

        fn custom_parse(
            parser: &mut tir::parse::text::Parser,
            _context: &tir::Context,
        ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
            Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
        }
    }

    dialect! {
        X86_64Dialect {
            name: "x86_64",
            operations: [VirtualReturnOp, VirtualBranchOp, VirtualCallOp, VirtualIndirectCallOp],
            operation_file: concat!(env!("OUT_DIR"), "/x86_64_ops.rs"),
        }
    }

    fn lower_func_and_return_to_asm_symbol(
        context: &tir::Context,
        op: &tir::OperationRef,
        rewriter: &mut tir::Rewriter,
    ) -> Result<bool, tir::PassError> {
        use tir::Operation;
        use tir::builtin::{FuncOp, ReturnOp};

        if let Some(func) = op.as_op::<FuncOp>() {
            // asm.symbol regions require an explicit symbol_end terminator.
            let body = func.body();
            let has_symbol_end = body
                .op_ids()
                .last()
                .map(|id| context.get_op(*id).name == tir::backend::SymbolEndOp::name())
                .unwrap_or(false);
            if !has_symbol_end {
                let mut b = tir::IRBuilder::new(body);
                b.insert(tir::backend::SymbolEndOpBuilder::new(context).build());
            }

            let sym_name = func
                .attributes()
                .iter()
                .find(|a| a.name == "sym_name")
                .and_then(|a| match &a.value {
                    AttributeValue::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown".to_string());

            let arg_regs = func
                .body()
                .arguments()
                .iter()
                .map(|arg| {
                    AttributeValue::Register(RegisterAttr::Virtual {
                        id: arg.id().number(),
                        class: Some(RegClass::GPR.id()),
                    })
                })
                .collect::<Vec<_>>();

            let lowered = tir::backend::SymbolOpBuilder::new(context)
                .body(op.op().regions[0])
                .attr("name", AttributeValue::Str(sym_name))
                .attr("arg_regs", AttributeValue::Array(arg_regs))
                .build();
            rewriter.replace_op(op, &lowered)?;
            return Ok(true);
        }

        if let Some(ret) = op.as_op::<ReturnOp>() {
            let mut builder = VirtualReturnOpBuilder::new(context);
            if let Some(value) = ret.operands().first().copied() {
                builder = builder.value(value);
            }
            let lowered = builder.build();
            rewriter.replace_op(op, &lowered)?;
            return Ok(true);
        }

        Ok(false)
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

    /// Emit the deferred unconditional branch (`vbr`, finalized to `jmp` after
    /// register allocation), forwarding any block arguments.
    fn emit_uncond_branch(
        context: &tir::Context,
        dest: tir::BlockId,
        args: &[tir::ValueId],
    ) -> Box<dyn Operation> {
        Box::new(
            VirtualBranchOpBuilder::new(context)
                .dest_args(args.to_vec())
                .attr("dest", AttributeValue::Block(dest))
                .build(),
        )
    }

    /// Emit the branch-if-nonzero fallback for a condition no branch rule
    /// fused: `test cond, cond` + `jne dest`.
    fn emit_branch_nonzero(
        context: &tir::Context,
        condition: tir::ValueId,
        dest: tir::BlockId,
    ) -> Vec<Box<dyn Operation>> {
        vec![
            Box::new(
                TestOpBuilder::new(context)
                    .attr("dst", virt(condition.number(), RegClass::GPR.id()))
                    .attr("src", virt(condition.number(), RegClass::GPR.id()))
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
        let value = tir::backend::int_attr(constant.attributes(), "value").ok_or_else(|| {
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
        use tir::builtin::AddressOfOp;

        let Some(addr_of) = op.as_op::<AddressOfOp>() else {
            return Ok(false);
        };
        let lea = LeaRipOpBuilder::new(context)
            .attr("dst", virt(addr_of.result().number(), RegClass::GPR.id()))
            .attr("imm", AttributeValue::Str(addr_of.sym_name()))
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

    struct X86CallEmitter;

    impl tir::backend::call_lowering::CallEmitter for X86CallEmitter {
        fn copy(
            &self,
            context: &tir::Context,
            dst: AttributeValue,
            src: AttributeValue,
        ) -> Box<dyn Operation> {
            mv(context, dst, src)
        }

        fn vcall(
            &self,
            context: &tir::Context,
            callee: String,
            clobbers: AttributeValue,
        ) -> Box<dyn Operation> {
            Box::new(
                VirtualCallOpBuilder::new(context)
                    .attr("callee", AttributeValue::Str(callee))
                    .attr("clobbers", clobbers)
                    .build(),
            )
        }

        fn vcall_indirect(
            &self,
            context: &tir::Context,
            callee: AttributeValue,
            clobbers: AttributeValue,
        ) -> Box<dyn Operation> {
            Box::new(
                VirtualIndirectCallOpBuilder::new(context)
                    .attr("callee_reg", callee)
                    .attr("clobbers", clobbers)
                    .build(),
            )
        }

        fn stack_arg_store(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            value: AttributeValue,
            offset: i64,
        ) -> Result<Box<dyn Operation>, tir::PassError> {
            Ok(Box::new(
                MovStoreDispOpBuilder::new(context)
                    .attr("base", phys(abi.sp.0, abi.sp.1))
                    .attr("imm", AttributeValue::Int(offset))
                    .attr("src", value)
                    .build(),
            ))
        }

        fn call_prefix(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            outgoing_size: u32,
        ) -> Vec<Box<dyn Operation>> {
            let reserved = i64::from(outgoing_size) + 8;
            vec![
                Box::new(
                    AddImmOpBuilder::new(context)
                        .attr("dst", phys(abi.sp.0, abi.sp.1))
                        .attr("imm", AttributeValue::Int(-reserved))
                        .build(),
                ),
                Box::new(
                    MovImm32OpBuilder::new(context)
                        .attr("dst", phys(RegClass::GPR32.id(), 0))
                        .attr("imm", AttributeValue::Int(0))
                        .build(),
                ),
            ]
        }

        fn call_suffix(
            &self,
            context: &tir::Context,
            abi: &tir::backend::abi::AbiInfo,
            outgoing_size: u32,
        ) -> Vec<Box<dyn Operation>> {
            let reserved = i64::from(outgoing_size) + 8;
            vec![Box::new(
                AddImmOpBuilder::new(context)
                    .attr("dst", phys(abi.sp.0, abi.sp.1))
                    .attr("imm", AttributeValue::Int(reserved))
                    .build(),
            )]
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
            op.attributes().iter().find_map(|a| match &a.value {
                AttributeValue::Register(RegisterAttr::Physical { index, .. })
                    if a.name == "base" =>
                {
                    Some(*index)
                }
                _ => None,
            })
        }
        fn attr(op: &dyn Operation, name: &str) -> AttributeValue {
            op.attributes()
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.value.clone())
                .expect("memory op operand attribute present")
        }
        // The allocated index of a physical register operand, or None if the
        // operand is still virtual (canonicalization runs post-RA, so a virtual
        // operand simply means "not this op" and is left unchanged).
        fn reg_index(op: &dyn Operation, name: &str) -> Option<u16> {
            op.attributes().iter().find_map(|a| match &a.value {
                AttributeValue::Register(RegisterAttr::Physical { index, .. })
                    if a.name == name =>
                {
                    Some(*index)
                }
                _ => None,
            })
        }
        // An immediate operand's integer value, or None for a symbol reference
        // (a relocation that cannot fold to the sign-extended imm8 form).
        fn imm_int(op: &dyn Operation, name: &str) -> Option<i64> {
            op.attributes().iter().find_map(|a| match &a.value {
                AttributeValue::Int(v) if a.name == name => Some(*v),
                _ => None,
            })
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
        escape!(
            Mov32LoadOp,
            Mov32LoadSibOpBuilder,
            Mov32LoadRbpOpBuilder,
            ["dst", "base"]
        );
        escape!(
            Mov32StoreOp,
            Mov32StoreSibOpBuilder,
            Mov32StoreRbpOpBuilder,
            ["base", "src"]
        );
        escape!(
            Mov16StoreOp,
            Mov16StoreSibOpBuilder,
            Mov16StoreRbpOpBuilder,
            ["base", "src"]
        );
        escape!(
            Mov8StoreOp,
            Mov8StoreSibOpBuilder,
            Mov8StoreRbpOpBuilder,
            ["base", "src"]
        );
        escape!(sib MovLoadDispOp, MovLoadDispSibOpBuilder, ["dst", "base", "imm"]);
        escape!(sib MovStoreDispOp, MovStoreDispSibOpBuilder, ["base", "imm", "src"]);

        // REX-free canonicalization: drop the REX byte GNU as omits when every
        // register index is low (< 8, or < 4 for the 8-bit forms that must avoid
        // the spl/bpl/sil/dil encodings), and fold group-1 immediates that fit a
        // sign-extended i8 into the 0x83 short form. Each op maps to exactly one
        // behavior-free variant, so selection is unaffected.
        const LO: u16 = 8;
        const B: u16 = 4;

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
        reg1_norex!(ShlCl32Op, ShlCl32NorexOpBuilder, "dst", LO);
        reg1_norex!(ShrCl32Op, ShrCl32NorexOpBuilder, "dst", LO);
        reg1_norex!(SarCl32Op, SarCl32NorexOpBuilder, "dst", LO);
        ri_norex!(RolImm32Op, RolImm32NorexOpBuilder, LO);
        ri_norex!(RorImm32Op, RorImm32NorexOpBuilder, LO);

        // Low-xmm SSE: drop the empty REX when both xmm operands are xmm0..xmm7.
        rr_norex!(AddsdOp, AddsdNorexOpBuilder, LO);
        rr_norex!(SubsdOp, SubsdNorexOpBuilder, LO);
        rr_norex!(MulsdOp, MulsdNorexOpBuilder, LO);
        rr_norex!(DivsdOp, DivsdNorexOpBuilder, LO);
        rr_norex!(MovsdOp, MovsdNorexOpBuilder, LO);

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
            let dest = br
                .attributes()
                .iter()
                .find_map(|attr| match (&attr.value, attr.name == "dest") {
                    (AttributeValue::Block(block), true) => Some(*block),
                    _ => None,
                })
                .ok_or_else(|| {
                    tir::PassError::InvalidRuleSet(
                        "branch is missing its 'dest' target".to_string(),
                    )
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
            let callee = call
                .attributes()
                .iter()
                .find_map(|attr| match (&attr.value, attr.name == "callee") {
                    (AttributeValue::Str(s), true) => Some(s.clone()),
                    _ => None,
                })
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
            let target = call
                .attributes()
                .iter()
                .find_map(|attr| match (&attr.value, attr.name == "callee_reg") {
                    (value @ AttributeValue::Register(_), true) => Some(value.clone()),
                    _ => None,
                })
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

    /// Register allocation target. Frame adjustment is `add rsp, ±size`; GPR
    /// spills use the displacement-based `mov` memory forms.
    struct X86RegAlloc;

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
            match class.name() {
                "GPR" => Box::new(
                    MovStoreDispOpBuilder::new(context)
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .attr("src", virt(value, class))
                        .build(),
                ),
                other => unimplemented!("x86-64 spilling for {other} is not implemented"),
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
            match class.name() {
                "GPR" => Box::new(
                    MovLoadDispOpBuilder::new(context)
                        .attr("dst", virt(value, class))
                        .attr("base", phys(frame.0, frame.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ),
                other => unimplemented!("x86-64 spilling for {other} is not implemented"),
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
            match class.name() {
                "GPR" => Box::new(
                    MovOpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                "GPR32" => Box::new(
                    Mov32OpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                "GPR16" => Box::new(
                    Mov16OpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                "GPR8" => Box::new(
                    Mov8OpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                "GPR8H" => Box::new(
                    Mov8HOpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                "XMM" => Box::new(
                    MovsdOpBuilder::new(context)
                        .attr("dst", virt(dst))
                        .attr("src", virt(src))
                        .build(),
                ),
                other => unreachable!("unknown x86-64 register class {other}"),
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
            // Each `push` shifts `rsp` by 8; pad so the total prologue adjustment
            // stays a multiple of 16, keeping `rsp` 16-byte aligned at call sites.
            let total = size as i64 + saved_frame_pad(saves.len());
            if total > 0 {
                ops.push(Box::new(
                    AddImmOpBuilder::new(context)
                        .attr("dst", phys(abi.sp.0, abi.sp.1))
                        .attr("imm", AttributeValue::Int(-total))
                        .build(),
                ));
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
            let total = size as i64 + saved_frame_pad(saves.len());
            if total > 0 {
                ops.push(Box::new(
                    AddImmOpBuilder::new(context)
                        .attr("dst", phys(abi.sp.0, abi.sp.1))
                        .attr("imm", AttributeValue::Int(total))
                        .build(),
                ));
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

        fn incoming_stack_arg_offset(
            &self,
            _abi: &tir::backend::abi::AbiInfo,
            frame_size: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
            stack_index: usize,
        ) -> i64 {
            let saved = saves.len() as i64 * 8;
            let locals = frame_size as i64 + saved_frame_pad(saves.len());
            saved + locals + 8 + stack_index as i64 * 8
        }

        fn emit_incoming_stack_arg_load(
            &self,
            context: &tir::Context,
            dst: &tir::backend::liveness::PhysReg,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Result<Box<dyn Operation>, tir::PassError> {
            if dst.0.name() != "GPR" {
                return Err(tir::PassError::InvalidRuleSet(format!(
                    "x86-64 stack arguments for register class {} are not supported",
                    dst.0.name()
                )));
            }
            Ok(Box::new(
                MovLoadDispOpBuilder::new(context)
                    .attr("dst", phys(dst.0, dst.1))
                    .attr("base", phys(frame.0, frame.1))
                    .attr("imm", AttributeValue::Int(offset))
                    .build(),
            ))
        }

        fn emit_frame_address(
            &self,
            context: &tir::Context,
            dst: &tir::backend::liveness::PhysReg,
            frame: &tir::backend::liveness::PhysReg,
            offset: i64,
        ) -> Result<Vec<Box<dyn Operation>>, tir::PassError> {
            if dst.0.name() != "GPR" {
                return Err(tir::PassError::InvalidRuleSet(format!(
                    "x86-64 stack allocation addresses for register class {} are not supported",
                    dst.0.name()
                )));
            }
            let mut ops = vec![mv(context, phys(dst.0, dst.1), phys(frame.0, frame.1))];
            if offset != 0 {
                ops.push(Box::new(
                    AddImmOpBuilder::new(context)
                        .attr("dst", phys(dst.0, dst.1))
                        .attr("imm", AttributeValue::Int(offset))
                        .build(),
                ));
            }
            Ok(ops)
        }
    }

    /// Extra stack padding (0 or 8 bytes) so `push`ing `count` callee-saved
    /// registers leaves the total prologue adjustment 16-byte aligned.
    fn saved_frame_pad(count: usize) -> i64 {
        if count % 2 == 1 { 8 } else { 0 }
    }

    // R_X86_64_PC32 = 2, R_X86_64_PLT32 = 4. Both scatter `S + A - P` into a
    // 4-byte pc-relative field; the addend is -4 because `P` addresses the field
    // start while the displacement is measured from the instruction's end.
    const R_X86_64_PC32: u32 = 2;
    const R_X86_64_PLT32: u32 = 4;

    fn object_format() -> tir::backend::binary::ObjectFormatInfo {
        use tir::backend::binary::{EM_X86_64, ElfClass, ObjectFormatInfo, RelocKind};
        ObjectFormatInfo {
            elf_machine: EM_X86_64,
            elf_class: ElfClass::Elf64,
            elf_flags: 0,
            reloc_for: |op| match op {
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
                    op,
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
            _mattr: Option<&str>,
        ) -> Result<Self, String> {
            match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "x86_64" | "amd64" | "x64" => {}
                other => return Err(format!("unknown x86-64 architecture '{other}'")),
            }
            let features = vec![Feature::X86, Feature::X86_64];
            validate_features(&features)?;
            Ok(Self { features })
        }

        /// The enabled ISA set.
        pub fn features(&self) -> &[Feature] {
            &self.features
        }
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

        fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
            tir::backend::isel::InstructionSelectPass::new(get_isel_rules(
                context,
                self.config.features(),
            ))
            .with_axioms(include_str!("isel.axioms"))
            .with_branch_emitters(tir::backend::isel::BranchEmitters {
                uncond: emit_uncond_branch,
                cond_nonzero: emit_branch_nonzero,
            })
            .with_op_lowering(lower_func_and_return_to_asm_symbol)
            .with_call_lowering(self.abi(), Box::new(X86CallEmitter))
        }

        fn regalloc_pass(&self) -> tir::backend::regalloc::RegisterAllocationPass {
            tir::backend::regalloc::RegisterAllocationPass::with_abi(
                Box::new(X86RegAlloc),
                self.abi(),
            )
        }

        fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![lower_constant, lower_addr_of]
        }

        fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![canonicalize_encodings, finalize_virtual_ops]
        }

        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            register_info()
        }

        fn abis(&self) -> &'static [tir::backend::abi::AbiInfo] {
            abis()
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
                    Some(name) => abi_by_name(name).ok_or_else(|| {
                        format!(
                            "unknown ABI '{name}' for x86_64 (available: {})",
                            abis()
                                .iter()
                                .map(|abi| abi.name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
                    None => default_abi(),
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

    #[cfg(test)]
    mod canonicalize_tests {
        use super::*;
        use tir::backend::lower::OpLoweringPass;
        use tir::builtin::{ModuleEndOpBuilder, ModuleOpBuilder};
        use tir::{IRBuilder, IRFormatter, PassManager};

        /// Rewrite a `mov_load` whose base is the physical register `base_index`
        /// and return the printed IR of the (single) resulting op.
        fn rewrite_load(base_index: u16) -> String {
            let context = tir::Context::with_default_dialects();
            context.register_dialect::<X86_64Dialect>();
            let module = ModuleOpBuilder::new(&context).build();
            let mut b = IRBuilder::new(module.body());
            b.insert(
                MovLoadOpBuilder::new(&context)
                    .attr("dst", phys(RegClass::GPR.id(), 0))
                    .attr("base", phys(RegClass::GPR.id(), base_index))
                    .build(),
            );
            b.insert(ModuleEndOpBuilder::new(&context).build());

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
                let mut b = IRBuilder::new(module.body());
                b.insert($build(&context));
                b.insert(ModuleEndOpBuilder::new(&context).build());
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
    fn x86_64_target_enables_its_x86_prerequisite() {
        let config = crate::TargetConfig::parse("x86_64", None, None).unwrap();
        assert_eq!(
            config.features(),
            &[crate::Feature::X86, crate::Feature::X86_64]
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

    #[test]
    #[ignore = "run with `cargo xtask axioms`"]
    fn committed_isel_axioms_are_fresh() {
        let context = tir::Context::with_default_dialects();
        let rules = crate::get_isel_rules(&context, crate::Feature::ALL);
        let discovered = tir::backend::isel::discover_axioms(&rules);
        assert_eq!(
            include_str!("isel.axioms"),
            tir::backend::isel::render_axioms_file(&discovered),
            "isel.axioms is stale; run `cargo run -p tir-tools --bin tir -- axioms --write`"
        );
    }
}
