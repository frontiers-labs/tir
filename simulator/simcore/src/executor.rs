//! The functional executor: the architectural oracle of the simulator. It
//! interprets TMDL-generated instruction semantics block by block, maintaining
//! only architectural state (registers, memory, PC). It knows nothing about
//! cycles — timing is recovered later by replaying the recorded trace against
//! a machine model (see [`crate::timing`]).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::rc::Rc;

use tir::Context;
use tir::backend::{InstructionDecoder, MachineContext, MachineInstruction, PerfCounter, SimTrap};

use crate::error::Error;
use crate::program::{MachineBlock, ProgramImage};

/// How a block's execution ended.
enum BlockExit {
    /// `until_pc` was reached mid-block; `pc` points at it.
    Until,
    /// PC moved to the next block (control transfer or fallthrough).
    Next,
    /// An exception handler requested a halt; `pc` points at the trapping
    /// instruction.
    Halted,
}

/// A single data-memory access performed by a retired instruction, captured
/// into the trace so a timing model can drive a memory hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MemAccess {
    pub addr: u64,
    pub size: u8,
    pub is_write: bool,
    pub kind: MemAccessKind,
}

/// The flavor of a recorded memory access. Timing models that only care about
/// address/size/direction ignore this; it distinguishes the atomic constructs
/// and fences for models that model reservation/ordering effects.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum MemAccessKind {
    #[default]
    Data,
    LoadReserved,
    StoreConditional {
        success: bool,
    },
    AtomicRmw,
    Fence {
        pred: u8,
        succ: u8,
        ifence: bool,
    },
}

/// What the simulation should do after an exception handler ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionAction {
    /// Resume at the next instruction.
    Continue,
    /// Stop the run cleanly; [`Executor::halted`] reports `true`.
    Halt,
}

/// Callback invoked when instruction semantics raise an exception (TMDL
/// `trap`, e.g. ecall/ebreak). Receives the executor (so it can inspect or
/// update architectural state), the cause code and the trapping PC.
pub type ExceptionHandler = Box<dyn FnMut(&mut Executor, u64, u64) -> ExceptionAction>;

#[derive(Default)]
pub struct Executor {
    program: Option<Rc<ProgramImage>>,
    /// All architectural registers, stored as raw byte lanes. Interpretation is
    /// routed by type at execution: an integer operand reads an `APInt`, a float
    /// operand an `APFloat`, a vector operand the lanes themselves — so a value
    /// is never forced through the wrong representation (e.g. a 128-bit vector
    /// through a 64-bit `APInt`). Keyed by physical file; sub-word classes (1-bit
    /// flags) occupy a whole byte. Absent keys read as zero.
    registers: HashMap<(String, u16), tir::utils::RawBits>,
    /// Map from register class name to its physical register file. Classes that
    /// share a file (e.g. AArch64 `GPR` and `GPRsp`) alias index-for-index, so
    /// register storage is keyed by file rather than by class. Classes absent
    /// from the map are their own file.
    register_files: HashMap<String, String>,
    /// Architectural width in bits per register class (e.g. RISC-V `GPR` is 32
    /// on rv32). Values are normalized to this width on write and produced at
    /// it on read, so e.g. rv32 arithmetic wraps at 32 bits. Classes absent
    /// from the map keep whatever width the behavior produced.
    register_widths: HashMap<String, u32>,
    /// Sub-register views departing from the default (bit offset 0, zero-extending
    /// writes). Classes absent from the map use the default. Populated from
    /// `TargetMachine::register_views`; drives narrow writes on x86.
    register_views: HashMap<String, tir::backend::regalloc::RegisterView>,
    /// TMDL ISA parameter values (e.g. `XLEN`) under the selected target
    /// configuration, consulted by instruction behaviors via
    /// [`MachineContext::isa_param`].
    isa_params: HashMap<String, i64>,
    memory: Vec<u8>,
    memory_base: u64,
    pc: u64,
    pc_explicitly_written: bool,
    record_trace: bool,
    trace: Vec<(tir::OpId, u64)>,
    /// Data-memory accesses per retired instruction, kept exactly parallel to
    /// `trace` (empty inner vec for non-memory instructions).
    mem_trace: Vec<Vec<MemAccess>>,
    /// Accesses of the instruction currently executing. Interior-mutable because
    /// `read_memory` takes `&self`; drained into `mem_trace` after each execute.
    mem_stage: std::cell::RefCell<Vec<MemAccess>>,
    /// Set only around a machine instruction's `execute`, so instruction-fetch
    /// reads in the decode-on-fetch path are not captured.
    capturing_mem: bool,
    /// Registers backed by performance counters (e.g. the RISC-V `cycle` CSR):
    /// reads return the counter value, writes are ignored.
    counter_registers: HashMap<(String, u16), PerfCounter>,
    /// Instructions retired so far. Drives every performance counter: the
    /// functional model retires one instruction per cycle, and time ticks with
    /// the cycle counter.
    retired_instructions: u64,
    exception_handler: Option<ExceptionHandler>,
    halted: bool,
    /// Decode-on-fetch state, used to execute raw machine code (an ELF loaded
    /// into `memory`) instead of a pre-built [`ProgramImage`]. The decoder turns
    /// the word at PC into an op built in `decode_context`; results are cached by
    /// address so a hot loop decodes each instruction once.
    decoder: Option<InstructionDecoder>,
    decode_context: Option<Context>,
    decode_cache: HashMap<u64, tir::OpId>,
    /// `(class, index)` pairs that read as a hardwired zero (e.g. AArch64 `xzr`).
    /// Checked on the *original* class before file aliasing, so `GPR[31]` (xzr)
    /// reads 0 even though it shares a storage slot with `GPRsp[31]` (sp).
    hardwired_zero: HashSet<(String, u16)>,
    /// LR/SC reservation of the single implicit hart: exact (address, size) of
    /// the last load_reserved. Multi-hart seam: this field moves into a per-hart
    /// struct together with `registers`/`pc` when harts become explicit, and
    /// remote-hart writes must then clear overlapping reservations.
    reservation: Option<(u64, u8)>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TraceOptions {
    pub instructions: bool,
    pub registers_after_each_instruction: bool,
    pub registers_at_end: bool,
}

impl Executor {
    pub fn new(memory_size: usize) -> Self {
        Self::new_at(memory_size, 0)
    }

