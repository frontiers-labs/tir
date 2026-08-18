//! Exercises backend target support: listing targets, registering a target's
//! dialects, and lowering builtin IR to a target dialect via the codegen pipeline.

use std::ffi::CString;

use tir_capi::*;

use super::support::{owned, parse_module};

const MODULE: &str = "module {\n  func.func @a(%0: !i32, %1: !i32) -> !i32 {\n    %2 = addi %0, %1 : !i32\n    func.return %2\n  }\n  module_end\n}";

#[test]
fn lists_supported_targets() {
    let targets = owned(tir_supported_targets()).unwrap();
    assert!(targets.contains("riscv64"), "got: {targets}");
}

#[test]
fn unknown_target_is_rejected() {
    let ctx = tir_context_create();
    let march = CString::new("nonsense").unwrap();
    let ok = unsafe {
        tir_context_register_target(ctx, march.as_ptr(), std::ptr::null(), std::ptr::null())
    };
    assert!(!ok);
    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn isel_lowers_to_target_dialect() {
    let ctx = tir_context_create();
    let module = parse_module(ctx, MODULE);
    assert_ne!(module, TIR_INVALID_ID);

    let march = CString::new("rv64i").unwrap();
    let ran = unsafe {
        tir_context_run_target_pipeline(
            ctx,
            module,
            march.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0, // TIR_STAGE_ISEL
        )
    };
    assert!(
        ran,
        "isel failed: {:?}",
        owned(unsafe { tir_op_to_string(ctx, module) })
    );

    let rendered = owned(unsafe { tir_op_to_string(ctx, module) }).unwrap();
    assert!(
        rendered.contains("riscv."),
        "expected riscv ops:\n{rendered}"
    );

    unsafe { tir_context_destroy(ctx) };
}
