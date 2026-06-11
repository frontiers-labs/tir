//! tir-mc is an IR to machine code compiler

use std::{error::Error, ffi::OsString};

use clap::{Args, ValueEnum};
use tir::{Context, IRFormatter, Operation, PassManager, builtin::FuncOp};
use tir_be_common::TargetMachine;

use crate::common::{InputKind, parse_module};

#[derive(Args)]
pub struct ToolArgs {
    /// Target CPU
    #[arg(long)]
    mcpu: Option<String>,
    /// Target architecture
    #[arg(long)]
    march: String,
    /// Target feature toggles (e.g. `+m,-zmmul`), applied on top of `--march`.
    #[arg(long)]
    mattr: Option<String>,
    /// Optional stage after which pipeline is stopped
    #[arg(value_enum, long)]
    stage: Option<Stage>,
    /// Input TIR file, or `-`/omitted for stdin.
    input: Option<OsString>,
    /// Input kind: TIR or assembly
    kind: Option<InputKind>,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Stage {
    /// Emit IR after instruction selection stage
    ISel,
    /// Emit IR after register allocation stage
    RegAlloc,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let target = tir_targets::select(&args.march, args.mcpu.as_deref(), args.mattr.as_deref())?;

    let context = Context::with_default_dialects();
    target.register_dialects(&context);

    let stage = args.stage;
    let stop_after = args.stage.unwrap_or(Stage::RegAlloc);

    let (module, needs_lowering) = parse_module(
        target.as_ref(),
        &context,
        args.input.as_ref(),
        args.kind.unwrap_or_default(),
    )?;
    let emit_assembly = stage.is_none() && !needs_lowering;

    if needs_lowering {
        let mut pm = create_pass_manager(&stop_after, target.as_ref(), &context);

        pm.run(&context, context.get_op(module.id()))
            .map_err(|e| format!("pass pipeline failed: {e}"))?;
    }

    if emit_assembly {
        let rendered = target
            .asm_printer(&context)
            .print_module(&context, &module)
            .map_err(|e| format!("failed to print assembly: {e}"))?;
        print!("{rendered}");
    } else {
        let mut rendered = String::new();
        let mut fmt = IRFormatter::new(&mut rendered);
        module
            .print(&mut fmt)
            .map_err(|e| format!("failed to print IR: {e}"))?;
        print!("{rendered}");
    }

    Ok(())
}

fn create_pass_manager(
    stage: &Stage,
    target: &dyn TargetMachine,
    context: &Context,
) -> PassManager {
    let mut pm = PassManager::new();

    pm.nest(FuncOp::name()).add_pass(target.isel_pass(context));

    if stage == &Stage::ISel {
        return pm;
    }

    pm.add_pass(target.regalloc_pass());
    pm
}
