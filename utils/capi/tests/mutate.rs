//! Exercises construction (textual op build + block/region creation) and
//! structural mutation (insert/remove), verified through inspection and print.

use std::ffi::{CStr, CString, c_char};

use tir_capi::*;

type Ctx = *const tir::Context;

fn parse_op_text(ctx: Ctx, text: &str) -> u32 {
    unsafe { tir_parse_op(ctx, text.as_ptr() as *const c_char, text.len()) }
}

fn render(ctx: Ctx, op: u32) -> String {
    let raw = unsafe { tir_op_to_string(ctx, op) };
    assert!(!raw.is_null());
    let s = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { tir_string_free(raw) };
    s
}

#[test]
fn build_block_from_scratch_wires_operands() {
    let ctx = tir_context_create();
    let builtin = CString::new("builtin").unwrap();
    let i = CString::new("i").unwrap();
    let args = [TirTypeArg { kind: 0, value: 32 }];
    let i32_ty =
        unsafe { tir_type_build(ctx, builtin.as_ptr(), i.as_ptr(), args.as_ptr(), args.len()) };
    assert_ne!(i32_ty, TIR_INVALID_ID);

    let region = unsafe { tir_region_create(ctx) };
    let arg_types = [i32_ty, i32_ty];
    let block = unsafe { tir_block_create(ctx, arg_types.as_ptr(), arg_types.len()) };
    assert!(unsafe { tir_region_append_block(ctx, region, block) });

    let a = unsafe { tir_block_arg(ctx, block, 0) };
    let b = unsafe { tir_block_arg(ctx, block, 1) };

    // Operand refs resolve by numeric value id, so the op wires to the args.
    let addi = parse_op_text(ctx, &format!("%0 = addi %{a}, %{b} : !i32"));
    assert_ne!(addi, TIR_INVALID_ID);
    assert!(unsafe { tir_block_append_op(ctx, block, addi) });

    assert_eq!(unsafe { tir_block_num_ops(ctx, block) }, 1);
    assert_eq!(unsafe { tir_block_op(ctx, block, 0) }, addi);
    assert_eq!(unsafe { tir_op_operand(ctx, addi, 0) }, a);
    assert_eq!(unsafe { tir_op_operand(ctx, addi, 1) }, b);

    // Chain a return on the addi result.
    let r = unsafe { tir_op_result(ctx, addi, 0) };
    let ret = parse_op_text(ctx, &format!("return %{r}"));
    assert!(unsafe { tir_block_append_op(ctx, block, ret) });
    assert_eq!(unsafe { tir_block_num_ops(ctx, block) }, 2);

    unsafe { tir_context_destroy(ctx) };
}

const MODULE: &str = r#"
module {
  func @f(%0: !i32) -> !i32 {
    %1 = addi %0, %0 : !i32
    return %1
  }
  module_end
}
"#;

fn func_body_block(ctx: Ctx, module: u32) -> u32 {
    let mr = unsafe { tir_op_region(ctx, module, 0) };
    let mb = unsafe { tir_region_block(ctx, mr, 0) };
    let func = unsafe { tir_block_op(ctx, mb, 0) };
    let fr = unsafe { tir_op_region(ctx, func, 0) };
    unsafe { tir_region_block(ctx, fr, 0) }
}

#[test]
fn insert_and_remove_in_parsed_module() {
    let ctx = tir_context_create();
    let module = unsafe { tir_parse_module(ctx, MODULE.as_ptr() as *const c_char, MODULE.len()) };
    assert_ne!(module, TIR_INVALID_ID);

    let body = func_body_block(ctx, module);
    let before = unsafe { tir_block_num_ops(ctx, body) };

    let constant = parse_op_text(ctx, "%0 = constant {value = 7} : !i32");
    assert_ne!(constant, TIR_INVALID_ID);
    assert!(unsafe { tir_block_insert_op(ctx, body, 0, constant) });
    assert_eq!(unsafe { tir_block_num_ops(ctx, body) }, before + 1);
    assert_eq!(unsafe { tir_block_op(ctx, body, 0) }, constant);
    assert!(render(ctx, module).contains("value = 7"));

    assert!(unsafe { tir_block_remove_op(ctx, body, constant) });
    assert_eq!(unsafe { tir_block_num_ops(ctx, body) }, before);
    assert!(!render(ctx, module).contains("value = 7"));

    unsafe { tir_context_destroy(ctx) };
}

#[test]
fn mutation_error_paths() {
    let ctx = tir_context_create();
    // Inserting an unknown op id fails.
    let block = unsafe { tir_block_create(ctx, std::ptr::null(), 0) };
    assert!(!unsafe { tir_block_append_op(ctx, block, 9999) });
    // Removing an op that isn't in the block fails.
    assert!(!unsafe { tir_block_remove_op(ctx, block, 9999) });
    // Parsing nonsense reports an error.
    assert_eq!(parse_op_text(ctx, "%% not an op"), TIR_INVALID_ID);
    unsafe { tir_context_destroy(ctx) };
}
