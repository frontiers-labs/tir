use tir::helpers::operation;
use tir::{ResourceAccess, ResourceEffect, ResourceEffects, Terminator, builtin::StateResource};

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

operation! {
    SectionOp {
        name: "section",
        dialect: "builtin",
        scheme: "() -> ()",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SectionEndOp {
        name: "section_end",
        dialect: "builtin",
        scheme: "() -> ()",
        interfaces: [Terminator],
    }
}

impl Terminator for SectionEndOp {}

operation! {
    SymbolOp {
        name: "symbol",
        dialect: "builtin",
        scheme: "() -> ()",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SymbolEndOp {
        name: "symbol_end",
        dialect: "builtin",
        scheme: "() -> ()",
        interfaces: [Terminator],
    }
}

impl Terminator for SymbolEndOp {}

// A data definition directive (`.dword 42`, `.string "hi"`, `.space 16`).
// `kind` names the directive, `value` holds the literal (Int or Str).
operation! {
    LiteralOp {
        name: "literal",
        dialect: "builtin",
        scheme: "() -> ()",
        attributes: A {
            kind: "Str",
        }
    }
}

// The address of a symbol, materialized inside a function. Module-level λ and δ
// values are not reachable from machine code, so the module prologue rewrites
// the uses of one into this op, and each target lowers it to the
// relocation-bearing form its assembler spells.
operation! {
    SymbolAddressOp {
        name: "symbol_address",
        dialect: "builtin",
        scheme: "() -> ptr.p<_>",
        attributes: A {
            sym_name: "Str",
        },
        results: R { result },
        interfaces: [tir::Pure, tir::Speculatable],
    }
}

impl tir::Speculatable for SymbolAddressOp {}

impl tir::Pure for SymbolAddressOp {}

/// The address of `name`, as an opaque pointer.
pub fn symbol_address_of(context: &tir::Context, name: &str) -> SymbolAddressOp {
    SymbolAddressOpBuilder::new(context)
        .sym_name(name)
        .result_type(tir::ptr::PtrType::opaque(context))
        .build()
}

operation! {
    DataRelocOp {
        name: "data_reloc",
        dialect: "builtin",
        scheme: "() -> ()",
        attributes: A {
            symbol: "Str",
            width: "UInt",
            addend: "Int",
        }
    }
}

operation! {
    VirtualCallOp {
        name: "vcall",
        dialect: "son",
        scheme: "(_*) -> _*",
        operands: O { args },
        results: R { results },
        attributes: A {
            callee: "Str",
            outgoing_stack_size: "UInt",
        },
        interfaces: [tir::backend::MachineInstruction, ResourceEffects],
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
        dialect: "son",
        scheme: "(backend.reg<_, _>, _*) -> _*",
        operands: O { callee, args },
        results: R { results },
        attributes: A {
            outgoing_stack_size: "UInt",
        },
        interfaces: [tir::backend::MachineInstruction, ResourceEffects],
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

fn call_effects(op: &tir::OpHandle) -> Vec<ResourceEffect> {
    [StateResource::Memory, StateResource::FpEnv]
        .into_iter()
        .map(|resource| ResourceEffect {
            resource,
            access: ResourceAccess::Change,
            observed: op
                .state_operands()
                .into_iter()
                .filter(|value| {
                    op.context.state_resource(op.context.get_value(*value).ty()) == Some(resource)
                })
                .collect(),
            produced: op
                .state_results()
                .into_iter()
                .filter(|value| {
                    op.context.state_resource(op.context.get_value(*value).ty()) == Some(resource)
                })
                .collect(),
        })
        .collect()
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
