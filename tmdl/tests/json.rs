use std::fs;

use tmdl::{Action, Compiler, OutputKind};

#[test]
fn emitted_json_validates_against_the_committed_schema() {
    validate_output("simple", "checks/Inputs/simple.tmdl", false);
}

#[test]
fn uncommon_expressions_validate_against_the_committed_schema() {
    validate_output("expressions", "checks/Inputs/atomics.tmdl", false);
}

#[test]
fn scheduling_models_validate_against_the_committed_schema() {
    validate_output("scheduling", "checks/Json/scheduling.tmdl", true);
}

#[test]
fn abi_declarations_validate_against_the_committed_schema() {
    validate_output("abi", "checks/Abi/riscv.tmdl", false);
}

fn validate_output(name: &str, input: &str, text_only: bool) {
    let output = std::env::temp_dir().join(format!("tmdl-json-{}-{name}.json", std::process::id()));
    let input = format!("{}/{input}", env!("CARGO_MANIFEST_DIR"));

    Compiler::builder()
        .action(Action::EmitAstJson)
        .output(OutputKind::File(output.to_string_lossy().into_owned()))
        .text_only(text_only)
        .add_input(&input)
        .build()
        .compile()
        .unwrap();

    let schema = serde_json::from_str(include_str!("../../docs/tmdl/ast-v1.schema.json")).unwrap();
    let instance = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();

    let _ = fs::remove_file(output);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
