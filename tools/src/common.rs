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

    let ty = match kind {
        InputKind::Auto => {
            if let Some(inp) = input_path.and_then(|i| i.to_str())
                && (inp.ends_with(".S") || inp.ends_with(".s") || inp.ends_with(".asm"))
            {
                InputKind::Assembly
            } else {
                InputKind::Tir
            }
        }
        _ => kind,
    };

    match ty {
        InputKind::Assembly => Ok((
            target
                .asm_parser(context)
                .parse_asm(context, &input)
                .map_err(|_| "failed to parse assembly input")?,
            false,
        )),
        InputKind::Tir => Ok((
            tir::parse::ir::parse_ir::<ModuleOp>(context, &input).map_err(|(span, err)| {
                format!("failed to parse input at byte {}: {err:?}", span.0)
            })?,
            true,
        )),
        InputKind::Auto => unreachable!(),
    }
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
