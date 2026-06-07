use std::{error::Error, ffi::OsString};

use clap::Args;
use tir::{Context, IRFormatter, Operation, builtin::ModuleOp};

use crate::common::{read_input, write_output};

#[derive(Args)]
pub struct ToolArgs {
    /// Pass pipeline in MLIR-style syntax, e.g. `builtin.func(mem2reg)`. May be
    /// repeated; repeated values are joined into one comma-separated pipeline.
    #[arg(long = "pass", short = 'p')]
    passes: Vec<String>,

    /// Output file, or `-` for stdout.
    #[arg(short = 'o', default_value = "-")]
    output: OsString,

    /// Input IR file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let input = read_input(args.input.as_ref())?;

    let context = Context::with_default_dialects();
    let module = tir::parse::ir::parse_ir::<ModuleOp>(&context, &input)
        .map_err(|(span, err)| format!("failed to parse input at byte {}: {err:?}", span.0))?;

    if !args.passes.is_empty() {
        let pipeline = args.passes.join(",");
        let mut pm = tir::parse_pipeline(&pipeline).map_err(|e| {
            format!(
                "{e} (available passes: {})",
                tir::registered_passes().join(", ")
            )
        })?;
        pm.run(&context, context.get_op(module.id()))
            .map_err(|e| format!("pass pipeline failed: {e}"))?;
    }

    let mut rendered = String::new();
    let mut fmt = IRFormatter::new(&mut rendered);
    module
        .print(&mut fmt)
        .map_err(|e| format!("failed to print IR: {e}"))?;

    write_output(args.output.as_os_str(), &rendered).map_err(|e| e.into())
}
