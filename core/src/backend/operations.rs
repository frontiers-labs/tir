use tir::helpers::operation;
use tir::{Any, Operation, Terminator};

use super::{ControlFlow, InstrInfo, MachineInstruction};

/// The `InstrInfo` of a virtual op: it names itself and how it transfers
/// control, and nothing else. A virtual op has no encoding, no assembly syntax
/// and no schedule — it is replaced before any of those are consulted.
const fn virtual_info(name: &'static str, control_flow: ControlFlow) -> InstrInfo {
    InstrInfo {
        name,
        mnemonic: name,
        control_flow,
        program: tir::backend::exec::Program::Unsupported("virtual operation"),
        ..InstrInfo::BASE
    }
}

/// [`virtual_info`] for a call: it runs a function, so it touches every object
/// the outside can reach and carries the chain of them.
const fn virtual_call_info(name: &'static str) -> InstrInfo {
    InstrInfo {
        effects: tir::backend::MemoryEffects {
            reads: true,
            writes: true,
        },
        ..virtual_info(name, ControlFlow::Unconditional)
    }
}

operation! {
    SectionOp {
        name: "section",
        dialect: "asm",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SectionEndOp {
        name: "section_end",
        dialect: "asm",
        interfaces: [Terminator],
    }
}

impl Terminator for SectionEndOp {}

operation! {
    SymbolOp {
        name: "symbol",
        dialect: "asm",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SymbolEndOp {
        name: "symbol_end",
        dialect: "asm",
        interfaces: [Terminator],
    }
}

impl Terminator for SymbolEndOp {}

// A data definition directive (`.dword 42`, `.string "hi"`, `.space 16`).
// `kind` names the directive, `value` holds the literal (Int or Str).
operation! {
    LiteralOp {
        name: "literal",
        dialect: "asm",
        attributes: A {
            kind: "Str",
        }
    }
}

operation! {
    DataRelocOp {
        name: "data_reloc",
        dialect: "asm",
        attributes: A {
            symbol: "Str",
            width: "UInt",
            addend: "Int",
        }
    }
}

operation! {
    BlockEndOp {
        name: "block_end",
        dialect: "asm",
        interfaces: [Terminator],
    }
}

impl Terminator for BlockEndOp {}

operation! {
    VirtualReturnOp {
        name: "vret",
        dialect: "asm",
        format: "custom",
        operands: O {
            values: "*Any",
        },
        interfaces: [Terminator, tir::backend::MachineInstruction],
    }
}

impl VirtualReturnOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        super::print_branch(fmt, self)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

impl VirtualReturnOpBuilder {
    pub fn value(self, value: tir::ValueId) -> Self {
        self.values(vec![value])
    }
}

impl Terminator for VirtualReturnOp {}

impl MachineInstruction for VirtualReturnOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = virtual_info("vret", ControlFlow::Unconditional);
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

operation! {
    VirtualBranchOp {
        name: "vbr",
        dialect: "asm",
        format: "custom",
        operands: O {
            dest_args: "*Any",
        },
        attributes: A {
            dest: "Block",
        },
        interfaces: [Terminator, tir::backend::MachineInstruction],
    }
}

impl MachineInstruction for VirtualBranchOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = virtual_info("vbr", ControlFlow::Unconditional);
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

impl Terminator for VirtualBranchOp {
    fn successors(&self) -> Vec<tir::BlockId> {
        super::branch_successors(self)
    }
}

impl VirtualBranchOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        super::print_branch(fmt, self)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

operation! {
    VirtualCallOp {
        name: "vcall",
        dialect: "asm",
        attributes: A {
            callee: "Str",
            outgoing_stack_size: "UInt",
        },
        interfaces: [tir::backend::MachineInstruction],
        state: "in_out",
    }
}

impl MachineInstruction for VirtualCallOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = virtual_call_info("vcall");
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

operation! {
    VirtualIndirectCallOp {
        name: "vcall_indirect",
        dialect: "asm",
        operands: O {
            callee: "tir::backend::RegClassType",
        },
        attributes: A {
            outgoing_stack_size: "UInt",
        },
        interfaces: [tir::backend::MachineInstruction],
        state: "in_out",
    }
}

impl MachineInstruction for VirtualIndirectCallOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = virtual_call_info("vcall_indirect");
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

impl VirtualCallOpBuilder {
    pub fn outgoing_stack_size(self, size: u64) -> Self {
        self.attr(
            "outgoing_stack_size",
            tir::attributes::AttributeValue::UInt(size),
        )
    }
}

impl VirtualIndirectCallOpBuilder {
    pub fn outgoing_stack_size(self, size: u64) -> Self {
        self.attr(
            "outgoing_stack_size",
            tir::attributes::AttributeValue::UInt(size),
        )
    }
}

impl VirtualCallOp {
    pub fn outgoing_stack_size(&self) -> u64 {
        outgoing_stack_size(self)
    }
}

impl VirtualIndirectCallOp {
    pub fn outgoing_stack_size(&self) -> u64 {
        outgoing_stack_size(self)
    }
}

fn outgoing_stack_size(op: &impl Operation) -> u64 {
    op.attr("outgoing_stack_size")
        .and_then(|value| match value {
            tir::attributes::AttributeValue::UInt(value) => Some(value),
            _ => None,
        })
        .expect("verified virtual calls have an outgoing stack size")
}
