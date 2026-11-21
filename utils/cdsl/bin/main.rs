use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use tir_cdsl::{Error, compile_file, resolve_crate, resolve_program_package};

enum Input {
    Package {
        name: String,
        selector: Vec<String>,
    },
    Qualified {
        prefix: String,
        selector: Vec<String>,
    },
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
    Binary,
    Program,
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
                ))
            }
            "--emit" => {
                emit = Some(
                    args.next()
                        .ok_or_else(|| Error("--emit needs a format".into()))?,
                )
            }
            "--output" => {
                output = Some(
                    args.next()
                        .ok_or_else(|| Error("--output needs a destination".into()))?,
                )
            }
            value if value.starts_with("::") || value.ends_with("::*") => {
                if selector.replace(value.to_owned()).is_some() {
                    return Err(Error("only one module selector is allowed".into()));
                }
            }
            _ => return Err(Error(format!("unknown argument {arg}"))),
        }
    }
    let emit = match emit.as_deref() {
        Some("ast") => Emit::Ast,
        Some("typed") => Emit::Typed,
        Some("bin") | Some("binary") => Emit::Binary,
        Some("program") => Emit::Program,
        _ => return Err(Error("expected --emit ast, typed, bin or program".into())),
    };
    let output = output.ok_or_else(|| Error("expected --output <file or ->".into()))?;
    let input = match (package, file, selector) {
        (Some(name), None, Some(selector)) if selector.starts_with("::") => {
            let selector = module_selector(&selector[2..])?;
            Input::Package { name, selector }
        }
        (None, None, Some(selector)) => {
            let (prefix, module) = selector
                .split_once("::")
                .filter(|(prefix, _)| !prefix.is_empty())
                .ok_or_else(|| Error("expected a package-qualified selector".into()))?;
            Input::Qualified {
                prefix: prefix.into(),
                selector: module_selector(module)?,
            }
        }
        (None, Some(file), None) => Input::File(file),
        _ => {
            return Err(Error(
                "choose <package>::module::*, --crate <package> ::module::*, or --input <file.cdsl>".into(),
            ));
        }
    };
    Ok(Options {
        input,
        output,
        emit,
    })
}

fn module_selector(input: &str) -> Result<Vec<String>, Error> {
    if input == "*" {
        return Ok(Vec::new());
    }
    let module = input
        .strip_suffix("::*")
        .ok_or_else(|| Error("selector must end in ::*".into()))?;
    let selector = module.split("::").map(str::to_owned).collect::<Vec<_>>();
    if selector.iter().any(String::is_empty) {
        return Err(Error("invalid module selector".into()));
    }
    Ok(selector)
}

fn run() -> Result<(), Error> {
    let options = options()?;
    let (program, selector) = match options.input {
        Input::Package { name, selector } => (resolve_crate(&name)?, selector),
        Input::Qualified { prefix, selector } => (resolve_program_package(&prefix)?, selector),
        Input::File(file) => (compile_file(&file)?, Vec::new()),
    };
    let bytes = if matches!(options.emit, Emit::Program) {
        program.lower()?.render_program(&selector)?.into_bytes()
    } else {
        let program = program.select(&selector)?;
        match options.emit {
            Emit::Ast => program.render().into_bytes(),
            Emit::Typed => program.lower()?.render()?.into_bytes(),
            Emit::Binary => program.lower()?.to_binary()?,
            Emit::Program => unreachable!(),
        }
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
