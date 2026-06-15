use std::{error::Error, ffi::OsString};

use clap::Args;
use tir::{Context, IRFormatter, OpId, Operation, builtin::ModuleOp};

use crate::common::{read_input, write_output};

/// Verify `op` and every operation nested in its regions. The framework's
/// `Operation::verify` only checks an op's own regions for terminators, so we
/// walk the tree ourselves and run each op's full verifier.
fn verify_recursive(context: &Context, op_id: OpId) -> Result<(), String> {
    let instance = context.get_op(op_id);
    instance
        .clone()
        .as_dyn_op()
        .verify(context)
        .map_err(|e| format!("verification failed: {e}"))?;

    for region_id in instance.regions.clone() {
        let region = context.get_region(region_id);
        for block in region.iter(context.clone()) {
            for child in block.op_ids() {
                verify_recursive(context, child)?;
            }
        }
    }
    Ok(())
}

#[derive(Args)]
pub struct ToolArgs {
    /// Pass pipeline in MLIR-style syntax, e.g. `builtin.func(mem2reg)`. May be
    /// repeated; repeated values are joined into one comma-separated pipeline.
    #[arg(long = "pass", short = 'p')]
    passes: Vec<String>,

    /// Verify the module after parsing and running passes.
    #[arg(long)]
    verify: bool,

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

    if args.verify {
        verify_recursive(&context, module.id())?;
    }

    let mut rendered = String::new();
    let mut fmt = IRFormatter::new(&mut rendered);
    module
        .print(&mut fmt)
        .map_err(|e| format!("failed to print IR: {e}"))?;

    write_output(args.output.as_os_str(), &rendered).map_err(|e| e.into())
}
