use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use tir_cdsl::{Error, compile_file, resolve_crate, resolve_program_package};

enum Input {
    Package { name: String },
    Qualified { prefix: String },
    File(PathBuf),
}

struct Options {
    input: Input,
    output: String,
    emit: Emit,
}

enum Emit {
    Ast,
    Typed,
    MiniMl,
    Egg,
    Tegg,
    Rust,
    Lean,
}

fn options() -> Result<Options, Error> {
    let mut args = env::args().skip(1);
    let mut file = None;
    let mut package = None;
    let mut selector = None;
    let mut emit = None;
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cargo" | "--crate" => {
                package = Some(
                    args.next()
                        .ok_or_else(|| Error(format!("{arg} needs a package name")))?,
                );
            }
            "--input" => {
                file = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| Error("--input needs a file".into()))?,
                ));
            }
            "--emit" => {
                emit = Some(
                    args.next()
                        .ok_or_else(|| Error("--emit needs a format".into()))?,
                );
            }
            "--output" | "--out" => {
                output = Some(
                    args.next()
                        .ok_or_else(|| Error(format!("{arg} needs a destination")))?,
                );
            }
            value if value.starts_with("::") || value.ends_with("::*") => {
                if selector.replace(value.to_owned()).is_some() {
                    return Err(Error("only one module selector is allowed".into()));
                }
            }
            value if !value.starts_with('-') && file.is_none() => {
                file = Some(PathBuf::from(value));
            }
            _ => return Err(Error(format!("unknown argument {arg}"))),
        }
    }
    let emit = match emit.as_deref() {
        Some("ast") => Emit::Ast,
        Some("typed") => Emit::Typed,
        Some("miniml") => Emit::MiniMl,
        Some("egg") => Emit::Egg,
        Some("bin") | Some("binary") | Some("tegg") => Emit::Tegg,
        Some("rust") | Some("program") => Emit::Rust,
        Some("lean") => Emit::Lean,
        _ => {
            return Err(Error(
                "expected --emit ast, typed, miniml, egg, tegg|bin, rust|program, or lean".into(),
            ));
        }
    };
    let output = output.unwrap_or_else(|| "-".into());
    let input = match (package, file, selector) {
        (Some(name), None, Some(sel)) if sel.starts_with("::") => Input::Package { name },
        (Some(name), None, None) => Input::Package { name },
        (None, None, Some(sel)) => {
            let (prefix, _) = sel
                .split_once("::")
                .filter(|(prefix, _)| !prefix.is_empty())
                .ok_or_else(|| Error("expected a package-qualified selector".into()))?;
            Input::Qualified {
                prefix: prefix.into(),
            }
        }
        (None, Some(file), None) => Input::File(file),
        _ => {
            return Err(Error(
                "choose <package>::module::*, --crate <package> ::module::*, or --input <file.cdsl>"
                    .into(),
            ));
        }
    };
    Ok(Options {
        input,
        output,
        emit,
    })
}

fn run() -> Result<(), Error> {
    let options = options()?;
    let mut program = match options.input {
        Input::Package { name } => resolve_crate(&name)?,
        Input::Qualified { prefix } => resolve_program_package(&prefix)?,
        Input::File(file) => compile_file(&file)?,
    };
    let bytes = match options.emit {
        Emit::Ast => program.render_typed().into_bytes(),
        Emit::Typed => format!(
            "Typechecked {} declarations cleanly (kernel OK).\n{}",
            program.signature.decl_order.len(),
            program.render_typed()
        )
        .into_bytes(),
        Emit::MiniMl => {
            let ml = program.erase()?;
            format!(
                "Erased to MiniML: {} decls, {} TEGG bytecode blobs.\n",
                ml.decls.len(),
                ml.bytecodes.len()
            )
            .into_bytes()
        }
        Emit::Egg => program.emit_egg()?.into_bytes(),
        Emit::Tegg => program.emit_tegg()?,
        Emit::Rust => program.emit_rust()?.into_bytes(),
        Emit::Lean => program.emit_lean().into_bytes(),
    };
    if options.output == "-" {
        io::stdout()
            .write_all(&bytes)
            .map_err(|error| Error(error.to_string()))
    } else {
        fs::write(&options.output, bytes)
            .map_err(|error| Error(format!("{}: {error}", options.output)))
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cdslc: {error}");
            ExitCode::FAILURE
        }
    }
}
