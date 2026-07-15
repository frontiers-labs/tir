use std::error::Error;

use tmdl::{Action, Compiler, OutputKind};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=defs");
    let out_dir = std::env::var("OUT_DIR")?;
    let inputs = [
        "./defs/ptx/main.tmdl",
        "./defs/ptx/versions.tmdl",
        "./defs/ptx/integer.tmdl",
        "./defs/ptx/logic.tmdl",
        "./defs/ptx/float.tmdl",
        "./defs/ptx/compare.tmdl",
        "./defs/ptx/movement.tmdl",
        "./defs/ptx/memory.tmdl",
        "./defs/ptx/control.tmdl",
        "./defs/ptx/sync.tmdl",
        "./defs/ptx/video.tmdl",
        "./defs/ptx/async.tmdl",
        "./defs/ptx/tensor.tmdl",
        "./defs/ptx/texture.tmdl",
    ];
    let split_inputs = &inputs[2..];
    let compile = |action, output| {
        let mut builder = Compiler::builder()
            .output(OutputKind::File(format!("{out_dir}/{output}")))
            .dialect(Some("ptx".to_string()))
            .action(action)
            // PTX is a text pseudo-ISA: its instructions have assembly syntax but no
            // binary encoding.
            .text_only(true)
            .custom_assembly(true);
        for input in inputs {
            builder = builder.add_input(input);
        }
        if matches!(action, Action::EmitRust) {
            for input in split_inputs {
                builder = builder.split_input(input);
            }
        }
        builder.build().compile()
    };

    compile(Action::EmitRust, "ptx.rs")?;
    compile(Action::EmitOperationList, "ptx_ops.rs")?;

    Ok(())
}
