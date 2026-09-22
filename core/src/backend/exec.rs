//! Shared engine behind TMDL-generated `MachineInstruction::execute` bodies.
//!
//! An execute body is a sequence of effects whose value terms all live in the
//! sem blob; the only per-instruction information is the symbol bindings, the
//! blob offsets, and the writeback destinations. Keeping the machinery here
//! means a generated op carries a table of [`SymSource`]s and one call per
//! effect instead of an inlined copy of the interpreter plumbing.

use crate::attributes::AttributeValue;
use crate::backend::regalloc::RegClassId;
use crate::backend::{InstrInfo, MachineContext, MachineMemory, SimTrap};

mod footprint;
mod frame;
mod protocol;
pub use footprint::MemoryRange;
pub use frame::SemanticFrame;
pub use protocol::{
    EffectRequest, EffectResponse, FrameYield, MemoryEffect, RequestId, ResponseValue,
};

/// How one slot of an instruction's entry symbol table is bound before its
/// behavior evaluates: the operand/ISA-parameter sources a TMDL behavior reads.
pub enum SymSource {
    /// Register named by the attribute, read as a scalar.
    RegisterAttr(&'static str),
    /// Register interpreted with its declared floating-point format.
    FloatRegisterAttr(&'static str, u32, u32),
    /// Register named by the attribute, read as raw byte lanes (wide classes).
    WideRegisterAttr(&'static str),
    /// Integer attribute, as a signed value of the given width.
    IntAttr(&'static str, u32),
    /// ISA parameter (e.g. RISC-V `XLEN`), falling back to the widest TMDL value
    /// when the machine does not configure ISA params.
    IsaParam(&'static str, i64),
    /// Fixed architectural register by class name and index.
    FixedRegister(&'static str, u16),
    /// Encoding index of the register named by the attribute (TMDL `regnum`).
    RegAttrIndex(&'static str),
}

fn register_phys(
    instance: &crate::OpHandle,
    mnemonic: &'static str,
    name: &'static str,
) -> Result<(RegClassId, u16), SimTrap> {
    // Execution runs on instructions whose registers are physical: decoded or
    // assembled input, or emitted code read back through its assignment.
    match crate::backend::reg_slot(instance, name) {
        Some(crate::backend::RegSlot::Phys(register)) => Ok(register),
        _ => Err(SimTrap::MissingAttribute {
            op: mnemonic,
            attribute: name,
        }),
    }
}

/// Builds the entry symbol table for one instruction execution.
pub fn init_syms(
    instance: &crate::OpHandle,
    machine: &mut dyn MachineContext,
    mnemonic: &'static str,
    sym_count: usize,
    sources: &[(usize, SymSource)],
) -> Result<Vec<tir::sem::Value>, SimTrap> {
    let mut syms: Vec<Option<tir::sem::Value>> = vec![None; sym_count];
    for (idx, source) in sources {
        syms[*idx] = Some(match source {
            SymSource::RegisterAttr(name) => {
                let (class, index) = register_phys(instance, mnemonic, name)?;
                tir::sem::value_from_register(machine.read_register(class.name(), index)?)
            }
            SymSource::FloatRegisterAttr(name, exponent, mantissa) => {
                let (class, index) = register_phys(instance, mnemonic, name)?;
                let bits = machine.read_register(class.name(), index)?;
                tir::sem::Value::Float(tir::utils::APFloat::from_bits(
                    *exponent,
                    *mantissa,
                    false,
                    bits.to_u64() as u128,
                ))
            }
            SymSource::WideRegisterAttr(name) => {
                let (class, index) = register_phys(instance, mnemonic, name)?;
                tir::sem::value_from_raw_bits(machine.read_register_bits(class.name(), index)?)
            }
            SymSource::IntAttr(name, width) => {
                let value = instance
                    .attr(name)
                    .as_ref()
                    .and_then(AttributeValue::as_int)
                    .ok_or(SimTrap::MissingAttribute {
                        op: mnemonic,
                        attribute: name,
                    })?;
                tir::sem::int_value_signed(*width, value)
            }
            SymSource::IsaParam(name, default) => {
                tir::sem::int_value_signed(64, machine.isa_param(name).unwrap_or(*default))
            }
            SymSource::FixedRegister(class, index) => {
                tir::sem::value_from_register(machine.read_register(class, *index)?)
            }
            SymSource::RegAttrIndex(name) => {
                let (_, index) = register_phys(instance, mnemonic, name)?;
                tir::sem::int_value(64, index as u64)
            }
        });
    }
    Ok(syms
        .into_iter()
        .map(|value| value.unwrap_or_else(|| tir::sem::int_value(64, 0)))
        .collect())
}

/// The sem programs and target facts every instruction of one target shares.
pub struct ExecEnv {
    pub kinds: &'static [tir::sem::SymKind],
    pub blob: &'static [u8],
    pub is_hardwired_zero: fn(&str, u16) -> bool,
}

/// Where an evaluated value lands.
pub enum Dest {
    Pc,
    /// The register named by the attribute.
    Reg(&'static str),
    /// A fixed architectural register, by class name and index.
    Fixed(&'static str, u16),
    /// No state effect: a `let`-free local assignment, a store, a fence. The
    /// term is still evaluated, because evaluating it is the memory operation.
    Discard,
}

/// One statement of an instruction's behavior. Value terms are blob offsets.
pub enum Effect {
    Assign {
        offset: u32,
        dest: Dest,
    },
    Bind {
        offset: u32,
        sym: usize,
    },
    Trap {
        offset: u32,
    },
    If {
        cond: u32,
        then: &'static [Effect],
        els: &'static [Effect],
    },
}

/// What executing an instruction does.
pub enum Program {
    Effects {
        /// The environment the effects' value terms are evaluated against.
        env: &'static ExecEnv,
        sym_count: usize,
        sources: &'static [(usize, SymSource)],
        effects: &'static [Effect],
    },
    /// The behavior has no executable form; executing traps with this reason.
    Unsupported(&'static str),
}

/// Synchronously drives an instruction frame against `machine`.
/// Register and PC writes remain provisional until completion; memory effects
/// retain the visibility and fault policy provided by the caller's context.
pub fn run(
    instance: &crate::OpHandle,
    info: &InstrInfo,
    machine: &mut dyn MachineContext,
) -> Result<(), SimTrap> {
    let mut frame = SemanticFrame::from_program(0, machine.read_pc(), instance, info, machine)?;
    let mut response = None;
    loop {
        match frame.resume(response)? {
            FrameYield::Complete => return frame.commit(machine),
            FrameYield::Effect(request) => {
                response = Some(EffectResponse {
                    id: request.id,
                    result: service_synchronously(machine, request.effect),
                });
            }
        }
    }
}

fn service_synchronously(
    machine: &mut dyn MachineContext,
    effect: MemoryEffect,
) -> Result<ResponseValue, SimTrap> {
    use crate::sem::Memory;
    let mut memory = MachineMemory(machine);
    match effect {
        MemoryEffect::Read { address, size } if size <= 8 => {
            memory.read_memory(address, size).map(ResponseValue::Word)
        }
        MemoryEffect::Read { address, size } => memory
            .read_memory_bytes(address, size)
            .map(ResponseValue::Bytes),
        MemoryEffect::Write { address, bytes } => {
            memory.write_memory_bytes(
                address,
                bytes.len(),
                crate::utils::RawBits::from_bytes(bytes),
            )?;
            Ok(ResponseValue::Done)
        }
        MemoryEffect::LoadReserved {
            address,
            size,
            ordering,
        } => memory
            .load_reserved(address, size, ordering)
            .map(ResponseValue::Word),
        MemoryEffect::StoreConditional {
            address,
            size,
            value,
            ordering,
        } => memory
            .store_conditional(address, size, value, ordering)
            .map(|success| ResponseValue::Word(u64::from(success))),
        MemoryEffect::AtomicRmw {
            op,
            address,
            size,
            value,
            ordering,
        } => memory
            .atomic_rmw(op, address, size, value, ordering)
            .map(ResponseValue::Word),
        MemoryEffect::Fence { pred, succ, kind } => {
            memory.fence(pred, succ, kind)?;
            Ok(ResponseValue::Done)
        }
        MemoryEffect::Exception { cause } => {
            memory.0.raise_exception(cause)?;
            Ok(ResponseValue::Done)
        }
    }
}
