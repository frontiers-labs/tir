//! tir-mc is an IR to machine code compiler

use std::io::Write;
use std::{error::Error, ffi::OsString};

use clap::{Args, ValueEnum};
use tir::backend::binary::{render_ascii, write_elf};
use tir::backend::pipeline::{StopAfter, build_pipeline};
use tir::{Context, IRFormatter, Operation};

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
    #[arg(value_enum, long, conflicts_with = "filetype")]
    stage: Option<Stage>,
    /// Output kind: textual assembly or an ELF object (binary or as text)
    #[arg(value_enum, long)]
    filetype: Option<FileType>,
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
    let target =
        tir::backend::select_target(&args.march, args.mcpu.as_deref(), args.mattr.as_deref())?;

    let context = Context::with_default_dialects();
    target.register_dialects(&context);

    let (module, needs_lowering) = parse_module(
        target.as_ref(),
        &context,
        args.input.as_ref(),
        args.kind.unwrap_or_default(),
    )?;

    let stop_after = match (args.stage, args.filetype) {
        (Some(Stage::ISel), _) => StopAfter::ISel,
        (Some(Stage::RegAlloc), _) | (None, None) => StopAfter::RegAlloc,
        (None, Some(_)) => StopAfter::Finalize,
    };

    if needs_lowering {
        let mut pm = build_pipeline(target.as_ref(), &context, stop_after);
        pm.run(&context, context.get_op(module.id()))
            .map_err(|e| format!("pass pipeline failed: {e}"))?;
    }

    // Without --filetype the legacy behavior is kept: assembly when the input
    // was already assembly, the IR after the requested stage otherwise.
    let filetype = match args.filetype {
        Some(filetype) => Some(filetype),
        None if args.stage.is_none() && !needs_lowering => Some(FileType::Asm),
        None => None,
    };

    let output = match filetype {
        Some(FileType::Asm) => target
            .asm_printer(&context)
            .print_module(&context, &module)
            .map_err(|e| format!("failed to print assembly: {e}"))?
            .into_bytes(),
        Some(FileType::Obj) | Some(FileType::ObjAscii) => {
            let fmt = target.object_format().ok_or_else(|| {
                format!("target '{}' does not support object emission", args.march)
            })?;
            let writer = target.binary_writer(&context).ok_or_else(|| {
                format!("target '{}' does not support object emission", args.march)
            })?;
            let obj = writer
                .write_module(&context, &module, &fmt)
                .map_err(|e| format!("failed to emit object: {e}"))?;
            match filetype {
                Some(FileType::Obj) => write_elf(&obj, &fmt),
                _ => render_ascii(&obj).into_bytes(),
            }
        }
        None => {
            let mut rendered = String::new();
            let mut fmt = IRFormatter::new(&mut rendered);
            module
                .print(&mut fmt)
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
