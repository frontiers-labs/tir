use std::error::Error;
use std::path::{Path, PathBuf};
mod schema;

pub fn compile(core: &Path) -> Result<(), Box<dyn Error>> {
    let definitions = core.join("defs/relations");
    println!(
        "cargo:rerun-if-changed={}",
        definitions.join("build.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        definitions.join("schema.rs").display()
    );
    let (sources, dependencies) = tir_cdsl::SourceMap::from_file(definitions.join("schema.cdsl"))
        .map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    for dependency in dependencies {
        println!("cargo:rerun-if-changed={}", dependency.display());
    }
    let module = tir_cdsl::compile(&sources).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let entries = schema::ENTRIES;
    let schema = tir_cdsl::relations::emit_schema(&module, entries)?;
    let mut engine = tir_egglog::EGraph::default();
    engine.run_program(engine.parse_program("ir-schema.egg", &schema)?)?;
    let output = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(output.join("ir-schema.egg"), schema)?;
    std::fs::write(
        output.join("ir-schema.bin"),
        engine.save_program_with_schema()?,
    )?;
    Ok(())
}
