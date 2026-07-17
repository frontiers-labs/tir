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
