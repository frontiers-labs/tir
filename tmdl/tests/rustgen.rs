use std::fs;

use tmdl::{Action, Compiler, OutputKind};

#[test]
fn split_input_emits_a_child_rust_module() {
    let dir = std::env::temp_dir().join(format!("tmdl-rustgen-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let shared = dir.join("shared.tmdl");
    let arithmetic = dir.join("arithmetic.tmdl");
    let output = dir.join("generated.rs");
    fs::write(
        &shared,
        r#"
isa TestIsa {}

register_class GPR for [TestIsa] {
    param ENCODING_LEN: Integer = 1;
    param WIDTH: Integer = 32;
    registers { r0..r1 => { traits = [] } }
}

template Binary for [TestIsa] {
    param MNEMONIC: String;
    operands { d: GPR, a: GPR }
    asm { "{self.MNEMONIC} {d}, {a}" }
}
"#,
    )
    .unwrap();
    fs::write(
        &arithmetic,
        r#"
instruction Add for [TestIsa] : Binary {
    param MNEMONIC: String = "add";
    behavior { todo(); }
}
"#,
    )
    .unwrap();

    Compiler::builder()
        .action(Action::EmitRust)
        .output(OutputKind::File(output.to_string_lossy().into_owned()))
        .dialect(Some("test".to_string()))
        .text_only(true)
        .add_input(shared.to_str().unwrap())
        .add_input(arithmetic.to_str().unwrap())
        .split_input(arithmetic.to_str().unwrap())
        .build()
        .compile()
        .unwrap();

    let root = fs::read_to_string(&output).unwrap();
    let child = fs::read_to_string(dir.join("arithmetic.rs")).unwrap();
    assert!(root.contains("mod arithmetic"));
    assert!(root.contains("include!(\"arithmetic.rs\")"));
    assert!(!root.contains("operation! {\n    AddOp"));
    assert!(child.contains("operation! {\n    AddOp"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn custom_assembly_omits_flat_instruction_parsers_and_printers() {
    let dir = std::env::temp_dir().join(format!("tmdl-rustgen-custom-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let input = dir.join("target.tmdl");
    let output = dir.join("generated.rs");
    fs::write(
        &input,
        r#"
isa TestIsa {}
register_class GPR for [TestIsa] {
    param ENCODING_LEN: Integer = 1;
    param WIDTH: Integer = 32;
    registers { r0..r1 => { traits = [] } }
}
template Unary for [TestIsa] {
    param MNEMONIC: String;
    operands { d: GPR }
    asm { "{self.MNEMONIC} {d}" }
}
instruction Add for [TestIsa] : Unary {
    param MNEMONIC: String = "add";
    behavior { todo(); }
}
"#,
    )
    .unwrap();

    Compiler::builder()
        .action(Action::EmitRust)
        .output(OutputKind::File(output.to_string_lossy().into_owned()))
        .dialect(Some("test".to_string()))
        .text_only(true)
        .custom_assembly(true)
        .add_input(input.to_str().unwrap())
        .build()
        .compile()
        .unwrap();

    let generated = fs::read_to_string(output).unwrap();
    assert!(generated.contains("pub fn asm_syntax()"));
    assert!(generated.contains("fn get_instruction_parsers("));
    assert!(!generated.contains("fn parse_add_inst"));
    assert!(!generated.contains("fn print_add_inst"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn split_output_rejects_stdout_before_compilation() {
    let error = Compiler::builder()
        .action(Action::EmitRust)
        .output(OutputKind::Stdout)
        .dialect(Some("test".to_string()))
        .add_input("missing.tmdl")
        .split_input("missing.tmdl")
        .build()
        .compile()
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "Code generation error: split Rust output cannot be written to stdout"
    );
}
