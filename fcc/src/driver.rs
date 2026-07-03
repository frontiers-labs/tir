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
    /// Print a detailed explanation of a diagnostic code, e.g. `--explain E0001`.
    #[arg(long, value_name = "CODE")]
    explain: Option<String>,
    #[command(subcommand)]
    command: Option<Commands>,
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

    if let Some(code) = cli.explain {
        match crate::diagnostics::explain(&code) {
            Some(text) => print!("{text}"),
            None => {
                eprintln!("fcc: unknown diagnostic code '{code}'");
                std::process::exit(1);
            }
        }
        return;
    }

    match cli.command {
        Some(Commands::Compile(args)) => run_compile(args),
        None => {
            eprintln!("fcc: no subcommand given; try `fcc compile` or `fcc --explain <CODE>`");
            std::process::exit(1);
        }
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
        let (name, source) = read_input(input);

        match args.stage {
            CompileStage::Preprocess => {
                for (tok, _) in preprocess(&name, &source, build_defines(&args.defines)) {
                    write!(out, "{tok}").unwrap();
                }
            }
            CompileStage::Tokens => {
                let tokens: Vec<Token> = preprocess(&name, &source, build_defines(&args.defines))
                    .into_iter()
                    .map(|(tok, _)| tok)
                    .collect();
                writeln!(out, "{tokens:#?}").unwrap();
            }
            CompileStage::Ast => {
                let unit = parse_source(&name, &source);
                write!(out, "{}", crate::ast::render(&unit)).unwrap();
            }
            CompileStage::Ir => {
                let unit = parse_source(&name, &source);
                let context = fcc_context();
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
                let bytes = emit_machine_code(&args, &name, &source);
                out.write_all(&bytes).unwrap();
            }
        }
    }
}

/// Read an input into its `(display name, source text)` pair. `-` reads stdin.
fn read_input(input: &OsString) -> (String, String) {
    if input == "-" {
        let mut source = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut source).unwrap_or_default();
        ("<stdin>".to_string(), source)
    } else {
        let source = std::fs::read_to_string(input).unwrap_or_else(|e| {
            eprintln!("fcc: cannot open input '{}': {e}", input.to_string_lossy());
            std::process::exit(1);
        });
        (input.to_string_lossy().into_owned(), source)
    }
}

fn lower_to_ir(context: &tir::Context, unit: &crate::ast::Ast) -> tir::builtin::ModuleOp {
    crate::codegen::codegen(context, unit).unwrap_or_else(|d| {
        d.eprint();
        std::process::exit(1);
    })
}

fn fcc_context() -> tir::Context {
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<crate::cir::CirDialect>();
    context
}

fn default_include_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(sdkroot) = std::env::var("SDKROOT") {
        paths.push(PathBuf::from(sdkroot).join("usr/include"));
    }
    if let Ok(output) = std::process::Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        && output.status.success()
    {
        let sdkroot = String::from_utf8_lossy(&output.stdout);
        paths.push(PathBuf::from(sdkroot.trim()).join("usr/include"));
    }
    paths.extend([
        PathBuf::from("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include"),
        PathBuf::from(
            "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/usr/include",
        ),
        PathBuf::from("/usr/include"),
    ]);

    let mut existing = Vec::new();
    for path in paths {
        if path.is_dir() && !existing.contains(&path) {
            existing.push(path);
        }
    }
    existing
}

/// Run the backend pipeline (mem2reg, instruction selection, register
/// allocation, finalization) and render assembly or an ELF object.
fn emit_machine_code(args: &CompileArgs, name: &str, source: &str) -> Vec<u8> {
    use tir::Operation;
    use tir::backend::pipeline::{StopAfter, build_pipeline};

    let Some(march) = args.march.as_deref() else {
        eprintln!("fcc: --march is required for the asm and obj stages");
        std::process::exit(1);
    };
    let target =
        tir::backend::select_target(march, args.mcpu.as_deref(), None).unwrap_or_else(|e| {
            eprintln!("fcc: {e}");
            std::process::exit(1);
        });

    let unit = parse_source(name, source);
    let context = fcc_context();
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

    crate::codegen::hoist_strings(&context, &module).unwrap_or_else(|e| {
        eprintln!("fcc: string lowering failed: {e}");
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
    tir::backend::binary::write_elf(&object, &format)
}

/// Preprocess `source`, reporting any `#error`/`#warning` diagnostics. Exits if
/// any of them is an error.
fn add_default_defines(defines: &mut HashMap<String, Token>) {
    use logos::Logos;
    for (name, value) in [
        ("__GNUC__", "4"),
        ("__GNUC_MINOR__", "2"),
        ("__GNUC_PATCHLEVEL__", "1"),
        ("__APPLE__", "1"),
        ("__MACH__", "1"),
        ("__STDC__", "1"),
        ("__STDC_VERSION__", "201710L"),
        ("__LP64__", "1"),
    ] {
        defines.entry(name.to_string()).or_insert_with(|| {
            Token::lexer(value)
                .next()
                .and_then(|r| r.ok())
                .unwrap_or(Token::Hash)
        });
    }
    let arch_define = match std::env::consts::ARCH {
        "aarch64" => "__arm64__",
        "x86_64" => "__x86_64__",
        _ => return,
    };
    defines
        .entry(arch_define.to_string())
        .or_insert(Token::Hash);
}

fn preprocess(
    name: &str,
    source: &str,
    mut defines: HashMap<String, Token>,
) -> Vec<(Token, crate::diagnostics::Span)> {
    add_default_defines(&mut defines);
    let include_paths = default_include_paths();
    let mut stream = preprocessed(name, source, defines, &include_paths);
    let tokens = stream.collect_tokens();
    let mut had_error = false;
    for diag in stream.diagnostics() {
        diag.eprint();
        had_error |= diag.is_error();
    }
    if had_error {
        std::process::exit(1);
    }
    tokens
}

fn parse_source(name: &str, source: &str) -> crate::ast::Ast {
    let tokens = preprocess(name, source, HashMap::new());
    crate::parser::parse(&tokens).unwrap_or_else(|diags| {
        for diag in &diags {
            diag.eprint();
        }
        std::process::exit(1);
    })
}
