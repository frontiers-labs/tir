use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    tir_cdsl::compile([("fcc::*", tir_cdsl::OutputKind::OpDef("cir.rs"))])?;
    Ok(())
}
