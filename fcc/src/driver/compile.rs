use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use super::actions::DriverOptions;
use crate::lang_options::LangOptions;
use crate::lexer::Token;
use crate::preprocessor::{IncludePaths, preprocessed};
use crate::toolchain::system_include_dirs;

/// Build the predefined-macro map from `-D` arguments. Each value is lexed to a
/// single token, mirroring how `#define NAME VALUE` is stored.
pub(super) fn build_defines(defines: &[String]) -> HashMap<String, Token> {
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

/// Read an input into its `(display name, source text)` pair. `-` reads stdin.
pub(super) fn read_input(input: &OsString) -> (String, String) {
    if input == "-" {
        let mut source = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut source).unwrap_or_default();
        ("<stdin>".to_string(), source)
    } else {
        let source = std::fs::read_to_string(input).unwrap_or_else(|e| {
            eprintln!(
                "fcc: error: cannot open input '{}': {e}",
                input.to_string_lossy()
            );
            std::process::exit(1);
        });
        (input.to_string_lossy().into_owned(), source)
    }
}

pub(super) fn lower_to_ir(
    context: &tir::Context,
    unit: crate::ast::Ast,
    options: LangOptions,
    march: Option<&str>,
    mabi: Option<&str>,
) -> tir::builtin::ModuleOp {
    let fail = |error: String| -> ! {
        eprintln!("fcc: error: {error}; pass --march explicitly");
        std::process::exit(1);
    };
    let march = march.unwrap_or(std::env::consts::ARCH);
    let machine =
        tir::backend::select_target_with_abi(march, None, None, mabi).unwrap_or_else(|e| fail(e));
    let target =
        crate::sema::TargetProfile::for_target(march, machine.as_ref()).unwrap_or_else(|e| fail(e));
    let typed =
        crate::sema::analyze_with_target(unit, options, target).unwrap_or_else(|diagnostics| {
            for diagnostic in diagnostics {
                diagnostic.eprint();
            }
            std::process::exit(1);
        });
    let module = crate::codegen::codegen(context, &typed).unwrap_or_else(|d| {
        d.eprint();
        std::process::exit(1);
    });
    lower_cir_structs(context, &module);
    raise_loops(context, &module);
    restructure(context, &module);
    describe_target(context, &module, machine.as_ref())
}

/// Replace the frontend's struct operations with pointer arithmetic, so no
/// `cir` operation survives in a function body: restructuring accepts CFG form
/// only, and `cir` is not part of it.
fn lower_cir_structs(context: &tir::Context, module: &tir::builtin::ModuleOp) {
    use tir::Operation;

    let mut pm = tir::PassManager::new();
    pm.add_pass(crate::passes::LowerCirStructsPass::new());
    pm.run(context, context.get_op(module.id()))
        .unwrap_or_else(|e| {
            eprintln!("fcc: error: struct lowering failed: {e}");
            std::process::exit(1);
        });
}

/// Turn the `cir` loop ops codegen emits into `scf.for` where the counted shape
/// is provable, and into blocks and branches where it is not. No `cir` operation
/// survives into restructuring.
fn raise_loops(context: &tir::Context, module: &tir::builtin::ModuleOp) {
    use tir::Operation;

    let mut pm = tir::PassManager::new();
    pm.nest::<tir::func::FuncOp>()
        .add_pass(crate::passes::RaiseLoopsPass::new());
    pm.run(context, context.get_op(module.id()))
        .unwrap_or_else(|e| {
            eprintln!("fcc: error: loop raising failed: {e}");
            std::process::exit(1);
        });
}

/// Raise the flat graph of blocks codegen emits back to structured control
/// flow. A function whose body is one block is left alone.
fn restructure(context: &tir::Context, module: &tir::builtin::ModuleOp) {
    use tir::Operation;

    let mut pm = tir::PassManager::new();
    pm.nest::<tir::func::FuncOp>()
        .add_pass(tir::passes::RestructurePass::new());
    pm.run(context, context.get_op(module.id()))
        .unwrap_or_else(|e| {
            eprintln!("fcc: error: restructuring failed: {e}");
            std::process::exit(1);
        });
}

