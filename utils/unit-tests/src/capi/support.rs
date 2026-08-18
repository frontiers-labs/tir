//! Shared helpers for the C ABI tests: parse/print through the `extern "C"`
//! exports and the error-reporting contract.

use std::ffi::{c_char, CStr};

use tir_capi::*;

pub type Ctx = *const tir::Context;

pub fn parse_module(ctx: Ctx, src: &str) -> u32 {
    unsafe { tir_parse_module(ctx, src.as_ptr() as *const c_char, src.len()) }
}

pub fn parse_op_text(ctx: Ctx, text: &str) -> u32 {
    unsafe { tir_parse_op(ctx, text.as_ptr() as *const c_char, text.len()) }
}

/// Take ownership of a string returned over the ABI, freeing the C allocation.
pub fn owned(raw: *mut c_char) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { tir_string_free(raw) };
    Some(s)
}

pub fn render(ctx: Ctx, id: u32) -> String {
    let raw = unsafe { tir_op_to_string(ctx, id) };
    assert!(
        !raw.is_null(),
        "tir_op_to_string returned null: {}",
        last_error()
    );
    owned(raw).unwrap()
}

pub fn last_error() -> String {
    let p = tir_last_error();
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned()
    }
}
