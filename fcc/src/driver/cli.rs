use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use super::actions::{DriverOptions, InputFile, OptLevel, StopPhase, build_actions};
use super::exec::execute;
use crate::lang_options::LangOptions;

/// fcc — a C compiler. Run `fcc cc <args>` (or invoke as `cc`/`gcc`) for the
/// gcc-compatible driver.
#[derive(Debug, Parser)]
#[command(name = "fcc")]
pub struct Cli {
    /// Print a detailed explanation of a diagnostic code, e.g. `--explain E0001`.
    #[arg(long, value_name = "CODE")]
    pub(super) explain: Option<String>,
    #[command(subcommand)]
    pub(super) command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Compile a single translation unit up to a chosen stage.
    Compile(CompileArgs),
}

/// Whether `name` is a subcommand of the native clap CLI.
pub(super) fn is_known_subcommand(name: &str) -> bool {
    use clap::CommandFactory;
    Cli::command()
        .get_subcommands()
        .any(|command| command.get_name() == name)
}

#[derive(Debug, Args)]
pub struct CompileArgs {
    /// C language dialect, e.g. c17, gnu17, or c23.
    #[arg(long = "std", value_name = "STANDARD", default_value_t)]
    pub(super) lang_options: LangOptions,
    /// Stage to stop after and emit.
    #[arg(long, value_enum, default_value_t = CompileStage::Preprocess)]
    pub(super) stage: CompileStage,
    /// Target architecture (required for the asm and obj stages).
    #[arg(long)]
    pub(super) march: Option<String>,
    /// Target CPU
    #[arg(long)]
    pub(super) mcpu: Option<String>,
    /// Target calling convention.
    #[arg(long)]
    pub(super) mabi: Option<String>,
    /// Output file, or `-` for stdout.
    #[arg(short = 'o', default_value = "-")]
    output: OsString,
    /// Predefine a macro, e.g. `-D NAME=VALUE` (or `-D NAME`).
    #[arg(short = 'D', value_name = "NAME[=VALUE]")]
    pub(super) defines: Vec<String>,
    /// Add a directory to the include search path, e.g. `-I DIR`.
    #[arg(short = 'I', value_name = "DIR")]
    pub(super) include_dirs: Vec<PathBuf>,
    /// Report memory statistics on stderr, as `TIR_MEM_STATS=1` does.
    #[arg(long = "mem-report")]
    mem_report: bool,
    /// Optimisation level: 0 (no mid-end round), 1, 2 or 3.
    #[arg(
        short = 'O',
        value_name = "LEVEL",
        default_value_t = 0,
        value_parser = clap::value_parser!(u8).range(0..=3)
    )]
    opt_level: u8,
    /// Mid-end pass pipeline in MLIR-style syntax, e.g.
    /// `func.func(verify-deps,instcombine-nodes)`. Replaces the default function
    /// pipeline; frontend lowering and the backend are unaffected.
    #[arg(long = "pipeline", value_name = "PIPELINE")]
    pipeline: Option<String>,
    /// Re-linearize every machine block by a seeded random topological order of
    /// its dependence graph, after selection and again after allocation. A
    /// differential-testing oracle: the program is the same one, so a change in
    /// behavior is a dependence the machine form does not carry.
    #[arg(long = "shuffle-machine-order")]
    shuffle_machine_order: bool,
    /// C source files, or `-` for stdin.
    inputs: Vec<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq, ValueEnum)]
pub enum CompileStage {
    /// Emit the preprocessed token stream as reconstructed source text.
    Preprocess,
    /// Emit the preprocessed token stream in its debug representation.
    Tokens,
    Ast,
    Ir,
    /// Emit textual assembly for the selected target.
    Asm,
    /// Emit an ELF relocatable object for the selected target.
    Obj,
}

pub(super) fn parse_cli<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args = args.into_iter().map(|arg| {
        let arg = arg.into();
        if arg == "-std" {
            OsString::from("--std")
        } else if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("-std=")) {
            OsString::from(format!("--std={value}"))
        } else {
            arg
        }
    });
    Cli::try_parse_from(args)
}

pub(super) fn run_compile(args: CompileArgs) {
    if args.mem_report {
        tir::memstats::enable();
    }
    let opts = lower(args);
    let actions = build_actions(&opts).unwrap_or_else(|e| {
        eprintln!("fcc: error: {e}");
        std::process::exit(1);
    });
    execute(&actions, &opts).unwrap_or_else(|e| {
        eprintln!("fcc: error: {e}");
        std::process::exit(1);
    });
}

pub(super) fn lower(args: CompileArgs) -> DriverOptions {
    let stop = match args.stage {
        CompileStage::Preprocess => StopPhase::Preprocess,
        CompileStage::Tokens => StopPhase::Tokens,
        CompileStage::Ast => StopPhase::Ast,
        CompileStage::Ir => StopPhase::Ir,
        CompileStage::Asm => StopPhase::Assembly,
        CompileStage::Obj => StopPhase::Object,
    };
    DriverOptions {
        inputs: args.inputs.into_iter().map(InputFile::CSource).collect(),
        output: Some(PathBuf::from(args.output)),
        stop,
        lang_options: args.lang_options,
        defines: args.defines,
        undefines: Vec::new(),
        include_dirs: args.include_dirs,
        march: args.march,
        mcpu: args.mcpu,
        mabi: args.mabi,
        lib_dirs: Vec::new(),
        libs: Vec::new(),
        pipeline: args.pipeline,
        opt_level: match args.opt_level {
            0 => OptLevel::O0,
            1 => OptLevel::O1,
            2 => OptLevel::O2,
            _ => OptLevel::O3,
        },
        shuffle_machine_order: args.shuffle_machine_order,
        dry_run: false,
    }
}
