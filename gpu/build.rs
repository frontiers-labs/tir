use std::error::Error;
use std::path::Path;

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
        builder.build().compile_with_rust_writer(|source, package| {
            Ok(tir_cdsl::CheckedProgram::emit_rust_source(source, package)?)
        })
    };

    compile(Action::EmitRust, "ptx.rs")?;
    compile(Action::EmitOperationList, "ptx_ops.rs")?;

    build_spirv(Path::new(&out_dir))?;
    Ok(())
}

fn build_spirv(out_dir: &Path) -> Result<(), Box<dyn Error>> {
    let source = Path::new("src/spirv");
    for file in ["lib.cdsl", "manual.cdsl", "generated.cdsl"] {
        println!("cargo:rerun-if-changed={}", source.join(file).display());
    }
    let package = tir_cdsl::CheckedProgram::compile_for_rust(&source.join("lib.cdsl"))?;
    for module in ["manual", "generated"] {
        let rust = package.emit_rust_filter("gpu", &format!("gpu::{module}::*"))?;
        std::fs::write(out_dir.join(format!("spirv_{module}.rs")), rust)?;
    }
    Ok(())
}
