use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::actions::{Action, DriverError, DriverOptions, InputFile, LinkInput, Output, StopPhase};
use super::compile::{
    build_defines, emit_machine_code, fcc_context, lower_to_ir, parse_source, preprocess,
    read_input,
};
use crate::lexer::Token;
use crate::toolchain::link_command;

pub fn execute(actions: &[Action], opts: &DriverOptions) -> Result<(), DriverError> {
    if opts.dry_run {
        dry_run(actions, opts);
        return Ok(());
    }
    let started = std::time::Instant::now();
    // Temp objects for compile-to-link live here until every link completes.
    let mut temp_dir: Option<TempDir> = None;
    let mut sink: Option<Box<dyn Write>> = None;
    let mut outputs: Vec<Option<PathBuf>> = Vec::with_capacity(actions.len());
    for (index, action) in actions.iter().enumerate() {
        match action {
            Action::Compile {
                input,
                stop,
                output: Output::Temp,
            } => {
                let dir = temp_dir.get_or_insert_with(|| {
                    TempDir::new().unwrap_or_else(|e| {
                        eprintln!("fcc: error: cannot create temporary directory: {e}");
                        std::process::exit(1);
                    })
                });
                let path = temp_object_path(dir, input, index);
                let mut file = File::create(&path).unwrap_or_else(|e| {
                    eprintln!(
                        "fcc: error: cannot open temporary '{}': {e}",
                        path.display()
                    );
                    std::process::exit(1);
                });
                run_compile_action(input, *stop, opts, &mut file);
                outputs.push(Some(path));
            }
            Action::Compile {
                input,
                stop,
                output,
            } => {
                let out = sink.get_or_insert_with(|| open_output(output));
                run_compile_action(input, *stop, opts, out.as_mut());
                outputs.push(None);
            }
            Action::Link { inputs, output } => {
                let objects: Vec<PathBuf> = inputs
                    .iter()
                    .map(|input| resolve_link_input(input, &outputs))
                    .collect();
                link_command(&objects, output, &opts.lib_dirs, &opts.libs)
                    .run()
                    .map_err(DriverError::LinkFailed)?;
                outputs.push(None);
            }
        }
    }
    tir::report_pass_timing(started.elapsed());
    Ok(())
}

fn resolve_link_input(input: &LinkInput, outputs: &[Option<PathBuf>]) -> PathBuf {
    match input {
        LinkInput::Object(path) => path.clone(),
        LinkInput::CompileOutput(idx) => outputs[*idx]
            .clone()
            .expect("link input references a non-temp compile action"),
    }
}

fn temp_object_path(dir: &TempDir, input: &InputFile, index: usize) -> PathBuf {
    let stem = match input {
        InputFile::CSource(p) => Path::new(p),
        InputFile::Object(p) => p.as_path(),
    }
    .file_stem()
    .unwrap_or_default();
    dir.path()
        .join(format!("{}-{index}.o", Path::new(stem).display()))
}

/// `-###`: print each planned action as a quoted pseudo command line to stdout
/// (so LIT `filecheck` can read it) and return without executing anything.
fn dry_run(actions: &[Action], opts: &DriverOptions) {
    let mut resolved: Vec<String> = Vec::with_capacity(actions.len());
    for action in actions {
        match action {
            Action::Compile {
                input,
                stop,
                output,
            } => {
                let out_name = resolve_output(output, input);
                println!(
                    " \"fcc\" \"{}\" \"-o\" \"{out_name}\" \"{}\"",
                    phase_flag(*stop),
                    input_display(input),
                );
                resolved.push(out_name);
            }
            Action::Link { inputs, output } => {
                let objects: Vec<PathBuf> = inputs
                    .iter()
                    .map(|input| match input {
                        LinkInput::Object(path) => path.clone(),
                        LinkInput::CompileOutput(idx) => PathBuf::from(&resolved[*idx]),
                    })
                    .collect();
                let argv =
                    link_command(&objects, output, &opts.lib_dirs, &opts.libs).display_argv();
                let quoted: Vec<String> = argv.iter().map(|a| format!("\"{a}\"")).collect();
                println!(" {}", quoted.join(" "));
                resolved.push(String::new());
            }
        }
    }
}

