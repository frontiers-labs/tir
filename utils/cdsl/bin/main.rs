use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use tir_cdsl::{Error, compile_file, resolve_crate};

enum Input {
    Package { name: String, selector: Vec<String> },
    File(PathBuf),
}

struct Options {
    input: Input,
    output: String,
    typed: bool,
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
            "--cargo" => {
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
            value if value.starts_with("::") => {
                if selector.replace(value.to_owned()).is_some() {
                    return Err(Error("only one module selector is allowed".into()));
                }
            }
            _ => return Err(Error(format!("unknown argument {arg}"))),
        }
    }
    let typed = match emit.as_deref() {
        Some("ast") => false,
        Some("typed") => true,
        _ => return Err(Error("expected --emit ast or typed".into())),
    };
    let output = output.ok_or_else(|| Error("expected --output <file or ->".into()))?;
    let input = match (package, file, selector) {
        (Some(name), None, Some(selector)) => {
            let selector = selector.strip_prefix("::").unwrap_or_default();
            let selector = if selector == "*" {
                ""
            } else {
                selector
                    .strip_suffix("::*")
                    .ok_or_else(|| Error("selector must end in ::*".into()))?
            };
            let selector = if selector.is_empty() {
                Vec::new()
            } else {
                selector.split("::").map(str::to_owned).collect::<Vec<_>>()
            };
            if selector.iter().any(String::is_empty) {
                return Err(Error("invalid module selector".into()));
            }
            Input::Package { name, selector }
        }
        (None, Some(file), None) => Input::File(file),
        _ => {
            return Err(Error(
                "choose --cargo <package> ::module::* or --input <file.cdsl>".into(),
            ));
        }
    };
    Ok(Options {
        input,
        output,
        typed,
    })
}

fn run() -> Result<(), Error> {
    let options = options()?;
    let program = match options.input {
        Input::Package { name, selector } => resolve_crate(&name)?.select(&selector)?,
        Input::File(file) => compile_file(&file)?,
    };
    let text = if options.typed {
        program.lower()?.render()?
    } else {
        program.render()
    };
    if options.output == "-" {
        io::stdout()
            .write_all(text.as_bytes())
            .map_err(|error| Error(error.to_string()))
    } else {
        fs::write(&options.output, text)
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
