use tir::helpers::dialect;

pub mod abi;
pub mod asm_syntax;
pub mod binary;
pub mod call_lowering;
pub mod isel;
mod lexer;
pub mod liveness;
pub mod lower;
mod operations;
mod parser;
pub mod pipeline;
mod printer;
pub mod regalloc;
pub mod sched;
pub mod target;

pub use operations::*;
pub use target::{
    ModelCheckTarget, TARGETS, TargetInfo, TargetMachine, select_target, select_target_with_abi,
    supported_targets,
};

// Re-exported so the `register_target!` macro can reference linkme from the
// backend crates without each of them depending on it directly.
pub use linkme;

pub use lexer::Token;
pub use lexer::lex;
pub use parser::{AsmInstructionParser, AsmParser};
pub use printer::{AsmInstructionPrinter, AsmPrintError, AsmPrinter};
use tir::attributes::{AttributeValue, RegisterAttr};
use tir::sem::{AtomicRmwOp, MemOrdering};
use tir::utils::APInt;

/// Decodes a 32-bit little-endian machine word into a freshly-built op in the
/// given `Context`, returning its id, or `None` if no instruction matches. The
/// inverse of a [`binary::InstructionEncoder`], generated per backend from the
/// same TMDL encoding tables and used to execute raw machine code (e.g. an ELF).
pub type InstructionDecoder = fn(&tir::Context, u32) -> Option<tir::OpId>;

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
    /// A synchronous exception raised by instruction semantics (TMDL `trap`,
    /// e.g. ecall/ebreak) that no installed handler absorbed. `cause` uses the
    /// target's cause encoding (RISC-V mcause for the riscv backend).
    Exception {
        cause: u64,
        pc: u64,
    },
}

/// A value written back to a register by instruction semantics: a scalar
/// (`APInt`, ≤64 bits) or a vector (`RawBits`, byte lanes) result.
pub enum RegisterValue {
    Int(APInt),
    Bits(tir::utils::RawBits),
}

impl RegisterValue {
    /// The low 64 bits (for a PC write, which is always scalar).
    pub fn to_u64(&self) -> u64 {
        match self {
            RegisterValue::Int(v) => v.to_u64(),
            RegisterValue::Bits(b) => b.resized(64).to_apint().to_u64(),
        }
    }
}

