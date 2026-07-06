//! `isasim`: the dynamic ISA simulator CLI. Parses a target assembly snippet,
//! runs it functionally on the `tir-sim` executor, and optionally replays the
//! recorded trace through a TMDL machine model for a cycle-approximate timing
//! report. See `docs/design/simulator.md`.

use clap::Parser;
use tir_sim::memsys::{CacheParams, MemParams, MemStats, MemorySystem};
use tir_sim::scoreboard::{EventHandler, Prf};
use tir_sim::timing::{self, TimingConfig};
use tir_sim::{Executor, ProgramImage, TraceOptions};

// Force the backend crates to be linked so their `register_target!` entries are
// included in the final binary; the target registry is otherwise their only user.
use tir_arm64 as _;
use tir_riscv as _;
use tir_x86_64 as _;

mod dump;
mod elf;
mod konata;
mod memory;

#[derive(Parser)]
struct Cli {
    /// Target architecture (e.g. `riscv64`, `rv64im`, `arm64`).
    #[arg(long)]
    march: String,
    /// Target CPU. A TMDL machine name (e.g. `scr1-3stage`) provides the default
    /// for `--machine`.
    #[arg(long)]
    mcpu: Option<String>,
    /// Target feature toggles (e.g. `+m,-zmmul`), applied on top of `--march`.
    #[arg(long)]
    mattr: Option<String>,
    #[arg(long, default_value_t = 65536)]
    mem_size: usize,
    #[arg(long, default_value_t = 0x80000000_u64)]
    mem_start_address: u64,
    #[arg(long)]
    entry: Option<String>,
    /// Address or symbol to stop at. Required for assembly snippets; optional for
    /// ELF images (which run until an exit syscall).
    #[arg(long)]
    until_pc: Option<String>,
    /// JSON memory image with `regions`: `{ "regions": [{"start":"0x80001000", "hex":"efbeadde"}] }`.
    #[arg(long)]
    memory_config: Option<String>,
    /// Disable the default snippet-test memory allocation installed when no memory config is supplied.
    #[arg(long, default_value_t = false)]
    no_default_memory: bool,
    #[arg(long, default_value_t = 100000)]
    max_cycles: u64,
    #[arg(long, default_value_t = false)]
    trace_instructions: bool,
    #[arg(long, default_value_t = false)]
    trace_registers_each: bool,
    #[arg(long, default_value_t = false)]
    trace_registers_end: bool,
    /// Report cycle-approximate timing after the functional run.
    #[arg(long, default_value_t = false)]
    timing: bool,
    /// Machine model for `--timing` (target-specific, e.g. `rv64-ooo`).
    #[arg(long)]
    machine: Option<String>,
    /// Branch predictor for `--timing`: `not-taken`, `btfn`, `tage`, or `batage`.
    #[arg(long, default_value = "btfn")]
    predictor: String,
    /// Memory hierarchy for `--timing`: `none` (fixed latency, default),
    /// `a720-like`, `a520-like`, or `test` (a tiny hierarchy for lit tests).
    #[arg(long, default_value = "none")]
    mem_model: String,
    /// Data prefetcher for `--timing` (requires `--mem-model`): `none` (default),
    /// `next-line`, or `stride`.
    #[arg(long, default_value = "none")]
    prefetcher: String,
    /// Tunable parameters for `tage`/`batage`, as `key=value,...` (e.g.
    /// `tables=12,min_hist=4,max_hist=640,log_table=10,tag_bits=12,ctr_bits=3`).
    #[arg(long, default_value = "")]
    predictor_config: String,
    /// Write a Konata/Kanata pipeline log of the `--timing` run to this path
    /// (`-` for stdout). View it with <https://github.com/shioyadan/Konata>.
    #[arg(long, requires = "timing")]
    konata: Option<String>,
    /// Write a structured JSON snapshot of architectural state (PC, GPRs, and any
    /// requested memory windows) to this path after the run. Used by the
    /// differential ISA test suite to compare against a golden oracle.
    #[arg(long)]
    dump_state: Option<String>,
    /// Memory window to include in `--dump-state`, formatted `addr:len` (e.g.
    /// `0x80008000:256`). Repeatable. Ignored unless `--dump-state` is set.
    #[arg(long)]
    dump_mem: Vec<String>,
    program: String,
}

