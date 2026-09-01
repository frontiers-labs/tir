use tir::helpers::dialect;

pub mod abi;
pub mod asm_desc;
pub mod asm_syntax;
pub mod binary;
pub mod call_lowering;
pub mod dependence;
pub mod exec;
pub mod isel;
mod lexer;
pub mod liveness;
pub mod lower;
mod operations;
mod parser;
pub mod pipeline;
pub mod prealloc;
mod printer;
pub mod regalloc;
mod registers;
pub mod sched;
pub mod shuffle_order;
pub mod target;
mod verify;

pub use operations::*;
pub use target::{
    ModelCheckTarget, TARGETS, TargetInfo, TargetMachine, select_target, select_target_with_abi,
    supported_targets,
};

// Re-exported so the `register_target!` macro can reference linkme from the
// backend crates without each of them depending on it directly.
pub use linkme;

pub use registers::{
    ARG_PINS_ATTR, ASSIGNMENT_ATTR, PINS_ATTR, RegAssignment, RegClassType, RegPort, RegSlot,
    SlotRef, assigned_register, fresh_reg, op_slot_register, phys_attr, pin_slot,
    reg_class_type_parser, reg_ports, reg_slot, reg_slots, retype_untyped, slot_class, slot_pin,
    slot_register, type_class, value_class,
};

pub use dependence::{Dependences, verify_block_order};
pub use shuffle_order::ShuffleMachineOrderPass;
pub use verify::verify_machine_ir;

pub use lexer::Token;
pub use lexer::lex;
pub use parser::{AsmCursor, AsmInstructionParser, AsmParser};
pub use printer::{AsmPrintError, AsmPrinter};
use tir::attributes::{AttributeValue, RegisterAttr};
use tir::sem::{AtomicRmwOp, MemOrdering};
use tir::utils::APInt;

/// Decodes a 32-bit little-endian machine word into a freshly-built op in the
/// given `Context`, returning its id, or `None` if no instruction matches. The
/// inverse of an instruction's [`binary::EncodeSpec`], generated per backend
/// from the same TMDL encoding tables and used to execute raw machine code
/// (e.g. an ELF).
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

/// Adapts a [`MachineContext`] to [`tir::sem::Memory`] for instruction behavior
/// evaluation. A single shared type: the target is already a `dyn`, so a
/// per-instruction adapter would only duplicate the whole interpreter through
/// monomorphization.
pub struct MachineMemory<'a>(pub &'a mut dyn MachineContext);

impl tir::sem::Memory for MachineMemory<'_> {
    type Error = SimTrap;

    fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, Self::Error> {
        self.0.read_memory(address, size)
    }

    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), Self::Error> {
        self.0.write_memory(address, size, value)
    }

    fn load_reserved(
        &mut self,
        address: u64,
        size: usize,
        ord: MemOrdering,
    ) -> Result<u64, Self::Error> {
        self.0.load_reserved(address, size, ord)
    }

    fn store_conditional(
        &mut self,
        address: u64,
        size: usize,
        value: u64,
        ord: MemOrdering,
    ) -> Result<bool, Self::Error> {
        self.0.store_conditional(address, size, value, ord)
    }

    fn atomic_rmw(
        &mut self,
        op: AtomicRmwOp,
        address: u64,
        size: usize,
        value: u64,
        ord: MemOrdering,
    ) -> Result<u64, Self::Error> {
        self.0.atomic_rmw(op, address, size, value, ord)
    }

    fn fence(&mut self, pred: u32, succ: u32, kind: u32) -> Result<(), Self::Error> {
        self.0.fence(pred, succ, kind)
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

/// What executing an instruction does to data memory, derived from the memory
/// builtins its TMDL behavior invokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryEffects {
    pub reads: bool,
    pub writes: bool,
}

impl MemoryEffects {
    pub const NONE: MemoryEffects = MemoryEffects {
        reads: false,
        writes: false,
    };
}

/// Everything the backend knows about one opcode, as one `'static` record.
///
/// Every per-opcode fact is a field here, reached through
/// [`MachineInstruction::info`] by type — printing, encoding, patching,
/// execution, register semantics, cost and scheduling. There is no string-keyed
/// side table to fall out of sync with; the op name is the one key, and machines
/// are indices into [`InstrInfo::sched`].
pub struct InstrInfo {
    /// The op's registered name, and the one key identifying this opcode.
    pub name: &'static str,
    pub mnemonic: &'static str,
    /// Encoded size in bytes over the encoding's shapes: `(min, max)`. Equal
    /// for a fixed-width instruction; the widest shape is what a fixup takes.
    pub width_bytes: (u8, u8),
    pub control_flow: ControlFlow,
    pub program: exec::Program,
    /// Fixed registers the behavior touches without naming them in an operand.
    pub implicit_regs: &'static [tir::attributes::ImplicitReg],
    /// The opcode's register slots, in declaration order: which are results and
    /// which operands, and the class each admits. See [`RegPort`].
    pub regs: &'static [RegPort],
    pub effects: MemoryEffects,
    /// Assembly syntax, or `None` for an opcode with no textual form.
    pub asm: Option<&'static asm_desc::InstrDesc>,
    pub encode: Option<&'static binary::EncodeSpec>,
    /// Machine-independent cost (a latency proxy) from the TMDL `unit` defaults.
    pub cost: u32,
    /// Scheduling class per machine, indexed by [`sched::MachineModel::id`].
    /// Empty for an opcode no machine describes.
    pub sched: &'static [sched::InstrSchedClass],
}

