use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

use clap::Args;
use tir::Context;
use tir_cdsl::{Schema, lexer::TokenStream};
use tir_egglog::{EGraph, Program};

#[derive(Args)]
#[command(
    after_help = "Declare the script's schema in CDSL and define `let execute = || { std::schedule::saturate((rule_a, rule_b)) };`. Relations and tables use their declared names. REPL declarations accumulate; each accepted revision runs the selected schedule when it exists."
)]
pub struct ToolArgs {
    /// Input TIR file
    input: PathBuf,
    /// CDSL file to execute; omit to read CDSL declarations interactively
    rules: Option<PathBuf>,
    /// Named CDSL schedule to run
    #[arg(long, default_value = "execute")]
    schedule: String,
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let input = std::fs::read_to_string(&args.input)
        .map_err(|err| format!("{}: {err}", args.input.display()))?;
    let context = Context::with_default_dialects();
    context.register_dialect::<tir_gpu::spirv::SpirvDialect>();
    crate::common::parse_tir(&context, &input)
        .map_err(|err| format!("{}: {err}", args.input.display()))?;
    let mut egraph = context.egraph();
    let schedule = args
        .schedule
        .split("::")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(path) = args.rules {
        let source =
            tir_cdsl::compile_file(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        execute(&mut egraph, &source, &schedule, false)
            .map_err(|err| format!("{}: {err}", path.display()))?;
    } else {
        repl(&mut egraph, &schedule)?;
    }
    Ok(())
}

fn execute(
    egraph: &mut EGraph,
    source: &tir_cdsl::Program,
    schedule: &[String],
    allow_missing_schedule: bool,
) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let module = &schedule[..schedule.len().saturating_sub(1)];
    let selected;
    let source = if !module.is_empty() && source.modules.iter().any(|part| part.path == module) {
        selected = source.clone().select(module)?;
        &selected
    } else {
        source
    };
    let checked = source.lower()?;
    let schema = Schema::with_engine(&checked, EGraph::from_database(egraph.database().clone()))?;
    if schema
        .run_local_schedule(&checked, schedule, egraph.database_mut())?
        .is_some()
    {
        eprintln!("egglog: total={:.6}s", start.elapsed().as_secs_f64());
        return Ok(());
    }
    let lowered = schema.lower(&checked)?;
    let Some(selected) = lowered.schedules().get(schedule) else {
        if allow_missing_schedule {
            return Ok(());
        }
        return Err(format!("CDSL schedule {} does not exist", schedule.join("::")).into());
    };
    let bytes = lowered.compile(schema.database())?.to_bytes();
    let database = egraph.database_mut();
    Program::from_bytes(&schema.to_bytes()?, database)?;
    let mut program = Program::from_bytes(&bytes, database)?;
    let compilation = start.elapsed();
    let start = Instant::now();
    program.run(database, selected, &[])?;
    eprintln!(
        "egglog: compile={:.6}s exec={:.6}s",
        compilation.as_secs_f64(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn repl(egraph: &mut EGraph, schedule: &[String]) -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut accepted = String::new();
    let mut pending = String::new();
    loop {
        if interactive {
            print!(
                "{}",
                if pending.is_empty() {
                    "egglog> "
                } else {
                    "...> "
                }
            );
            io::stdout().flush()?;
        }
        if input.read_line(&mut pending)? == 0 {
            if !pending.trim().is_empty() {
                tir_cdsl::compile_source(&format!("{accepted}{pending}"))?;
            }
            return Ok(());
        }
        let stream = match TokenStream::from_source(&pending) {
            Ok(stream) => stream,
            Err(errors)
                if errors
                    .iter()
                    .all(|error| error.message == "unterminated block comment")
                    || TokenStream::from_source(&format!("{pending}\"")).is_ok() =>
            {
                continue;
            }
            Err(errors) => {
                eprintln!("{errors:?}");
                pending.clear();
                continue;
            }
        };
        match tir_cdsl::parser::parse(stream) {
            Err(errors) if errors.iter().all(|error| error.found.is_none()) => continue,
            Err(errors) => eprintln!("{errors:?}"),
            Ok(ast) if ast.declarations.is_empty() => {}
            Ok(_) => {
                let revision = format!("{accepted}{pending}");
                match tir_cdsl::compile_source(&revision) {
                    Ok(program) => {
                        // Loading and running can change the database before an error.
                        execute(egraph, &program, schedule, true)
                            .map_err(|error| error.to_string())?;
                        accepted = revision;
                    }
                    Err(err) => eprintln!("{err}"),
                }
            }
        }
        pending.clear();
    }
}