fn main() {
    let args = Cli::parse();
    let bytes = std::fs::read(&args.program).expect("failed to read program path");

    let target =
        tir::backend::select_target(&args.march, args.mcpu.as_deref(), args.mattr.as_deref())
            .unwrap_or_else(|error| {
                eprintln!("{error}");
                std::process::exit(2);
            });

    let context = tir::Context::with_default_dialects();
    target.register_dialects(&context);

    // An ELF image is executed by decode-on-fetch; anything else is an assembly
    // snippet parsed into a program image.
    if bytes.starts_with(b"\x7fELF") {
        run_elf(&args, target.as_ref(), &context, &bytes);
        return;
    }

    let src = String::from_utf8(bytes).expect("program is neither ELF nor UTF-8 assembly");
    let asm_parser = target.asm_parser(&context);
    let module = asm_parser
        .parse_asm(&context, &src)
        .expect("failed to parse assembly");

    let program = ProgramImage::from_module(
        &context,
        module,
        args.mem_start_address,
        args.entry.as_deref(),
    )
    .expect("failed to build program image");

    // `--until-pc` accepts either a symbol name or a numeric address, so tests
    // can stop at a label without hand-computing its address.
    let until_arg = args.until_pc.as_deref().unwrap_or_else(|| {
        eprintln!("--until-pc is required for assembly snippets");
        std::process::exit(2);
    });
    let until_pc = resolve_pc(until_arg, &program.symbols);
    let mut executor = Executor::new_at(args.mem_size, args.mem_start_address);
    // Teach the executor which register classes share a physical file so, e.g.,
    // a value written via AArch64 `GPRsp` reads back through `GPR`.
    let register_info = target.register_info();
    let register_files = register_info
        .classes
        .iter()
        .map(|c| (c.name.to_string(), c.file.to_string()))
        .collect();
    executor.set_register_files(register_files);
    executor.set_hardwired_zero_registers(target.hardwired_zero_registers().iter().copied());

    // Install the selected ISA's parameters and register widths so behaviors
    // execute with the configured XLEN (e.g. rv32 arithmetic wraps at 32 bits).
    executor.set_isa_params(target.isa_params());
    executor.set_register_widths(target.register_widths());
    executor.set_register_views(target.register_views());
    // Route reads of counter-backed registers (e.g. the RISC-V cycle/instret
    // CSRs) to the executor's performance counters.
    executor.set_counter_registers(target.counter_registers());
    // Bare-metal snippets have no exception environment: report ecall/ebreak
    // and stop the run cleanly instead of failing it.
    executor.set_exception_handler(Box::new(|_executor, cause, pc| {
        println!("trap: cause={cause} pc={pc:#x}");
        tir_sim::ExceptionAction::Halt
    }));

    if let Some(path) = &args.memory_config {
        memory::load_memory_config(&mut executor, path);
    } else if !args.no_default_memory {
        memory::install_default_test_memory(
            &mut executor,
            target.name(),
            args.mem_start_address,
            args.mem_size,
        );
    }

    // Pick the timing model up front so a bad `--machine` fails before running.
    let model = select_timing_model(&args, target.as_ref(), &mut executor);

    executor.load(program).expect("failed to load program");
    let trace = TraceOptions {
        instructions: args.trace_instructions,
        registers_after_each_instruction: args.trace_registers_each,
        registers_at_end: args.trace_registers_end,
    };
    let mut stdout = std::io::stdout();
    executor
        .run_with_trace(until_pc, args.max_cycles, trace, &mut stdout)
        .expect("program execution failed");

    if let Some(model) = model {
        report_timing(
            &args,
            target.as_ref(),
            &context,
            &register_info,
            &model,
            &executor,
        );
    }

    if let Some(path) = &args.dump_state {
        dump::write_state_dump(&executor, path, &args.dump_mem);
    }
}

