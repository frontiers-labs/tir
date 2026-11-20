use tir::{
    Operation, ResourceAccess, ResourceEffect, ResourceEffects, Terminator, builtin::StateResource,
};

use super::{ControlFlow, InstrInfo, MachineInstruction};

const fn virtual_info(name: &'static str, control_flow: ControlFlow) -> InstrInfo {
    InstrInfo {
        name,
        mnemonic: name,
        control_flow,
        program: tir::backend::exec::Program::Unsupported("virtual operation"),
        ..InstrInfo::BASE
    }
}

const fn virtual_call_info(name: &'static str) -> InstrInfo {
    InstrInfo {
        effects: tir::backend::MemoryEffects {
            reads: true,
            writes: true,
        },
        ..virtual_info(name, ControlFlow::Unconditional)
    }
}

__cdsl_asm_SectionOp!();

__cdsl_asm_SectionEndOp!();

impl Terminator for SectionEndOp {}

__cdsl_asm_SymbolOp!();

__cdsl_asm_SymbolEndOp!();

impl Terminator for SymbolEndOp {}

// A data definition directive (`.dword 42`, `.string "hi"`, `.space 16`).
// `kind` names the directive, `value` holds the literal (Int or Str).
__cdsl_asm_LiteralOp!();

// The address of a symbol, materialized inside a function. Module-level λ and δ
// values are not reachable from machine code, so the module prologue rewrites
// the uses of one into this op, and each target lowers it to the
// relocation-bearing form its assembler spells.
__cdsl_asm_SymbolAddressOp!();

impl tir::Speculatable for SymbolAddressOp {}

impl tir::Pure for SymbolAddressOp {}

/// The address of `name`, as an opaque pointer.
pub fn symbol_address_of(context: &tir::Context, name: &str) -> SymbolAddressOp {
    SymbolAddressOpBuilder::new(context)
        .sym_name(name)
        .result_type(tir::ptr::PtrType::opaque(context))
        .build()
}

__cdsl_asm_DataRelocOp!();

__cdsl_asm_BlockEndOp!();

impl Terminator for BlockEndOp {}

__cdsl_asm_VirtualReturnOp!();

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

__cdsl_asm_VirtualBranchOp!();

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

// A parallel copy: every result takes its source at once, as one point. The
// register allocator splits the live ranges at a constrained instruction with
// one (Hack's thesis, paper § 4) and sequences it on the assigned registers.
__cdsl_asm_ParallelCopyOp!();

impl MachineInstruction for ParallelCopyOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = virtual_info("pcopy", ControlFlow::None);
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

__cdsl_asm_VirtualCallOp!();

impl MachineInstruction for VirtualCallOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = virtual_call_info("vcall");
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

__cdsl_asm_VirtualIndirectCallOp!();

impl MachineInstruction for VirtualIndirectCallOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = virtual_call_info("vcall_indirect");
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

fn call_effects(op: &tir::OpHandle) -> Vec<ResourceEffect> {
    vec![ResourceEffect {
        resource: StateResource::Memory,
        access: ResourceAccess::Change,
        observed: op.state_operands().into_vec(),
        produced: op.state_results().into_vec(),
    }]
}

impl ResourceEffects for VirtualCallOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        call_effects(&self.0)
    }
}

impl ResourceEffects for VirtualIndirectCallOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        call_effects(&self.0)
    }
}
