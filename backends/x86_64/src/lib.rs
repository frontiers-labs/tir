//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

pub use isa::{X86_64Dialect, instruction_encoders, instruction_patchers};

mod isa {
    // Generated code: not everything is used without a `TargetMachine` impl.
    #![allow(dead_code, unused_variables, unused_mut, clippy::all)]

    use tir::Operation;
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
                // Control flow
                JmpOp,
                CallOp,
                RetOp
            ],
        }
    }

    impl X86_64Dialect {
        pub fn get_asm_parser(&self) -> tir_be_common::AsmParser {
            tir_be_common::AsmParser::new(get_instruction_parsers(Feature::ALL).0)
        }

        pub fn get_asm_printer(&self) -> tir_be_common::AsmPrinter {
            tir_be_common::AsmPrinter::new(get_instruction_printers())
        }
    }

    pub fn instruction_encoders()
    -> std::collections::HashMap<String, tir_be_common::binary::InstructionEncoder> {
        get_instruction_encoders()
    }

    pub fn instruction_patchers()
    -> std::collections::HashMap<String, tir_be_common::binary::InstructionPatcher> {
        get_instruction_patchers()
    }
}
