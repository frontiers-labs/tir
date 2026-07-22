use std::error::Error;

use clap::{Parser, Subcommand};

// Force-link target registrations; linkme drops crates the binary never references.
use tir_arm64 as _;
use tir_gpu as _;
use tir_riscv as _;
use tir_x86_64 as _;

mod common;

pub mod llvm_import;
pub mod mc;
pub mod model_check;
pub mod opt;
pub mod readobj;
pub mod sched;
pub mod spirv_io;

pub fn tools_main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::Mc(args) => mc::run(args),
        Command::ModelCheck(args) => model_check::run(args),
        Command::Opt(args) => opt::run(args),
        Command::Readobj(args) => readobj::run(args),
        Command::Sched(args) => sched::run(args),
        Command::LlvmImport(args) => llvm_import::run(args),
        Command::SpirvImport(args) => spirv_io::import(args),
        Command::SpirvExport(args) => spirv_io::export(args),
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Compile machine code
    Mc(mc::ToolArgs),
    /// Model-check a hardware implementation against TMDL semantics
    ModelCheck(model_check::ToolArgs),
    /// Run optimizations on IR
    Opt(opt::ToolArgs),
    /// Dump headers, symbols and relocations of an object file
    Readobj(readobj::ToolArgs),
    /// Print the data dependence graph of machine IR
    Sched(sched::ToolArgs),
    /// Import LLVM textual IR into TIR
    LlvmImport(llvm_import::ToolArgs),
    /// Import a SPIR-V binary into human-readable TIR
    SpirvImport(spirv_io::ImportArgs),
    /// Export human-readable TIR to a SPIR-V binary
    SpirvExport(spirv_io::ExportArgs),
}

#[derive(Parser)]
#[command(name = "tir", version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
