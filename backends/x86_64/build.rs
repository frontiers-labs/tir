use std::error::Error;

use tmdl::{Action, Compiler, OutputKind};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=defs");
    let compiler = Compiler::builder()
        .add_input("./defs/main.tmdl")
        .add_input("./defs/base.tmdl")
        .add_input("./defs/arith_ext.tmdl")
        .add_input("./defs/conditional.tmdl")
        .add_input("./defs/memory_ext.tmdl")
        .add_input("./defs/float.tmdl")
        .output(OutputKind::File(format!(
            "{}/x86_64.rs",
            std::env::var("OUT_DIR")?
        )))
        .dialect(Some("x86_64".to_string()))
        .action(Action::EmitRust)
        .build();

    Ok(compiler.compile()?)
}
