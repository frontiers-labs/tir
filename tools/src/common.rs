use std::{
    error::Error,
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
};

use clap::ValueEnum;
use tir::backend::TargetMachine;
use tir::{Context, builtin::ModuleOp};

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum InputKind {
    #[default]
    Auto,
    Tir,
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
        InputKind::Auto => unreachable!(),
    }
}

/// Whether the input holds TIR or assembly, resolving [`InputKind::Auto`] by
/// file extension.
pub fn resolve_kind(input_path: Option<&OsString>, kind: InputKind) -> InputKind {
    if kind != InputKind::Auto {
        return kind;
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
