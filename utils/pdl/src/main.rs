use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use tir_pdl::{compile, compile_to_rust, lex};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Emit {
    Ast,
    Rust,
    Tokens,
}

#[derive(Debug, Parser)]
struct Cli {
    input: PathBuf,
    #[arg(long, value_enum, default_value = "rust")]
    emit: Emit,
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let source = fs::read_to_string(&cli.input)?;
    let result = match cli.emit {
        Emit::Ast => compile(&source).map(|file| format!("{file:#?}\n")),
        Emit::Rust => compile_to_rust(&source),
        Emit::Tokens => {
            let (tokens, diagnostics) = lex(&source);
            if diagnostics.is_empty() {
                Ok(format!("{tokens:#?}\n"))
            } else {
                Err(diagnostics)
            }
        }
    };
    match result {
        Ok(output) => match cli.output {
            Some(path) => fs::write(path, output)?,
            None => io::stdout().write_all(output.as_bytes())?,
        },
        Err(diagnostics) => {
            let mut stderr = io::stderr().lock();
            let file_name = cli.input.display().to_string();
            for diagnostic in diagnostics {
                diagnostic.write(&file_name, &source, &mut stderr)?;
            }
            std::process::exit(1);
        }
    }
    Ok(())
}
