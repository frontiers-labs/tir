mod ast;
mod btor2gen;
mod compiler;
mod encoding;
mod error;
mod expander;
mod fninline;
mod json;
mod lexer;
mod markdown;
mod parser;
mod rustgen;
mod sem_expr_state;
mod sema;
mod shapes;
mod smtlibgen;
mod typeck;
mod types;
mod utils;
mod widths;

use chumsky::prelude::*;

pub type Span = SimpleSpan;
pub type Spanned<T> = (T, Span);

pub use compiler::{Action, Compiler, OutputKind, compiler_main};

pub use expander::{Diag, MacroTable, StringArena, collect_macros, expand};
pub use lexer::lex;
pub use parser::parse;
pub use sema::analyze as sema_analyze;
pub use typeck::check as type_check;
pub use types::*;
