//! A prototype importer from LLVM textual IR into TIR.
//!
//! [`parse_module`] reads LLVM IR into a small [`ast`], and [`import`] lowers
//! that AST into a TIR module using the `builtin` and `ptr` dialects. Only the
//! instructions TIR can currently represent are converted; anything else is
//! reported as an [`Error`].

pub mod ast;
mod convert;
pub mod error;
mod lexer;
mod parser;

pub use convert::import;
pub use error::Error;
pub use parser::parse_module;

use tir::Context;
use tir::builtin::ModuleOp;

/// Parse LLVM textual IR and lower it to a TIR module in one step.
pub fn import_str(context: &Context, src: &str) -> Result<ModuleOp, Error> {
    import(context, &parse_module(src)?)
}
