#[path = "defs/relations/build.rs"]
mod relations;

use std::{error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    relations::compile(std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR")?))?;
    println!("cargo:rerun-if-changed=defs/eqsat.pdl");
    let library = tir_pdl::compile(include_str!("defs/eqsat.pdl"))
        .map_err(|errors| format!("eqsat PDL: {errors:?}"))?;
    let program = tir_pdl::emit_egglog(&library, "EqSat0")?;
    if program.obligations.iter().any(|obligation| {
        !matches!(
            obligation.rule.as_str(),
            "forward-integer" | "forward-pointer"
        )
    }) {
        return Err("default EqSat library has an unbound checked rule".into());
    }
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("eqsat-pdl.rs"), tir_pdl::file_to_rust(&library))?;
    Ok(())
}
