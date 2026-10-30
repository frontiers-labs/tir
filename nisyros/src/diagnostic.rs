//! Thread-local details for the most recent recorded backend failure.

use super::*;
use std::cell::Cell;

#[derive(Clone, Copy)]
struct LastDiagnostic {
    status: i32,
    backend_code: i32,
    message: &'static str,
}

std::thread_local! {
    static LAST: Cell<LastDiagnostic> = const { Cell::new(LastDiagnostic {
        status: SUCCESS,
        backend_code: 0,
        message: "",
    }) };
}

pub(super) fn set_backend_diagnostic(status: i32, backend_code: i32, message: &'static str) {
    LAST.with(|last| {
        last.set(LastDiagnostic {
            status,
            backend_code,
            message,
        })
    });
}

#[repr(C)]
pub struct DiagnosticInfo {
    header: DescriptorHeader,
    status: i32,
    backend_code: i32,
}

/// Reads the calling thread's most recent recorded backend failure.
///
/// # Safety
/// `out` must be complete, `bytes` writable, and nonnull `message` must hold
/// its input capacity of writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_diagnostic_get(
    out: *mut DiagnosticInfo,
    bytes: *mut u32,
    message: *mut c_char,
) -> i32 {
    if out.is_null() || bytes.is_null() {
        return INVALID_ARGUMENT;
    }
    let out = unsafe { &mut *out };
    let status = validate_header(&out.header, std::mem::size_of::<DiagnosticInfo>());
    if status != SUCCESS {
        return status;
    }
    let diagnostic = LAST.with(Cell::get);
    out.status = diagnostic.status;
    out.backend_code = diagnostic.backend_code;
    let required = diagnostic.message.len() as u32 + 1;
    let capacity = unsafe { *bytes };
    unsafe { *bytes = required };
    if message.is_null() {
        return SUCCESS;
    }
    if capacity != 0 {
        let copied = diagnostic.message.len().min(capacity as usize - 1);
        unsafe {
            ptr::copy_nonoverlapping(diagnostic.message.as_ptr(), message.cast::<u8>(), copied);
            *message.add(copied) = 0;
        }
    }
    if capacity < required {
        INCOMPLETE
    } else {
        SUCCESS
    }
}