impl InstrInfo {
    /// The neutral record the generated ones are written against: nothing
    /// modeled. A generated `InstrInfo` spells only the facts that depart from
    /// it, so an opcode with no syntax, encoding or schedule stays one line per
    /// fact it actually has.
    pub const BASE: InstrInfo = InstrInfo {
        name: "",
        mnemonic: "",
        width_bytes: (0, 0),
        control_flow: ControlFlow::None,
        program: exec::Program::Unsupported("instruction has no behavior"),
        implicit_regs: &[],
        regs: &[],
        effects: MemoryEffects::NONE,
        asm: None,
        encode: None,
        cost: 1,
        sched: &[],
    };

    /// This opcode's scheduling class on `machine`, or
    /// [`sched::InstrSchedClass::DEFAULT`] when no machine describes it or
    /// `machine` is not one of the target's (see [`sched::MachineModel::id`]).
    pub fn sched_on(&self, machine: &sched::MachineModel) -> sched::InstrSchedClass {
        self.sched
            .get(machine.id)
            .copied()
            .unwrap_or(sched::InstrSchedClass::DEFAULT)
    }
}

pub trait MachineInstruction {
    fn verify_interface(
        &self,
        _this: &dyn tir::Operation,
        _context: &tir::Context,
    ) -> Result<(), tir::Error> {
        Ok(())
    }
    fn info(&self) -> &'static InstrInfo;
    fn instance(&self) -> &tir::OpHandle;
    fn mnemonic(&self) -> &'static str {
        self.info().mnemonic
    }
    /// How many bytes this instruction encodes to: the width of the shape its
    /// operands select, so a guarded encoding reports what it actually emits.
    /// Operands still holding values (before register allocation) select no
    /// shape, and the widest is reported.
    fn width_bytes(&self) -> u8 {
        let info = self.info();
        match info.encode {
            Some(spec) => binary::encoded_width(self.instance(), spec, &RegAssignment::default()),
            None => info.width_bytes.1,
        }
    }
    fn execute(&self, machine: &mut dyn MachineContext) -> Result<(), SimTrap> {
        exec::run(self.instance(), self.info(), machine)
    }
    fn control_flow(&self) -> ControlFlow {
        self.info().control_flow
    }
}

/// Print a machine instruction as IR: its name and one entry per register slot
/// and attribute, in port order.
///
/// A slot holding a value prints as `%3:GPR` — its number and the class its
/// type names — becoming `%3:rbx` once the enclosing symbol's
/// [`RegAssignment`] has placed it; a slot naming a physical register prints
/// that register's assembly name. One shared printer: what a slot is called and
/// what class it admits are fields of the opcode's [`InstrInfo`], not generated
/// code.
pub fn print_machine_op<T: tir::Operation>(
    fmt: &mut tir::IRFormatter,
    op: &T,
) -> Result<(), std::fmt::Error> {
    let handle = op.handle().clone();
    let context = handle.context.upgrade();
    fmt.write(format!("{}.{}", T::dialect(), T::name()))?;
    let slots = registers::reg_slots(&handle);
    let mut first = true;
    let open = |fmt: &mut tir::IRFormatter, first: &mut bool| {
        let text = if *first { " {" } else { ", " };
        *first = false;
        fmt.write(text)
    };
    for slot in &slots {
        open(fmt, &mut first)?;
        fmt.write(format!("{} = ", slot.port.name))?;
        match slot.slot {
            RegSlot::Phys((class, index)) => print_register(fmt, class, index)?,
            RegSlot::Value(value) => {
                fmt.write(format!("%{}", value.number()))?;
                // The register a value ended up in, or — before allocation —
                // the class its type says it lives in.
                match registers::assigned_register(&context, &handle, value) {
                    Some((class, index)) => {
                        fmt.write(":")?;
                        print_register(fmt, class, index)?;
                    }
                    None => {
                        if let Some(class) = registers::value_class(&context, value) {
                            fmt.write(format!(":{}", class.name()))?;
                        }
                    }
                }
            }
        }
    }
    for attr in op.attributes() {
        let name = context.resolve(attr.name);
        if slots.iter().any(|slot| slot.port.name == name) {
            continue;
        }
        open(fmt, &mut first)?;
        fmt.write(format!("{name} = "))?;
        attr.value.print(fmt, &context)?;
    }
    if !first {
        fmt.write("}")?;
    }
    // Memory order is an operand like any other: an instruction that touches
    // memory names the state it observed and the one it leaves behind.
    tir::builtin::print_state_clause(
        fmt,
        tir::builtin::trailing_state_operand(&context, &handle),
        tir::builtin::trailing_state_result(&context, &handle),
    )?;
    fmt.write("\n")
}

