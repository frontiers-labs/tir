//! `tir sched` is a static instruction throughput analyzer, similar to
//! `llvm-mca` or Intel's `IACA`. It prints a rough approximation of a code
//! region's behavior on a device pipeline without executing it: the region is
//! repeated `--iterations` times and fed to the shared scoreboard engine in
//! `tir-sim`, which reconstructs data dependencies from each instruction's
//! read/written registers and assigns dispatch/issue/retire cycles against a
//! TMDL-generated [`MachineModel`](tir_be_common::sched::MachineModel). The
//! engine is the same one `isasim --timing` replays executed traces through,
//! so the two views can never disagree about an instruction's cost.

use std::{error::Error, ffi::OsString};

use clap::Args;
use tir::Context;
use tir_be_common::liveness::op_regs;
use tir_be_common::{MachineInstruction, SectionOp, SymbolOp};
use tir_sim::scoreboard::{self, Prf, ScoreboardInstr, TimingConfig, phys_regs};

use crate::common::{InputKind, parse_module};
use crate::sched::event::View;

mod event;

/// The scheduling fallback when no `--model` is selected: a generic single-issue
/// core with no functional units, so every instruction resolves to the
/// single-cycle [`InstrSchedClass::DEFAULT`].
const GENERIC_MODEL: tir_be_common::sched::MachineModel = tir_be_common::sched::MachineModel {
    name: "generic",
    issue_width: 1,
    resources: &[],
    buffers: &[],
    pipeline: &[],
    forwards: &[],
    reg_files: &[],
    sched: &[],
};

#[derive(Args)]
pub struct ToolArgs {
    /// Target CPU
    #[arg(long)]
    mcpu: Option<String>,
    /// Target architecture
    #[arg(long)]
    march: String,
    /// Performance model / machine to analyze against (e.g. `ooo`, `in-order`).
    /// Omitted: a generic single-issue core that costs every instruction one
    /// cycle (the scheduling fallback when no machine is selected).
    #[arg(long)]
    model: Option<String>,
    /// Number of times the region is repeated through the pipeline.
    #[arg(long, default_value_t = 100)]
    iterations: usize,
    /// Report format.
    #[arg(long, value_enum, default_value_t = View::Resource)]
    view: View,
    /// Input assembly file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let target = tir_targets::select(&args.march, args.mcpu.as_deref()).ok_or_else(|| {
        format!(
            "unknown target '{}' (supported: {})",
            args.march,
            tir_targets::supported_targets().join(", ")
        )
    })?;

    let context = Context::with_default_dialects();
    target.register_dialects(&context);

    let model = match &args.model {
        Some(name) => target.machine_model(name).ok_or_else(|| {
            format!(
                "unknown machine '{}' for target '{}' (one of: {})",
                name,
                target.name(),
                target.machines().join(", ")
            )
        })?,
        None => GENERIC_MODEL,
    };

    let (module, _) = parse_module(
        target.as_ref(),
        &context,
        args.input.as_ref(),
        InputKind::Assembly,
    )?;

    // Collect the region's machine instructions in program order, resolving each to
    // its scheduling class and the physical registers it reads/writes.
    let asm_printer = target.asm_printer(&context);
    let mut op_ids = Vec::new();
    collect_instructions(&context, module.body(), &mut op_ids);

    let mut base = Vec::with_capacity(op_ids.len());
    for op_id in op_ids {
        let op = context.get_op(op_id);
        let Some(mi) = op.clone().as_interface::<dyn MachineInstruction>() else {
            continue;
        };
        let mnemonic = mi.mnemonic();
        let regs = op_regs(&op);
        let text = asm_printer
            .print_instruction(&op)?
            .ok_or_else(|| format!("no assembly printer registered for '{}'", op.name()))?;
        base.push(ScoreboardInstr {
            text,
            class: model.sched_class(mnemonic),
            defs: phys_regs(&regs.defs),
            uses: phys_regs(&regs.uses),
            branch: None,
        });
    }

    if base.is_empty() {
        return Err("no machine instructions found in input".into());
    }

    let prf = Prf::for_target(&target.register_info(), &model);
    let mut handler = event::make(args.view);
    scoreboard::run(
        &model,
        &base,
        args.iterations.max(1),
        &TimingConfig::for_model(&model),
        None,
        Some(&prf),
        Some(handler.as_mut()),
    );
    print!("{}", handler.render());

    Ok(())
}

/// Recursively gather the ids of every machine instruction reachable from `block`,
/// in program order, descending through `section`/`symbol` containers.
fn collect_instructions(
    context: &Context,
    block: std::sync::Arc<tir::Block>,
    out: &mut Vec<tir::OpId>,
) {
    for op_id in block.op_ids() {
        let op = context.get_op(op_id);
        if let Some(section) = op.clone().as_op::<SectionOp>() {
            collect_instructions(context, section.body(), out);
        } else if let Some(symbol) = op.clone().as_op::<SymbolOp>() {
            collect_instructions(context, symbol.body(), out);
        } else if op.as_interface::<dyn MachineInstruction>().is_some() {
            out.push(op_id);
        }
    }
}
