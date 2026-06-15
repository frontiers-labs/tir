//! Generic C ABI over the TIR core. The IR is uniform and resolves
//! dialects/ops/types by name at runtime, so this single surface drives every
//! dialect without per-op wrapper code.
//!
//! Ownership: `TirContext` and `TirPassManager` are heap handles freed with
//! their `*_destroy` functions. IR entities are addressed by their dense `u32`
//! ids (here `TirOpId`) relative to a context and need no freeing. Strings
//! returned as `char*` are owned by the caller and freed with
//! [`tir_string_free`] (except [`tir_last_error`], which the library owns).
//! Fallible calls return a sentinel ([`TIR_INVALID_ID`], `usize::MAX`, null, or
//! `false`) and set a thread-local message readable via [`tir_last_error`].

mod inspect;
mod mutate;
pub use inspect::*;
pub use mutate::*;

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Arc;

use tir::builtin::ModuleOp;
use tir::{Context, IRFormatter, OpId, OpInstance, Operation, PassManager};

/// Sentinel returned by id-producing functions on failure.
pub const TIR_INVALID_ID: u32 = u32::MAX;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

pub(crate) fn set_error(msg: impl Into<Vec<u8>>) {
    let c = CString::new(msg)
        .unwrap_or_else(|_| CString::new("TIR error message contained a NUL byte").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

/// Run `f`, converting any panic into a last-error and returning `default`.
pub(crate) fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_error("internal panic crossed the TIR FFI boundary");
            default
        }
    }
}

/// Borrow a context handle, or set an error and return `default` if null.
pub(crate) unsafe fn ctx_ref<'a, T>(
    ctx: *const Context,
    default: T,
    f: impl FnOnce(&'a Context) -> T,
) -> T {
    let Some(ctx) = (unsafe { ctx.as_ref() }) else {
        set_error("null TirContext passed to TIR FFI");
        return default;
    };
    f(ctx)
}

/// Common entry wrapper for context-bound calls: catches panics, clears the
/// last error, and borrows the context, returning `default` on any failure.
pub(crate) fn with_context<T: Copy>(
    ctx: *const Context,
    default: T,
    f: impl FnOnce(&Context) -> T,
) -> T {
    guard(default, || {
        clear_error();
        unsafe { ctx_ref(ctx, default, f) }
    })
}

/// Look up an operation by raw id, setting an error and returning `None` if the
/// id is unknown (e.g. erased). Avoids panicking on a dangling id.
pub(crate) fn op_instance(ctx: &Context, id: u32) -> Option<Arc<OpInstance>> {
    let op_id = OpId::from_number(id);
    if ctx.has_operation(op_id) {
        Some(ctx.get_op(op_id))
    } else {
        set_error(format!("no operation with id {id}"));
        None
    }
}

/// Convert an owned string into a heap C string for the caller to free, or null
/// (with an error set) if it contains an interior NUL.
pub(crate) fn into_cstring(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => {
            set_error("string contained a NUL byte");
            ptr::null_mut()
        }
    }
}

/// Read a `(ptr, len)` pair as `&str`, or set an error and return `None`.
pub(crate) unsafe fn str_from_raw<'a>(ptr: *const c_char, len: usize) -> Option<&'a str> {
    if ptr.is_null() {
        set_error("null string passed to TIR FFI");
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(_) => {
            set_error("string passed to TIR FFI was not valid UTF-8");
            None
        }
    }
}

/// Create a context preloaded with the default dialects (builtin, ptr, scf,
/// vector). Free with [`tir_context_destroy`].
#[unsafe(no_mangle)]
pub extern "C" fn tir_context_create() -> *mut Context {
    guard(ptr::null_mut(), || {
        clear_error();
        Box::into_raw(Box::new(Context::with_default_dialects()))
    })
}

/// Destroy a context created by [`tir_context_create`]. Null is ignored.
///
/// # Safety
/// `ctx` must be null or a context from [`tir_context_create`] not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_context_destroy(ctx: *mut Context) {
    guard((), || {
        if !ctx.is_null() {
            drop(unsafe { Box::from_raw(ctx) });
        }
    })
}

/// Message of the most recent failure on this thread, or null if none. The
/// pointer is valid until the next TIR call on the same thread.
#[unsafe(no_mangle)]
pub extern "C" fn tir_last_error() -> *const c_char {
    LAST_ERROR.with(|e| match &*e.borrow() {
        Some(c) => c.as_ptr(),
        None => ptr::null(),
    })
}

