//! Backend target support. Registering a target installs its dialects into a
//! context so the generic ABI can parse, build and inspect target-specific IR
//! and run target passes. Target passes and op schemas are linked in
//! automatically by depending on the backend crates directly.

use std::ffi::{CStr, c_char};

// Force the backend crates to be linked so their target, pass, and schema
// registrations are included in the cdylib.
use tir::backend::pipeline::{StopAfter, build_pipeline};
use tir::backend::{select_target, supported_targets};
use tir_arm64 as _;
use tir_riscv as _;
use tir_x86_64 as _;

use crate::{guard, into_cstring, op_instance, set_error, with_context};

unsafe fn opt_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(p) }.to_str().ok()
    }
}

/// Comma-separated list of supported backend target names (e.g. `riscv64`).
/// Free with [`crate::tir_string_free`].
#[unsafe(no_mangle)]
pub extern "C" fn tir_supported_targets() -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        into_cstring(supported_targets().join(","))
    })
}

/// Register the dialects of the target named by `march` (with optional `mcpu`
/// and `mattr`) into `ctx`, enabling target-specific IR. Returns false on error.
///
/// # Safety
/// `ctx` must be a valid context handle; the string arguments must each be null
/// or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_context_register_target(
    ctx: *const tir::Context,
    march: *const c_char,
    mcpu: *const c_char,
    mattr: *const c_char,
) -> bool {
    with_context(ctx, false, |ctx| {
        let Some(march) = (unsafe { opt_str(march) }) else {
            set_error("march must be a valid non-null string");
            return false;
        };
        match select_target(march, unsafe { opt_str(mcpu) }, unsafe { opt_str(mattr) }) {
            Ok(machine) => {
                machine.register_dialects(ctx);
                true
            }
            Err(e) => {
                set_error(e);
                false
            }
        }
    })
}

/// Lower the op `root` for the target named by `march` by registering the
/// target's dialects and running its codegen pipeline up to `stage`
/// (`TIR_STAGE_ISEL`, `TIR_STAGE_REGALLOC`, or `TIR_STAGE_FINALIZE`). Returns
/// false on error.
///
/// # Safety
/// `ctx` must be a valid context handle; the string arguments must each be null
/// or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_context_run_target_pipeline(
    ctx: *const tir::Context,
    root: u32,
    march: *const c_char,
    mcpu: *const c_char,
    mattr: *const c_char,
    stage: i32,
) -> bool {
    with_context(ctx, false, |ctx| {
        let Some(march) = (unsafe { opt_str(march) }) else {
            set_error("march must be a valid non-null string");
            return false;
        };
        let stop = match stage {
            0 => StopAfter::ISel,
            1 => StopAfter::RegAlloc,
            2 => StopAfter::Finalize,
            _ => {
                set_error("invalid stage (expected TIR_STAGE_ISEL/REGALLOC/FINALIZE)");
                return false;
            }
        };
        let target = match select_target(march, unsafe { opt_str(mcpu) }, unsafe { opt_str(mattr) })
        {
            Ok(t) => t,
            Err(e) => {
                set_error(e);
                return false;
            }
        };
        target.register_dialects(ctx);
        let Some(op) = op_instance(ctx, root) else {
            return false;
        };
        let mut pm = build_pipeline(target.as_ref(), ctx, stop);
        match pm.run(ctx, op) {
            Ok(()) => true,
            Err(e) => {
                set_error(format!("target pipeline failed: {e}"));
                false
            }
        }
    })
}
