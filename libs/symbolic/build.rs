use std::{error::Error, path::PathBuf};
fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=src/ir-schema.egg");
    let mut engine = tir_egglog::EGraph::default();
    let commands = engine.parse_program("ir-schema.egg", include_str!("src/ir-schema.egg"))?;
    engine.run_program(commands)?;
    let path = PathBuf::from(std::env::var("OUT_DIR")?).join("ir-schema.bin");
    std::fs::write(path, engine.save_program_with_schema()?)?;
    Ok(())
}