/// The schema of every registered operation as a JSON array (dialect, name,
/// operands, results, attributes, interfaces). Enables generating typed
/// language bindings without per-op code. Free with [`tir_string_free`].
#[unsafe(no_mangle)]
pub extern "C" fn tir_schema_json() -> *mut c_char {
    guard(ptr::null_mut(), || {
        clear_error();
        into_cstring(tir::schema_json())
    })
}

/// Free a string returned by the TIR FFI. Null is ignored.
///
/// # Safety
/// `s` must be null or a string returned by this library, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Parse a textual module from `src` (`len` bytes). Returns the module op id, or
/// [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context; `src` must be null or readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_parse_module(
    ctx: *const Context,
    src: *const c_char,
    len: usize,
) -> u32 {
    guard(TIR_INVALID_ID, || {
        clear_error();
        unsafe {
            ctx_ref(ctx, TIR_INVALID_ID, |ctx| {
                let Some(src) = str_from_raw(src, len) else {
                    return TIR_INVALID_ID;
                };
                match tir::parse::ir::parse_ir::<ModuleOp>(ctx, src) {
                    Ok(module) => module.id().number(),
                    Err((span, err)) => {
                        set_error(format!("parse failed at byte {}: {err:?}", span.0));
                        TIR_INVALID_ID
                    }
                }
            })
        }
    })
}

/// Render the op `id` to a newly allocated C string, or null on error. Free the
/// result with [`tir_string_free`].
///
/// # Safety
/// `ctx` must be a valid context from [`tir_context_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_to_string(ctx: *const Context, id: u32) -> *mut c_char {
    guard(ptr::null_mut(), || {
        clear_error();
        unsafe {
            ctx_ref(ctx, ptr::null_mut(), |ctx| {
                let Some(op) = op_instance(ctx, id) else {
                    return ptr::null_mut();
                };
                let mut rendered = String::new();
                let mut fmt = IRFormatter::new(&mut rendered);
                if let Err(e) = op.as_dyn_op().print(&mut fmt) {
                    set_error(format!("failed to print op: {e}"));
                    return ptr::null_mut();
                }
                into_cstring(rendered)
            })
        }
    })
}

/// Parse an MLIR-style pass pipeline (e.g. `builtin.func(mem2reg)`) from a
/// null-terminated string. Returns a manager handle, or null on error. Free
/// with [`tir_pipeline_destroy`].
///
/// # Safety
/// `spec` must be null or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_pipeline_parse(spec: *const c_char) -> *mut PassManager {
    guard(ptr::null_mut(), || {
        clear_error();
        if spec.is_null() {
            set_error("null pipeline spec passed to TIR FFI");
            return ptr::null_mut();
        }
        let spec = match unsafe { CStr::from_ptr(spec) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_error("pipeline spec was not valid UTF-8");
                return ptr::null_mut();
            }
        };
        match tir::parse_pipeline(spec) {
            Ok(pm) => Box::into_raw(Box::new(pm)),
            Err(e) => {
                set_error(format!(
                    "{e} (available passes: {})",
                    tir::registered_passes().join(", ")
                ));
                ptr::null_mut()
            }
        }
    })
}

/// Run a parsed pipeline over the op `root` in `ctx`. Returns false on error.
///
/// # Safety
/// `pm` must be a valid pipeline and `ctx` a valid context, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_pipeline_run(
    pm: *mut PassManager,
    ctx: *const Context,
    root: u32,
) -> bool {
    guard(false, || {
        clear_error();
        let Some(pm) = (unsafe { pm.as_mut() }) else {
            set_error("null TirPassManager passed to TIR FFI");
            return false;
        };
        unsafe {
            ctx_ref(ctx, false, |ctx| {
                let Some(op) = op_instance(ctx, root) else {
                    return false;
                };
                match pm.run(ctx, op) {
                    Ok(()) => true,
                    Err(e) => {
                        set_error(format!("pass pipeline failed: {e}"));
                        false
                    }
                }
            })
        }
    })
}

/// Destroy a pipeline created by [`tir_pipeline_parse`]. Null is ignored.
///
/// # Safety
/// `pm` must be null or a pipeline from [`tir_pipeline_parse`] not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_pipeline_destroy(pm: *mut PassManager) {
    guard((), || {
        if !pm.is_null() {
            drop(unsafe { Box::from_raw(pm) });
        }
    })
}
