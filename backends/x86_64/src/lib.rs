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
                // Control flow
                JmpOp,
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
                .with_op_lowering(lower_func_and_return_to_asm_symbol)
        }

        fn regalloc_pass(&self) -> tir::backend::regalloc::RegisterAllocationPass {
            tir::backend::regalloc::RegisterAllocationPass::new(Box::new(X86RegAlloc))
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
