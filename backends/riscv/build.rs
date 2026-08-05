use std::error::Error;

use tmdl::{Action, Compiler, OutputKind};

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    let inputs = [
        "./defs/main.tmdl",
        "./defs/base.tmdl",
        "./defs/multiplication.tmdl",
        "./defs/float.tmdl",
        "./defs/compressed.tmdl",
        "./defs/atomics.tmdl",
        "./defs/zifencei.tmdl",
        "./defs/zicsr.tmdl",
        "./defs/perf.tmdl",
        "./defs/vector.tmdl",
        "./defs/vector_int.tmdl",
        "./defs/vector_mask.tmdl",
        "./defs/vector_red.tmdl",
        "./defs/vector_perm.tmdl",
        "./defs/vector_widen.tmdl",
        "./defs/vector_fixed.tmdl",
        "./defs/vector_mem.tmdl",
        "./defs/vector_float.tmdl",
        "./defs/syntacore_scr1.tmdl",
    ];
    for input in &inputs {
        println!("cargo:rerun-if-changed={input}");
    }
    let compile = |action, output| {
        let mut builder = Compiler::builder()
            .output(OutputKind::File(format!("{out_dir}/{output}")))
            .dialect(Some("riscv".to_string()))
            .action(action);
        for input in inputs {
            builder = builder.add_input(input);
        }
        builder.build().compile()
    };

    compile(Action::EmitRust, "riscv.rs")?;
    compile(Action::EmitOperationList, "riscv_ops.rs")?;

    Ok(())
}
