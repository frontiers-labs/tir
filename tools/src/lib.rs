use std::error::Error;

use clap::{Parser, Subcommand};

// Force the backend crates to be linked so their `register_target!` entries are
// included in the final binary; the target registry is otherwise their only user.
use tir_arm64 as _;
use tir_riscv as _;
use tir_x86_64 as _;

mod common;

pub mod mc;
pub mod opt;
pub mod readobj;
pub mod sched;

pub fn tools_main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::Mc(args) => mc::run(args),
        Command::Opt(args) => opt::run(args),
        Command::Readobj(args) => readobj::run(args),
        Command::Sched(args) => sched::run(args),
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Compile machine code
    Mc(mc::ToolArgs),
    /// Run optimizations on IR
    Opt(opt::ToolArgs),
    /// Dump headers, symbols and relocations of an object file
    Readobj(readobj::ToolArgs),
    /// Print the data dependence graph of machine IR
    Sched(sched::ToolArgs),
}

#[derive(Parser)]
#[command(name = "tir", version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
