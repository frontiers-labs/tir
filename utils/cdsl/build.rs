use std::{env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../../libs/adt/src/apint.rs");
    println!("cargo:rerun-if-changed={}", source.display());
    let text = fs::read_to_string(source)?;
    let (implementation, _) = text
        .split_once("#[cfg(test)]")
        .ok_or("APInt test boundary changed")?;
    fs::write(
        PathBuf::from(env::var("OUT_DIR")?).join("apint.rs"),
        implementation,
    )?;
    Ok(())
}
