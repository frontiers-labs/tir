use std::fs;

use serde_json::Value;
use tmdl::{Action, Compiler, OutputKind};

use super::support::fixture;

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
    validate_output("fusion", "checks/Rust/fusion-guard.tmdl", true);
}

#[test]
fn abi_declarations_validate_against_the_committed_schema() {
    validate_output("abi", "checks/Abi/riscv.tmdl", false);
}

fn validate_output(name: &str, input: &str, text_only: bool) {
    let output = std::env::temp_dir().join(format!("tmdl-json-{}-{name}.json", std::process::id()));

    Compiler::builder()
        .action(Action::EmitAstJson)
        .output(OutputKind::File(output.to_string_lossy().into_owned()))
        .text_only(text_only)
        .add_input(&fixture(input))
        .build()
        .compile()
        .unwrap();

    let schema: Value =
        serde_json::from_str(include_str!("../../../../docs/tmdl/ast-v2.schema.json")).unwrap();
    let instance = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    let mut errors = Vec::new();
    validate(&schema, &schema, &instance, "$", &mut errors);

    let _ = fs::remove_file(output);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// Checks `instance` against `schema`, covering the JSON Schema keywords the
/// committed schema uses. An unknown keyword panics, so a schema that grows a
/// new one cannot pass unchecked.
fn validate(root: &Value, schema: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    let mut fail = |what: String| errors.push(format!("{path}: {what}"));
    let matches = |schema: &Value| {
        let mut errors = Vec::new();
        validate(root, schema, instance, path, &mut errors);
        errors.is_empty()
    };
    let mut nested = Vec::new();
    for (keyword, rule) in schema.as_object().unwrap() {
        match keyword.as_str() {
            // Annotations; `format` only names the Rust integer width.
            "$schema" | "$defs" | "title" | "description" | "format" => {}
            "$ref" => {
                let name = rule.as_str().unwrap().strip_prefix("#/$defs/").unwrap();
                nested.push((&root["$defs"][name], instance, path.to_string()));
            }
            "type" => {
                let is = |ty: &Value| match ty.as_str().unwrap() {
                    "null" => instance.is_null(),
                    "boolean" => instance.is_boolean(),
                    "integer" => instance.is_i64() || instance.is_u64(),
                    "string" => instance.is_string(),
                    "array" => instance.is_array(),
                    "object" => instance.is_object(),
                    other => panic!("unsupported type '{other}'"),
                };
                if !rule
                    .as_array()
                    .map_or_else(|| is(rule), |types| types.iter().any(is))
                {
                    fail(format!("expected type {rule}"));
                }
            }
            "const" => {
                if instance != rule {
                    fail(format!("expected {rule}"));
                }
            }
            "enum" => {
                if !rule.as_array().unwrap().contains(instance) {
                    fail(format!("expected one of {rule}"));
                }
            }
            "minimum" | "maximum" => {
                let integer = |v: &Value| v.as_i64().map(i128::from).or(v.as_u64().map(i128::from));
                if let Some(value) = integer(instance) {
                    let bound = integer(rule).unwrap();
                    if (keyword == "minimum" && value < bound)
                        || (keyword == "maximum" && value > bound)
                    {
                        fail(format!("{value} violates {keyword} {bound}"));
                    }
                }
            }
            "anyOf" => {
                if !rule.as_array().unwrap().iter().any(&matches) {
                    fail("matches no anyOf alternative".to_string());
                }
            }
            "oneOf" => {
                let count = rule
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|s| matches(s))
                    .count();
                if count != 1 {
                    fail(format!("matches {count} oneOf alternatives"));
                }
            }
            "required" => {
                if let Some(object) = instance.as_object() {
                    for key in rule.as_array().unwrap() {
                        if !object.contains_key(key.as_str().unwrap()) {
                            fail(format!("missing property {key}"));
                        }
                    }
                }
            }
            "properties" => {
                for (key, value) in instance.as_object().into_iter().flatten() {
                    if let Some(property) = rule.get(key) {
                        nested.push((property, value, format!("{path}.{key}")));
                    }
                }
            }
            "additionalProperties" => {
                assert_eq!(rule, &Value::Bool(false));
                for key in instance
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(key, _)| key)
                {
                    if schema
                        .get("properties")
                        .is_none_or(|known| known.get(key).is_none())
                    {
                        fail(format!("unexpected property '{key}'"));
                    }
                }
            }
            "items" => {
                for (index, item) in instance.as_array().into_iter().flatten().enumerate() {
                    nested.push((rule, item, format!("{path}[{index}]")));
                }
            }
            other => panic!("unsupported schema keyword '{other}'"),
        }
    }
    for (schema, instance, path) in nested {
        validate(root, schema, instance, &path, errors);
    }
}
