use std::{error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let source = "src/cir.cdsl";
    println!("cargo:rerun-if-changed={source}");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    let rust =
        tir_cdsl::CheckedProgram::emit_rust_source(&std::fs::read_to_string(source)?, "fcc")?;
    std::fs::write(output.join("cir.rs"), rust)?;
    Ok(())
}
