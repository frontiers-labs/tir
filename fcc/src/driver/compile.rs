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
) -> tir::builtin::ModuleOp {
    let target = match march {
        Some(march) => crate::sema::TargetProfile::for_march(march),
        None => crate::sema::TargetProfile::host(),
    }
    .unwrap_or_else(|error| {
        eprintln!("fcc: error: {error}; pass --march explicitly");
        std::process::exit(1);
    });
    let typed =
        crate::sema::analyze_with_target(unit, options, target).unwrap_or_else(|diagnostics| {
            for diagnostic in diagnostics {
                diagnostic.eprint();
            }
            std::process::exit(1);
        });
    crate::codegen::codegen(context, &typed).unwrap_or_else(|d| {
        d.eprint();
        std::process::exit(1);
    })
}

pub(super) fn fcc_context() -> tir::Context {
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<crate::cir::CirDialect>();
    context
}

/// Run the backend pipeline (mem2reg, instruction selection, register
/// allocation, finalization) and render assembly or an ELF object.
pub(super) fn emit_machine_code(
    opts: &DriverOptions,
    name: &str,
    source: &str,
    emit_assembly: bool,
) -> Vec<u8> {
    use tir::Operation;
    use tir::backend::pipeline::{StopAfter, build_pipeline};

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
    );
    let context = fcc_context();
    target.register_dialects(&context);
    let module = lower_to_ir(&context, unit, opts.lang_options, Some(march));

    let mut pm = tir::PassManager::new();
    pm.add_pass(crate::passes::LowerCirStructsPass::new());
    let function_pipeline = pm.nest::<tir::builtin::FuncOp>();
    function_pipeline.add_pass(crate::passes::LowerCirControlFlowPass::new());
    function_pipeline.add_pass(tir::passes::Mem2RegPass::new());
    function_pipeline.add_pass(tir::passes::InstCombinePass::new());
    function_pipeline.add_pass(tir::passes::ScfToCfgPass::new());
    let module_op = context.get_op(module.id());
    pm.run(&context, module_op.clone()).unwrap_or_else(|e| {
        eprintln!("fcc: error: control-flow lowering failed: {e}");
        std::process::exit(1);
    });

    crate::codegen::hoist_strings(&context, &module).unwrap_or_else(|e| {
        eprintln!("fcc: error: string lowering failed: {e}");
        std::process::exit(1);
    });

    let mut pm = build_pipeline(target.as_ref(), &context, StopAfter::Finalize);
    pm.run(&context, module_op).unwrap_or_else(|e| {
        eprintln!("fcc: error: backend pipeline failed: {e}");
        std::process::exit(1);
    });

    if emit_assembly {
        let rendered = target
            .asm_printer(&context)
            .print_module(&context, &module)
            .unwrap_or_else(|e| {
                eprintln!("fcc: error: failed to print assembly: {e}");
                std::process::exit(1);
            });
        return rendered.into_bytes();
    }

    let (Some(format), Some(writer)) = (target.object_format(), target.binary_writer(&context))
    else {
        eprintln!("fcc: error: target '{march}' does not support object emission");
        std::process::exit(1);
    };
    let object = writer
        .write_module(&context, &module, &format)
        .unwrap_or_else(|e| {
            eprintln!("fcc: error: failed to emit object: {e}");
            std::process::exit(1);
        });
    tir::backend::binary::write_elf(&object, &format)
}

/// Preprocess `source`, reporting any `#error`/`#warning` diagnostics. Exits if
/// any of them is an error.
fn add_default_defines(defines: &mut HashMap<String, Token>, options: LangOptions) {
    use logos::Logos;
    let mut predefined = vec![
        ("__GNUC__", "4"),
        ("__GNUC_MINOR__", "2"),
        ("__GNUC_PATCHLEVEL__", "1"),
        ("__STDC__", "1"),
        ("__LP64__", "1"),
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
    let arch_define = match std::env::consts::ARCH {
        "aarch64" => "__arm64__",
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
) -> Vec<(Token, crate::diagnostics::Span)> {
    add_default_defines(&mut defines, options);
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
) -> crate::ast::Ast {
    let tokens = preprocess(
        name,
        source,
        build_defines(defines),
        undefines,
        include_dirs,
        options,
    );
    crate::parser::parse(&tokens, options).unwrap_or_else(|diags| {
        for diag in &diags {
            diag.eprint();
        }
        std::process::exit(1);
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::add_default_defines;
    use crate::lang_options::{LangOptions, StdVersion};

    #[test]
    fn c89_omits_stdc_version() {
        let mut defines = HashMap::new();
        add_default_defines(
            &mut defines,
            LangOptions {
                std_version: StdVersion::C89,
                gnu_extensions: false,
            },
        );
        assert!(!defines.contains_key("__STDC_VERSION__"));
    }

    #[test]
    fn c99_sets_stdc_version() {
        let mut defines = HashMap::new();
        add_default_defines(
            &mut defines,
            LangOptions {
                std_version: StdVersion::C99,
                gnu_extensions: false,
            },
        );
        assert_eq!(defines["__STDC_VERSION__"].to_string(), "199901L");
    }
}
