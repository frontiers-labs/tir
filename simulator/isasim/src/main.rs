//! `isasim`: the dynamic ISA simulator CLI. Parses a target assembly snippet,
//! runs it functionally on the `tir-sim` executor, and optionally replays the
//! recorded trace through a TMDL machine model for a cycle-approximate timing
//! report. See `docs/design/simulator.md`.

use clap::Parser;
use tir_sim::scoreboard::Prf;
use tir_sim::timing::{self, TimingConfig};
use tir_sim::{Executor, ProgramImage, TraceOptions};

mod dump;
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
    #[arg(long)]
    until_pc: String,
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
    /// Branch predictor for `--timing`: `not-taken` or `btfn`.
    #[arg(long, default_value = "btfn")]
    predictor: String,
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
    let src = std::fs::read_to_string(&args.program).expect("failed to read program path");

    let target = tir_targets::select(&args.march, args.mcpu.as_deref(), args.mattr.as_deref())
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(2);
        });

    let context = tir::Context::with_default_dialects();
    target.register_dialects(&context);
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
    let until_pc = resolve_pc(&args.until_pc, &program.symbols);
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

    // Install the selected ISA's parameters and register widths so behaviors
    // execute with the configured XLEN (e.g. rv32 arithmetic wraps at 32 bits).
    executor.set_isa_params(target.isa_params());
    executor.set_register_widths(target.register_widths());

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
    let model = if args.timing {
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
        let m = target.machine_model(name).unwrap_or_else(|| {
            eprintln!(
                "unknown machine '{}' for target '{}' (one of: {})",
                name,
                target.name(),
                target.machines().join(", "),
            );
            std::process::exit(2);
        });
        executor.enable_trace_recording();
        Some(m)
    } else {
        None
    };

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
        let mut predictor = tir_sim::predictor::by_name(&args.predictor).unwrap_or_else(|| {
            eprintln!(
                "unknown predictor '{}' (expected: not-taken, btfn)",
                args.predictor
            );
            std::process::exit(2);
        });
        let config = TimingConfig::for_model(&model);
        let prf = Prf::for_target(&register_info, &model);
        let result = timing::simulate(
            &model,
            &context,
            executor.trace(),
            &config,
            predictor.as_mut(),
            Some(&prf),
        );
        println!(
            "timing[{} / {}]: {} instructions, {} cycles, IPC {:.3}, {} mispredicts",
            model.name,
            predictor.name(),
            result.instructions,
            result.cycles,
            result.ipc(),
            result.mispredicts,
        );
    }

    if let Some(path) = &args.dump_state {
        dump::write_state_dump(&executor, path, &args.dump_mem);
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
