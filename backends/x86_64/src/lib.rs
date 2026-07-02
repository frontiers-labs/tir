//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

pub use isa::X86_64Dialect;
pub use isa::{Feature, get_isel_rules};

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
        }
    }

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
            operations: [
                // Register/register ALU
                AddOp,
                SubOp,
                AndOp,
                OrOp,
                XorOp,
                MovOp,
                // Register/immediate ALU
                AddImmOp,
                AndImmOp,
                OrImmOp,
                XorImmOp,
                MovImmOp,
                // Shift by immediate
                ShlImmOp,
                ShrImmOp,
                SarImmOp,
                // Memory operands
                MovLoadOp,
                MovStoreOp,
                // EFLAGS definers
                CmpOp,
                TestOp,
                // Control flow
                JmpOp,
                JumpEqOp,
                JumpNotEqOp,
                JumpLessOp,
                JumpGreaterEqOp,
                JumpBelowOp,
                JumpAboveEqOp,
                CallOp,
                RetOp,
                JmpIndirectOp,
                CallIndirectOp,
                Add32Op,
                Sub32Op,
                And32Op,
                Or32Op,
                Xor32Op,
                Mov32Op,
                AddImm32Op,
                AndImm32Op,
                OrImm32Op,
                XorImm32Op,
                MovImm32Op,
                ShlImm32Op,
                ShrImm32Op,
                SarImm32Op,
                Add16Op,
                Sub16Op,
                And16Op,
                Or16Op,
                Xor16Op,
                Mov16Op,
                AddImm16Op,
                AndImm16Op,
                OrImm16Op,
                XorImm16Op,
                MovImm16Op,
                ShlImm16Op,
                ShrImm16Op,
                SarImm16Op,
                Add8Op,
                Sub8Op,
                And8Op,
                Or8Op,
                Xor8Op,
                Mov8Op,
                AddImm8Op,
                AndImm8Op,
                OrImm8Op,
                XorImm8Op,
                MovImm8Op,
                ShlImm8Op,
                ShrImm8Op,
                SarImm8Op,
                VirtualBranchOp,
                Add8HOp,
                Sub8HOp,
                And8HOp,
                Or8HOp,
                Xor8HOp,
                Mov8HOp,
                AddImm8HOp,
                AndImm8HOp,
                OrImm8HOp,
                XorImm8HOp,
                MovImm8HOp,
                ShlImm8HOp,
                ShrImm8HOp,
                SarImm8HOp,
                ImulOp,
                Imul32Op,
                VirtualReturnOp
            ],
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
                        class: Some("GPR".to_string()),
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

    fn virt(value: u32, class: &str) -> AttributeValue {
        AttributeValue::Register(RegisterAttr::Virtual {
            id: value,
            class: Some(class.to_string()),
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
                    .attr("dst", virt(condition.number(), "GPR"))
                    .attr("src", virt(condition.number(), "GPR"))
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
        if i32::try_from(value).is_err() {
            return Err(tir::PassError::InvalidRuleSet(format!(
                "constant {value} does not fit mov imm32; wide constant materialization is not implemented"
            )));
        }

        let mov = MovImmOpBuilder::new(context)
            .attr("dst", virt(constant.result().number(), "GPR"))
            .attr("imm", AttributeValue::Int(value))
            .build();
        rewriter.replace_op(op, &mov)?;
        Ok(true)
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

        Ok(false)
    }

    /// The x86-64 stack pointer (`rsp`, GPR index 4).
    const SP: (&str, u16) = ("GPR", 4);

    fn phys(class: &str, index: u16) -> AttributeValue {
        AttributeValue::Register(RegisterAttr::Physical {
            class: class.to_string(),
            index,
        })
    }

    /// Register allocation target. Frame adjustment is `add rsp, ±size`; spill
    /// slots would need memory operands, which this prototype's ISA does not
    /// model, so spilling is left unimplemented (no test reaches it).
    struct X86RegAlloc;

    impl tir::backend::regalloc::TargetRegAlloc for X86RegAlloc {
        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            register_info()
        }

        fn frame_register(&self) -> (String, u16) {
            (SP.0.to_string(), SP.1)
        }

        fn emit_spill_store(
            &self,
            _context: &tir::Context,
            _value: u32,
            _class: &str,
            _frame: &(String, u16),
            _offset: i64,
        ) -> Box<dyn Operation> {
            unimplemented!("x86-64 spilling needs memory operands, out of prototype scope")
        }

        fn emit_spill_reload(
            &self,
            _context: &tir::Context,
            _value: u32,
            _class: &str,
            _frame: &(String, u16),
            _offset: i64,
        ) -> Box<dyn Operation> {
            unimplemented!("x86-64 spilling needs memory operands, out of prototype scope")
        }

        fn emit_copy(
            &self,
            context: &tir::Context,
            class: &str,
            dst: u32,
            src: u32,
        ) -> Box<dyn Operation> {
            let virt = |id: u32| {
                AttributeValue::Register(RegisterAttr::Virtual {
                    id,
                    class: Some(class.to_string()),
                })
            };
            match class {
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
                other => unreachable!("unknown x86-64 register class {other}"),
            }
        }

        fn emit_prologue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
            vec![Box::new(
                AddImmOpBuilder::new(context)
                    .attr("dst", phys(SP.0, SP.1))
                    .attr("imm", AttributeValue::Int(-(size as i64)))
                    .build(),
            )]
        }

        fn emit_epilogue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
            vec![Box::new(
                AddImmOpBuilder::new(context)
                    .attr("dst", phys(SP.0, SP.1))
                    .attr("imm", AttributeValue::Int(size as i64))
                    .build(),
            )]
        }
    }

    fn object_format() -> tir::backend::binary::ObjectFormatInfo {
        use tir::backend::binary::{ElfClass, ObjectFormatInfo};
        // EM_X86_64.
        ObjectFormatInfo {
            elf_machine: 62,
            elf_class: ElfClass::Elf64,
            elf_flags: 0,
            reloc_for: |_| None,
            pc_rel_scale: |_| 0,
            // rel32 displacements are measured from the end of the instruction
            // (RIP points past the branch when the displacement applies).
            pc_rel_from_end: |op| {
                matches!(
                    op,
                    "jmp" | "je" | "jne" | "jl" | "jge" | "jb" | "jae" | "call"
                )
            },
        }
    }

    struct X86Target;

    impl tir::backend::TargetMachine for X86Target {
        fn name(&self) -> &'static str {
            "x86_64"
        }

        fn register_dialects(&self, context: &tir::Context) {
            context.register_dialect::<tir::backend::AsmDialect>();
            context.register_dialect::<X86_64Dialect>();
        }

        fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
            tir::backend::isel::InstructionSelectPass::new(get_isel_rules(context, Feature::ALL))
                .with_axioms(include_str!("isel.axioms"))
                .with_branch_emitters(tir::backend::isel::BranchEmitters {
                    uncond: emit_uncond_branch,
                    cond_nonzero: emit_branch_nonzero,
                })
                .with_op_lowering(lower_func_and_return_to_asm_symbol)
        }

        fn regalloc_pass(&self) -> tir::backend::regalloc::RegisterAllocationPass {
            tir::backend::regalloc::RegisterAllocationPass::new(Box::new(X86RegAlloc))
        }

        fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![lower_constant]
        }

        fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
            vec![finalize_virtual_ops]
        }

        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            register_info()
        }

        fn asm_parser(&self, _context: &tir::Context) -> tir::backend::AsmParser {
            let (parsers, disabled) = get_instruction_parsers(Feature::ALL);
            tir::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
        }

        fn asm_printer(&self, context: &tir::Context) -> tir::backend::AsmPrinter {
            context
                .find_dialect::<X86_64Dialect>()
                .expect("x86_64 dialect must be registered before building an asm printer")
                .get_asm_printer()
        }

        fn machine_model(&self, name: &str) -> Option<tir::backend::sched::MachineModel> {
            machine_model(name, Feature::ALL)
        }

        fn machines(&self) -> Vec<&'static str> {
            machines(Feature::ALL)
        }

        fn isa_params(&self) -> Vec<(&'static str, i64)> {
            isa_params(Feature::ALL)
        }

        fn register_widths(&self) -> Vec<(&'static str, u32)> {
            register_widths(Feature::ALL)
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
        _mcpu: Option<&str>,
        _mattr: Option<&str>,
    ) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
        match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "x86_64" | "amd64" | "x64" => Ok(Some(Box::new(X86Target))),
            _ => Ok(None),
        }
    }

    tir::register_target!(select_x86_64, ["x86_64"]);
}

#[cfg(test)]
mod tests {
    #[test]
    fn committed_isel_axioms_are_fresh() {
        let context = tir::Context::with_default_dialects();
        let discovered = tir::backend::isel::discover_axioms(&crate::get_isel_rules(
            &context,
            crate::Feature::ALL,
        ));
        assert_eq!(
            include_str!("isel.axioms"),
            tir::backend::isel::render_axioms_file(&discovered),
            "isel.axioms is stale; run `cargo run -p tir-tools --bin tir -- axioms --write`"
        );
    }
}
