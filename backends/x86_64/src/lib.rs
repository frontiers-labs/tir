//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

pub use isa::X86_64Dialect;

mod isa {
    // Generated code: not everything is used by this asm-focused prototype.
    #![allow(dead_code, unused_variables, unused_mut, clippy::all)]

    use tir::Operation;
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::helpers::{dialect, operation};

    include!(concat!(env!("OUT_DIR"), "/x86_64.rs"));

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
                CallIndirectOp
            ],
        }
    }

    impl X86_64Dialect {
        pub fn get_asm_printer(&self) -> tir_be_common::AsmPrinter {
            tir_be_common::AsmPrinter::new(get_instruction_printers())
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

    impl tir_be_common::regalloc::TargetRegAlloc for X86RegAlloc {
        fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
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

    fn object_format() -> tir_be_common::binary::ObjectFormatInfo {
        use tir_be_common::binary::{ElfClass, ObjectFormatInfo};
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

    impl tir_be_common::TargetMachine for X86Target {
        fn name(&self) -> &'static str {
            "x86_64"
        }

        fn register_dialects(&self, context: &tir::Context) {
            context.register_dialect::<tir_be_common::AsmDialect>();
            context.register_dialect::<X86_64Dialect>();
        }

        fn isel_pass(&self, context: &tir::Context) -> tir_be_common::isel::InstructionSelectPass {
            tir_be_common::isel::InstructionSelectPass::new(get_isel_rules(context, Feature::ALL))
        }

        fn regalloc_pass(&self) -> tir_be_common::regalloc::RegisterAllocationPass {
            tir_be_common::regalloc::RegisterAllocationPass::new(Box::new(X86RegAlloc))
        }

        fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
            register_info()
        }

        fn asm_parser(&self, _context: &tir::Context) -> tir_be_common::AsmParser {
            let (parsers, disabled) = get_instruction_parsers(Feature::ALL);
            tir_be_common::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
        }

        fn asm_printer(&self, context: &tir::Context) -> tir_be_common::AsmPrinter {
            context
                .find_dialect::<X86_64Dialect>()
                .expect("x86_64 dialect must be registered before building an asm printer")
                .get_asm_printer()
        }

        fn machine_model(&self, name: &str) -> Option<tir_be_common::sched::MachineModel> {
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

        fn object_format(&self) -> Option<tir_be_common::binary::ObjectFormatInfo> {
            Some(object_format())
        }

        fn binary_writer(
            &self,
            _context: &tir::Context,
        ) -> Option<tir_be_common::binary::BinaryWriter> {
            Some(tir_be_common::binary::BinaryWriter::new(
                get_instruction_encoders(),
                get_instruction_patchers(),
            ))
        }
    }

    fn select_x86_64(
        march: &str,
        _mcpu: Option<&str>,
        _mattr: Option<&str>,
    ) -> Result<Option<Box<dyn tir_be_common::TargetMachine>>, String> {
        match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "x86_64" | "amd64" | "x64" => Ok(Some(Box::new(X86Target))),
            _ => Ok(None),
        }
    }

    tir_be_common::register_target!(select_x86_64, ["x86_64"]);
}
