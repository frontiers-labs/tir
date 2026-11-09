//! An isolated compiler and pure evaluator for CDSL source modules.
pub mod ast;
mod check;
pub mod compiler;
pub mod core;
pub mod diagnostic;
mod eval;
mod lexer;
#[cfg(feature = "rust-output")]
pub mod output;
pub mod parser;
pub mod relations;
mod resolve;
pub mod types;

pub use compiler::{CheckedModule, SourceMap, compile, compile_with_requirements};
pub use eval::{EvaluationLimits, Value};
