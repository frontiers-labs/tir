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
        let file_byte_bits = self.class_byte_bits(file);
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
        let file_byte_bits = self.class_byte_bits(&file);
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
        let accesses: Vec<MemAccess> = self.mem_stage.get_mut().drain(..).collect();
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
        op: &std::sync::Arc<tir::OpInstance>,
        machine_inst: &dyn MachineInstruction,
    ) -> String {
        let mut pieces = Vec::new();
        for attr in &op.attributes {
            let mut value_buf = String::new();
            let mut formatter = tir::IRFormatter::new(&mut value_buf);
            if attr.value.print(&mut formatter, context).is_ok() {
                pieces.push(format!("{}={}", attr.name, value_buf));
            } else {
                pieces.push(format!("{}=<print-error>", attr.name));
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

#[cfg(test)]
mod tests {
    use tir::Context;
    use tir::backend::{AsmDialect, MachineInstruction};
    use tir::utils::APInt;
    use tir_riscv::RiscvDialect;

    use crate::{Executor, ProgramImage, TraceOptions, error::Error};

    #[test]
    fn run_stops_before_until_pc() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global first
            first:
              add x1, x1, x1
            .global second
            second:
              add x2, x2, x2
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program = ProgramImage::from_module(&context, module, 0x8000_0000, Some("first"))
            .expect("program builder must succeed");

        let until_pc = program.entry_pc;
        let mut executor = Executor::new(4096);
        tir::backend::MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, 3))
            .unwrap();
        executor.load(program).unwrap();
        executor.run(until_pc, 10).unwrap();

        let x1 = tir::backend::MachineContext::read_register(&executor, "GPR", 1).unwrap();
        let x2 = tir::backend::MachineContext::read_register(&executor, "GPR", 2).unwrap();
        assert_eq!(x1.to_u64(), 3);
        assert_eq!(x2.to_u64(), 0);
        assert_eq!(tir::backend::MachineContext::read_pc(&executor), until_pc);
    }

    #[test]
    fn rv32_configuration_wraps_arithmetic_at_32_bits() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        // Symbols are laid out in reverse declaration order: `first` executes at
        // 0x8000_0000 and falls through to `last` at 0x8000_000c.
        let asm = "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              lui  x1, 524288
              add  x3, x1, x1
              addi x4, x0, -1
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();

        let rv32 = [tir_riscv::Feature::RV32I];
        let mut executor = Executor::new_at(4096, 0x8000_0000);
        executor.set_isa_params(tir_riscv::isa_params(&rv32));
        executor.set_register_widths(tir_riscv::register_widths(&rv32));
        executor.load(program).unwrap();
        executor.run(0x8000_000c, 10).unwrap();

        let reg = |idx| tir::backend::MachineContext::read_register(&executor, "GPR", idx).unwrap();
        // lui keeps 32-bit values (no sign extension into a 64-bit register),
        // the doubled value wraps to zero, and -1 is the 32-bit all-ones.
        assert_eq!((reg(1).to_u64(), reg(1).width()), (0x8000_0000, 32));
        assert_eq!(reg(3).to_u64(), 0);
        assert_eq!(reg(4).to_u64(), 0xFFFF_FFFF);
    }

    #[test]
    fn run_traps_when_max_cycles_exhausted() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global first
            first:
              add x1, x1, x1
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();
        let mut executor = Executor::new(4096);
        executor.load(program).unwrap();

        let err = executor.run(0xFFFF_FFFF, 0).unwrap_err();
        match err {
            Error::Trap(tir::backend::SimTrap::MaxCyclesExceeded { .. }) => {}
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn run_keeps_hardwired_zero_register_immutable() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global first
            first:
              add x0, x1, x1
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program = ProgramImage::from_module(&context, module, 0x8000_0000, Some("first"))
            .expect("program builder must succeed");
        let mut executor = Executor::new(4096);
        tir::backend::MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, 7))
            .unwrap();

        let inst_id = *program
            .blocks
            .first()
            .and_then(|block| block.instructions.first())
            .expect("program should contain one machine instruction");
        let inst_op = context.get_op(inst_id);
        let machine_inst = inst_op
            .clone()
            .as_interface::<dyn MachineInstruction>()
            .expect("expected machine instruction in symbol body");
        machine_inst.execute(&mut executor).unwrap();

        let x0 = tir::backend::MachineContext::read_register(&executor, "GPR", 0).unwrap();
        assert_eq!(x0.to_u64(), 0);
    }

    #[test]
    fn run_with_trace_emits_instruction_and_registers() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global first
            first:
              add x1, x1, x1
            .global second
            second:
              add x2, x2, x2
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program = ProgramImage::from_module(&context, module, 0x8000_0000, Some("first"))
            .expect("program builder must succeed");
        let mut executor = Executor::new(4096);
        tir::backend::MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, 2))
            .unwrap();
        executor.load(program).unwrap();

        let mut trace_output = Vec::new();
        let err = executor
            .run_with_trace(
                u64::MAX,
                1,
                TraceOptions {
                    instructions: true,
                    registers_after_each_instruction: true,
                    registers_at_end: true,
                },
                &mut trace_output,
            )
            .unwrap_err();
        match err {
            Error::Trap(tir::backend::SimTrap::MaxCyclesExceeded { .. }) => {}
            Error::MissingFallthrough { .. } => {}
            other => panic!("unexpected error: {:?}", other),
        }

        let trace_text = String::from_utf8(trace_output).unwrap();
        assert!(trace_text.contains("pc=0x"));
        assert!(trace_text.contains("add"));
        assert!(trace_text.contains("registers:"));
        assert!(trace_text.contains("final registers:"));
    }

    #[test]
    fn riscv_load_store_execute_against_memory_window() {
        use tir::Operation;
        use tir::attributes::{AttributeValue, RegisterAttr};
        use tir::backend::MachineContext;

        fn gpr(index: u16) -> AttributeValue {
            AttributeValue::Register(RegisterAttr::Physical {
                class: tir_riscv::RegClass::GPR.id(),
                index,
            })
        }

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let base = 0x8000_0000;
        let data = base + 0x100;

        let mut executor = Executor::new_at(4096, base);
        MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, data)).unwrap();
        MachineContext::write_memory(&mut executor, data, 4, 0x1234_5678).unwrap();

        let lw = tir_riscv::LoadWordOpBuilder::new(&context)
            .attr("rd", gpr(2))
            .attr("rs1", gpr(1))
            .attr("imm", AttributeValue::Int(0))
            .build();
        let sw = tir_riscv::StoreWordOpBuilder::new(&context)
            .attr("rs2", gpr(2))
            .attr("rs1", gpr(1))
            .attr("imm", AttributeValue::Int(4))
            .build();

        context
            .get_op(lw.id())
            .as_interface::<dyn MachineInstruction>()
            .unwrap()
            .execute(&mut executor)
            .unwrap();
        context
            .get_op(sw.id())
            .as_interface::<dyn MachineInstruction>()
            .unwrap()
            .execute(&mut executor)
            .unwrap();

        let x2 = MachineContext::read_register(&executor, "GPR", 2).unwrap();
        assert_eq!(x2.to_u64(), 0x1234_5678);
        assert_eq!(
            MachineContext::read_memory(&executor, data + 4, 4).unwrap(),
            0x1234_5678
        );
    }

    #[test]
    fn mem_trace_records_loads_and_stores_parallel_to_trace() {
        use tir::backend::MachineContext;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        // Reverse declaration order: `first` executes at 0x8000_0000 and falls
        // through to `last` at 0x8000_000c after three instructions.
        let asm = "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              lw  x2, 0(x1)
              sw  x2, 4(x1)
              add x3, x2, x2
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let base = 0x8000_0000;
        let program = ProgramImage::from_module(&context, module, base, Some("first")).unwrap();

        let data = base + 0x100;
        let mut executor = Executor::new_at(4096, base);
        executor.enable_trace_recording();
        MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, data)).unwrap();
        MachineContext::write_memory(&mut executor, data, 4, 0x1234_5678).unwrap();
        executor.load(program).unwrap();
        executor.run(0x8000_000c, 10).unwrap();

        assert_eq!(executor.trace().len(), 3);
        assert_eq!(executor.mem_trace().len(), executor.trace().len());
        assert_eq!(
            executor.mem_trace()[0],
            vec![crate::MemAccess {
                addr: data,
                size: 4,
                is_write: false,
                ..Default::default()
            }]
        );
        assert_eq!(
            executor.mem_trace()[1],
            vec![crate::MemAccess {
                addr: data + 4,
                size: 4,
                is_write: true,
                ..Default::default()
            }]
        );
        assert!(executor.mem_trace()[2].is_empty(), "add touches no memory");
    }

    #[test]
    fn zicsr_csr_instructions_read_then_modify() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        // Symbols are laid out in reverse declaration order: `first` executes
        // at 0x8000_0000 and falls through to `last` at 0x8000_0010.
        let asm = "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              csrrw x2, mscratch, x1
              csrrs x3, mscratch, x4
              csrrc x5, mscratch, x6
              csrrwi x7, mscratch, 9
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();

        let mut executor = Executor::new(4096);
        let mut write = |idx, v| {
            tir::backend::MachineContext::write_register(
                &mut executor,
                "GPR",
                idx,
                APInt::new(64, v),
            )
            .unwrap()
        };
        write(1, 0b0011);
        write(4, 0b0100);
        write(6, 0b0010);
        executor.load(program).unwrap();
        executor.run(0x8000_0010, 10).unwrap();

        let reg = |class, idx| {
            tir::backend::MachineContext::read_register(&executor, class, idx)
                .unwrap()
                .to_u64()
        };
        // Every form returns the pre-write CSR value in rd, then applies its
        // modification: write, set bits, clear bits, write immediate.
        assert_eq!(reg("GPR", 2), 0, "csrrw reads the initial mscratch");
        assert_eq!(reg("GPR", 3), 0b0011, "csrrs reads the csrrw result");
        assert_eq!(reg("GPR", 5), 0b0111, "csrrc reads the csrrs result");
        assert_eq!(reg("GPR", 7), 0b0101, "csrrwi reads the csrrc result");
        // CSRs live in the register file at their architectural address.
        assert_eq!(reg("CSR", 0x340), 9, "csrrwi wrote its immediate");
    }

    #[test]
    fn zicsr_set_and_clear_with_x0_do_not_write_the_csr() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              csrrs x2, mscratch, x0
              csrrsi x3, mscratch, 0
              csrrs x5, mscratch, x4
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();

        let mut executor = Executor::new(4096);
        tir::backend::MachineContext::write_register(
            &mut executor,
            "CSR",
            0x340,
            APInt::new(64, 0b1010),
        )
        .unwrap();
        tir::backend::MachineContext::write_register(
            &mut executor,
            "GPR",
            4,
            APInt::new(64, 0b0101),
        )
        .unwrap();
        executor.load(program).unwrap();
        executor.run(0x8000_000C, 10).unwrap();

        let reg = |class, idx| {
            tir::backend::MachineContext::read_register(&executor, class, idx)
                .unwrap()
                .to_u64()
        };
        // csrrs/csrrsi with a zero source read the CSR into rd but leave it
        // untouched: neither instruction may write it.
        assert_eq!(reg("GPR", 2), 0b1010, "csrrs x0 reads mscratch");
        assert_eq!(
            reg("GPR", 3),
            0b1010,
            "csrrsi 0 reads the unchanged mscratch"
        );
        // A non-x0 source still sets bits: the pre-write value goes to rd, the
        // OR of the operands to the CSR.
        assert_eq!(reg("GPR", 5), 0b1010, "csrrs reads before setting");
        assert_eq!(reg("CSR", 0x340), 0b1111, "csrrs x4 set the masked bits");
    }

    #[test]
    fn counter_registers_track_retired_instructions_and_ignore_writes() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              add x1, x1, x1
              add x1, x1, x1
              csrrw x0, instret, x1
              csrrs x2, instret, x0
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();

        let mut executor = Executor::new(4096);
        executor.set_counter_registers([(
            "CSR",
            0xC02,
            tir::backend::PerfCounter::InstructionsRetired,
        )]);
        executor.load(program).unwrap();
        executor.run(0x8000_0010, 10).unwrap();

        // The csrrw write to the read-only counter is ignored; the csrrs read
        // sees the three instructions retired before it.
        let x2 = tir::backend::MachineContext::read_register(&executor, "GPR", 2).unwrap();
        assert_eq!(x2.to_u64(), 3);
        assert_eq!(executor.retired_instructions(), 4);
    }

    #[test]
    fn rv32_counter_high_half_reads_upper_bits() {
        let rv32 = [tir_riscv::Feature::RV32I, tir_riscv::Feature::Zicsr];
        let mut executor = Executor::new(64);
        executor.set_register_widths(tir_riscv::register_widths(&rv32));
        executor.set_counter_registers([
            ("CSR", 0xC00, tir::backend::PerfCounter::Cycles),
            ("CSR", 0xC80, tir::backend::PerfCounter::CyclesHigh),
        ]);
        executor.retired_instructions = 0x0000_0005_8000_0001;

        let reg = |idx| tir::backend::MachineContext::read_register(&executor, "CSR", idx).unwrap();
        // cycle returns the low word (the CSR class is 32 bits wide on rv32),
        // cycleh the upper word of the same 64-bit counter.
        assert_eq!((reg(0xC00).to_u64(), reg(0xC00).width()), (0x8000_0001, 32));
        assert_eq!(reg(0xC80).to_u64(), 5);
    }

    #[test]
    fn ecall_without_handler_surfaces_an_exception_trap() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global first
            first:
              ecall
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();
        let mut executor = Executor::new(4096);
        executor.load(program).unwrap();

        let err = executor.run(0xFFFF_FFFF, 10).unwrap_err();
        match err {
            Error::Trap(tir::backend::SimTrap::Exception { cause, pc }) => {
                assert_eq!(cause, 11, "ecall raises environment-call-from-M-mode");
                assert_eq!(pc, 0x8000_0000);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn exception_handler_controls_run_outcome() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              ecall
              addi x1, x0, 7
              ebreak
              addi x2, x0, 9
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();

        let traps = Rc::new(RefCell::new(Vec::new()));
        let seen = traps.clone();
        let mut executor = Executor::new(4096);
        executor.set_exception_handler(Box::new(move |_executor, cause, pc| {
            seen.borrow_mut().push((cause, pc));
            // Resume after the ecall, stop at the ebreak.
            if cause == 11 {
                crate::ExceptionAction::Continue
            } else {
                crate::ExceptionAction::Halt
            }
        }));
        executor.load(program).unwrap();
        executor.run(0x8000_0010, 10).unwrap();

        assert!(executor.halted());
        assert_eq!(
            *traps.borrow(),
            vec![(11, 0x8000_0000), (3, 0x8000_0008)],
            "handler saw the ecall and the ebreak with their PCs"
        );
        let reg = |idx| {
            tir::backend::MachineContext::read_register(&executor, "GPR", idx)
                .unwrap()
                .to_u64()
        };
        assert_eq!(reg(1), 7, "execution resumed after the ecall");
        assert_eq!(reg(2), 0, "the halt stopped execution at the ebreak");
    }

    /// `cmp` writes all four AArch64 condition flags (`PSTATE` n/z/c/v), and a
    /// conditional branch reads them back. Both used to be silently dropped: the
    /// multi-assignment behaviors only emitted one write (or none), and flag paths
    /// could not be lowered at all. Flags live in a register class with index-less
    /// registers, so this also exercises the canonical-index support that ports to
    /// any target with status/flag registers.
    #[test]
    fn arm64_compare_sets_flags_and_conditional_branch_reads_them() {
        use tir::Operation;
        use tir::attributes::{AttributeValue, RegisterAttr};
        use tir::backend::{MachineContext, MachineInstruction};

        fn gpr(index: u16) -> AttributeValue {
            AttributeValue::Register(RegisterAttr::Physical {
                class: tir_arm64::RegClass::GPR.id(),
                index,
            })
        }

        // PSTATE flag slots, assigned by declaration order in the register class.
        const N: u16 = 0;
        const Z: u16 = 1;
        const C: u16 = 2;
        const V: u16 = 3;

        let context = Context::with_default_dialects();
        context.register_dialect::<tir::backend::AsmDialect>();
        context.register_dialect::<tir_arm64::Arm64Dialect>();

        let exec_cmp = |x0: u64, x1: u64| -> Executor {
            let mut ex = Executor::new(64);
            MachineContext::write_register(&mut ex, "GPR", 0, APInt::new(64, x0)).unwrap();
            MachineContext::write_register(&mut ex, "GPR", 1, APInt::new(64, x1)).unwrap();
            let cmp = tir_arm64::CompareOpBuilder::new(&context)
                .attr("rn", gpr(0))
                .attr("rm", gpr(1))
                .build();
            let mi = context
                .get_op(cmp.id())
                .as_interface::<dyn MachineInstruction>()
                .expect("cmp is a machine instruction");
            mi.execute(&mut ex).expect("cmp executes");
            ex
        };
        let flag = |ex: &Executor, idx: u16| {
            MachineContext::read_register(ex, "PSTATE", idx)
                .unwrap()
                .to_u64()
        };

        // Equal operands: Z and C set, N and V clear.
        let eq = exec_cmp(5, 5);
        assert_eq!(flag(&eq, Z), 1, "Z set when operands are equal");
        assert_eq!(flag(&eq, N), 0);
        assert_eq!(flag(&eq, C), 1, "C set: 5 >=u 5");
        assert_eq!(flag(&eq, V), 0);

        // 5 - 7 is negative and borrows: N set, Z and C clear.
        let lt = exec_cmp(5, 7);
        assert_eq!(flag(&lt, Z), 0);
        assert_eq!(flag(&lt, N), 1, "N set: 5 - 7 is negative");
        assert_eq!(flag(&lt, C), 0, "C clear: 5 <u 7");

        // A b.eq reads Z: taken when set, fall-through (pc + 4) when clear.
        let run_beq = |z: u64| -> u64 {
            let mut ex = Executor::new(64);
            MachineContext::write_pc(&mut ex, 0x1000);
            MachineContext::write_register(&mut ex, "PSTATE", Z, APInt::new(1, z)).unwrap();
            let beq = tir_arm64::BranchEqOpBuilder::new(&context)
                .attr("imm", AttributeValue::Int(4))
                .build();
            let mi = context
                .get_op(beq.id())
                .as_interface::<dyn MachineInstruction>()
                .expect("b.eq is a machine instruction");
            mi.execute(&mut ex).expect("b.eq executes");
            MachineContext::read_pc(&ex)
        };
        // imm=4, target = pc + (sext(imm) << 2) = 0x1000 + 16.
        assert_eq!(run_beq(1), 0x1010, "branch taken when Z is set");
        assert_eq!(
            run_beq(0),
            0x1004,
            "fall-through (pc + width) when Z is clear"
        );

        // `bl` writes two destinations: the link register (x30 = pc + 4) and PC.
        // Both used to be dropped because only one assignment was ever emitted.
        let mut ex = Executor::new(64);
        MachineContext::write_pc(&mut ex, 0x2000);
        let bl = tir_arm64::BranchLinkOpBuilder::new(&context)
            .attr("imm", AttributeValue::Int(3))
            .build();
        let mi = context
            .get_op(bl.id())
            .as_interface::<dyn MachineInstruction>()
            .expect("bl is a machine instruction");
        mi.execute(&mut ex).expect("bl executes");
        let x30 = MachineContext::read_register(&ex, "GPR", 30)
            .unwrap()
            .to_u64();
        assert_eq!(
            x30, 0x2004,
            "link register holds the return address (pc + 4)"
        );
        assert_eq!(
            MachineContext::read_pc(&ex),
            0x2000 + (3 << 2),
            "pc takes the branch target"
        );
    }

    /// An executor configured with x86 register files, widths, and sub-register
    /// views, so writes through the GPR8/GPR16/GPR8H classes exercise the
    /// merge/offset policies.
    fn x86_executor() -> Executor {
        let mut ex = Executor::new(64);
        let info = tir_x86_64::register_info();
        let files: std::collections::HashMap<String, String> = info
            .classes
            .iter()
            .map(|c| (c.name.to_string(), c.file.to_string()))
            .collect();
        ex.set_register_files(files);
        ex.set_register_widths(tir_x86_64::register_widths(tir_x86_64::Feature::ALL));
        ex.set_register_views(tir_x86_64::register_views(tir_x86_64::Feature::ALL));
        ex
    }

    #[test]
    fn x86_16bit_write_preserves_upper_bits() {
        use tir::backend::MachineContext;
        let mut ex = x86_executor();
        // rax = all ones, then a 16-bit write to ax leaves bits 63:16 untouched.
        MachineContext::write_register(&mut ex, "GPR", 0, APInt::new(64, u64::MAX)).unwrap();
        MachineContext::write_register(&mut ex, "GPR16", 0, APInt::new(16, 0x1234)).unwrap();
        assert_eq!(
            MachineContext::read_register(&ex, "GPR", 0)
                .unwrap()
                .to_u64(),
            0xFFFF_FFFF_FFFF_1234
        );
        assert_eq!(
            MachineContext::read_register(&ex, "GPR16", 0)
                .unwrap()
                .to_u64(),
            0x1234
        );
    }

    #[test]
    fn x86_high_byte_is_bits_15_8() {
        use tir::backend::MachineContext;
        let mut ex = x86_executor();
        MachineContext::write_register(&mut ex, "GPR", 0, APInt::new(64, 0xAAAA_AAAA_AAAA_0000))
            .unwrap();
        // al is bits 7:0, ah is bits 15:8; each write leaves the other byte and
        // the upper 48 bits alone.
        MachineContext::write_register(&mut ex, "GPR8", 0, APInt::new(8, 0x11)).unwrap();
        MachineContext::write_register(&mut ex, "GPR8H", 0, APInt::new(8, 0x22)).unwrap();
        assert_eq!(
            MachineContext::read_register(&ex, "GPR8", 0)
                .unwrap()
                .to_u64(),
            0x11,
            "al unchanged by the ah write"
        );
        assert_eq!(
            MachineContext::read_register(&ex, "GPR8H", 0)
                .unwrap()
                .to_u64(),
            0x22
        );
        assert_eq!(
            MachineContext::read_register(&ex, "GPR16", 0)
                .unwrap()
                .to_u64(),
            0x2211,
            "ax == ah:al"
        );
        assert_eq!(
            MachineContext::read_register(&ex, "GPR", 0)
                .unwrap()
                .to_u64(),
            0xAAAA_AAAA_AAAA_2211,
            "bits 63:16 preserved"
        );
    }

    #[test]
    fn x86_32bit_write_zero_extends() {
        use tir::backend::MachineContext;
        let mut ex = x86_executor();
        MachineContext::write_register(&mut ex, "GPR", 0, APInt::new(64, u64::MAX)).unwrap();
        MachineContext::write_register(&mut ex, "GPR32", 0, APInt::new(32, 0xDEAD_BEEF)).unwrap();
        assert_eq!(
            MachineContext::read_register(&ex, "GPR", 0)
                .unwrap()
                .to_u64(),
            0x0000_0000_DEAD_BEEF,
            "a 32-bit write zeroes bits 63:32"
        );
    }

    #[test]
    fn x86_write_al_then_read_rax() {
        use tir::backend::MachineContext;
        let mut ex = x86_executor();
        MachineContext::write_register(&mut ex, "GPR8", 0, APInt::new(8, 0x7F)).unwrap();
        let rax = MachineContext::read_register(&ex, "GPR", 0).unwrap();
        assert_eq!((rax.to_u64(), rax.width()), (0x7F, 64));
    }

    const ATOMIC_BASE: u64 = 0x8000_0000;
    const ATOMIC_ADDR: u64 = ATOMIC_BASE + 0x40;

    #[test]
    fn lr_then_sc_at_same_key_succeeds_and_writes() {
        use tir::backend::MachineContext;
        use tir::sem::MemOrdering;
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);
        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 0x1111_1111).unwrap();

        let old = ex
            .load_reserved(ATOMIC_ADDR, 4, MemOrdering::Relaxed)
            .unwrap();
        assert_eq!(old, 0x1111_1111);
        let ok = ex
            .store_conditional(ATOMIC_ADDR, 4, 0x2222_2222, MemOrdering::Relaxed)
            .unwrap();
        assert!(ok);
        assert_eq!(
            MachineContext::read_memory(&ex, ATOMIC_ADDR, 4).unwrap(),
            0x2222_2222
        );
    }

    #[test]
    fn sc_without_reservation_fails_and_leaves_memory() {
        use tir::backend::MachineContext;
        use tir::sem::MemOrdering;
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);
        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 0x1111_1111).unwrap();

        let ok = ex
            .store_conditional(ATOMIC_ADDR, 4, 0x2222_2222, MemOrdering::Relaxed)
            .unwrap();
        assert!(!ok);
        assert_eq!(
            MachineContext::read_memory(&ex, ATOMIC_ADDR, 4).unwrap(),
            0x1111_1111
        );
    }

    #[test]
    fn sc_with_mismatched_address_or_size_fails() {
        use tir::backend::MachineContext;
        use tir::sem::MemOrdering;
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);
        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 0x1111_1111).unwrap();

        ex.load_reserved(ATOMIC_ADDR, 4, MemOrdering::Relaxed)
            .unwrap();
        let wrong_addr = ex
            .store_conditional(ATOMIC_ADDR + 4, 4, 0x2222_2222, MemOrdering::Relaxed)
            .unwrap();
        assert!(
            !wrong_addr,
            "different address must not match the reservation"
        );

        ex.load_reserved(ATOMIC_ADDR, 4, MemOrdering::Relaxed)
            .unwrap();
        let wrong_size = ex
            .store_conditional(ATOMIC_ADDR, 2, 0x2222, MemOrdering::Relaxed)
            .unwrap();
        assert!(!wrong_size, "different size must not match the reservation");
    }

    #[test]
    fn second_sc_after_success_fails() {
        use tir::backend::MachineContext;
        use tir::sem::MemOrdering;
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);
        ex.load_reserved(ATOMIC_ADDR, 4, MemOrdering::Relaxed)
            .unwrap();
        assert!(
            ex.store_conditional(ATOMIC_ADDR, 4, 0x1, MemOrdering::Relaxed)
                .unwrap()
        );
        assert!(
            !ex.store_conditional(ATOMIC_ADDR, 4, 0x2, MemOrdering::Relaxed)
                .unwrap(),
            "the reservation is consumed by the first successful SC"
        );
    }

    #[test]
    fn plain_store_between_lr_and_sc_keeps_reservation() {
        use tir::backend::MachineContext;
        use tir::sem::MemOrdering;
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);
        ex.load_reserved(ATOMIC_ADDR, 4, MemOrdering::Relaxed)
            .unwrap();
        // Documented policy: a plain store by the same hart does not clear the
        // reservation, so the following SC still succeeds.
        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 0xDEAD_BEEF).unwrap();
        assert!(
            ex.store_conditional(ATOMIC_ADDR, 4, 0x2222_2222, MemOrdering::Relaxed)
                .unwrap()
        );
        assert_eq!(
            MachineContext::read_memory(&ex, ATOMIC_ADDR, 4).unwrap(),
            0x2222_2222
        );
    }

    #[test]
    fn atomic_rmw_add_min_maxu() {
        use tir::backend::MachineContext;
        use tir::sem::{AtomicRmwOp, MemOrdering};
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);

        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 5).unwrap();
        let old = ex
            .atomic_rmw(AtomicRmwOp::Add, ATOMIC_ADDR, 4, 7, MemOrdering::Relaxed)
            .unwrap();
        assert_eq!(old, 5, "amo returns the old value");
        assert_eq!(
            MachineContext::read_memory(&ex, ATOMIC_ADDR, 4).unwrap(),
            12
        );

        // Unsigned max at 32 bits: 0x8000_0000 (high bit set) beats 1.
        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 0x8000_0000).unwrap();
        let old = ex
            .atomic_rmw(AtomicRmwOp::MaxU, ATOMIC_ADDR, 4, 1, MemOrdering::Relaxed)
            .unwrap();
        assert_eq!(old, 0x8000_0000);
        assert_eq!(
            MachineContext::read_memory(&ex, ATOMIC_ADDR, 4).unwrap(),
            0x8000_0000
        );

        // Signed min at 32 bits: 0xFFFF_FFFF (-1) beats 5.
        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 5).unwrap();
        let old = ex
            .atomic_rmw(
                AtomicRmwOp::Min,
                ATOMIC_ADDR,
                4,
                0xFFFF_FFFF,
                MemOrdering::Relaxed,
            )
            .unwrap();
        assert_eq!(old, 5);
        assert_eq!(
            MachineContext::read_memory(&ex, ATOMIC_ADDR, 4).unwrap(),
            0xFFFF_FFFF
        );
    }

    #[test]
    fn fence_records_kind_and_changes_nothing() {
        use tir::backend::MachineContext;
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);
        MachineContext::write_register(&mut ex, "GPR", 1, APInt::new(64, 7)).unwrap();
        ex.record_trace = true;
        ex.capturing_mem = true;

        ex.fence(0b0011, 0b0011, 0).unwrap();
        {
            let staged = ex.mem_stage.borrow();
            assert_eq!(staged.len(), 1);
            assert_eq!(
                staged[0].kind,
                crate::MemAccessKind::Fence {
                    pred: 0b0011,
                    succ: 0b0011,
                    ifence: false,
                }
            );
        }
        ex.fence(0, 0, 1).unwrap();
        assert_eq!(
            ex.mem_stage.borrow()[1].kind,
            crate::MemAccessKind::Fence {
                pred: 0,
                succ: 0,
                ifence: true,
            }
        );
        assert_eq!(
            MachineContext::read_register(&ex, "GPR", 1)
                .unwrap()
                .to_u64(),
            7,
            "fence leaves architectural state untouched"
        );
    }

    #[test]
    fn mem_trace_records_atomic_kinds() {
        use tir::backend::MachineContext;
        use tir::sem::{AtomicRmwOp, MemOrdering};
        let mut ex = Executor::new_at(4096, ATOMIC_BASE);
        // Set up memory before enabling capture so these writes are not recorded.
        MachineContext::write_memory(&mut ex, ATOMIC_ADDR, 4, 5).unwrap();
        ex.record_trace = true;
        ex.capturing_mem = true;

        ex.load_reserved(ATOMIC_ADDR, 4, MemOrdering::Relaxed)
            .unwrap();
        ex.store_conditional(ATOMIC_ADDR, 4, 6, MemOrdering::Relaxed)
            .unwrap();
        ex.atomic_rmw(AtomicRmwOp::Add, ATOMIC_ADDR, 4, 1, MemOrdering::Relaxed)
            .unwrap();

        let kinds: Vec<_> = ex.mem_stage.borrow().iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            vec![
                crate::MemAccessKind::LoadReserved,
                crate::MemAccessKind::StoreConditional { success: true },
                crate::MemAccessKind::AtomicRmw,
            ]
        );
    }
}
