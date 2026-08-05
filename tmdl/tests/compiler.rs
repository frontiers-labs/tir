use tmdl::{Action, Compiler, OutputKind};

#[test]
fn compiles_an_in_memory_source() {
    let output =
        std::env::temp_dir().join(format!("tmdl-embedded-source-{}.txt", std::process::id()));
    Compiler::builder()
        .action(Action::EmitTokens)
        .add_source("embedded.tmdl", "isa Test {}")
        .output(OutputKind::File(output.to_string_lossy().into_owned()))
        .build()
        .compile()
        .unwrap();

    let tokens = std::fs::read_to_string(&output).unwrap();
    assert!(tokens.contains("Isa"), "{tokens}");
    std::fs::remove_file(output).unwrap();
}

#[test]
fn invalid_source_fails_compilation() {
    let output =
        std::env::temp_dir().join(format!("tmdl-invalid-source-{}.rs", std::process::id()));
    let result = Compiler::builder()
        .action(Action::EmitRust)
        .dialect(Some("test".to_string()))
        .add_source("invalid.tmdl", "isa Test { found")
        .output(OutputKind::File(output.to_string_lossy().into_owned()))
        .build()
        .compile();

    assert!(result.is_err());
    let _ = std::fs::remove_file(output);
}
