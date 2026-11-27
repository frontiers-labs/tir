mod ast;
mod btor2gen;
mod compiler;
mod encoding;
mod error;
mod expander;
mod fninline;
#[cfg(feature = "json")]
mod json;
mod lexer;
mod markdown;
mod module_names;
mod module_tree;
#[cfg(feature = "rust-cli")]
mod package;
mod parser;
mod rustgen;
mod sem_expr_state;
mod sema;
mod semgen;
pub mod shapes;
mod smtlibgen;
mod typeck;
mod types;
mod utils;
mod widths;

use chumsky::prelude::*;

pub type Span = SimpleSpan;
pub type Spanned<T> = (T, Span);

#[cfg(feature = "rust-cli")]
pub use compiler::build_backend;
pub use compiler::{Action, Compiler, OutputKind};
#[cfg(feature = "rust-cli")]
pub use package::{build_backend_package, package_main};
pub use smtlibgen::{
    EncodingFieldMetadata, EncodingShapeMetadata, FlatStateFieldMetadata, InstructionMetadata,
    MemoryAccessMetadata, OperandMetadata, RegisterClassMetadata, SmtMetadata,
};

pub use expander::{Diag, MacroTable, StringArena, collect_macros, expand};
pub use lexer::lex;
pub use parser::parse;
pub use sema::analyze as sema_analyze;
pub use typeck::check as type_check;
pub use types::*;
