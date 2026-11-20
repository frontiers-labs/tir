use tmdl::{Action, Compiler, OutputKind};

fn main() {
    let fixtures = "tests/core-fixtures.cdsl";
    let input = "tests/conditional-latency.tmdl";
    let output = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed={fixtures}");
    let source = std::fs::read_to_string(fixtures).unwrap();
    let rust = tir_cdsl::CheckedProgram::emit_rust_source(&source, "tir-unit-tests").unwrap();
    std::fs::write(format!("{output}/core_fixtures.rs"), rust).unwrap();
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
