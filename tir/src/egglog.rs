use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

use clap::Args;
use tir::Context;
use tir_cdsl::lexer::TokenStream;
use tir_egglog::{EGraph, Program, Schedule};

#[derive(Args)]
#[command(
    after_help = "Write a CDSL v3 file defining relations, rules, and a `Schedule` (e.g. `pub let execute: Schedule = ...;`). REPL declarations accumulate; each accepted revision compiles to TEGG v15 bytecode and runs the selected schedule when it exists."
)]
pub struct ToolArgs {
    /// Input TIR file
    input: PathBuf,
    /// CDSL file to execute; omit to read CDSL declarations interactively
    rules: Option<PathBuf>,
    /// Named CDSL schedule/program binding to run
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
    if let Some(path) = args.rules {
        let mut program =
            tir_cdsl::compile_file(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        execute(&mut egraph, &mut program, &args.schedule, false)
            .map_err(|err| format!("{}: {err}", path.display()))?;
    } else {
        repl(&mut egraph, &args.schedule)?;
    }
    Ok(())
}

fn ir_schedule_to_egglog(sched: &tir_cdsl::tegg::IrSchedule) -> Schedule {
    match sched {
        tir_cdsl::tegg::IrSchedule::Run(rs, 0) => Schedule::saturate(Schedule::run(rs)),
        tir_cdsl::tegg::IrSchedule::Run(rs, 1) => Schedule::run(rs),
        tir_cdsl::tegg::IrSchedule::Run(rs, n) => Schedule::repeat(*n, Schedule::run(rs)),
        tir_cdsl::tegg::IrSchedule::Seq(steps) => {
            Schedule::seq(steps.iter().map(ir_schedule_to_egglog))
        }
        tir_cdsl::tegg::IrSchedule::Repeat(0, inner) => {
            Schedule::saturate(ir_schedule_to_egglog(inner))
        }
        tir_cdsl::tegg::IrSchedule::Repeat(n, inner) => {
            Schedule::repeat(*n, ir_schedule_to_egglog(inner))
        }
    }
}

fn execute(
    egraph: &mut EGraph,
    checked: &mut tir_cdsl::CheckedProgram,
    schedule: &str,
    allow_missing_schedule: bool,
) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let ml = checked.erase_for_mode(None, tir_cdsl::erase::EraseMode::Program)?;
    let selected = ml.decls.iter().find_map(|decl| match decl {
        tir_cdsl::erase::MlDecl::ProgramBundle(bundle) => {
            let sched = bundle
                .schedules
                .iter()
                .find(|s| checked.signature.interner.resolve(s.qname) == schedule)?;
            let code = bundle
                .code_bytes
                .as_deref()
                .unwrap_or(bundle.catalog_bytes.as_slice());
            Some((
                Some(bundle.catalog_bytes.as_slice()),
                code,
                ir_schedule_to_egglog(&sched.schedule),
            ))
        }
        tir_cdsl::erase::MlDecl::Def(
            name,
            tir_cdsl::erase::MlExpr::Bytecode {
                bc_bytes,
                bc_schedule,
                ..
            },
        ) if checked.signature.interner.resolve(*name) == schedule => Some((
            None,
            bc_bytes.as_slice(),
            ir_schedule_to_egglog(bc_schedule),
        )),
        _ => None,
    });
    let Some((catalog, bytes, sched)) = selected else {
        if allow_missing_schedule {
            return Ok(());
        }
        return Err(format!("CDSL schedule {schedule} does not exist").into());
    };
    let database = egraph.database_mut();
    if let Some(catalog) = catalog {
        let _ = Program::from_bytes(catalog, database)?;
    }
    let mut program = Program::from_bytes(bytes, database)?;
    let compilation = start.elapsed();
    let start = Instant::now();
    program.run(database, &sched, &[])?;
    eprintln!(
        "egglog: compile={:.6}s exec={:.6}s",
        compilation.as_secs_f64(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn repl(egraph: &mut EGraph, schedule: &str) -> Result<(), Box<dyn Error>> {
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
        match tir_cdsl::parser::parse(&stream) {
            Err(errors) if errors.iter().all(|error| error.found.is_none()) => continue,
            Err(errors) => eprintln!("{errors:?}"),
            Ok(ast) if ast.root_decls().is_empty() => {}
            Ok(_) => {
                let revision = format!("{accepted}{pending}");
                match tir_cdsl::compile_source(&revision) {
                    Ok(mut program) => {
                        execute(egraph, &mut program, schedule, true)
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
