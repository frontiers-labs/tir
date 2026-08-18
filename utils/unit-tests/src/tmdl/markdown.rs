use std::fs;

use tmdl::{Action, Compiler, OutputKind};

use super::support::fixture;

#[test]
fn markdown_batch_creates_an_index_and_instruction_family_page() {
    let output = std::env::temp_dir().join(format!("tmdl-markdown-{}", std::process::id()));
    let _ = fs::remove_dir_all(&output);

    Compiler::builder()
        .action(Action::EmitMarkdown)
        .dialect(Some("test".to_string()))
        .output(OutputKind::Batch(output.display().to_string()))
        .add_input(&fixture("checks/Inputs/simple.tmdl"))
        .build()
        .compile()
        .unwrap();

    let index = fs::read_to_string(output.join("index.md")).unwrap();
    let instructions = fs::read_to_string(output.join("simple.md")).unwrap();
    assert!(index.contains("# test ISA Reference"));
    assert!(index.contains("[Simple](./simple.md)"));
    assert!(instructions.contains("# Simple"));
    assert!(instructions.contains("### `add`"));

    fs::remove_dir_all(output).unwrap();
}
