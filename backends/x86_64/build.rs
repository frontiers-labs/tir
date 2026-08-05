use std::error::Error;

use tmdl::{Action, Compiler, OutputKind};

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    let inputs = [
        "defs/main.tmdl",
        "defs/base.tmdl",
        "defs/arith_ext.tmdl",
        "defs/conditional.tmdl",
        "defs/memory_ext.tmdl",
        "defs/atomics.tmdl",
        "defs/ordering.tmdl",
        "defs/float.tmdl",
        "defs/perf.tmdl",
        "defs/cpu/intel/tiger_lake.tmdl",
    ];
    for input in &inputs {
        println!("cargo:rerun-if-changed={input}");
    }
    let compile = |action, output| {
        let mut builder = Compiler::builder()
            .output(OutputKind::File(format!("{out_dir}/{output}")))
            .dialect(Some("x86_64".to_string()))
            .action(action);
        for input in inputs {
            builder = builder.add_input(input);
        }
        builder.build().compile()
    };

    compile(Action::EmitRust, "x86_64.rs")?;
    compile(Action::EmitOperationList, "x86_64_ops.rs")?;

    Ok(())
}
