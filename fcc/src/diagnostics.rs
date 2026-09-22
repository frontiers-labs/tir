//! The `fcc` diagnostic system: numbered, self-describing errors and warnings
//! rendered with [`ariadne`].

mod catalog;
mod diagnostic;
mod source;

pub use catalog::*;
pub use diagnostic::{Diagnostic, explain};
pub use source::{FileId, Span, file_source, intern_file};
