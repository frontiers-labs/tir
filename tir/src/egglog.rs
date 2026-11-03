use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use clap::Args;
use tir::Context;
use tir_egglog::{EGraph, ast::Command};

#[derive(Args)]
pub struct ToolArgs {
    /// Input TIR file
    input: PathBuf,
    /// Egglog file to execute; omit to read commands interactively
    rules: Option<PathBuf>,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let input = std::fs::read_to_string(&args.input)
        .map_err(|err| format!("{}: {err}", args.input.display()))?;
    let context = Context::with_default_dialects();
    context.register_dialect::<tir_gpu::spirv::SpirvDialect>();
    crate::common::parse_tir(&context, &input)
        .map_err(|err| format!("{}: {err}", args.input.display()))?;

    let mut egraph = context.egraph_with_rules()?;

    if let Some(path) = args.rules {
        let source =
            std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let commands = egraph
            .parse_program("egglog input", &source)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        execute(&mut egraph, commands).map_err(|err| format!("{}: {err}", path.display()))?;
    } else {
        repl(&mut egraph)?;
    }
    Ok(())
}

fn execute(egraph: &mut EGraph, commands: Vec<Command>) -> Result<(), Box<dyn Error>> {
    let (outputs, timing) = egraph.run_program_timed(commands)?;
    for output in outputs {
        print!("{output}");
    }
    eprintln!(
        "egglog: compile={:.6}s exec={:.6}s",
        timing.compilation.as_secs_f64(),
        timing.execution.as_secs_f64()
    );
    Ok(())
}

fn repl(egraph: &mut EGraph) -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut stdin = stdin.lock();
    let mut source = String::new();
    loop {
        if interactive {
            print!(
                "{}",
                if source.is_empty() {
                    "egglog> "
                } else {
                    "...> "
                }
            );
            io::stdout().flush()?;
        }
        if stdin.read_line(&mut source)? == 0 {
            egraph
                .parse_program("stdin", &source)
                .map_err(|err| err.to_string())?;
            return Ok(());
        }
        match egraph.parse_program("stdin", &source) {
            Err(tir_egglog::Error::Unfinished(_)) => continue,
            Err(err) => eprintln!("{err}"),
            Ok(commands) if commands.is_empty() => {}
            Ok(commands) => {
                if let Err(err) = execute(egraph, commands) {
                    eprintln!("{err}");
                }
            }
        }
        source.clear();
    }
}
