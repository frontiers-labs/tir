use std::error::Error;

use tmdl::{Action, Compiler, OutputKind};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=defs");
    let compiler = Compiler::builder()
        .add_input("./defs/main.tmdl")
        .add_input("./defs/scalar.tmdl")
        .add_input("./defs/perf.tmdl")
        .output(OutputKind::File(format!(
            "{}/x86.rs",
            std::env::var("OUT_DIR")?
        )))
        .dialect(Some("x86".to_string()))
        .action(Action::EmitRust)
        .build();

    Ok(compiler.compile()?)
}
