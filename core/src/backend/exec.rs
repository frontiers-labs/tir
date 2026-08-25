//! Shared engine behind TMDL-generated `MachineInstruction::execute` bodies.
//!
//! An execute body is a sequence of effects whose value terms all live in the
//! sem blob; the only per-instruction information is the symbol bindings, the
//! blob offsets, and the writeback destinations. Keeping the machinery here
//! means a generated op carries a table of [`SymSource`]s and one call per
//! effect instead of an inlined copy of the interpreter plumbing.

use crate::attributes::{AttributeValue, RegisterAttr};
use crate::backend::regalloc::RegClassId;
use crate::backend::{InstrInfo, MachineContext, MachineMemory, RegisterValue, SimTrap};

/// How one slot of an instruction's entry symbol table is bound before its
/// behavior evaluates: the operand/ISA-parameter sources a TMDL behavior reads.
pub enum SymSource {
    /// Register named by the attribute, read as a scalar.
    RegisterAttr(&'static str),
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
    match instance.attr(name) {
        Some(AttributeValue::Register(RegisterAttr::Physical { class, index, .. })) => {
            Ok((class, index))
        }
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

/// Evaluates the behavior value term stored at `offset` in the sem blob against
/// the current symbol table, interpreting memory operations through `machine`.
pub fn eval(
    kinds: &[tir::sem::SymKind],
    blob: &[u8],
    offset: u32,
    syms: &[tir::sem::Value],
    machine: &mut dyn MachineContext,
    mnemonic: &'static str,
) -> Result<RegisterValue, SimTrap> {
    let mut g = tir::sem::SemGraph::new();
    {
        use tir::sem::ExtendSemBytes as _;
        g.extend_sem_bytes(kinds, blob, offset)
    };
    let mut memory = MachineMemory(machine);
    match tir::sem::execute_with_memory(&g, syms, &mut memory)? {
        tir::sem::Value::Int(i) => Ok(RegisterValue::Int(i)),
        // A float result (e.g. `fadd`) and a lane concatenation (a vector
        // destination) are written back as raw bytes; the destination
        // register's storage keeps the bit pattern.
        tir::sem::Value::Float(f) => Ok(RegisterValue::Bits(tir::utils::RawBits::from_apfloat(&f))),
        tir::sem::Value::RawBits(b) => Ok(RegisterValue::Bits(b)),
        tir::sem::Value::Iterator(_) => Err(SimTrap::InvalidInstruction {
            op: mnemonic,
            reason: "instruction semantic expression did not evaluate to a register value"
                .to_string(),
        }),
    }
}

/// Writes `value` to the register named by attribute `name`, skipping the
/// target's hardwired-zero registers.
pub fn writeback_attr(
    instance: &crate::OpHandle,
    machine: &mut dyn MachineContext,
    mnemonic: &'static str,
    name: &'static str,
    value: RegisterValue,
    is_hardwired_zero: fn(&str, u16) -> bool,
) -> Result<(), SimTrap> {
    let (class, index) = register_phys(instance, mnemonic, name)?;
    if !is_hardwired_zero(class.name(), index) {
        machine.write_register_value(class.name(), index, value)?;
    }
    Ok(())
}

/// Writes `value` to a fixed architectural register, skipping the target's
/// hardwired-zero registers.
pub fn writeback_fixed(
    machine: &mut dyn MachineContext,
    class: &'static str,
    index: u16,
    value: RegisterValue,
    is_hardwired_zero: fn(&str, u16) -> bool,
) -> Result<(), SimTrap> {
    if !is_hardwired_zero(class, index) {
        machine.write_register_value(class, index, value)?;
    }
    Ok(())
}

/// Parks a `let`-bound value back into the symbol table; later statements read
/// the symbol instead of re-evaluating the term.
pub fn bind_sym(syms: &mut [tir::sem::Value], index: usize, value: RegisterValue) {
    syms[index] = match value {
        RegisterValue::Int(i) => tir::sem::value_from_register(i),
        RegisterValue::Bits(b) => tir::sem::value_from_raw_bits(b),
    };
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

/// Executes one instruction against `machine`, driven by its static info.
pub fn run(
    instance: &crate::OpHandle,
    info: &InstrInfo,
    machine: &mut dyn MachineContext,
) -> Result<(), SimTrap> {
    match &info.program {
        Program::Unsupported(reason) => Err(SimTrap::InvalidInstruction {
            op: info.mnemonic,
            reason: reason.to_string(),
        }),
        Program::Effects {
            env,
            sym_count,
            sources,
            effects,
        } => {
            let mut syms = init_syms(instance, machine, info.mnemonic, *sym_count, sources)?;
            run_effects(instance, info.mnemonic, env, machine, &mut syms, effects)
        }
    }
}

fn run_effects(
    instance: &crate::OpHandle,
    mnemonic: &'static str,
    env: &'static ExecEnv,
    machine: &mut dyn MachineContext,
    syms: &mut [tir::sem::Value],
    effects: &[Effect],
) -> Result<(), SimTrap> {
    let evaluate = |offset: u32, syms: &[tir::sem::Value], machine: &mut dyn MachineContext| {
        eval(env.kinds, env.blob, offset, syms, machine, mnemonic)
    };
    for effect in effects {
        match effect {
            Effect::Assign { offset, dest } => {
                let value = evaluate(*offset, syms, machine)?;
                match dest {
                    Dest::Pc => machine.write_pc(value.to_u64()),
                    Dest::Reg(name) => writeback_attr(
                        instance,
                        machine,
                        mnemonic,
                        name,
                        value,
                        env.is_hardwired_zero,
                    )?,
                    Dest::Fixed(class, index) => {
                        writeback_fixed(machine, class, *index, value, env.is_hardwired_zero)?
                    }
                    Dest::Discard => {}
                }
            }
            Effect::Bind { offset, sym } => {
                let value = evaluate(*offset, syms, machine)?;
                bind_sym(syms, *sym, value);
            }
            Effect::Trap { offset } => {
                let value = evaluate(*offset, syms, machine)?;
                machine.raise_exception(value.to_u64())?;
            }
            Effect::If { cond, then, els } => {
                let value = evaluate(*cond, syms, machine)?;
                let taken = if value.to_u64() != 0 { then } else { els };
                run_effects(instance, mnemonic, env, machine, syms, taken)?;
            }
        }
    }
    Ok(())
}
