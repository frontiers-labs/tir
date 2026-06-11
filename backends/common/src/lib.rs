use tir::helpers::dialect;

pub mod isel;
mod lexer;
pub mod liveness;
mod operations;
mod parser;
mod printer;
pub mod regalloc;
pub mod sched;
pub mod target;

pub use operations::*;
pub use target::{TARGETS, TargetInfo, TargetMachine, select_target, supported_targets};

// Re-exported so the `register_target!` macro can reference linkme from the
// backend crates without each of them depending on it directly.
pub use linkme;

pub use lexer::Token;
pub use lexer::lex;
pub use parser::{AsmInstructionParser, AsmParser};
pub use printer::{AsmInstructionPrinter, AsmPrintError, AsmPrinter};
use tir::attributes::{AttributeValue, RegisterAttr};
use tir::utils::APInt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimTrap {
    MissingRegister {
        class: String,
        index: u16,
    },
    MissingAttribute {
        op: &'static str,
        attribute: &'static str,
    },
    InvalidAttribute {
        op: &'static str,
        attribute: &'static str,
    },
    InvalidInstruction {
        op: &'static str,
        reason: String,
    },
    BadAddress {
        address: u64,
        size: usize,
    },
    ProgramNotLoaded,
    PcNotMapped {
        pc: u64,
    },
    MaxCyclesExceeded {
        max_cycles: u64,
        until_pc: u64,
    },
}

pub trait MachineContext {
    fn read_register(&self, class: &str, index: u16) -> Result<APInt, SimTrap>;
    fn write_register(&mut self, class: &str, index: u16, value: APInt) -> Result<(), SimTrap>;
    fn read_memory(&self, address: u64, size: usize) -> Result<u64, SimTrap>;
    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), SimTrap>;
    fn read_pc(&self) -> u64;
    fn write_pc(&mut self, value: u64);
    /// The value of a TMDL ISA parameter (e.g. RISC-V `XLEN`) under the selected
    /// target configuration, or `None` when unconfigured (behaviors then fall
    /// back to the widest TMDL value).
    fn isa_param(&self, name: &str) -> Option<i64> {
        let _ = name;
        None
    }
}

/// How an instruction affects control flow, derived at TMDL-compile time from
/// whether (and on which paths) its `behavior` assigns the `PC` register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    /// Never writes PC: a sequential instruction.
    None,
    /// Writes PC only on some paths: a conditional branch, subject to
    /// direction prediction.
    Conditional,
    /// Writes PC on every path: a jump/call/return.
    Unconditional,
}

pub trait MachineInstruction {
    fn verify_interface(
        &self,
        _this: &dyn tir::Operation,
        _context: &tir::Context,
    ) -> Result<(), tir::Error> {
        Ok(())
    }
    fn mnemonic(&self) -> &'static str;
    fn width_bytes(&self) -> u8;
    fn execute(&self, machine: &mut dyn MachineContext) -> Result<(), SimTrap>;
    fn control_flow(&self) -> ControlFlow {
        ControlFlow::None
    }
}

pub fn register_attr(
    attrs: &[tir::attributes::NamedAttribute],
    name: &str,
) -> Option<(String, u16)> {
    attrs.iter().find_map(|attr| {
        if attr.name != name {
            return None;
        }
        match &attr.value {
            AttributeValue::Register(RegisterAttr::Physical { class, index }) => {
                Some((class.clone(), *index))
            }
            _ => None,
        }
    })
}

/// Print a virtual branch/terminator op for debugging: its mnemonic, operands as
/// `%N`, then each block-reference attribute as `^bbN`. Shared by the targets'
/// virtual branch ops so successor formatting is not duplicated per target.
pub fn print_branch(
    fmt: &mut tir::IRFormatter,
    op: &dyn tir::Operation,
    mnemonic: &str,
) -> Result<(), std::fmt::Error> {
    fmt.write(mnemonic)?;
    for (i, value) in op.operands().iter().enumerate() {
        fmt.write(if i == 0 { " " } else { ", " })?;
        fmt.write(format!("%{}", value.number()))?;
    }
    for attr in op.attributes() {
        if let AttributeValue::Block(block) = &attr.value {
            fmt.write(format!(" ^bb{}", fmt.region_block_number(*block)))?;
        }
    }
    fmt.write("\n")
}

pub fn int_attr(attrs: &[tir::attributes::NamedAttribute], name: &str) -> Option<i64> {
    attrs.iter().find_map(|attr| {
        if attr.name != name {
            return None;
        }
        match attr.value {
            AttributeValue::Int(i) => Some(i),
            AttributeValue::UInt(u) => i64::try_from(u).ok(),
            _ => None,
        }
    })
}

pub mod ops {
    pub use crate::operations::*;
}

dialect! {
    AsmDialect {
        name: "asm",
        operations: [SectionOp, SectionEndOp, SymbolOp, SymbolEndOp, LiteralOp, BlockEndOp],
    }
}
