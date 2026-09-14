use std::{
    error::Error,
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
};

use clap::{Args, ValueEnum};
use tir::backend::TargetMachine;
use tir::{Context, Operation, builtin::ModuleOp};

/// The target flags a code-generating tool takes besides `--march`, which is
/// the tool's own: only a tool that can read the target from its input makes it
/// optional. The display orders leave room for it in second place.
#[derive(Args)]
pub struct TargetArgs {
    /// Target CPU
    #[arg(long, display_order = 0)]
    mcpu: Option<String>,
    /// Target feature toggles (e.g. `+m,-zmmul`), applied on top of `--march`.
    #[arg(long, display_order = 2)]
    mattr: Option<String>,
    /// Target calling convention.
    #[arg(long, display_order = 3)]
    mabi: Option<String>,
}

impl TargetArgs {
    /// The target `march` names, as the rest of the flags configure it.
    pub fn select(&self, march: &str) -> Result<Box<dyn TargetMachine>, String> {
        tir::backend::select_target_with_abi(
            march,
            self.mcpu.as_deref(),
            self.mattr.as_deref(),
            self.mabi.as_deref(),
        )
    }
}

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum InputKind {
    #[default]
    Auto,
    Tir,
    Llvm,
    Assembly,
}

/// Parse the tool input into a module. Returns the module and whether it still
/// needs lowering (assembly is already in machine-IR form; TIR is not).
pub fn parse_module(
    target: &dyn TargetMachine,
    context: &Context,
    input_path: Option<&OsString>,
    kind: InputKind,
) -> Result<(ModuleOp, bool), Box<dyn Error>> {
    let input = read_input(input_path)?;

    match resolve_kind(input_path, kind) {
        InputKind::Assembly => {
            // A target with its own assembly syntax (e.g. PTX) parses text
            // directly; otherwise fall back to the shared flat assembler.
            if let Some(result) = target.parse_asm_text(context, &input) {
                return Ok((
                    result.map_err(|e| format!("failed to parse assembly: {e}"))?,
                    false,
                ));
            }
            Ok((
                target
                    .asm_parser(context)
                    .parse_asm(context, &input)
                    .map_err(|_| "failed to parse assembly input")?,
                false,
            ))
        }
        InputKind::Tir => Ok((parse_tir(context, &input)?, true)),
        InputKind::Llvm => {
            let module = tir_llvm::import_str(context, &input)
                .map_err(|e| format!("llvm import failed: {e}"))?;
            let mut attributes = context.get_op(module.id()).attributes().to_vec();
            for (name, value) in [
                (tir::DATA_LAYOUT, target.data_layout()),
                (tir::TARGET_ENV, target.target_env()),
            ] {
                if let Some(value) = value {
                    attributes.push(context.named_attribute(name, value));
                }
            }
            context.set_op_attributes(module.id(), attributes);
            Ok((
                context.get_op(module.id()).as_op::<ModuleOp>().unwrap(),
                true,
            ))
        }
        InputKind::Auto => unreachable!(),
    }
}

/// Whether the input holds TIR, LLVM IR or assembly, resolving [`InputKind::Auto`] by
/// file extension.
pub fn resolve_kind(input_path: Option<&OsString>, kind: InputKind) -> InputKind {
    if kind != InputKind::Auto {
        return kind;
    }
    if input_path
        .and_then(|path| path.to_str())
        .is_some_and(|path| path.ends_with(".ll"))
    {
        return InputKind::Llvm;
    }
    let is_assembly = input_path
        .and_then(|path| path.to_str())
        .is_some_and(|path| {
            [".S", ".s", ".asm", ".ptx"]
                .iter()
                .any(|extension| path.ends_with(extension))
        });
    if is_assembly {
        InputKind::Assembly
    } else {
        InputKind::Tir
    }
}

/// Parse TIR text into a module. Needs no target: the dialects the text uses
/// must already be registered.
pub fn parse_tir(context: &Context, input: &str) -> Result<ModuleOp, Box<dyn Error>> {
    tir::parse::ir::parse_ir::<ModuleOp>(context, input)
        .map_err(|(span, err)| format!("failed to parse input at byte {}: {err:?}", span.0).into())
}

pub fn read_input(path: Option<&OsString>) -> Result<String, io::Error> {
    let mut input = String::new();
    match path {
        Some(path) if path != "-" => File::open(path)?.read_to_string(&mut input)?,
        _ => io::stdin().read_to_string(&mut input)?,
    };
    Ok(input)
}

pub fn write_output(path: &std::ffi::OsStr, contents: &str) -> Result<(), io::Error> {
    if path == "-" {
        print!("{contents}");
        io::stdout().flush()
    } else {
        std::fs::write(path, contents)
    }
}
