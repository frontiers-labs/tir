use tir_cdsl::{Error, OutputKind};

fn main() -> Result<(), Error> {
    tir_cdsl::compile([("core::ir::*", OutputKind::Catalog("ir_catalog.rs"))])
}