/// Select and validate the `--timing` machine model (defaulting from `--mcpu`),
/// enabling trace recording on the executor. Returns `None` when `--timing` is
/// off; exits on an unknown model.
fn select_timing_model(
    args: &Cli,
    target: &dyn tir::backend::TargetMachine,
    executor: &mut Executor,
) -> Option<tir::backend::sched::MachineModel> {
    if !args.timing {
        return None;
    }
    let name = args
        .machine
        .as_deref()
        .or_else(|| target.default_machine())
        .unwrap_or_else(|| {
            eprintln!(
                "--timing requires --machine or --mcpu (one of: {})",
                target.machines().join(", "),
            );
            std::process::exit(2);
        });
    let model = target.machine_model(name).unwrap_or_else(|| {
        eprintln!(
            "unknown machine '{}' for target '{}' (one of: {})",
            name,
            target.name(),
            target.machines().join(", "),
        );
        std::process::exit(2);
    });
    executor.enable_trace_recording();
    Some(model)
}

/// Replay the recorded trace through `model` and print a cycle-approximate
/// timing summary (plus an optional Konata pipeline log).
fn report_timing(
    args: &Cli,
    target: &dyn tir::backend::TargetMachine,
    context: &tir::Context,
    register_info: &tir::backend::regalloc::RegisterInfo,
    model: &tir::backend::sched::MachineModel,
    executor: &Executor,
) {
    let mut predictor = tir_sim::predictor::by_name(&args.predictor, &args.predictor_config)
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(2);
        });
    let config = TimingConfig::for_model(model);
    let prf = Prf::for_target(register_info, model);
    let printer = target.asm_printer(context);
    let disasm = |id: tir::OpId, pc: u64| {
        let op = context.get_op(id);
        let text = printer
            .print_instruction(context, &op)
            .ok()
            .flatten()
            .unwrap_or_else(|| op.name().to_string());
        format!("{pc:#x}: {text}")
    };
    let mut konata_view = args.konata.as_ref().map(|_| {
        let labels = executor
            .trace()
            .iter()
            .map(|(id, pc)| disasm(*id, *pc))
            .collect();
        konata::KonataView::new(labels)
    });
    let mut mem = mem_model(&args.mem_model);
    match (mem.as_mut(), args.prefetcher.as_str()) {
        (Some(mem), name) => {
            if let Some(pf) = tir_sim::prefetch::prefetcher_by_name(name, mem.line())
                .unwrap_or_else(|error| {
                    eprintln!("{error}");
                    std::process::exit(2);
                })
            {
                mem.set_prefetcher(pf);
            }
        }
        (None, "none") => {}
        (None, _) => {
            eprintln!("--prefetcher requires --mem-model");
            std::process::exit(2);
        }
    }
    let mem_trace = mem.as_ref().map(|_| executor.mem_trace());
    let result = timing::simulate(
        model,
        context,
        executor.trace(),
        &config,
        predictor.as_mut(),
        Some(&prf),
        mem_trace,
        mem.as_mut(),
        konata_view.as_mut().map(|v| v as &mut dyn EventHandler),
    );
    if let Some(view) = konata_view.as_mut() {
        build_speculation(view, executor, context, model, &disasm);
    }
    if let (Some(path), Some(view)) = (&args.konata, &konata_view) {
        let log = view.render();
        if path == "-" {
            print!("{log}");
        } else {
            std::fs::write(path, log).expect("failed to write konata log");
        }
    }
    println!(
        "timing[{} / {}]: {} instructions, {} cycles, IPC {:.3}, {} mispredicts",
        model.name,
        predictor.name(),
        result.instructions,
        result.cycles,
        result.ipc(),
        result.mispredicts,
    );
    if let Some(mem) = &mem {
        print_mem_stats(mem.stats(), args.prefetcher != "none");
    }
}

