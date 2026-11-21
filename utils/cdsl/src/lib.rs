//! CDSL parsing, HM elaboration and checked construction descriptions.
//!
//! The HM kernel is extracted and checked against its general Lean specification.
//! Source contract elaboration does not constitute a mechanized dependent-type soundness
//! proof or certify a subsequently mutated host IR revision.

extern crate proc_macro;

pub mod ast;
mod build_script;
mod compiler;
mod engine;
mod error;
pub mod format;
pub mod hm;
pub mod infer;
mod kind;
mod layout;
pub mod lexer;
mod lower;
mod normalize;
pub mod parser;
mod parser_syntax;
mod rust;
pub mod syntax;
mod totality;

pub use build_script::{OutputKind, compile};
pub use compiler::{
    CompiledModule, Program, compile_file, compile_source, compile_tokens, resolve_crate,
    resolve_program_package,
};
pub use engine::{LoweredProgram, Schema};
pub use error::Error;
pub use lower::{
    CheckedMethods, CheckedOpInterfaces, CheckedProgram, InterfaceId, OperationKindId,
};