/// Record the target's data layout and hardware description on the module, so
/// the emitted IR carries the ABI it was compiled for instead of depending on
/// the driver flags to be lowered correctly.
fn describe_target(
    context: &tir::Context,
    module: &tir::builtin::ModuleOp,
    machine: &dyn tir::backend::TargetMachine,
) -> tir::builtin::ModuleOp {
    use tir::Operation;

    let mut attributes = context.get_op(module.id()).attributes().to_vec();
    let specs = [
        (tir::DATA_LAYOUT, machine.data_layout()),
        (tir::TARGET_ENV, machine.target_env()),
    ];
    for (name, spec) in specs {
        if let Some(spec) = spec {
            attributes.push(context.named_attribute(name, spec));
        }
    }
    context.set_op_attributes(module.id(), attributes);
    context
        .get_op(module.id())
        .as_op::<tir::builtin::ModuleOp>()
        .expect("the module keeps its identity")
}

pub(super) fn fcc_context() -> tir::Context {
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<crate::cir::CirDialect>();
    context
}

/// Run the backend pipeline (promotion, instruction selection, register
/// allocation, finalization) and render assembly or an ELF object.
pub(super) fn emit_machine_code(
    opts: &DriverOptions,
    name: &str,
    source: &str,
    emit_assembly: bool,
) -> Vec<u8> {
    use tir::Operation;
    use tir::backend::binary::ObjectEmission;
    use tir::backend::pipeline::{Oracles, lower_and_emit};

    let Some(march) = opts.march.as_deref() else {
        eprintln!("fcc: error: --march is required for the asm and obj stages");
        std::process::exit(1);
    };
    let target = tir::backend::select_target_with_abi(
        march,
        opts.mcpu.as_deref(),
        None,
        opts.mabi.as_deref(),
    )
    .unwrap_or_else(|e| {
        eprintln!("fcc: error: {e}");
        std::process::exit(1);
    });

    let unit = parse_source(
        name,
        source,
        &opts.defines,
        &opts.undefines,
        &opts.include_dirs,
        opts.lang_options,
        opts.march.as_deref(),
    );
    let context = fcc_context();
    target.register_dialects(&context);
    let module = lower_to_ir(
        &context,
        unit,
        opts.lang_options,
        Some(march),
        opts.mabi.as_deref(),
    );

    let mut pm = tir::PassManager::new();
    if let Some(spec) = &opts.pipeline {
        pm = tir::parse_pipeline(spec).unwrap_or_else(|e| {
            eprintln!("fcc: error: {e}");
            std::process::exit(1);
        });
    } else {
        let function_pipeline = pm.nest::<tir::func::FuncOp>();
        // Construction's last step: the locals the frontend spelled as slots
        // become the values the regions carry on ports.
        function_pipeline.add_pass(tir::passes::PromotePass::new());
        function_pipeline.add_pass(tir::passes::ThreadStatePass::new());
        function_pipeline.add_pass(tir::passes::InstCombinePass::new());
        // Loop scheduling reads the chains too, and what it leaves behind — a
        // rebuilt nest, an unrolled body — is address arithmetic nobody has
        // folded yet, so the simplifier runs once more over it.
        function_pipeline.add_pass(tir::passes::AffineSchedulePass::new());
        function_pipeline.add_pass(tir::passes::InstCombinePass::new());
    }
    // Data lowering consumes the δ ops, so the functions that name them must
    // hold symbol addresses of their own by then.
    pm.add_pass(tir::passes::MaterializeSymbolAddressesPass::new());
    pm.run(&context, context.get_op(module.id()))
        .unwrap_or_else(|e| {
            eprintln!("fcc: error: control-flow lowering failed: {e}");
            std::process::exit(1);
        });

    crate::codegen::lower_data(&context, &module).unwrap_or_else(|e| {
        eprintln!("fcc: error: data lowering failed: {e}");
        std::process::exit(1);
    });

    let die = |e: String| -> ! {
        eprintln!("fcc: error: {e}");
        std::process::exit(1);
    };
    let oracles = Oracles {
        shuffle_machine_order: opts.shuffle_machine_order,
    };

    if emit_assembly {
        let printer = tir::backend::AsmPrinter::new();
        let mut rendered = String::new();
        lower_and_emit(
            target.as_ref(),
            &context,
            &module,
            oracles,
            |context, op| {
                printer
                    .print_op(context, op, &mut rendered)
                    .map_err(|e| format!("failed to print assembly: {e}"))
            },
        )
        .unwrap_or_else(|e| die(e));
        return rendered.into_bytes();
    }

    let Some(format) = target.object_format() else {
        eprintln!("fcc: error: target '{march}' does not support object emission");
        std::process::exit(1);
    };
    let writer = tir::backend::binary::BinaryWriter::new();
    let mut emission = ObjectEmission::default();
    lower_and_emit(
        target.as_ref(),
        &context,
        &module,
        oracles,
        |context, op| {
            writer
                .write_op(context, op, &mut emission, &format)
                .map_err(|e| format!("failed to emit object: {e}"))
        },
    )
    .unwrap_or_else(|e| die(e));
    let object = writer.finish(emission, &format).unwrap_or_else(|e| {
        eprintln!("fcc: error: failed to emit object: {e}");
        std::process::exit(1);
    });
    tir::backend::binary::write_elf(&object, &format)
}