/// Print one line per cache level plus DRAM and writeback totals, in a stable
/// FileCheck-friendly format.
fn print_mem_stats(stats: &MemStats, prefetching: bool) {
    let line = |name: &str, s: &tir_sim::memsys::LevelStats| {
        println!(
            "mem[{name}]: {} accesses, {} hits, {} misses",
            s.accesses, s.hits, s.misses
        );
    };
    line("l1i", &stats.l1i);
    line("l1d", &stats.l1d);
    line("l2", &stats.l2);
    line("l3", &stats.l3);
    println!("mem[dram]: {} accesses", stats.dram_accesses);
    println!("mem[writebacks]: {}", stats.writebacks);
    if prefetching {
        let p = stats.prefetch;
        println!(
            "mem[pf]: {} issued, {} useful, {} late",
            p.issued, p.useful, p.late
        );
    }
}

/// Build the memory hierarchy for `--mem-model`, or `None` for the default
/// fixed-latency model. Presets approximate measured Cortex-A720/A520 caches
/// (64-byte lines throughout); `test` is a tiny hierarchy so lit tests can force
/// misses with small footprints. Exits on an unknown name.
fn mem_model(name: &str) -> Option<MemorySystem> {
    let c = |size, ways, latency, banks, mshrs| CacheParams {
        size,
        ways,
        line: 64,
        latency,
        banks,
        mshrs,
    };
    let params = match name {
        "none" => return None,
        "a720-like" => MemParams {
            l1i: c(64 << 10, 4, 4, 2, 8),
            l1d: c(64 << 10, 4, 4, 4, 12),
            l2: Some(c(512 << 10, 8, 9, 4, 24)),
            l3: Some(c(6 << 20, 12, 16, 8, 32)),
            dram_latency: 439,
            dram_streams: 20,
        },
        "a520-like" => MemParams {
            l1i: c(32 << 10, 4, 5, 2, 4),
            l1d: c(32 << 10, 4, 5, 2, 8),
            l2: Some(c(4 << 20, 8, 74, 4, 16)),
            l3: None,
            dram_latency: 453,
            dram_streams: 24,
        },
        "test" => MemParams {
            l1i: c(4 << 10, 4, 2, 1, 4),
            l1d: c(4 << 10, 4, 2, 1, 4),
            l2: Some(c(16 << 10, 8, 8, 1, 8)),
            l3: None,
            dram_latency: 50,
            dram_streams: 4,
        },
        other => {
            eprintln!("unknown --mem-model '{other}' (one of: none, a720-like, a520-like, test)");
            std::process::exit(2);
        }
    };
    Some(MemorySystem::new(params))
}

