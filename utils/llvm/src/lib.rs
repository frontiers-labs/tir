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

/// Parse LLVM textual IR and lower it using the selected target's data layout.
/// With no target layout, pointer sizes default to 64 bits.
pub fn import_str(
    context: &Context,
    src: &str,
    data_layout: Option<&tir::attributes::AttributeValue>,
) -> Result<ModuleOp, Error> {
    import(context, &parse_module(src)?, data_layout)
}
