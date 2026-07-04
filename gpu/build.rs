use std::error::Error;

use tmdl::{Action, Compiler, OutputKind};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=defs");
    let compiler = Compiler::builder()
        .add_input("./defs/ptx/main.tmdl")
        .add_input("./defs/ptx/instructions.tmdl")
        .output(OutputKind::File(format!(
            "{}/ptx.rs",
            std::env::var("OUT_DIR")?
        )))
        .dialect(Some("ptx".to_string()))
        .action(Action::EmitRust)
        // PTX is a text pseudo-ISA: its instructions have assembly syntax but no
        // binary encoding.
        .text_only(true)
        .build();

    Ok(compiler.compile()?)
}