/// Hand `new` the state ports `old` carried, so a lowering that replaces one
/// instruction with another keeps it on the chain.
///
/// The ports are grown onto `new` whatever its own opcode says about memory: a
/// call's effect is the callee's, and no per-opcode record can state it —
/// `bl` and `jal` write a link register and touch no memory of their own, yet
/// the call they finalize observes and leaves every object the outside can
/// reach. Called before `new` is inserted, because an opcode's builder cannot
/// know the chain.
pub fn forward_state(context: &tir::Context, old: &tir::OpHandle, new: &dyn tir::Operation) {
    let (Some(observed), Some(published)) = (
        tir::builtin::trailing_state_operand(context, old),
        tir::builtin::trailing_state_result(context, old),
    ) else {
        return;
    };
    context.append_operand(new.id(), observed);
    context.adopt_result(new.id(), published);
}

/// Whether the operation exists only to name a memory state: the root of a
/// chain, a merge of several, or the re-naming of a merge. They carry the
/// mid-end's memory order into machine IR and assemble to nothing.
pub fn names_memory_state(op: &tir::OpHandle) -> bool {
    op.is::<tir::state::EntryStateOp>()
        || op.is::<tir::state::JoinOp>()
        || op.is::<tir::state::SplitOp>()
}

fn print_register(
    fmt: &mut tir::IRFormatter,
    class: tir::backend::regalloc::RegClassId,
    index: u16,
) -> Result<(), std::fmt::Error> {
    match (class.print_name)(index, false) {
        Some(name) => fmt.write(name),
        None => fmt.write(format!("{}[{}]", class.name(), index)),
    }
}

pub fn register_attr(
    op: &impl tir::Operation,
    name: &str,
) -> Option<(tir::backend::regalloc::RegClassId, u16)> {
    match op.attr(name)? {
        AttributeValue::Register(RegisterAttr::Physical { class, index }) => Some((class, index)),
        _ => None,
    }
}

/// Print a virtual branch/terminator op for debugging: its mnemonic, operands as
/// `%N`, then each block-reference attribute as `^bbN`. Shared by the targets'
/// virtual branch ops so successor formatting is not duplicated per target.
pub fn print_branch<T: tir::Operation>(
    fmt: &mut tir::IRFormatter,
    op: &T,
) -> Result<(), std::fmt::Error> {
    fmt.write(format!("{}.{}", T::dialect(), T::name()))?;
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

/// The IEEE-754 bit pattern of an f64 `constantf`, as the immediate a
/// materializing move takes. `None` unless the constant's result is f64.
pub fn f64_constant_bits(context: &tir::Context, op: &crate::builtin::ConstantFOp) -> Option<i64> {
    let ty = context.get_value(op.result()).ty();
    let is_f64 = (context.get_type_data(ty).as_ref() as &dyn std::any::Any)
        .downcast_ref::<crate::builtin::FloatType>()
        .is_some_and(|float| float.bit_width() == 64);
    if !is_f64 {
        return None;
    }
    match crate::Operation::attr(op, "value") {
        Some(AttributeValue::F64(value)) => Some(value.to_bits() as i64),
        _ => None,
    }
}

pub fn int_attr(op: &impl tir::Operation, name: &str) -> Option<i64> {
    op.attr(name).as_ref().and_then(AttributeValue::as_int)
}

pub mod ops {
    pub use crate::backend::operations::*;
}

dialect! {
    AsmDialect {
        name: "asm",
        operations: [
            SectionOp,
            SectionEndOp,
            SymbolOp,
            SymbolEndOp,
            LiteralOp,
            DataRelocOp,
            BlockEndOp,
            VirtualReturnOp,
            VirtualBranchOp,
            VirtualCallOp,
            VirtualIndirectCallOp,
        ],
    }
}

pub fn emit_uncond_branch(
    context: &tir::Context,
    dest: tir::BlockId,
    args: &[tir::ValueId],
) -> Box<dyn tir::Operation> {
    Box::new(
        VirtualBranchOpBuilder::new(context)
            .dest_args(args.to_vec())
            .attr("dest", AttributeValue::Block(dest))
            .build(),
    )
}
