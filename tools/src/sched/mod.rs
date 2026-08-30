//! `tir sched` is a static instruction throughput analyzer, similar to
//! `llvm-mca` or Intel's `IACA`. It prints a rough approximation of a code
//! region's behavior on a device pipeline without executing it: the region is
//! repeated `--iterations` times and fed to the shared scoreboard engine in
//! `tir-sim`, which reconstructs data dependencies from each instruction's
//! read/written registers and assigns dispatch/issue/retire cycles against a
//! TMDL-generated [`MachineModel`](tir::backend::sched::MachineModel). The
//! engine is the same one `isasim --timing` replays executed traces through,
//! so the two views can never disagree about an instruction's cost.

use std::{error::Error, ffi::OsString};

use clap::Args;
use tir::Context;
use tir::backend::TargetMachine;
use tir::backend::binary::{ObjectFile, SectionKind};
use tir::backend::liveness::execution_regs;
use tir::backend::{MachineInstruction, SectionOp, SymbolOp};
use tir::builtin::ModuleOp;
use tir_sim::scoreboard::{self, Prf, ScoreboardInstr, TimingConfig, phys_regs};

use crate::common::{InputKind, parse_module};
use crate::sched::event::View;

mod event;

/// The scheduling fallback when no `--model` is selected: a generic single-issue
/// core with no functional units. It is no target's machine, so every
/// instruction resolves to the single-cycle [`InstrSchedClass::DEFAULT`].
const GENERIC_MODEL: tir::backend::sched::MachineModel = tir::backend::sched::MachineModel {
    name: "generic",
    id: usize::MAX,
    issue_width: 1,
    frontend: None,
    resources: &[],
    buffers: &[],
    pipeline: &[],
    forwards: &[],
    reg_files: &[],
    fusions: &[],
};

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
    /// Target calling convention.
    #[arg(long)]
    mabi: Option<String>,
    /// Performance model / machine to analyze against (e.g. `ooo`, `in-order`).
    /// Omitted: the machine implied by `--mcpu` when it names one, otherwise a
    /// generic single-issue core that costs every instruction one cycle.
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
    let target = tir::backend::select_target_with_abi(
        &args.march,
        args.mcpu.as_deref(),
        args.mattr.as_deref(),
        args.mabi.as_deref(),
    )?;

    let context = Context::with_default_dialects();
    target.register_dialects(&context);

    let model = match args.model.as_deref().or_else(|| target.default_machine()) {
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
    let asm_printer = tir::backend::AsmPrinter::new();
    let abi = (!target.abis().is_empty()).then(|| target.abi());
    let prf = Prf::for_target(&target.register_info(), &model, abi);
    let mut op_ids = Vec::new();
    collect_instructions(&context, module.body(), &mut op_ids);
    let layout = encoded_instruction_layout(target.as_ref(), &context, &module, op_ids.len())?;

    let mut base = Vec::with_capacity(op_ids.len());
    for (index, op_id) in op_ids.into_iter().enumerate() {
        let op = context.get_op(op_id);
        let Some(mi) = op.clone().as_interface::<dyn MachineInstruction>() else {
            continue;
        };
        let info = mi.info();
        let regs = execution_regs(&op);
        let text = asm_printer
            .print_instruction(&context, &op, &tir::backend::RegAssignment::default())?
            .ok_or_else(|| format!("'{}' has no assembly syntax", op.name()))?;
        let (pc, width_bytes) = layout
            .as_ref()
            .and_then(|layout| layout.get(index).copied())
            .unwrap_or((0, u16::from(info.width_bytes.max(1))));
        base.push(ScoreboardInstr {
            text,
            op_name: info.name.to_string(),
            class: info.sched_on(&model),
            defs: phys_regs(&regs.phys_defs, Some(&prf)),
            uses: phys_regs(&regs.phys_uses, Some(&prf)),
            branch: None,
            pc,
            width_bytes,
            mem: Vec::new(),
        });
    }

    if base.is_empty() {
        return Err("no machine instructions found in input".into());
    }
    let unroll_stride = layout
        .as_deref()
        .map(static_unroll_stride)
        .unwrap_or_else(|| {
            base.iter()
                .map(|instruction| u64::from(instruction.width_bytes))
                .sum()
        });

    let mut handler = event::make(args.view);
    scoreboard::run(
        &model,
        &base,
        args.iterations.max(1),
        &TimingConfig::for_model(&model).with_unroll_stride(unroll_stride),
        None,
        Some(&prf),
        None,
        Some(handler.as_mut()),
    );
    print!("{}", handler.render());

    Ok(())
}

type InstructionLayout = Vec<(u64, u16)>;

fn static_unroll_stride(layout: &[(u64, u16)]) -> u64 {
    layout
        .iter()
        .map(|(pc, width)| pc.saturating_add(u64::from(*width)))
        .max()
        .unwrap_or(0)
}

fn encoded_instruction_layout(
    target: &dyn TargetMachine,
    context: &Context,
    module: &ModuleOp,
    expected: usize,
) -> Result<Option<InstructionLayout>, Box<dyn Error>> {
    let Some(format) = target.object_format() else {
        return Ok(None);
    };
    let writer = tir::backend::binary::BinaryWriter::new();
    let object = writer
        .write_module(context, module, &format)
        .map_err(|error| format!("failed to encode scheduling input: {error}"))?;
    text_instruction_layout(&object, expected)
        .map(Some)
        .ok_or_else(|| "encoded instruction layout does not match scheduling input".into())
}

fn text_instruction_layout(object: &ObjectFile, expected: usize) -> Option<InstructionLayout> {
    let mut base = 0u64;
    let mut layout = Vec::new();
    for section in object
        .sections
        .iter()
        .filter(|section| section.kind == SectionKind::Text)
    {
        let alignment = section.align.max(1);
        base = base.div_ceil(alignment) * alignment;
        layout.extend(
            section
                .insn_spans
                .iter()
                .map(|(offset, width)| (base + offset, u16::from(*width))),
        );
        base = base.saturating_add(section.data.len() as u64);
    }
    (layout.len() == expected).then_some(layout)
}

/// Recursively gather the ids of every machine instruction reachable from `block`,
/// in program order, descending through `section`/`symbol` containers.
fn collect_instructions(context: &Context, block: tir::BlockHandle, out: &mut Vec<tir::OpId>) {
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
