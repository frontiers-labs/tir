//! Structured (non-textual) type construction and the type schema.

use std::ffi::{CStr, CString, c_char};

use tir_capi::*;

fn owned(raw: *mut c_char) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { tir_string_free(raw) };
    Some(s)
}

fn build(ctx: *const tir::Context, dialect: &str, name: &str, args: &[TirTypeArg]) -> u32 {
    let d = CString::new(dialect).unwrap();
    let n = CString::new(name).unwrap();
    unsafe { tir_type_build(ctx, d.as_ptr(), n.as_ptr(), args.as_ptr(), args.len()) }
}

#[test]
fn builds_scalar_and_parametric_types() {
    let ctx = tir_context_create();

    // builtin.i with a u32 width.
    let i32 = build(ctx, "builtin", "i", &[TirTypeArg { kind: 0, value: 32 }]);
    assert_ne!(i32, TIR_INVALID_ID);
    assert_eq!(
        owned(unsafe { tir_type_to_string(ctx, i32) }).unwrap(),
        "!i32"
    );

    // ptr.p parameterised by another type id.
    let ptr = build(
        ctx,
        "ptr",
        "p",
        &[TirTypeArg {
            kind: 4, // TIR_TYPEARG_TYPE
            value: i32 as u64,
        }],
    );
    assert_ne!(ptr, TIR_INVALID_ID);
    assert_eq!(
        owned(unsafe { tir_type_to_string(ctx, ptr) }).unwrap(),
        "!ptr.p<!i32>"
    );

    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn rejects_unknown_type_and_bad_args() {
    let ctx = tir_context_create();
    assert_eq!(build(ctx, "builtin", "nope", &[]), TIR_INVALID_ID);
    // Wrong arg kind for the width parameter.
    assert_eq!(
        build(ctx, "builtin", "i", &[TirTypeArg { kind: 4, value: 0 }]),
        TIR_INVALID_ID
    );
    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn schema_lists_types() {
    let json = owned(tir_type_schema_json()).unwrap();
    assert!(json.contains("\"name\":\"i\""), "{json}");
    assert!(json.contains("\"name\":\"p\""), "{json}");
    assert!(json.contains("\"kind\":\"u32\""), "{json}");
    assert!(json.contains("\"kind\":\"type\""), "{json}");
}
