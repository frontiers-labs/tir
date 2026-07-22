mod ast;
mod codegen;
mod diagnostic;
mod lexer;
mod parser;
mod sema;

use chumsky::span::SimpleSpan;

pub use ast::*;
pub use diagnostic::Diagnostic;
pub use lexer::{Token, lex};

pub type Span = SimpleSpan;
pub type Spanned<T> = (T, Span);

pub fn compile(source: &str) -> Result<File, Vec<Diagnostic>> {
    let (tokens, mut diagnostics) = lex(source);
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let (file, parse_diagnostics) = parser::parse(source.len(), &tokens);
    diagnostics.extend(parse_diagnostics);
    let Some(file) = file else {
        return Err(diagnostics);
    };
    diagnostics.extend(sema::analyze(&file));
    if diagnostics.is_empty() {
        Ok(file)
    } else {
        Err(diagnostics)
    }
}

pub fn compile_to_rust(source: &str) -> Result<String, Vec<Diagnostic>> {
    let file = compile(source)?;
    codegen::generate(&file)
}
