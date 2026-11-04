use tir::helpers::operation;
use tir::{
    Operation, ResourceAccess, ResourceEffect, ResourceEffects, Terminator, builtin::StateResource,
};

use crate::backend::{ControlFlow, InstrInfo, MachineInstruction};

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
        dialect: "son",
        scheme: "() -> ()",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SectionEndOp {
        name: "section_end",
        dialect: "son",
        scheme: "() -> ()",
        interfaces: [Terminator],
    }
}

impl Terminator for SectionEndOp {}

operation! {
    SymbolOp {
        name: "symbol",
        dialect: "son",
        scheme: "() -> ()",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SymbolEndOp {
        name: "symbol_end",
        dialect: "son",
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
        dialect: "son",
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
        dialect: "son",
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
        dialect: "son",
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

pub use crate::backend::SonDialect;
use crate::ty::{Ctor, Kind, TypeCtor};
use crate::{Context, Error, Verifiable};
use tir_symbolic::term::{no_carrier, valid};

/// The constructor of an explicit control-flow token.
pub static CONTROL: Ctor = Ctor::new(TypeCtor {
    dialect: "son",
    name: "control",
    params: &[],
    kind: Kind::Data,
    validate: valid,
    carrier: no_carrier,
});

operation! {
    MergeOp {
        name: "merge",
        dialect: "son",
        scheme: "(son.control*) -> son.control",
        operands: O { incoming },
        results: R { control },
        regions: R { arguments: Region { kind: Nodes, } },
        verifier: "true",
    }
}

impl Verifiable for MergeOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let region = self.arguments();
        if self.operands().is_empty() || !region.op_ids().is_empty() || !region.results().is_empty()
        {
            return Err(Error::ExpectedToken(
                "incoming edges and an empty arguments region",
            ));
        }
        for incoming in self.operands() {
            let edge = context
                .get_value(incoming)
                .defining_op()
                .and_then(|id| context.get_op(id).as_op::<EdgeOp>())
                .ok_or(Error::ExpectedOperation("son", "edge"))?;
            let args = edge.operands();
            if args.len() != region.ports().len() + 1
                || args[1..]
                    .iter()
                    .zip(region.ports())
                    .any(|(&arg, port)| context.get_value(arg).ty() != port.ty())
            {
                return Err(Error::ExpectedType);
            }
        }
        Ok(())
    }
}

operation! {
    EdgeOp {
        name: "edge",
        dialect: "son",
        scheme: "(son.control, _*) -> son.control",
        operands: O { control, arguments },
        results: R { edge },
        state: "in",
    }
}

operation! {
    BranchOp {
        name: "branch",
        dialect: "son",
        scheme: "(son.control, int<1>) -> son.control*",
        operands: O { control, predicate },
        results: R { arms },
        verifier: "true",
    }
}

impl Verifiable for BranchOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        if self.0.results().len() != 2
            || context.get_value(self.operands()[1]).ty()
                != tir::builtin::IntegerType::new(context, 1)
        {
            return Err(Error::ExpectedType);
        }
        Ok(())
    }
}
