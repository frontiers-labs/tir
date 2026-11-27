use tmdl::{Action, Compiler, OutputKind};

fn main() {
    tir_cdsl::compile([(
        "tir-unit-tests::*",
        tir_cdsl::OutputKind::OpDef("core_fixtures.rs"),
    )])
    .unwrap();

    let input = "tests/conditional-latency.tmdl";
    let output = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed={input}");
    for (action, name) in [
        (Action::EmitRust, "latency.rs"),
        (Action::EmitOperationList, "latency_ops.rs"),
    ] {
        Compiler::builder()
            .action(action)
            .dialect(Some("latency".to_string()))
            .output(OutputKind::File(format!("{output}/{name}")))
            .add_input(input)
            .build()
            .compile_with_rust_writer(|source, package| {
                Ok(tir_cdsl::CheckedProgram::emit_rust_source(source, package)?)
            })
            .unwrap();
    }
}
