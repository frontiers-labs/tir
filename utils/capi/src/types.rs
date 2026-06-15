//! Structured type construction. Types are built by dialect-qualified name from
//! typed arguments (no textual form), driven by the `TYPE_SCHEMAS` registry that
//! `#[derive(TirType)]` populates. The schema is also exposed as JSON so
//! language bindings can generate typed constructors.

use std::ffi::{CStr, c_char};

use tir::{TypeArg, TypeId};

use crate::{TIR_INVALID_ID, guard, into_cstring, set_error, with_context};

/// A single argument to [`tir_type_build`]. `kind` selects how `value` is read:
/// `TIR_TYPEARG_U32/U64/I64/BOOL` read it as that scalar; `TIR_TYPEARG_TYPE`
/// reads it as a type id.
#[repr(C)]
pub struct TirTypeArg {
    pub kind: i32,
    pub value: u64,
}

fn decode(arg: &TirTypeArg) -> Option<TypeArg> {
    Some(match arg.kind {
        0 => TypeArg::U32(arg.value as u32),
        1 => TypeArg::U64(arg.value),
        2 => TypeArg::I64(arg.value as i64),
        3 => TypeArg::Bool(arg.value != 0),
        4 => TypeArg::Type(TypeId::from_number(arg.value as u32)),
        _ => return None,
    })
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(p) }.to_str().ok()
    }
}

/// The schema of every registered type (dialect, name, typed params) as a JSON
/// array. Free with [`crate::tir_string_free`].
#[unsafe(no_mangle)]
pub extern "C" fn tir_type_schema_json() -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        into_cstring(tir::type_schema_json())
    })
}

/// Build the type `dialect.name` from `n` structured arguments. Returns its id,
/// or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle; `dialect` and `name` must be valid
/// NUL-terminated C strings; `args` must be null or readable for `n` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_type_build(
    ctx: *const tir::Context,
    dialect: *const c_char,
    name: *const c_char,
    args: *const TirTypeArg,
    n: usize,
) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let (Some(dialect), Some(name)) = (unsafe { cstr(dialect) }, unsafe { cstr(name) }) else {
            set_error("dialect and name must be valid non-null strings");
            return TIR_INVALID_ID;
        };
        let raw: &[TirTypeArg] = if n == 0 {
            &[]
        } else if args.is_null() {
            set_error("null args with nonzero count");
            return TIR_INVALID_ID;
        } else {
            unsafe { std::slice::from_raw_parts(args, n) }
        };
        let mut decoded = Vec::with_capacity(raw.len());
        for arg in raw {
            match decode(arg) {
                Some(v) => decoded.push(v),
                None => {
                    set_error(format!("invalid type argument kind {}", arg.kind));
                    return TIR_INVALID_ID;
                }
            }
        }
        match tir::build_type(ctx, dialect, name, &decoded) {
            Ok(ty) => ty.number(),
            Err(e) => {
                set_error(e);
                TIR_INVALID_ID
            }
        }
    })
}
