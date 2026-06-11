use std::error::Error;

use tmdl::{Action, Compiler, OutputKind};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=defs");
    let compiler = Compiler::builder()
        .add_input("./defs/main.tmdl")
        .add_input("./defs/base.tmdl")
        .add_input("./defs/multiplication.tmdl")
        .add_input("./defs/zicsr.tmdl")
        .add_input("./defs/perf.tmdl")
        .add_input("./defs/syntacore_scr1.tmdl")
        .output(OutputKind::File(format!(
            "{}/riscv.rs",
            std::env::var("OUT_DIR")?
        )))
        .dialect(Some("riscv".to_string()))
        .action(Action::EmitRust)
        .build();

    Ok(compiler.compile()?)
}