/// Recover the wrong-path (speculative) instruction stream under each
/// mispredicted branch and hand it to the Konata view. A trace replay only holds
/// the committed path, so we re-derive what the front end *would* have fetched:
/// starting from the predicted (wrong) direction, we decode straight down the
/// instruction stream, following taken branches/jumps through the learned target
/// table (a backward conditional is assumed taken, i.e. a loop continuing). These
/// instructions are squashed when the branch resolves, so their values never
/// matter — only their shape and cost, which is all the timeline needs.
fn build_speculation(
    view: &mut konata::KonataView,
    executor: &Executor,
    context: &tir::Context,
    model: &tir::backend::sched::MachineModel,
    disasm: &dyn Fn(tir::OpId, u64) -> String,
) {
    use std::collections::HashMap;
    use tir::backend::{ControlFlow, MachineInstruction};

    let trace = executor.trace();
    let width_of = |id: tir::OpId| {
        context
            .get_op(id)
            .as_interface::<dyn MachineInstruction>()
            .map(|mi| u64::from(mi.width_bytes()))
            .unwrap_or(4)
    };
    // Learned targets: every taken control transfer seen in the committed trace.
    let mut btb: HashMap<u64, u64> = HashMap::new();
    for pair in trace.windows(2) {
        let (id, pc) = pair[0];
        if pair[1].1 != pc.wrapping_add(width_of(id)) {
            btb.insert(pc, pair[1].1);
        }
    }

    for branch in view.mispredicted_branches() {
        let cap = view.spec_window(branch);
        if cap == 0 {
            continue;
        }
        let (branch_id, branch_pc) = trace[branch];
        let fallthrough = branch_pc.wrapping_add(width_of(branch_id));
        let actual_taken = trace
            .get(branch + 1)
            .is_some_and(|&(_, pc)| pc != fallthrough);
        // The wrong path is the direction the branch did *not* go.
        let mut pc = if actual_taken {
            fallthrough
        } else {
            match btb.get(&branch_pc) {
                Some(&target) => target,
                None => continue, // never saw it taken, so the target is unknown
            }
        };

        let mut instrs = Vec::with_capacity(cap);
        while instrs.len() < cap {
            let Some(id) = executor.decode_at(pc) else {
                break;
            };
            let Some(mi) = context.get_op(id).as_interface::<dyn MachineInstruction>() else {
                break;
            };
            let class = model.sched_class(mi.mnemonic());
            instrs.push(konata::SpecInstr {
                label: disasm(id, pc),
                is_memory: konata::is_memory_class(class.resources),
            });
            let width = u64::from(mi.width_bytes());
            pc = match mi.control_flow() {
                ControlFlow::Unconditional => match btb.get(&pc) {
                    Some(&target) => target,
                    None => break,
                },
                // Assume backward conditionals taken (loop continues), forward
                // ones fall through — the usual speculative guess.
                ControlFlow::Conditional => match btb.get(&pc) {
                    Some(&target) if target < pc => target,
                    _ => pc.wrapping_add(width),
                },
                ControlFlow::None => pc.wrapping_add(width),
            };
        }
        view.add_speculation(branch, instrs);
    }
}

/// Load a statically-linked ELF64 executable into guest memory and run it by
/// decode-on-fetch, servicing the handful of Linux AArch64 syscalls the
/// freestanding benchmark runner uses. Stops at the exit syscall (or `--until-pc`
/// / `--max-cycles`), then reports the exit status.
fn run_elf(
    args: &Cli,
    target: &dyn tir::backend::TargetMachine,
    context: &tir::Context,
    bytes: &[u8],
) {
    use tir::backend::MachineContext;
    let loaded = elf::load_executable(bytes).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });
    let decoder = target.instruction_decoder().unwrap_or_else(|| {
        eprintln!("target '{}' has no machine-code decoder", target.name());
        std::process::exit(2);
    });

    // Lay out a single flat region covering every PT_LOAD segment plus a stack
    // above them.
    let page = 0x1000u64;
    let base = loaded.min_vaddr & !(page - 1);
    let seg_top = (loaded.max_vaddr_end + page - 1) & !(page - 1);
    let stack_size = 1u64 << 20;
    let mem_size = (seg_top - base + stack_size) as usize;
    let mut executor = Executor::new_at(mem_size, base);

    let register_info = target.register_info();
    let register_files = register_info
        .classes
        .iter()
        .map(|c| (c.name.to_string(), c.file.to_string()))
        .collect();
    executor.set_register_files(register_files);
    executor.set_hardwired_zero_registers(target.hardwired_zero_registers().iter().copied());
    executor.set_isa_params(target.isa_params());
    executor.set_register_widths(target.register_widths());
    executor.set_register_views(target.register_views());
    executor.set_counter_registers(target.counter_registers());

    for seg in &loaded.segments {
        let start = seg.offset as usize;
        let end = start + seg.filesz as usize;
        executor
            .write_bytes(seg.vaddr, &bytes[start..end])
            .expect("ELF segment does not fit in guest memory");
    }

    // Stack pointer at the top of the region, 16-byte aligned (AArch64 requires
    // 16-byte SP alignment).
    let sp = (base + mem_size as u64 - 16) & !0xF;
    executor
        .write_register("GPRsp", 31, tir::utils::APInt::new(64, sp))
        .expect("failed to initialize stack pointer");

    executor.set_decoder(context.clone(), decoder);
    executor.set_entry(loaded.entry);

    let exit_code = std::rc::Rc::new(std::cell::Cell::new(0i32));
    executor.set_exception_handler(Box::new({
        let exit_code = exit_code.clone();
        move |ex, _cause, _pc| syscall(ex, &exit_code)
    }));

    let model = select_timing_model(args, target, &mut executor);

    let until_pc = args.until_pc.as_deref().map(parse_addr).unwrap_or(u64::MAX);
    let trace = TraceOptions {
        instructions: args.trace_instructions,
        registers_after_each_instruction: args.trace_registers_each,
        registers_at_end: args.trace_registers_end,
    };
    let mut stdout = std::io::stdout();
    executor
        .run_with_trace(until_pc, args.max_cycles, trace, &mut stdout)
        .expect("program execution failed");

    println!("exit: {}", exit_code.get());

    if let Some(model) = model {
        report_timing(args, target, context, &register_info, &model, &executor);
    }

    if let Some(path) = &args.dump_state {
        dump::write_state_dump(&executor, path, &args.dump_mem);
    }
}