pub trait MachineContext {
    fn read_register(&self, class: &str, index: u16) -> Result<APInt, SimTrap>;
    fn write_register(&mut self, class: &str, index: u16, value: APInt) -> Result<(), SimTrap>;
    /// Read a register wider than a word (e.g. a 128-bit SIMD register) as raw
    /// byte lanes. The default handles ≤64-bit classes by widening the scalar
    /// value; register files with >64-bit classes override this.
    fn read_register_bits(&self, class: &str, index: u16) -> Result<tir::utils::RawBits, SimTrap> {
        Ok(tir::utils::RawBits::from_apint(
            &self.read_register(class, index)?,
        ))
    }
    /// Write a register from raw byte lanes (a vector result). The default narrows
    /// to a scalar for ≤64-bit classes; wide register files override this.
    fn write_register_bits(
        &mut self,
        class: &str,
        index: u16,
        value: tir::utils::RawBits,
    ) -> Result<(), SimTrap> {
        self.write_register(class, index, value.to_apint())
    }
    /// Write either a scalar or vector interpreter result, dispatching to the
    /// matching typed method.
    fn write_register_value(
        &mut self,
        class: &str,
        index: u16,
        value: RegisterValue,
    ) -> Result<(), SimTrap> {
        match value {
            RegisterValue::Int(v) => self.write_register(class, index, v),
            RegisterValue::Bits(b) => self.write_register_bits(class, index, b),
        }
    }
    fn read_memory(&self, address: u64, size: usize) -> Result<u64, SimTrap>;
    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), SimTrap>;
    /// Read `size` bytes and register a reservation covering the access. The
    /// default has no reservation concept and behaves like a plain read.
    fn load_reserved(
        &mut self,
        address: u64,
        size: usize,
        _ord: MemOrdering,
    ) -> Result<u64, SimTrap> {
        self.read_memory(address, size)
    }
    /// Write `value` iff a valid reservation covers the access, returning success.
    /// The default has no reservation concept, so the write always succeeds.
    fn store_conditional(
        &mut self,
        address: u64,
        size: usize,
        value: u64,
        _ord: MemOrdering,
    ) -> Result<bool, SimTrap> {
        self.write_memory(address, size, value)?;
        Ok(true)
    }
    /// Single-copy-atomic read-modify-write; returns the old memory value. The
    /// default reads, applies `op` at `size*8` bits, and writes back.
    fn atomic_rmw(
        &mut self,
        op: AtomicRmwOp,
        address: u64,
        size: usize,
        value: u64,
        _ord: MemOrdering,
    ) -> Result<u64, SimTrap> {
        let old = self.read_memory(address, size)?;
        let width = (size as u32) * 8;
        let result = op.apply(APInt::new(width, old), APInt::new(width, value));
        self.write_memory(address, size, result.to_u64())?;
        Ok(old)
    }
    /// Memory/instruction fence. The default has no ordering state and is a no-op.
    fn fence(&mut self, _pred: u32, _succ: u32, _kind: u32) -> Result<(), SimTrap> {
        Ok(())
    }
    fn read_pc(&self) -> u64;
    fn write_pc(&mut self, value: u64);
    /// The value of a TMDL ISA parameter (e.g. RISC-V `XLEN`) under the selected
    /// target configuration, or `None` when unconfigured (behaviors then fall
    /// back to the widest TMDL value).
    fn isa_param(&self, name: &str) -> Option<i64> {
        let _ = name;
        None
    }
    /// Raise a synchronous exception from instruction semantics (TMDL `trap`).
    /// Implementations may absorb it (e.g. emulate an environment call) and
    /// return `Ok`; the default surfaces it as a [`SimTrap::Exception`].
    fn raise_exception(&mut self, cause: u64) -> Result<(), SimTrap> {
        Err(SimTrap::Exception {
            cause,
            pc: self.read_pc(),
        })
    }
}

/// A hardware performance counter a target maps onto one of its registers
/// (e.g. the RISC-V `cycle`/`time`/`instret` CSRs). The `High` variants
/// deliver the upper 32 bits of the 64-bit counter, for targets that split
/// counters across two registers (RV32 `cycleh`/`timeh`/`instreth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfCounter {
    Cycles,
    Time,
    InstructionsRetired,
    CyclesHigh,
    TimeHigh,
    InstructionsRetiredHigh,
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
) -> Option<(tir::backend::regalloc::RegClassId, u16)> {
    attrs.iter().find_map(|attr| {
        if attr.name != name {
            return None;
        }
        match &attr.value {
            AttributeValue::Register(RegisterAttr::Physical { class, index }) => {
                Some((*class, *index))
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

/// The successor blocks a branch-shaped op transfers control to: every block
/// referenced by one of its attributes. Target branch instructions store their
/// destination as an `AttributeValue::Block` (the immediate operand rewritten by
/// branch selection); the virtual branch op carries its `dest` the same way. A
/// register-indirect or return transfer references no block and so has no static
/// successors. Shared by the generated `Terminator` impls.
pub fn branch_successors(op: &dyn tir::Operation) -> Vec<tir::BlockId> {
    op.attributes()
        .iter()
        .filter_map(|attr| match &attr.value {
            AttributeValue::Block(block) => Some(*block),
            _ => None,
        })
        .collect()
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
    pub use crate::backend::operations::*;
}

dialect! {
    AsmDialect {
        name: "asm",
        operations: [SectionOp, SectionEndOp, SymbolOp, SymbolEndOp, LiteralOp, BlockEndOp],
    }
}
