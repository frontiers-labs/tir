use tir_cdsl::{Error, OutputKind};

fn main() -> Result<(), Error> {
    tir_cdsl::compile([("core::ir::*", OutputKind::Program("ir_program.rs"))])
}
