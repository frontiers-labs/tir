use std::{error::Error, ffi::OsString};

use clap::Args;
use tir::{Context, IRFormatter, Operation};

use crate::common::{TargetArgs, read_input, write_output};

#[derive(Args)]
pub struct ToolArgs {
    /// Target architecture whose ABI applies to the imported module.
    #[arg(long)]
    march: Option<String>,

    #[command(flatten)]
    target: TargetArgs,

    /// Output file, or `-` for stdout.
    #[arg(short = 'o', default_value = "-")]
    output: OsString,

    /// Input LLVM textual IR file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let input = read_input(args.input.as_ref())?;

    let context = Context::with_default_dialects();
    let module = match args.march.as_deref() {
        Some(march) => {
            let target = args.target.select(march)?;
            tir_llvm::import_str_with_abi(&context, &input, target.abi())
        }
        None => tir_llvm::import_str(&context, &input),
    }
    .map_err(|e| format!("llvm import failed: {e}"))?;

    let mut rendered = String::new();
    let mut fmt = IRFormatter::new(&mut rendered);
    module
        .print(&mut fmt)
        .map_err(|e| format!("failed to print IR: {e}"))?;

    write_output(args.output.as_os_str(), &rendered).map_err(|e| e.into())
}