/// Preprocess `source`, reporting any `#error`/`#warning` diagnostics. Exits if
/// any of them is an error.
fn add_default_defines(
    defines: &mut HashMap<String, Token>,
    options: LangOptions,
    march: Option<&str>,
) {
    use logos::Logos;
    let mut predefined = vec![
        ("__GNUC__", "4"),
        ("__GNUC_MINOR__", "2"),
        ("__GNUC_PATCHLEVEL__", "1"),
        ("__STDC__", "1"),
        ("__STDC_HOSTED__", "1"),
        ("__LP64__", "1"),
        ("__CHAR_BIT__", "8"),
        ("__extension__", ""),
        ("__inline", "inline"),
        ("__inline__", "inline"),
    ];
    if cfg!(target_os = "macos") {
        predefined.push(("__APPLE__", "1"));
        predefined.push(("__MACH__", "1"));
    }
    if cfg!(target_os = "linux") {
        predefined.push(("__linux__", "1"));
        predefined.push(("__unix__", "1"));
    }
    for (name, value) in predefined {
        defines.entry(name.to_string()).or_insert_with(|| {
            Token::lexer(value)
                .next()
                .and_then(|r| r.ok())
                .unwrap_or(Token::Hash)
        });
    }
    defines
        .entry("__VERSION__".to_string())
        .or_insert_with(|| Token::StringLiteral(format!("fcc {}", env!("CARGO_PKG_VERSION"))));
    let stdc_version = match options.std_version {
        crate::lang_options::StdVersion::C89 => None,
        crate::lang_options::StdVersion::C99 => Some("199901L"),
        crate::lang_options::StdVersion::C11 => Some("201112L"),
        crate::lang_options::StdVersion::C17 => Some("201710L"),
        crate::lang_options::StdVersion::C23 => Some("202311L"),
    };
    if let Some(value) = stdc_version {
        defines
            .entry("__STDC_VERSION__".to_string())
            .or_insert_with(|| {
                Token::lexer(value)
                    .next()
                    .and_then(|result| result.ok())
                    .unwrap()
            });
    }
    let arch_define = match march.unwrap_or(std::env::consts::ARCH) {
        "aarch64" | "arm64" => "__arm64__",
        "x86_64" => "__x86_64__",
        _ => return,
    };
    defines
        .entry(arch_define.to_string())
        .or_insert(Token::Hash);
}

pub(super) fn preprocess(
    name: &str,
    source: &str,
    mut defines: HashMap<String, Token>,
    undefines: &[String],
    include_dirs: &[PathBuf],
    options: LangOptions,
    march: Option<&str>,
) -> Vec<(Token, crate::diagnostics::Span)> {
    add_default_defines(&mut defines, options, march);
    for name in undefines {
        defines.remove(name);
    }
    let include_paths = IncludePaths {
        user: include_dirs.to_vec(),
        system: system_include_dirs(),
    };
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

pub(super) fn parse_source(
    name: &str,
    source: &str,
    defines: &[String],
    undefines: &[String],
    include_dirs: &[PathBuf],
    options: LangOptions,
    march: Option<&str>,
) -> crate::ast::Ast {
    let tokens = preprocess(
        name,
        source,
        build_defines(defines),
        undefines,
        include_dirs,
        options,
        march,
    );
    crate::parser::parse(&tokens, options).unwrap_or_else(|diags| {
        for diag in &diags {
            diag.eprint();
        }
        std::process::exit(1);
    })
}