fn phase_flag(stop: StopPhase) -> &'static str {
    match stop {
        StopPhase::Preprocess => "-E",
        StopPhase::Assembly => "-S",
        StopPhase::Object => "-c",
        StopPhase::Tokens => "-tokens",
        StopPhase::Ast => "-ast",
        StopPhase::Ir => "-ir",
        StopPhase::Link => "-link",
    }
}

fn input_display(input: &InputFile) -> String {
    match input {
        InputFile::CSource(path) => path.to_string_lossy().into_owned(),
        InputFile::Object(path) => path.display().to_string(),
    }
}

fn resolve_output(output: &Output, input: &InputFile) -> String {
    match output {
        Output::Stdout => "-".to_string(),
        Output::File(path) => path.display().to_string(),
        Output::Temp => {
            let path = match input {
                InputFile::CSource(p) => Path::new(p),
                InputFile::Object(p) => p.as_path(),
            };
            let stem = path.file_stem().unwrap_or_default();
            std::env::temp_dir()
                .join(Path::new(stem).with_extension("o"))
                .display()
                .to_string()
        }
    }
}

fn open_output(output: &Output) -> Box<dyn Write> {
    match output {
        Output::Stdout => Box::new(BufWriter::new(io::stdout())),
        Output::File(path) => Box::new(BufWriter::new(File::create(path).unwrap_or_else(|e| {
            eprintln!("fcc: error: cannot open output '{}': {e}", path.display());
            std::process::exit(1);
        }))),
        Output::Temp => unreachable!("temp outputs are written directly in execute"),
    }
}

fn run_compile_action(
    input: &InputFile,
    stop: StopPhase,
    opts: &DriverOptions,
    out: &mut dyn Write,
) {
    let InputFile::CSource(path) = input else {
        unreachable!("compile actions only take C sources");
    };
    let (name, source) = read_input(path);

    match stop {
        StopPhase::Preprocess => {
            for (tok, _) in preprocess(
                &name,
                &source,
                build_defines(&opts.defines),
                &opts.undefines,
                &opts.include_dirs,
                opts.lang_options,
                opts.march.as_deref(),
            ) {
                write!(out, "{tok}").unwrap();
            }
        }
        StopPhase::Tokens => {
            let tokens: Vec<Token> = preprocess(
                &name,
                &source,
                build_defines(&opts.defines),
                &opts.undefines,
                &opts.include_dirs,
                opts.lang_options,
                opts.march.as_deref(),
            )
            .into_iter()
            .map(|(tok, _)| tok)
            .collect();
            writeln!(out, "{tokens:#?}").unwrap();
        }
        StopPhase::Ast => {
            let unit = parse_source(
                &name,
                &source,
                &opts.defines,
                &opts.undefines,
                &opts.include_dirs,
                opts.lang_options,
                opts.march.as_deref(),
            );
            write!(out, "{}", crate::ast::render(&unit)).unwrap();
        }
        StopPhase::Ir => {
            let unit = parse_source(
                &name,
                &source,
                &opts.defines,
                &opts.undefines,
                &opts.include_dirs,
                opts.lang_options,
                opts.march.as_deref(),
            );
            let context = fcc_context();
            let module = lower_to_ir(
                &context,
                unit,
                opts.lang_options,
                opts.march.as_deref(),
                opts.mabi.as_deref(),
            );
            // An optimizing level runs its mid-end here; -O0 prints the
            // conversion itself, which is what the backend will be handed.
            if opts.opt_level.rounds().is_some() {
                use tir::Operation;
                super::compile::mid_end(opts, false)
                    .run(&context, context.get_op(module.id()))
                    .unwrap_or_else(|e| {
                        eprintln!("fcc: error: mid-end failed: {e}");
                        std::process::exit(1);
                    });
            }
            let mut ir = String::new();
            let mut fmt = tir::IRFormatter::new(&mut ir);
            tir::print_ir(&module, &context, &mut fmt).unwrap_or_else(|e| {
                eprintln!("fcc: error: failed to print IR: {e}");
                std::process::exit(1);
            });
            write!(out, "{ir}").unwrap();
        }
        StopPhase::Assembly => {
            out.write_all(&emit_machine_code(opts, &name, &source, true))
                .unwrap();
        }
        StopPhase::Object => {
            out.write_all(&emit_machine_code(opts, &name, &source, false))
                .unwrap();
        }
        StopPhase::Link => unreachable!("link is not a compile action"),
    }
}
