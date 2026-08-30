//! tir-mc is an IR to machine code compiler

use std::io::Write;
use std::{error::Error, ffi::OsString};

use clap::{Args, ValueEnum};
use tir::backend::binary::{ObjectEmission, render_ascii, write_elf};
use tir::backend::pipeline::{Oracles, StopAfter, build_pipeline, lower_and_emit};
use tir::{Context, IRFormatter, Operation};

use crate::common::{InputKind, parse_module, parse_tir, read_input, resolve_kind};

#[derive(Args)]
pub struct ToolArgs {
    /// Target CPU
    #[arg(long)]
    mcpu: Option<String>,
    /// Target architecture. Defaults to the `arch` the input's `target_env`
    /// declares, which only TIR input can carry.
    #[arg(long)]
    march: Option<String>,
    /// Target feature toggles (e.g. `+m,-zmmul`), applied on top of `--march`.
    #[arg(long)]
    mattr: Option<String>,
    /// Target calling convention.
    #[arg(long)]
    mabi: Option<String>,
    /// Optional stage after which pipeline is stopped
    #[arg(value_enum, long, conflicts_with = "filetype")]
    stage: Option<Stage>,
    /// Output kind: textual assembly or an ELF object (binary or as text)
    #[arg(value_enum, long)]
    filetype: Option<FileType>,
    /// Re-linearize every machine block by a seeded random topological order of
    /// its dependence graph, after selection and again after allocation. A
    /// differential-testing oracle: the program is the same one, so a change in
    /// behavior is a dependence the machine form does not carry.
    #[arg(long)]
    shuffle_machine_order: bool,
    /// Output path; `-` writes to stdout.
    #[arg(short = 'o', default_value = "-")]
    output: OsString,
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

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum FileType {
    /// Textual assembly
    Asm,
    /// ELF relocatable object
    Obj,
    /// Object bytes rendered as text, for lit tests
    ObjAscii,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let select = |march: &str| {
        tir::backend::select_target_with_abi(
            march,
            args.mcpu.as_deref(),
            args.mattr.as_deref(),
            args.mabi.as_deref(),
        )
    };

    let context = Context::with_default_dialects();
    let kind = resolve_kind(args.input.as_ref(), args.kind.unwrap_or_default());

    // Without --march the module says what it targets, so it is parsed first —
    // with the default dialects only, which is enough for IR that still needs
    // lowering.
    let (target, module, needs_lowering) = match args.march.as_deref() {
        Some(march) => {
            let target = select(march)?;
            target.register_dialects(&context);
            let (module, needs_lowering) =
                parse_module(target.as_ref(), &context, args.input.as_ref(), kind)?;
            (target, module, needs_lowering)
        }
        None if kind == InputKind::Assembly => {
            return Err("--march is required for assembly input".into());
        }
        None => {
            let module = parse_tir(&context, &read_input(args.input.as_ref())?)?;
            let arch = tir::TargetEnv::for_op(&context, module.id())
                .and_then(|env| env.arch().map(str::to_string))
                .ok_or("no --march given and the input declares no target_env 'arch'")?;
            let target = select(&arch)?;
            target.register_dialects(&context);
            (target, module, true)
        }
    };

    if needs_lowering {
        tir::verify_op_tree(&context, module.id())
            .map_err(|e| format!("verification failed: {e}"))?;
    }

    let oracles = Oracles {
        shuffle_machine_order: args.shuffle_machine_order,
    };
    let stop_after = match (args.stage, args.filetype) {
        (Some(Stage::ISel), _) => StopAfter::ISel,
        (Some(Stage::RegAlloc), _) | (None, None) => StopAfter::RegAlloc,
        (None, Some(_)) => StopAfter::Finalize,
    };

    // Without --filetype the legacy behavior is kept: assembly when the input
    // was already assembly, the IR after the requested stage otherwise.
    let filetype = match args.filetype {
        Some(filetype) => Some(filetype),
        None if args.stage.is_none() && !needs_lowering => Some(FileType::Asm),
        None => None,
    };

    // Emitting bytes means each function can be lowered, emitted and dropped on
    // its own. A stage that stops short prints the machine IR, and a target that
    // renders assembly text for the whole module at once (PTX) has nowhere to
    // put one symbol, so both keep the module-wide pipeline.
    let per_symbol = needs_lowering
        && stop_after == StopAfter::Finalize
        && match filetype {
            Some(FileType::Asm) => target.print_asm_text(&context, &module).is_none(),
            Some(_) => true,
            None => false,
        };

    if needs_lowering && !per_symbol {
        let mut pm = build_pipeline(target.as_ref(), &context, stop_after, oracles);
        pm.run(&context, context.get_op(module.id()))
            .map_err(|e| format!("pass pipeline failed: {e}"))?;
    }

    let output = match filetype {
        Some(FileType::Asm) if per_symbol => {
            let printer = tir::backend::AsmPrinter::new();
            let mut rendered = String::new();
            lower_and_emit(
                target.as_ref(),
                &context,
                &module,
                oracles,
                |context, op| {
                    printer
                        .print_op(context, op, &mut rendered)
                        .map_err(|e| format!("failed to print assembly: {e}"))
                },
            )?;
            rendered.into_bytes()
        }
        Some(FileType::Asm) => match target.print_asm_text(&context, &module) {
            Some(result) => result
                .map_err(|e| format!("failed to print assembly: {e}"))?
                .into_bytes(),
            None => tir::backend::AsmPrinter::new()
                .print_module(&context, &module)
                .map_err(|e| format!("failed to print assembly: {e}"))?
                .into_bytes(),
        },
        Some(FileType::Obj) | Some(FileType::ObjAscii) => {
            let fmt = target.object_format().ok_or_else(|| {
                format!(
                    "target '{}' does not support object emission",
                    target.name()
                )
            })?;
            let writer = tir::backend::binary::BinaryWriter::new();
            let obj = if per_symbol {
                let mut emission = ObjectEmission::default();
                lower_and_emit(
                    target.as_ref(),
                    &context,
                    &module,
                    oracles,
                    |context, op| {
                        writer
                            .write_op(context, op, &mut emission, &fmt)
                            .map_err(|e| format!("failed to emit object: {e}"))
                    },
                )?;
                writer
                    .finish(emission, &fmt)
                    .map_err(|e| format!("failed to emit object: {e}"))?
            } else {
                writer
                    .write_module(&context, &module, &fmt)
                    .map_err(|e| format!("failed to emit object: {e}"))?
            };
            match filetype {
                Some(FileType::Obj) => write_elf(&obj, &fmt),
                _ => render_ascii(&obj).into_bytes(),
            }
        }
        None => {
            let mut rendered = String::new();
            let mut fmt = IRFormatter::new(&mut rendered);
            tir::print_ir(&module, &context, &mut fmt)
                .map_err(|e| format!("failed to print IR: {e}"))?;
            rendered.into_bytes()
        }
    };

    if args.output == "-" {
        std::io::stdout().write_all(&output)?;
    } else {
        std::fs::write(&args.output, &output)?;
    }

    Ok(())
}
