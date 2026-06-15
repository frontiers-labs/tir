use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::lexer::Token;
use crate::preprocessor::preprocessed;

#[derive(Debug, Parser)]
#[command(name = "fcc")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Compile(CompileArgs),
}

#[derive(Debug, Args)]
pub struct CompileArgs {
    #[arg(long, value_enum, default_value_t = CompileStage::Preprocess)]
    stage: CompileStage,
    /// Target architecture (required for the asm and obj stages).
    #[arg(long)]
    march: Option<String>,
    /// Target CPU
    #[arg(long)]
    mcpu: Option<String>,
    #[arg(short = 'o', default_value = "-")]
    output: OsString,
    /// Predefine a macro, e.g. `-D NAME=VALUE` (or `-D NAME`).
    #[arg(short = 'D', value_name = "NAME[=VALUE]")]
    defines: Vec<String>,
    inputs: Vec<OsString>,
}

/// Build the predefined-macro map from `-D` arguments. Each value is lexed to a
/// single token, mirroring how `#define NAME VALUE` is stored.
fn build_defines(defines: &[String]) -> HashMap<String, Token> {
    use logos::Logos;
    defines
        .iter()
        .map(|d| {
            let (name, value) = match d.split_once('=') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None => (d.to_string(), "1".to_string()),
            };
            let tok = Token::lexer(value.trim())
                .next()
                .and_then(|r| r.ok())
                .unwrap_or(Token::Hash);
            (name, tok)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, ValueEnum)]
pub enum CompileStage {
    /// Emit the preprocessed token stream as reconstructed source text.
    Preprocess,
    /// Emit the preprocessed token stream in its debug representation.
    Tokens,
    Ast,
    Ir,
    /// Emit textual assembly for the selected target.
    Asm,
    /// Emit an ELF relocatable object for the selected target.
    Obj,
}

pub fn compiler_main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Compile(args) => run_compile(args),
    }
}

fn run_compile(args: CompileArgs) {
    let mut out: Box<dyn Write> = if args.output == "-" {
        Box::new(BufWriter::new(io::stdout()))
    } else {
        let path = PathBuf::from(&args.output);
        Box::new(BufWriter::new(File::create(&path).unwrap_or_else(|e| {
            eprintln!(
                "fcc: cannot open output '{}': {e}",
                args.output.to_string_lossy()
            );
            std::process::exit(1);
        })))
    };

    for input in &args.inputs {
        let reader: Box<dyn io::Read> = if input == "-" {
            Box::new(io::stdin())
        } else {
            Box::new(File::open(input).unwrap_or_else(|e| {
                eprintln!("fcc: cannot open input '{}': {e}", input.to_string_lossy());
                std::process::exit(1);
            }))
        };

        match args.stage {
            CompileStage::Preprocess => {
                emit_preprocess(
                    &mut out,
                    preprocessed(reader, build_defines(&args.defines), &[]),
                );
            }
            CompileStage::Tokens => {
                let tokens: Vec<Token> =
                    preprocessed(reader, build_defines(&args.defines), &[]).collect();
                writeln!(out, "{tokens:#?}").unwrap();
            }
            CompileStage::Ast => {
                let unit = parse_source(reader);
                write!(out, "{}", crate::ast::render(&unit)).unwrap();
            }
            CompileStage::Ir => {
                let unit = parse_source(reader);
                let context = tir::Context::with_default_dialects();
                let module = lower_to_ir(&context, &unit);
                let mut ir = String::new();
                let mut fmt = tir::IRFormatter::new(&mut ir);
                use tir::Operation;
                module.print(&mut fmt).unwrap_or_else(|e| {
                    eprintln!("fcc: failed to print IR: {e}");
                    std::process::exit(1);
                });
                write!(out, "{ir}").unwrap();
            }
            CompileStage::Asm | CompileStage::Obj => {
                let bytes = emit_machine_code(&args, reader);
                out.write_all(&bytes).unwrap();
            }
        }
    }
}

fn lower_to_ir(context: &tir::Context, unit: &crate::ast::Ast) -> tir::builtin::ModuleOp {
    crate::codegen::codegen(context, unit).unwrap_or_else(|e| {
        eprintln!("fcc: codegen error: {e}");
        std::process::exit(1);
    })
}

/// Run the backend pipeline (mem2reg, instruction selection, register
/// allocation, finalization) and render assembly or an ELF object.
fn emit_machine_code(args: &CompileArgs, reader: Box<dyn io::Read>) -> Vec<u8> {
    use tir::Operation;
    use tir_be_common::pipeline::{StopAfter, build_pipeline};

    let Some(march) = args.march.as_deref() else {
        eprintln!("fcc: --march is required for the asm and obj stages");
        std::process::exit(1);
    };
    let target = tir_targets::select(march, args.mcpu.as_deref(), None).unwrap_or_else(|e| {
        eprintln!("fcc: {e}");
        std::process::exit(1);
    });

    let unit = parse_source(reader);
    let context = tir::Context::with_default_dialects();
    target.register_dialects(&context);
    let module = lower_to_ir(&context, &unit);

    let mut pm = tir::PassManager::new();
    pm.nest(tir::builtin::FuncOp::name())
        .add_pass(tir::passes::Mem2RegPass::new());
    let module_op = context.get_op(module.id());
    pm.run(&context, module_op.clone()).unwrap_or_else(|e| {
        eprintln!("fcc: mem2reg failed: {e}");
        std::process::exit(1);
    });

    let mut pm = build_pipeline(target.as_ref(), &context, StopAfter::Finalize);
    pm.run(&context, module_op).unwrap_or_else(|e| {
        eprintln!("fcc: backend pipeline failed: {e}");
        std::process::exit(1);
    });

    if args.stage == CompileStage::Asm {
        let rendered = target
            .asm_printer(&context)
            .print_module(&context, &module)
            .unwrap_or_else(|e| {
                eprintln!("fcc: failed to print assembly: {e}");
                std::process::exit(1);
            });
        return rendered.into_bytes();
    }

    let (Some(format), Some(writer)) = (target.object_format(), target.binary_writer(&context))
    else {
        eprintln!("fcc: target '{march}' does not support object emission");
        std::process::exit(1);
    };
    let object = writer
        .write_module(&context, &module, &format)
        .unwrap_or_else(|e| {
            eprintln!("fcc: failed to emit object: {e}");
            std::process::exit(1);
        });
    tir_be_common::binary::write_elf(&object, &format)
}

fn parse_source(reader: Box<dyn io::Read>) -> crate::ast::Ast {
    let tokens: Vec<Token> = preprocessed(reader, HashMap::new(), &[]).collect();
    crate::parser::parse(&tokens).unwrap_or_else(|errors| {
        for e in errors {
            eprintln!("fcc: parse error: {e}");
        }
        std::process::exit(1);
    })
}

fn emit_preprocess(out: &mut dyn Write, tokens: impl Iterator<Item = Token>) {
    for tok in tokens {
        write!(out, "{tok}").unwrap();
    }
}
