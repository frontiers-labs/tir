//! Validates the operation schema exposed over the C ABI: it must be valid JSON
//! covering every default dialect, with faithful per-op detail.

use std::ffi::CStr;

use serde_json::Value;
use tir_capi::*;

fn schema() -> Value {
    let raw = tir_schema_json();
    assert!(!raw.is_null());
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { tir_string_free(raw) };
    serde_json::from_str(&json).expect("schema must be valid JSON")
}

#[test]
fn covers_default_dialects() {
    let ops = schema();
    let ops = ops.as_array().unwrap();
    assert!(!ops.is_empty());

    let dialects: std::collections::HashSet<&str> =
        ops.iter().map(|o| o["dialect"].as_str().unwrap()).collect();
    for d in ["builtin", "ptr", "scf", "vector"] {
        assert!(dialects.contains(d), "schema missing dialect `{d}`");
    }
}

#[test]
fn describes_addi_faithfully() {
    let ops = schema();
    let addi = ops
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["dialect"] == "builtin" && o["name"] == "addi")
        .expect("builtin.addi should be in the schema");

    let operands = addi["operands"].as_array().unwrap();
    assert_eq!(operands.len(), 2);
    assert_eq!(operands[0]["name"], "lhs");
    assert_eq!(operands[1]["name"], "rhs");
    assert_eq!(addi["results"].as_array().unwrap().len(), 1);

    let interfaces: Vec<&str> = addi["interfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(interfaces.contains(&"Commutative"));
    assert!(interfaces.contains(&"SameOperandType"));
}
