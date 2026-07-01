// Force the backend crates to be linked so their `register_target!` entries are
// included in the final binary; the target registry is otherwise their only user.
use tir_arm64 as _;
use tir_riscv as _;
use tir_x86_64 as _;

pub mod ast;
pub mod codegen;
pub mod diagnostics;
pub mod driver;
pub mod lexer;
pub mod parser;
pub mod preprocessor;

#[cfg(test)]
mod codegen_tests;
