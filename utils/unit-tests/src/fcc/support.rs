//! Shared lex → parse → analyze → codegen pipeline helpers for the fcc unit
//! tests.

use fcc::diagnostics::{intern_file, Span};
use fcc::lang_options::LangOptions;
use fcc::lexer::Token;
use logos::Logos;

pub fn lex(name: &str, source: &str) -> Vec<(Token, Span)> {
    let file = intern_file(name, source);
    Token::lexer(source)
        .spanned()
        .map(|(token, span)| (token.unwrap(), Span::new(file, span.start)))
        .collect()
}

/// Parse and analyze under c23 for the given target.
pub fn typed_for(source: &str, march: &str) -> fcc::sema::TypedAst {
    let options: LangOptions = "c23".parse().unwrap();
    let ast = fcc::parser::parse(&lex("<sema-test>", source), options).expect("parse");
    let target = fcc::sema::TargetProfile::for_march(march).unwrap();
    fcc::sema::analyze_with_target(ast, options, target).expect("sema")
}

pub fn fcc_context() -> tir::Context {
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<fcc::cir::CirDialect>();
    context
}

/// Run the frontend pipeline down to CIR under the default language options.
pub fn lower(source: &str) -> (tir::Context, tir::builtin::ModuleOp) {
    let options = Default::default();
    let unit = fcc::parser::parse(&lex("<test>", source), options).expect("parse");
    let unit = fcc::sema::analyze(unit, options).expect("sema");
    let context = fcc_context();
    let module = fcc::codegen::codegen(&context, &unit).expect("codegen");
    (context, module)
}

pub fn print_ir(module: &tir::builtin::ModuleOp) -> String {
    let mut out = String::new();
    let mut fmt = tir::IRFormatter::new(&mut out);
    tir::Operation::print(module, &mut fmt).expect("print");
    out
}

/// Lower `source` to CIR and print it.
pub fn compile_ir(source: &str) -> String {
    let (_context, module) = lower(source);
    print_ir(&module)
}
