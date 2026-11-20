use std::{error::Error, path::PathBuf};
use tir_cdsl::{Schema, ast::Declaration};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let source = manifest.join("src/ir.cdsl");
    println!("cargo:rerun-if-changed={}", source.display());
    let checked = tir_cdsl::compile_file(&source)?.lower()?;
    let mut names = Schema::ir_names(&[]);
    for module in &checked.artifact().modules {
        if module.path == ["tables"] {
            for declaration in &module.ast.declarations {
                if let Declaration::Axiom { name, .. } = &declaration.value {
                    names.insert(vec!["tables".into(), name.clone()], name.clone());
                }
            }
        }
    }
    let schema = Schema::with_names(&checked, names)?;
    let output = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(output.join("ir-schema.bin"), schema.to_bytes()?)?;
    std::fs::write(output.join("ir-rules.bin"), schema.compile_bytes(&checked)?)?;
    Ok(())
}