/// Service a Linux AArch64 syscall raised by `svc` (syscall number in x8, args
/// in x0-x5). Implements only what the freestanding benchmark runner uses.
fn syscall(
    ex: &mut Executor,
    exit_code: &std::rc::Rc<std::cell::Cell<i32>>,
) -> tir_sim::ExceptionAction {
    use tir::backend::MachineContext;
    let reg = |ex: &Executor, i: u16| ex.read_register("GPR", i).map(|v| v.to_u64()).unwrap_or(0);
    let num = reg(ex, 8);
    match num {
        // exit / exit_group
        93 | 94 => {
            exit_code.set(reg(ex, 0) as i32);
            tir_sim::ExceptionAction::Halt
        }
        // write(fd, buf, len)
        64 => {
            use std::io::Write;
            let (fd, buf, len) = (reg(ex, 0), reg(ex, 1), reg(ex, 2));
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len {
                out.push(ex.read_memory(buf + i, 1).map(|v| v as u8).unwrap_or(0));
            }
            if fd == 2 {
                let _ = std::io::stderr().write_all(&out);
            } else {
                let _ = std::io::stdout().write_all(&out);
            }
            let _ = ex.write_register("GPR", 0, tir::utils::APInt::new(64, len));
            tir_sim::ExceptionAction::Continue
        }
        other => {
            eprintln!("unimplemented syscall {other}");
            tir_sim::ExceptionAction::Halt
        }
    }
}

/// Resolve a `--until-pc` argument to an address. The argument may be a `0x`
/// hex literal, a decimal address, or the name of a symbol in the program.
fn resolve_pc(arg: &str, symbols: &std::collections::BTreeMap<String, u64>) -> u64 {
    if let Some(hex) = arg.strip_prefix("0x").or_else(|| arg.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).expect("invalid hex address");
    }
    if let Ok(addr) = arg.parse::<u64>() {
        return addr;
    }
    *symbols.get(arg).unwrap_or_else(|| {
        eprintln!("--until-pc: '{arg}' is neither an address nor a known symbol");
        std::process::exit(2);
    })
}

/// Parse a `0x`-hex or decimal address.
pub(crate) fn parse_addr(addr: &str) -> u64 {
    if let Some(hex) = addr.strip_prefix("0x").or_else(|| addr.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).expect("invalid hex address")
    } else {
        addr.parse::<u64>().expect("invalid decimal address")
    }
}
