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
