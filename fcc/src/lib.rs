pub mod ast;
pub mod cir;
pub mod codegen;
pub mod diagnostics;
pub mod driver;
pub mod lexer;
pub mod parser;
pub mod preprocessor;

#[cfg(test)]
mod codegen_tests;
