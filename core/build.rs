use std::{collections::BTreeMap, error::Error, path::PathBuf};
use tir_cdsl::CheckedProgram;

struct RustOutput {
    filter: &'static str,
    destination: &'static str,
}

const RUST_OUTPUTS: &[RustOutput] = &[
    RustOutput {
        filter: "core::dialects::son::*",
        destination: "son_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::builtin::*",
        destination: "builtin.rs",
    },
    RustOutput {
        filter: "core::dialects::fp::*",
        destination: "fp.rs",
    },
    RustOutput {
        filter: "core::dialects::asm::*",
        destination: "asm_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::cfg::*",
        destination: "cfg_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::func::*",
        destination: "func_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::layout::*",
        destination: "layout_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::ptr::*",
        destination: "ptr_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::scf::*",
        destination: "scf_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::state::*",
        destination: "state_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::type_def::*",
        destination: "type_def_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::vector::*",
        destination: "vector_ops.rs",
    },
];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    println!("cargo:rerun-if-changed={}", manifest.join("defs").display());
    let output = PathBuf::from(std::env::var("OUT_DIR")?);
    let package = CheckedProgram::compile_for_rust(&manifest.join("defs/lib.cdsl"))?;
    let mut files = BTreeMap::new();
    for file in RUST_OUTPUTS {
        let std::collections::btree_map::Entry::Vacant(destination) = files.entry(file.destination)
        else {
            return Err(format!("duplicate Rust output {}", file.destination).into());
        };
        destination.insert(package.emit_rust_filter("core", file.filter)?);
    }
    for (destination, rust) in files {
        std::fs::write(output.join(destination), rust)?;
    }
    Ok(())
}
