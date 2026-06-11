//! The functional executor: the architectural oracle of the simulator. It
//! interprets TMDL-generated instruction semantics block by block, maintaining
//! only architectural state (registers, memory, PC). It knows nothing about
//! cycles — timing is recovered later by replaying the recorded trace against
//! a machine model (see [`crate::timing`]).

use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;

use tir::Context;
use tir_be_common::{MachineContext, MachineInstruction, PerfCounter, SimTrap};

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
    registers: HashMap<(String, u16), tir::utils::APInt>,
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
    /// Registers backed by performance counters (e.g. the RISC-V `cycle` CSR):
    /// reads return the counter value, writes are ignored.
    counter_registers: HashMap<(String, u16), PerfCounter>,
    /// Instructions retired so far. Drives every performance counter: the
    /// functional model retires one instruction per cycle, and time ticks with
    /// the cycle counter.
    retired_instructions: u64,
    exception_handler: Option<ExceptionHandler>,
    halted: bool,
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

    /// Configure architectural register widths per class (from
    /// `TargetMachine::register_widths`).
    pub fn set_register_widths(&mut self, widths: impl IntoIterator<Item = (&'static str, u32)>) {
        self.register_widths = widths
            .into_iter()
            .map(|(class, width)| (class.to_string(), width))
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

    /// The recorded dynamic instruction stream as `(op, pc)` pairs, in execution
    /// order. The PC lets a timing model reconstruct branch directions/outcomes.
    pub fn trace(&self) -> &[(tir::OpId, u64)] {
        &self.trace
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
        let result = self.run_inner(until_pc, max_cycles, trace, out);
        if trace.registers_at_end {
            self.emit_register_dump(out, "final registers");
        }
        result
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
                    op: op.name,
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
            machine_inst.execute(self)?;
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

    pub fn register_snapshot(&self) -> Vec<(String, u16, tir::utils::APInt)> {
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
                    value.to_u64(),
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
        // The program counter is held specially (it drives instruction fetch), but
        // semantics reference it as the `PC` register class (e.g. `PC::pc`).
        if class == "PC" {
            return Ok(self.resize_to_class_width(class, tir::utils::APInt::new(64, self.pc)));
        }
        if let Some(&counter) = self.counter_registers.get(&(class.to_string(), index)) {
            let value = self.counter_value(counter);
            return Ok(self.resize_to_class_width(class, tir::utils::APInt::new(64, value)));
        }
        let key = (self.register_file(class).to_string(), index);
        if let Some(value) = self.registers.get(&key) {
            return Ok(self.resize_to_class_width(class, value.clone()));
        }
        Ok(self.resize_to_class_width(class, tir::utils::APInt::new(64, 0)))
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
        let file = self.register_file(class).to_string();
        self.registers.insert((file, index), value);
        Ok(())
    }

    fn isa_param(&self, name: &str) -> Option<i64> {
        self.isa_params.get(name).copied()
    }

    fn read_memory(&self, address: u64, size: usize) -> Result<u64, SimTrap> {
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

    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), SimTrap> {
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
    use tir::utils::APInt;
    use tir_be_common::{AsmDialect, MachineInstruction};
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
        tir_be_common::MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, 3))
            .unwrap();
        executor.load(program).unwrap();
        executor.run(until_pc, 10).unwrap();

        let x1 = tir_be_common::MachineContext::read_register(&executor, "GPR", 1).unwrap();
        let x2 = tir_be_common::MachineContext::read_register(&executor, "GPR", 2).unwrap();
        assert_eq!(x1.to_u64(), 3);
        assert_eq!(x2.to_u64(), 0);
        assert_eq!(tir_be_common::MachineContext::read_pc(&executor), until_pc);
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

        let reg =
            |idx| tir_be_common::MachineContext::read_register(&executor, "GPR", idx).unwrap();
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
            Error::Trap(tir_be_common::SimTrap::MaxCyclesExceeded { .. }) => {}
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
        tir_be_common::MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, 7))
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

        let x0 = tir_be_common::MachineContext::read_register(&executor, "GPR", 0).unwrap();
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
        tir_be_common::MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, 2))
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
            Error::Trap(tir_be_common::SimTrap::MaxCyclesExceeded { .. }) => {}
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
        use tir_be_common::MachineContext;

        fn gpr(index: u16) -> AttributeValue {
            AttributeValue::Register(RegisterAttr::Physical {
                class: "GPR".to_string(),
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
            tir_be_common::MachineContext::write_register(
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
            tir_be_common::MachineContext::read_register(&executor, class, idx)
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
            tir_be_common::PerfCounter::InstructionsRetired,
        )]);
        executor.load(program).unwrap();
        executor.run(0x8000_0010, 10).unwrap();

        // The csrrw write to the read-only counter is ignored; the csrrs read
        // sees the three instructions retired before it.
        let x2 = tir_be_common::MachineContext::read_register(&executor, "GPR", 2).unwrap();
        assert_eq!(x2.to_u64(), 3);
        assert_eq!(executor.retired_instructions(), 4);
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
            Error::Trap(tir_be_common::SimTrap::Exception { cause, pc }) => {
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
            tir_be_common::MachineContext::read_register(&executor, "GPR", idx)
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
        use tir_be_common::{MachineContext, MachineInstruction};

        fn gpr(index: u16) -> AttributeValue {
            AttributeValue::Register(RegisterAttr::Physical {
                class: "GPR".to_string(),
                index,
            })
        }

        // PSTATE flag slots, assigned by declaration order in the register class.
        const N: u16 = 0;
        const Z: u16 = 1;
        const C: u16 = 2;
        const V: u16 = 3;

        let context = Context::with_default_dialects();
        context.register_dialect::<tir_be_common::AsmDialect>();
        context.register_dialect::<arm64::Arm64Dialect>();

        let exec_cmp = |x0: u64, x1: u64| -> Executor {
            let mut ex = Executor::new(64);
            MachineContext::write_register(&mut ex, "GPR", 0, APInt::new(64, x0)).unwrap();
            MachineContext::write_register(&mut ex, "GPR", 1, APInt::new(64, x1)).unwrap();
            let cmp = arm64::CompareOpBuilder::new(&context)
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
            let beq = arm64::BranchEqOpBuilder::new(&context)
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
        let bl = arm64::BranchLinkOpBuilder::new(&context)
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
}
