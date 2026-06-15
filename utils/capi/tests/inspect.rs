//! Exercises the read-only inspection surface by traversing a parsed module.

use std::ffi::{CStr, c_char};

use tir_capi::*;

const MODULE: &str = r#"
module {
  func @f(%0: !i32, %1: !i32) -> !i32 {
    %2 = muli %0, %1 : !i32
    %3 = constant {value = 1} : !i32
    %4 = addi %2, %3 : !i32
    return %4
  }
  module_end
}
"#;

type Ctx = *const tir::Context;

fn parse(ctx: Ctx) -> u32 {
    unsafe { tir_parse_module(ctx, MODULE.as_ptr() as *const c_char, MODULE.len()) }
}

fn owned(raw: *mut c_char) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { tir_string_free(raw) };
    Some(s)
}

fn name(ctx: Ctx, op: u32) -> String {
    owned(unsafe { tir_op_name(ctx, op) }).unwrap()
}

/// Depth-first collection of every op id reachable from `op`.
fn collect(ctx: Ctx, op: u32, out: &mut Vec<u32>) {
    out.push(op);
    for r in 0..unsafe { tir_op_num_regions(ctx, op) } {
        let region = unsafe { tir_op_region(ctx, op, r) };
        for b in 0..unsafe { tir_region_num_blocks(ctx, region) } {
            let block = unsafe { tir_region_block(ctx, region, b) };
            for o in 0..unsafe { tir_block_num_ops(ctx, block) } {
                collect(ctx, unsafe { tir_block_op(ctx, block, o) }, out);
            }
        }
    }
}

fn find(ctx: Ctx, root: u32, op_name: &str) -> u32 {
    let mut ops = Vec::new();
    collect(ctx, root, &mut ops);
    *ops.iter()
        .find(|&&o| name(ctx, o) == op_name)
        .unwrap_or_else(|| panic!("no `{op_name}` op found"))
}

#[test]
fn traverses_structure_and_operands() {
    let ctx = tir_context_create();
    let module = parse(ctx);
    assert_ne!(module, TIR_INVALID_ID);

    assert_eq!(name(ctx, module), "module");
    assert_eq!(
        owned(unsafe { tir_op_dialect(ctx, module) }).unwrap(),
        "builtin"
    );
    assert_eq!(unsafe { tir_op_num_regions(ctx, module) }, 1);

    let muli = find(ctx, module, "muli");
    assert_eq!(unsafe { tir_op_num_operands(ctx, muli) }, 2);
    assert_eq!(unsafe { tir_op_num_results(ctx, muli) }, 1);

    // The result's type renders back to `!i32`.
    let result = unsafe { tir_op_result(ctx, muli, 0) };
    let ty = unsafe { tir_value_type(ctx, result) };
    assert_ne!(ty, TIR_INVALID_ID);
    assert_eq!(
        owned(unsafe { tir_type_to_string(ctx, ty) }).unwrap(),
        "!i32"
    );

    // addi's first operand is muli's result (SSA def-use through the ABI).
    let addi = find(ctx, module, "addi");
    assert_eq!(unsafe { tir_op_operand(ctx, addi, 0) }, result);

    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn reads_attributes() {
    let ctx = tir_context_create();
    let module = parse(ctx);
    let constant = find(ctx, module, "constant");

    let n = unsafe { tir_op_num_attributes(ctx, constant) };
    assert!(n >= 1);

    let idx = (0..n)
        .find(|&i| {
            owned(unsafe { tir_op_attribute_name(ctx, constant, i) }).as_deref() == Some("value")
        })
        .expect("constant should carry a `value` attribute");

    let kind = unsafe { tir_op_attribute_kind(ctx, constant, idx) };
    // `1` parses as a signed integer attribute.
    assert_eq!(kind, 1, "expected TIR_ATTR_INT");
    let mut v: i64 = 0;
    assert!(unsafe { tir_op_attribute_int(ctx, constant, idx, &mut v) });
    assert_eq!(v, 1);

    // Reading it as the wrong kind fails without crashing.
    let mut b = false;
    assert!(!unsafe { tir_op_attribute_bool(ctx, constant, idx, &mut b) });

    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn invalid_ids_report_errors() {
    let ctx = tir_context_create();
    assert_eq!(unsafe { tir_op_num_operands(ctx, 9999) }, usize::MAX);
    assert_eq!(unsafe { tir_op_operand(ctx, 9999, 0) }, TIR_INVALID_ID);
    assert_eq!(unsafe { tir_region_num_blocks(ctx, 9999) }, usize::MAX);
    assert_eq!(unsafe { tir_value_type(ctx, 9999) }, TIR_INVALID_ID);
    assert!(unsafe { tir_op_name(ctx, 9999) }.is_null());
    unsafe { tir_context_destroy(ctx) };
}
