use std::error::Error;

use clap::{Parser, Subcommand};

mod common;

pub mod axioms;
pub mod llvm_import;
pub mod mc;
pub mod opt;
pub mod readobj;
pub mod sched;

pub fn tools_main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::Axioms(args) => axioms::run(args),
        Command::Mc(args) => mc::run(args),
        Command::Opt(args) => opt::run(args),
        Command::Readobj(args) => readobj::run(args),
        Command::Sched(args) => sched::run(args),
        Command::LlvmImport(args) => llvm_import::run(args),
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Regenerate a backend's discovered isel bridge axioms
    Axioms(axioms::ToolArgs),
    /// Compile machine code
    Mc(mc::ToolArgs),
    /// Run optimizations on IR
    Opt(opt::ToolArgs),
    /// Dump headers, symbols and relocations of an object file
    Readobj(readobj::ToolArgs),
    /// Print the data dependence graph of machine IR
    Sched(sched::ToolArgs),
    /// Import LLVM textual IR into TIR
    LlvmImport(llvm_import::ToolArgs),
}

#[derive(Parser)]
#[command(name = "tir", version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
