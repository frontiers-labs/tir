//! End-to-end exercise of the C ABI: parse -> run pipeline -> print, plus the
//! error-reporting contract. Calls the `extern "C"` exports directly.

use std::ffi::CString;

use tir_capi::*;

use super::support::{last_error, parse_module, render};

const MODULE: &str = r#"
module {
  func.func @f(%0: !i32, %1: !i32) -> !i32 {
    %2 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    ptr.store %0, %2
    %3 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    ptr.store %1, %3
    %4 = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    %5 = ptr.load %2 : !i32
    %6 = ptr.load %3 : !i32
    %7 = muli %5, %6 : !i32
    ptr.store %7, %4
    %8 = ptr.load %4 : !i32
    %9 = constant {value = 1} : !i32
    %10 = addi %8, %9 : !i32
    func.return %10
  }
  module_end
}
"#;

#[test]
fn parse_run_pipeline_print_roundtrip() {
    let ctx = tir_context_create();
    assert!(!ctx.is_null());

    let module = parse_module(ctx, MODULE);
    assert_ne!(module, TIR_INVALID_ID, "parse failed: {}", last_error());

    let before = render(ctx, module);
    assert!(
        before.contains("ptr.alloca"),
        "expected allocas before promotion:\n{before}"
    );

    let spec = CString::new("func.func(restructure-nodes,promote-nodes),fixpoint<3>(func.func(verify-deps,instcombine-nodes))")
        .unwrap();
    let pm = unsafe { tir_pipeline_parse(spec.as_ptr()) };
    assert!(!pm.is_null(), "pipeline parse failed: {}", last_error());
    assert!(
        unsafe { tir_pipeline_run(pm, ctx, module) },
        "pipeline run failed: {}",
        last_error()
    );
    unsafe { tir_pipeline_destroy(pm) };

    let after = render(ctx, module);
    assert!(
        !after.contains("ptr.alloca"),
        "promotion should remove allocas:\n{after}"
    );
    let header = after
        .lines()
        .find(|line| line.contains("func.func @f("))
        .expect("the function prints");
    let ports: Vec<&str> = header
        .split(['(', ')'])
        .nth(1)
        .expect("ports")
        .split(", ")
        .map(|port| port.split(':').next().unwrap())
        .collect();
    assert!(
        after.contains(&format!("muli {}, {}", ports[0], ports[1])),
        "expected promoted operands:\n{after}"
    );

    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn parse_error_sets_last_error() {
    let ctx = tir_context_create();
    let id = parse_module(ctx, "this is not valid IR");
    assert_eq!(id, TIR_INVALID_ID);
    assert!(!last_error().is_empty());
    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn print_unknown_id_returns_null() {
    let ctx = tir_context_create();
    let raw = unsafe { tir_op_to_string(ctx, 9999) };
    assert!(raw.is_null());
    assert!(last_error().contains("9999"));
    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn unknown_pass_reports_available() {
    let spec = CString::new("func.func(definitely_not_a_pass)").unwrap();
    let pm = unsafe { tir_pipeline_parse(spec.as_ptr()) };
    assert!(pm.is_null());
    assert!(last_error().contains("available passes"));
}

#[test]
fn null_inputs_are_rejected() {
    assert_eq!(parse_module(std::ptr::null(), MODULE), TIR_INVALID_ID);
    assert!(unsafe { tir_op_to_string(std::ptr::null(), 0) }.is_null());
    assert!(!unsafe { tir_pipeline_run(std::ptr::null_mut(), std::ptr::null(), 0) });
}