    pub fn new_at(memory_size: usize, memory_base: u64) -> Self {
        Self {
            memory: vec![0u8; memory_size],
            memory_base,
            ..Self::default()
        }
    }

    pub fn load(&mut self, program: ProgramImage) -> Result<(), Error> {
        if self.program.is_some() {
            return Err(Error::ProgramAlreadyLoaded);
        }
        self.pc = program.entry_pc;
        self.program = Some(Rc::new(program));
        Ok(())
    }

    /// Configure decode-on-fetch execution of raw machine code already present in
    /// `memory`: `decoder` turns the word at PC into an op built in `context`.
    /// Used instead of [`Executor::load`] to run an ELF image (see
    /// [`Executor::set_entry`]).
    pub fn set_decoder(&mut self, context: Context, decoder: InstructionDecoder) {
        self.decode_context = Some(context);
        self.decoder = Some(decoder);
    }

    /// Set the program counter (the entry point of a decode-on-fetch run).
    pub fn set_entry(&mut self, pc: u64) {
        self.pc = pc;
    }

    /// Copy `bytes` into guest memory starting at `address` (e.g. an ELF
    /// segment). Bounds-checked against the backing region.
    pub fn write_bytes(&mut self, address: u64, bytes: &[u8]) -> Result<(), SimTrap> {
        let offset = address
            .checked_sub(self.memory_base)
            .ok_or(SimTrap::BadAddress {
                address,
                size: bytes.len(),
            })?;
        let start = usize::try_from(offset).map_err(|_| SimTrap::BadAddress {
            address,
            size: bytes.len(),
        })?;
        let end = start.checked_add(bytes.len()).ok_or(SimTrap::BadAddress {
            address,
            size: bytes.len(),
        })?;
        if end > self.memory.len() {
            return Err(SimTrap::BadAddress {
                address,
                size: bytes.len(),
            });
        }
        self.memory[start..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Record the dynamic instruction stream (the executed op ids, in order) so a
    /// timing model can replay it. Off by default to avoid the memory cost.
    pub fn enable_trace_recording(&mut self) {
        self.record_trace = true;
    }

    /// Declare which register classes share a physical register file (class name
    /// -> file name). With this set, a value written through one class is
    /// visible through any aliasing class, matching real hardware (e.g. AArch64
    /// `GPR`/`GPRsp`). Without it, each class is its own independent file.
    pub fn set_register_files(&mut self, register_files: HashMap<String, String>) {
        self.register_files = register_files;
    }

    /// Configure which `(class, index)` registers read as a hardwired zero (from
    /// `TargetMachine::hardwired_zero_registers`).
    pub fn set_hardwired_zero_registers(
        &mut self,
        registers: impl IntoIterator<Item = (&'static str, u16)>,
    ) {
        self.hardwired_zero = registers
            .into_iter()
            .map(|(class, index)| (class.to_string(), index))
            .collect();
    }

    /// Configure architectural register widths per class (from
    /// `TargetMachine::register_widths`).
    pub fn set_register_widths(&mut self, widths: impl IntoIterator<Item = (&'static str, u32)>) {
        self.register_widths = widths
            .into_iter()
            .map(|(class, width)| (class.to_string(), width))
            .collect();
    }

    /// Configure sub-register views per class (from
    /// `TargetMachine::register_views`).
    pub fn set_register_views(
        &mut self,
        views: impl IntoIterator<Item = (&'static str, tir::backend::regalloc::RegisterView)>,
    ) {
        self.register_views = views
            .into_iter()
            .map(|(class, view)| (class.to_string(), view))
            .collect();
    }

    /// Configure TMDL ISA parameter values (from `TargetMachine::isa_params`).
    pub fn set_isa_params(&mut self, params: impl IntoIterator<Item = (&'static str, i64)>) {
        self.isa_params = params
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect();
    }

    /// Configure which registers are backed by performance counters (from
    /// `TargetMachine::counter_registers`).
    pub fn set_counter_registers(
        &mut self,
        counters: impl IntoIterator<Item = (&'static str, u16, PerfCounter)>,
    ) {
        self.counter_registers = counters
            .into_iter()
            .map(|(class, index, counter)| ((class.to_string(), index), counter))
            .collect();
    }

    /// Install the callback invoked when instruction semantics raise an
    /// exception (ecall/ebreak). Without one, exceptions surface as
    /// [`SimTrap::Exception`] errors from [`Executor::run`].
    pub fn set_exception_handler(&mut self, handler: ExceptionHandler) {
        self.exception_handler = Some(handler);
    }

    /// Instructions retired by this executor so far.
    pub fn retired_instructions(&self) -> u64 {
        self.retired_instructions
    }

    /// Whether an exception handler stopped the run.
    pub fn halted(&self) -> bool {
        self.halted
    }

    fn counter_value(&self, counter: PerfCounter) -> u64 {
        match counter {
            PerfCounter::Cycles | PerfCounter::Time | PerfCounter::InstructionsRetired => {
                self.retired_instructions
            }
            PerfCounter::CyclesHigh
            | PerfCounter::TimeHigh
            | PerfCounter::InstructionsRetiredHigh => self.retired_instructions >> 32,
        }
    }

    /// Resize `value` to a class's architectural width: truncate wider values,
    /// zero-extend narrower ones. Identity for unconfigured classes.
    fn resize_to_class_width(&self, class: &str, value: tir::utils::APInt) -> tir::utils::APInt {
        match self.register_widths.get(class) {
            Some(&width) if value.width() > width => value.truncate(width),
            Some(&width) if value.width() < width => value.zero_extend(width),
            _ => value,
        }
    }

    /// Canonicalize a register class to the physical file it draws from.
    fn register_file<'a>(&'a self, class: &'a str) -> &'a str {
        self.register_files
            .get(class)
            .map(String::as_str)
            .unwrap_or(class)
    }

    /// A register class's architectural width in bits (64 if unregistered).
    fn class_bit_width(&self, class: &str) -> u32 {
        self.register_widths.get(class).copied().unwrap_or(64)
    }

    /// The class width rounded up to a whole number of bytes (the byte-lane size
    /// its stored value occupies; a 1-bit flag still uses one byte).
    fn class_byte_bits(&self, class: &str) -> usize {
        (self.class_bit_width(class).div_ceil(8) * 8) as usize
    }

    /// The physical slot width of a register file in bits: the widest view any
    /// class takes of it. The file-defining class can be the narrow view (the
    /// RISC-V f file is FPR64-wide although its file class FPR32 is 32 bits),
    /// so sizing the slot by the file class alone would truncate wide writes.
    fn file_slot_bits(&self, file: &str) -> usize {
        self.register_files
            .iter()
            .filter(|(_, target)| target.as_str() == file)
            .map(|(class, _)| self.class_byte_bits(class))
            .chain([self.class_byte_bits(file)])
            .max()
            .unwrap_or(64)
    }

    /// The class-width byte lanes of a register, honoring the special reads
    /// (PC, hardwired-zero, performance counters). Absent registers read zero.
    fn read_register_raw(&self, class: &str, index: u16) -> Result<tir::utils::RawBits, SimTrap> {
        let byte_bits = self.class_byte_bits(class);
        if class == "PC" {
            return Ok(
                tir::utils::RawBits::from_apint(&tir::utils::APInt::new(64, self.pc))
                    .resized(byte_bits),
            );
        }
        if self.hardwired_zero.contains(&(class.to_string(), index)) {
            return Ok(tir::utils::RawBits::new(byte_bits));
        }
        if let Some(&counter) = self.counter_registers.get(&(class.to_string(), index)) {
            let value = self.counter_value(counter);
            return Ok(
                tir::utils::RawBits::from_apint(&tir::utils::APInt::new(64, value))
                    .resized(byte_bits),
            );
        }
        let file = self.register_file(class);
        let file_byte_bits = self.file_slot_bits(file);
        let key = (file.to_string(), index);
        let slot = self
            .registers
            .get(&key)
            .cloned()
            .unwrap_or_else(|| tir::utils::RawBits::new(file_byte_bits))
            .resized(file_byte_bits);
        let off = self.register_views.get(class).map_or(0, |v| v.bit_offset);
        if off == 0 {
            Ok(slot.resized(byte_bits))
        } else {
            let shifted = slot.to_apint().lshr(off);
            Ok(tir::utils::RawBits::from_apint(&shifted).resized(byte_bits))
        }
    }

    /// Store `bytes` into a register's file slot at the storage file's width. A
    /// narrow class with a merge policy or nonzero bit offset splices its value
    /// into the slot, preserving the untouched bits; otherwise the value is
    /// zero-extended across the whole element.
    fn store_register_raw(&mut self, class: &str, index: u16, bytes: tir::utils::RawBits) {
        let file = self.register_file(class).to_string();
        let file_byte_bits = self.file_slot_bits(&file);
        let key = (file.clone(), index);
        let view = self.register_views.get(class).copied().unwrap_or_default();
        let stored = if view.merge || view.bit_offset != 0 {
            let w = self.class_bit_width(class);
            let off = view.bit_offset;
            let slot = self
                .registers
                .get(&key)
                .cloned()
                .unwrap_or_else(|| tir::utils::RawBits::new(file_byte_bits))
                .resized(file_byte_bits)
                .to_apint();
            let sw = slot.width();
            let val = bytes.to_apint();
            let val = match val.width() {
                width if width > w => val.truncate(w),
                width if width < w => val.zero_extend(w),
                _ => val,
            };
            let val_shifted = val.zero_extend(sw).shl(off);
            let mask = tir::utils::APInt::max_value(w, false)
                .zero_extend(sw)
                .shl(off);
            tir::utils::RawBits::from_apint(&slot.and(&mask.not()).or(&val_shifted))
        } else {
            bytes.resized(file_byte_bits)
        };
        self.registers.insert(key, stored);
    }

    /// The recorded dynamic instruction stream as `(op, pc)` pairs, in execution
    /// order. The PC lets a timing model reconstruct branch directions/outcomes.
    pub fn trace(&self) -> &[(tir::OpId, u64)] {
        &self.trace
    }

    /// Data-memory accesses per retired instruction, parallel to [`Executor::trace`].
    pub fn mem_trace(&self) -> &[Vec<MemAccess>] {
        &self.mem_trace
    }

    /// Bounds-checked little-endian read of `size` bytes, without trace recording.
    /// The recording wrapper lives in the [`MachineContext`] impl so the atomic
    /// methods can reuse the raw read while tagging their own access kind.
    fn read_memory_raw(&self, address: u64, size: usize) -> Result<u64, SimTrap> {
        let offset = address
            .checked_sub(self.memory_base)
            .ok_or(SimTrap::BadAddress { address, size })?;
        let start = usize::try_from(offset).map_err(|_| SimTrap::BadAddress { address, size })?;
        let end = start
            .checked_add(size)
            .ok_or(SimTrap::BadAddress { address, size })?;
        if end > self.memory.len() {
            return Err(SimTrap::BadAddress { address, size });
        }
        let mut value = 0u64;
        for (offset, byte) in self.memory[start..end].iter().enumerate() {
            value |= u64::from(*byte) << (offset * 8);
        }
        Ok(value)
    }

    /// Bounds-checked little-endian write of `size` bytes, without trace recording.
    fn write_memory_raw(&mut self, address: u64, size: usize, value: u64) -> Result<(), SimTrap> {
        let offset = address
            .checked_sub(self.memory_base)
            .ok_or(SimTrap::BadAddress { address, size })?;
        let start = usize::try_from(offset).map_err(|_| SimTrap::BadAddress { address, size })?;
        let end = start
            .checked_add(size)
            .ok_or(SimTrap::BadAddress { address, size })?;
        if end > self.memory.len() {
            return Err(SimTrap::BadAddress { address, size });
        }
        for offset in 0..size {
            self.memory[start + offset] = ((value >> (offset * 8)) & 0xFF) as u8;
        }
        Ok(())
    }

    /// Record a memory-trace access, gated exactly like the plain read/write paths
    /// (only while capturing a machine instruction's execute with recording on).
    fn record_mem_access(&self, access: MemAccess) {
        if self.record_trace && self.capturing_mem {
            self.mem_stage.borrow_mut().push(access);
        }
    }

    /// Run `execute`, capturing its data-memory accesses, then drain them into
    /// `mem_trace` (in lockstep with the `trace` push) when recording.
    fn execute_capturing(&mut self, machine_inst: &dyn MachineInstruction) -> Result<(), SimTrap> {
        self.capturing_mem = true;
        let result = machine_inst.execute(self);
        self.capturing_mem = false;
        let accesses = std::mem::take(self.mem_stage.get_mut());
        if self.record_trace {
            self.mem_trace.push(accesses);
        }
        result
    }

    /// Decode the instruction at `pc` without executing it, using whichever fetch
    /// path is configured (decode-on-fetch memory + decoder, or a loaded
    /// [`ProgramImage`]). Lets a timing model walk down a *mispredicted* (never
    /// executed) path to recover the speculative instruction stream a real core
    /// would have fetched. Returns `None` if `pc` is unmapped or does not sit on
    /// an instruction boundary.
    pub fn decode_at(&self, pc: u64) -> Option<tir::OpId> {
        if let (Some(decoder), Some(context)) = (self.decoder, &self.decode_context) {
            let word = self.read_memory(pc, 4).ok()? as u32;
            return decoder(context, word);
        }
        let program = self.program.as_ref()?;
        let block = program
            .blocks
            .iter()
            .find(|b| pc >= b.start_address && pc < b.start_address + b.byte_len)?;
        let mut addr = block.start_address;
        for &op_id in &block.instructions {
            if addr == pc {
                return Some(op_id);
            }
            let width = program
                .context
                .get_op(op_id)
                .as_interface::<dyn MachineInstruction>()?
                .width_bytes();
            addr += u64::from(width);
        }
        None
    }

    pub fn run(&mut self, until_pc: u64, max_cycles: u64) -> Result<(), Error> {
        let mut sink = std::io::sink();
        self.run_with_trace(until_pc, max_cycles, TraceOptions::default(), &mut sink)
    }

    pub fn run_with_trace(
        &mut self,
        until_pc: u64,
        max_cycles: u64,
        trace: TraceOptions,
        out: &mut dyn Write,
    ) -> Result<(), Error> {
        let result = if self.program.is_some() {
            self.run_inner(until_pc, max_cycles, trace, out)
        } else {
            self.run_decoded_inner(until_pc, max_cycles, trace, out)
        };
        if trace.registers_at_end {
            self.emit_register_dump(out, "final registers");
        }
        result
    }

    /// Decode-on-fetch fetch loop: read the 4-byte word at PC, decode it into an
    /// op (cached by address), execute it, and advance. Runs raw machine code
    /// loaded into `memory` (an ELF), stopping on `until_pc`, an exception
    /// handler's halt (e.g. an exit syscall), or the `max_cycles` fuse.
    fn run_decoded_inner(
        &mut self,
        until_pc: u64,
        max_cycles: u64,
        trace: TraceOptions,
        out: &mut dyn Write,
    ) -> Result<(), Error> {
        let context = self.decode_context.clone().ok_or(Error::ProgramNotLoaded)?;
        let decoder = self.decoder.ok_or(Error::ProgramNotLoaded)?;
        for _ in 0..max_cycles {
            let pc = self.pc;
            if pc == until_pc {
                return Ok(());
            }
            let op_id = match self.decode_cache.get(&pc) {
                Some(&id) => id,
                None => {
                    let word = self.read_memory(pc, 4)? as u32;
                    let id = decoder(&context, word).ok_or(SimTrap::InvalidInstruction {
                        op: "<decode>",
                        reason: format!("no instruction matches word 0x{word:08x} at pc 0x{pc:x}"),
                    })?;
                    self.decode_cache.insert(pc, id);
                    id
                }
            };
            let op = context.get_op(op_id);
            let machine_inst = op
                .clone()
                .as_interface::<dyn MachineInstruction>()
                .ok_or_else(|| SimTrap::InvalidInstruction {
                    op: op.name().as_str(),
                    reason: "operation does not implement MachineInstruction".to_string(),
                })?;
            if trace.instructions {
                let line = format!(
                    "pc=0x{pc:016x}  {}",
                    Self::format_instruction_line(&context, &op, machine_inst.as_ref())
                );
                Self::emit_trace_line(out, &line);
            }
            if self.record_trace {
                self.trace.push((op_id, pc));
            }
            self.pc = pc;
            self.pc_explicitly_written = false;
            self.execute_capturing(machine_inst.as_ref())?;
            self.retired_instructions += 1;
            if trace.registers_after_each_instruction {
                self.emit_register_dump(out, "registers");
            }
            if self.halted {
                return Ok(());
            }
            if !self.pc_explicitly_written {
                self.pc = pc.wrapping_add(u64::from(machine_inst.width_bytes()));
            }
        }
        Err(SimTrap::MaxCyclesExceeded {
            max_cycles,
            until_pc,
        }
        .into())
    }

    /// The fetch loop: resolve PC to a block, execute it, repeat. `max_cycles`
    /// bounds the number of executed *blocks* — a runaway-loop fuse, not a
    /// timing statement.
    fn run_inner(
        &mut self,
        until_pc: u64,
        max_cycles: u64,
        trace: TraceOptions,
        out: &mut dyn Write,
    ) -> Result<(), Error> {
        let program = self.program.clone().ok_or(Error::ProgramNotLoaded)?;
        for _ in 0..max_cycles {
            if self.pc == until_pc {
                return Ok(());
            }
            let block = program
                .block_at(self.pc)
                .ok_or(SimTrap::PcNotMapped { pc: self.pc })?;
            match self.exec_block(&program.context, block, until_pc, trace, out)? {
                BlockExit::Until | BlockExit::Halted => return Ok(()),
                BlockExit::Next => {}
            }
        }
        Err(SimTrap::MaxCyclesExceeded {
            max_cycles,
            until_pc,
        }
        .into())
    }

    /// Execute one block straight-line, stopping early on `until_pc` or an
    /// explicit PC write (control transfer). On normal exit, PC advances to the
    /// fallthrough block.
    fn exec_block(
        &mut self,
        context: &Context,
        block: &MachineBlock,
        until_pc: u64,
        trace: TraceOptions,
        out: &mut dyn Write,
    ) -> Result<BlockExit, Error> {
        let mut inst_pc = block.start_address;
        for &op_id in &block.instructions {
            if inst_pc == until_pc {
                self.pc = inst_pc;
                return Ok(BlockExit::Until);
            }
            let op = context.get_op(op_id);
            let machine_inst = op
                .clone()
                .as_interface::<dyn MachineInstruction>()
                .ok_or_else(|| SimTrap::InvalidInstruction {
                    op: op.name().as_str(),
                    reason: "operation does not implement MachineInstruction".to_string(),
                })?;
            if trace.instructions {
                let line = format!(
                    "pc=0x{inst_pc:016x}  {}",
                    Self::format_instruction_line(context, &op, machine_inst.as_ref())
                );
                Self::emit_trace_line(out, &line);
            }
            if self.record_trace {
                self.trace.push((op_id, inst_pc));
            }
            // Expose this instruction's own address so PC-relative semantics
            // (`PC::pc`) resolve correctly even mid-block.
            self.pc = inst_pc;
            self.pc_explicitly_written = false;
            self.execute_capturing(machine_inst.as_ref())?;
            self.retired_instructions += 1;
            if trace.registers_after_each_instruction {
                self.emit_register_dump(out, "registers");
            }
            if self.halted {
                return Ok(BlockExit::Halted);
            }
            if self.pc_explicitly_written {
                // A control transfer wrote PC: `self.pc` holds the target, and
                // the next block is resolved by the fetch loop.
                return Ok(BlockExit::Next);
            }
            inst_pc = inst_pc.wrapping_add(u64::from(machine_inst.width_bytes()));
        }
        match block.fallthrough_pc {
            Some(next_pc) => {
                self.pc = next_pc;
                Ok(BlockExit::Next)
            }
            None => Err(Error::MissingFallthrough { pc: inst_pc }),
        }
    }

    pub fn register_snapshot(&self) -> Vec<(String, u16, tir::utils::RawBits)> {
        let mut regs = self
            .registers
            .iter()
            .map(|((class, idx), value)| (class.clone(), *idx, value.clone()))
            .collect::<Vec<_>>();
        regs.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        regs
    }

    fn format_instruction_line(
        context: &Context,
        op: &tir::OpHandle,
        machine_inst: &dyn MachineInstruction,
    ) -> String {
        let mut pieces = Vec::new();
        for attr in op.attributes() {
            let name = context.resolve(attr.name);
            let mut value_buf = String::new();
            let mut formatter = tir::IRFormatter::new(&mut value_buf);
            if attr.value.print(&mut formatter, context).is_ok() {
                pieces.push(format!("{name}={value_buf}"));
            } else {
                pieces.push(format!("{name}=<print-error>"));
            }
        }
        if pieces.is_empty() {
            machine_inst.mnemonic().to_string()
        } else {
            format!("{} {}", machine_inst.mnemonic(), pieces.join(", "))
        }
    }

    fn emit_register_dump(&self, out: &mut dyn Write, label: &str) {
        let snapshot = self.register_snapshot();
        Self::emit_trace_line(out, &format!("{label}:"));
        if snapshot.is_empty() {
            Self::emit_trace_line(out, "  <none>");
            return;
        }
        for (class, index, value) in snapshot {
            Self::emit_trace_line(
                out,
                &format!(
                    "  {}[{}] = 0x{:x} (width={})",
                    class,
                    index,
                    value.to_apint(),
                    value.width()
                ),
            );
        }
    }

    fn emit_trace_line(out: &mut dyn Write, line: &str) {
        let _ = writeln!(out, "{line}");
    }
}

impl MachineContext for Executor {
    fn read_register(&self, class: &str, index: u16) -> Result<tir::utils::APInt, SimTrap> {
        // Interpret the stored byte lanes as an integer at the class width. Only
        // scalar (<=64-bit) classes take this path; wider classes read as bits.
        let bytes = self.read_register_raw(class, index)?;
        Ok(self.resize_to_class_width(class, bytes.to_apint()))
    }

    fn read_register_bits(&self, class: &str, index: u16) -> Result<tir::utils::RawBits, SimTrap> {
        self.read_register_raw(class, index)
    }

    fn write_register(
        &mut self,
        class: &str,
        index: u16,
        value: tir::utils::APInt,
    ) -> Result<(), SimTrap> {
        let value = self.resize_to_class_width(class, value);
        if class == "PC" {
            self.write_pc(value.to_u64());
            return Ok(());
        }
        // Counter-backed registers are read-only; writes (e.g. the write-back a
        // csrrs with rs1=x0 performs) are ignored.
        if self
            .counter_registers
            .contains_key(&(class.to_string(), index))
        {
            return Ok(());
        }
        self.store_register_raw(class, index, tir::utils::RawBits::from_apint(&value));
        Ok(())
    }

    fn write_register_bits(
        &mut self,
        class: &str,
        index: u16,
        value: tir::utils::RawBits,
    ) -> Result<(), SimTrap> {
        if class == "PC" {
            self.write_pc(value.resized(64).to_apint().to_u64());
            return Ok(());
        }
        if self
            .counter_registers
            .contains_key(&(class.to_string(), index))
        {
            return Ok(());
        }
        self.store_register_raw(class, index, value);
        Ok(())
    }

    fn isa_param(&self, name: &str) -> Option<i64> {
        self.isa_params.get(name).copied()
    }

    fn read_memory(&self, address: u64, size: usize) -> Result<u64, SimTrap> {
        let value = self.read_memory_raw(address, size)?;
        self.record_mem_access(MemAccess {
            addr: address,
            size: size as u8,
            is_write: false,
            kind: MemAccessKind::Data,
        });
        Ok(value)
    }

    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), SimTrap> {
        self.write_memory_raw(address, size, value)?;
        self.record_mem_access(MemAccess {
            addr: address,
            size: size as u8,
            is_write: true,
            kind: MemAccessKind::Data,
        });
        Ok(())
    }

    fn load_reserved(
        &mut self,
        address: u64,
        size: usize,
        _ord: tir::sem::MemOrdering,
    ) -> Result<u64, SimTrap> {
        let value = self.read_memory_raw(address, size)?;
        self.reservation = Some((address, size as u8));
        self.record_mem_access(MemAccess {
            addr: address,
            size: size as u8,
            is_write: false,
            kind: MemAccessKind::LoadReserved,
        });
        Ok(value)
    }

    fn store_conditional(
        &mut self,
        address: u64,
        size: usize,
        value: u64,
        _ord: tir::sem::MemOrdering,
    ) -> Result<bool, SimTrap> {
        // Success requires an exact (address, size) match; the reservation is
        // consumed on both paths (matches Spike). Plain stores do not clear it.
        let ok = self.reservation == Some((address, size as u8));
        self.reservation = None;
        if ok {
            self.write_memory_raw(address, size, value)?;
        }
        self.record_mem_access(MemAccess {
            addr: address,
            size: size as u8,
            is_write: ok,
            kind: MemAccessKind::StoreConditional { success: ok },
        });
        Ok(ok)
    }

    fn atomic_rmw(
        &mut self,
        op: tir::sem::AtomicRmwOp,
        address: u64,
        size: usize,
        value: u64,
        _ord: tir::sem::MemOrdering,
    ) -> Result<u64, SimTrap> {
        let old = self.read_memory_raw(address, size)?;
        let width = (size as u32) * 8;
        let result = op.apply(
            tir::utils::APInt::new(width, old),
            tir::utils::APInt::new(width, value),
        );
        self.write_memory_raw(address, size, result.to_u64())?;
        self.record_mem_access(MemAccess {
            addr: address,
            size: size as u8,
            is_write: true,
            kind: MemAccessKind::AtomicRmw,
        });
        Ok(old)
    }

    fn fence(&mut self, pred: u32, succ: u32, kind: u32) -> Result<(), SimTrap> {
        self.record_mem_access(MemAccess {
            addr: 0,
            size: 0,
            is_write: false,
            kind: MemAccessKind::Fence {
                pred: pred as u8,
                succ: succ as u8,
                ifence: kind == 1,
            },
        });
        Ok(())
    }

    fn read_pc(&self) -> u64 {
        self.pc
    }

    fn write_pc(&mut self, value: u64) {
        self.pc = value;
        self.pc_explicitly_written = true;
    }

    fn raise_exception(&mut self, cause: u64) -> Result<(), SimTrap> {
        let pc = self.pc;
        let Some(mut handler) = self.exception_handler.take() else {
            return Err(SimTrap::Exception { cause, pc });
        };
        let action = handler(self, cause, pc);
        if self.exception_handler.is_none() {
            self.exception_handler = Some(handler);
        }
        match action {
            ExceptionAction::Continue => Ok(()),
            ExceptionAction::Halt => {
                self.halted = true;
                Ok(())
            }
        }
    }
}
