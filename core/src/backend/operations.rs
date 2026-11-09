use tir::{ResourceAccess, ResourceEffect, ResourceEffects, Terminator, builtin::StateResource};

use super::{ControlFlow, InstrInfo};

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

tir::helpers::op_def! {
    file: "defs/converted/backend_operations.cdsl",
    hosts: {
        section_end_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        symbol_end_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        symbol_address_iface_Pure => (tir :: Pure, &tir::PureCallbacks::DEFAULT),
        symbol_address_iface_Speculatable => (tir :: Speculatable, &tir::SpeculatableCallbacks::DEFAULT),
        block_end_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        vret_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        vret_iface_MachineInstruction => (tir :: backend :: MachineInstruction, &VRET_MACHINE_INSTRUCTION),
        vret_printer => vret_print,
        vret_parser => vret_parse,
        vbr_iface_Terminator => (Terminator, &VBR_TERMINATOR),
        vbr_iface_MachineInstruction => (tir :: backend :: MachineInstruction, &VBR_MACHINE_INSTRUCTION),
        vbr_printer => vbr_print,
        vbr_parser => vbr_parse,
        pcopy_iface_MachineInstruction => (tir :: backend :: MachineInstruction, &PCOPY_MACHINE_INSTRUCTION),
        vcall_iface_MachineInstruction => (tir :: backend :: MachineInstruction, &VCALL_MACHINE_INSTRUCTION),
        vcall_iface_ResourceEffects => (ResourceEffects, &CALL_RESOURCE_EFFECTS),
        vcall_indirect_iface_MachineInstruction => (tir :: backend :: MachineInstruction, &VCALL_INDIRECT_MACHINE_INSTRUCTION),
        vcall_indirect_iface_ResourceEffects => (ResourceEffects, &CALL_RESOURCE_EFFECTS),
    },
}

// A data definition directive (`.dword 42`, `.string "hi"`, `.space 16`).
// `kind` names the directive, `value` holds the literal (Int or Str).

// The address of a symbol, materialized inside a function. Module-level λ and δ
// values are not reachable from machine code, so the module prologue rewrites
// the uses of one into this op, and each target lowers it to the
// relocation-bearing form its assembler spells.

/// The address of `name`, as an opaque pointer.
pub fn symbol_address_of(context: &tir::Context, name: &str) -> crate::OpHandle {
    r#symbol_address(
        context,
        tir::attributes::AttributeValue::Str((name).into()),
        tir::ptr::PtrType::opaque(context),
        Default::default(),
        tir::InsertionPoint::Root,
    )
}

fn vret_print(op: &tir::OpHandle, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
    super::print_branch(fmt, op)
}

fn vret_parse(
    parser: &mut tir::parse::text::Parser,
    _context: &tir::Context,
    _definition: &'static tir::OpDefSpec,
) -> Result<tir::OpHandle, (tir::parse::Span, tir::Error)> {
    Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
}

fn vret_info(_op: &tir::OpHandle) -> &'static InstrInfo {
    static INFO: InstrInfo = virtual_info("vret", ControlFlow::Unconditional);
    &INFO
}

const VRET_MACHINE_INSTRUCTION: super::MachineInstructionCallbacks =
    super::MachineInstructionCallbacks {
        info: vret_info,
        instance: |op| op,
        sched_on: None,
        mnemonic: None,
        width_bytes: None,
        execute: None,
        control_flow: None,
    };

fn vbr_info(_op: &tir::OpHandle) -> &'static InstrInfo {
    static INFO: InstrInfo = virtual_info("vbr", ControlFlow::Unconditional);
    &INFO
}

const VBR_MACHINE_INSTRUCTION: super::MachineInstructionCallbacks =
    super::MachineInstructionCallbacks {
        info: vbr_info,
        instance: |op| op,
        sched_on: None,
        mnemonic: None,
        width_bytes: None,
        execute: None,
        control_flow: None,
    };

const VBR_TERMINATOR: tir::TerminatorCallbacks = tir::TerminatorCallbacks {
    successors: Some(super::branch_successors),
};

fn vbr_print(op: &tir::OpHandle, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
    super::print_branch(fmt, op)
}

fn vbr_parse(
    parser: &mut tir::parse::text::Parser,
    _context: &tir::Context,
    _definition: &'static tir::OpDefSpec,
) -> Result<tir::OpHandle, (tir::parse::Span, tir::Error)> {
    Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
}

// A parallel copy: every result takes its source at once, as one point. The
// register allocator splits the live ranges at a constrained instruction with
// one (Hack's thesis, paper § 4) and sequences it on the assigned registers.

fn pcopy_info(_op: &tir::OpHandle) -> &'static InstrInfo {
    static INFO: InstrInfo = virtual_info("pcopy", ControlFlow::None);
    &INFO
}

const PCOPY_MACHINE_INSTRUCTION: super::MachineInstructionCallbacks =
    super::MachineInstructionCallbacks {
        info: pcopy_info,
        instance: |op| op,
        sched_on: None,
        mnemonic: None,
        width_bytes: None,
        execute: None,
        control_flow: None,
    };

fn vcall_info(_op: &tir::OpHandle) -> &'static InstrInfo {
    static INFO: InstrInfo = virtual_call_info("vcall");
    &INFO
}

const VCALL_MACHINE_INSTRUCTION: super::MachineInstructionCallbacks =
    super::MachineInstructionCallbacks {
        info: vcall_info,
        instance: |op| op,
        sched_on: None,
        mnemonic: None,
        width_bytes: None,
        execute: None,
        control_flow: None,
    };

fn vcall_indirect_info(_op: &tir::OpHandle) -> &'static InstrInfo {
    static INFO: InstrInfo = virtual_call_info("vcall_indirect");
    &INFO
}

const VCALL_INDIRECT_MACHINE_INSTRUCTION: super::MachineInstructionCallbacks =
    super::MachineInstructionCallbacks {
        info: vcall_indirect_info,
        instance: |op| op,
        sched_on: None,
        mnemonic: None,
        width_bytes: None,
        execute: None,
        control_flow: None,
    };

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

const CALL_RESOURCE_EFFECTS: tir::ResourceEffectsCallbacks = tir::ResourceEffectsCallbacks {
    resource_effects: call_effects,
};
