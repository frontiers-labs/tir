//! Generic read-only inspection of the IR graph: operations, their operands,
//! results, regions and attributes, plus region/block traversal, value types,
//! and type rendering. Every entity is addressed by its raw `u32` id relative
//! to the context, so this drives any dialect without per-op code.

use std::ffi::c_char;

use tir::attributes::AttributeValue;
use tir::{RegionId, TypeId, ValueId};

use crate::{TIR_INVALID_ID, into_cstring, op_instance, set_error, with_context};

/// Discriminant codes returned by [`tir_op_attribute_kind`]; kept in sync with
/// the `TIR_ATTR_*` macros injected into the generated header.
fn attr_kind_code(value: &AttributeValue) -> i32 {
    match value {
        AttributeValue::Str(_) => 0,
        AttributeValue::Int(_) => 1,
        AttributeValue::UInt(_) => 2,
        AttributeValue::F32(_) => 3,
        AttributeValue::F64(_) => 4,
        AttributeValue::Bool(_) => 5,
        AttributeValue::Array(_) => 6,
        AttributeValue::Dict(_) => 7,
        AttributeValue::Register(_) => 8,
        AttributeValue::Type(_) => 9,
        AttributeValue::Block(_) => 10,
    }
}

/// The `i`-th attribute value of op `op`, or `None` (with an error set) if the
/// op or index is invalid.
fn attr_value(ctx: &tir::Context, op: u32, i: usize) -> Option<AttributeValue> {
    let op = op_instance(ctx, op)?;
    match op.attributes.get(i) {
        Some(a) => Some(a.value.clone()),
        None => {
            set_error(format!("attribute index {i} out of range"));
            None
        }
    }
}

/// Name of op `op` as an owned C string, or null on error. Free with `tir_string_free`.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_name(ctx: *const tir::Context, op: u32) -> *mut c_char {
    with_context(ctx, std::ptr::null_mut(), |ctx| {
        op_instance(ctx, op).map_or(std::ptr::null_mut(), |o| into_cstring(o.name.to_string()))
    })
}

/// Dialect of op `op` as an owned C string, or null on error. Free with `tir_string_free`.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_dialect(ctx: *const tir::Context, op: u32) -> *mut c_char {
    with_context(ctx, std::ptr::null_mut(), |ctx| {
        op_instance(ctx, op).map_or(std::ptr::null_mut(), |o| {
            into_cstring(o.dialect.to_string())
        })
    })
}

/// Number of operands of op `op`, or `usize::MAX` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_num_operands(ctx: *const tir::Context, op: u32) -> usize {
    with_context(ctx, usize::MAX, |ctx| {
        op_instance(ctx, op).map_or(usize::MAX, |o| o.operands.len())
    })
}

/// The `i`-th operand value id of op `op`, or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_operand(ctx: *const tir::Context, op: u32, i: usize) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let Some(op) = op_instance(ctx, op) else {
            return TIR_INVALID_ID;
        };
        match op.operands.get(i) {
            Some(v) => v.number(),
            None => {
                set_error(format!("operand index {i} out of range"));
                TIR_INVALID_ID
            }
        }
    })
}

/// Number of results of op `op`, or `usize::MAX` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_num_results(ctx: *const tir::Context, op: u32) -> usize {
    with_context(ctx, usize::MAX, |ctx| {
        op_instance(ctx, op).map_or(usize::MAX, |o| o.results.len())
    })
}

/// The `i`-th result value id of op `op`, or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_result(ctx: *const tir::Context, op: u32, i: usize) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let Some(op) = op_instance(ctx, op) else {
            return TIR_INVALID_ID;
        };
        match op.results.get(i) {
            Some(v) => v.number(),
            None => {
                set_error(format!("result index {i} out of range"));
                TIR_INVALID_ID
            }
        }
    })
}

/// Number of regions of op `op`, or `usize::MAX` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_num_regions(ctx: *const tir::Context, op: u32) -> usize {
    with_context(ctx, usize::MAX, |ctx| {
        op_instance(ctx, op).map_or(usize::MAX, |o| o.regions.len())
    })
}

/// The `i`-th region id of op `op`, or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_region(ctx: *const tir::Context, op: u32, i: usize) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let Some(op) = op_instance(ctx, op) else {
            return TIR_INVALID_ID;
        };
        match op.regions.get(i) {
            Some(r) => r.number(),
            None => {
                set_error(format!("region index {i} out of range"));
                TIR_INVALID_ID
            }
        }
    })
}

/// Type id of value `value`, or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_value_type(ctx: *const tir::Context, value: u32) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let vid = ValueId::from_number(value);
        if !ctx.has_value(vid) {
            set_error(format!("no value with id {value}"));
            return TIR_INVALID_ID;
        }
        ctx.get_value(vid).ty().number()
    })
}

/// Number of blocks in region `region`, or `usize::MAX` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_region_num_blocks(ctx: *const tir::Context, region: u32) -> usize {
    with_context(ctx, usize::MAX, |ctx| {
        let rid = RegionId::from_number(region);
        if !ctx.has_region(rid) {
            set_error(format!("no region with id {region}"));
            return usize::MAX;
        }
        ctx.get_region(rid).iter(ctx.clone()).len()
    })
}

/// The `i`-th block id of region `region`, or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_region_block(ctx: *const tir::Context, region: u32, i: usize) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let rid = RegionId::from_number(region);
        if !ctx.has_region(rid) {
            set_error(format!("no region with id {region}"));
            return TIR_INVALID_ID;
        }
        match ctx.get_region(rid).iter(ctx.clone()).nth(i) {
            Some(block) => block.id().number(),
            None => {
                set_error(format!("block index {i} out of range"));
                TIR_INVALID_ID
            }
        }
    })
}

/// Number of operations in block `block`, or `usize::MAX` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_num_ops(ctx: *const tir::Context, block: u32) -> usize {
    with_context(ctx, usize::MAX, |ctx| {
        let bid = tir::BlockId::from_number(block);
        if !ctx.has_block(bid) {
            set_error(format!("no block with id {block}"));
            return usize::MAX;
        }
        ctx.get_block(bid).op_ids().len()
    })
}

/// The `i`-th operation id of block `block`, or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_op(ctx: *const tir::Context, block: u32, i: usize) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let bid = tir::BlockId::from_number(block);
        if !ctx.has_block(bid) {
            set_error(format!("no block with id {block}"));
            return TIR_INVALID_ID;
        }
        match ctx.get_block(bid).op_ids().get(i) {
            Some(op) => op.number(),
            None => {
                set_error(format!("op index {i} out of range"));
                TIR_INVALID_ID
            }
        }
    })
}

/// Number of block arguments of block `block`, or `usize::MAX` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_num_args(ctx: *const tir::Context, block: u32) -> usize {
    with_context(ctx, usize::MAX, |ctx| {
        let bid = tir::BlockId::from_number(block);
        if !ctx.has_block(bid) {
            set_error(format!("no block with id {block}"));
            return usize::MAX;
        }
        ctx.get_block(bid).arguments().len()
    })
}

/// The `i`-th block-argument value id of block `block`, or [`TIR_INVALID_ID`] on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_arg(ctx: *const tir::Context, block: u32, i: usize) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let bid = tir::BlockId::from_number(block);
        if !ctx.has_block(bid) {
            set_error(format!("no block with id {block}"));
            return TIR_INVALID_ID;
        }
        match ctx.get_block(bid).arguments().get(i) {
            Some(arg) => arg.id().number(),
            None => {
                set_error(format!("block argument index {i} out of range"));
                TIR_INVALID_ID
            }
        }
    })
}

/// Render type `ty` to an owned C string, or null on error. Free with `tir_string_free`.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_type_to_string(ctx: *const tir::Context, ty: u32) -> *mut c_char {
    with_context(ctx, std::ptr::null_mut(), |ctx| {
        into_cstring(ctx.type_to_string(TypeId::from_number(ty)))
    })
}

/// Number of attributes of op `op`, or `usize::MAX` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_num_attributes(ctx: *const tir::Context, op: u32) -> usize {
    with_context(ctx, usize::MAX, |ctx| {
        op_instance(ctx, op).map_or(usize::MAX, |o| o.attributes.len())
    })
}

/// Name of the `i`-th attribute of op `op` as an owned C string, or null on
/// error. Free with `tir_string_free`.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_name(
    ctx: *const tir::Context,
    op: u32,
    i: usize,
) -> *mut c_char {
    with_context(ctx, std::ptr::null_mut(), |ctx| {
        let Some(op) = op_instance(ctx, op) else {
            return std::ptr::null_mut();
        };
        match op.attributes.get(i) {
            Some(a) => into_cstring(a.name.clone()),
            None => {
                set_error(format!("attribute index {i} out of range"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Kind code of the `i`-th attribute of op `op` (see the `TIR_ATTR_*` macros),
/// or `-1` on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_kind(ctx: *const tir::Context, op: u32, i: usize) -> i32 {
    with_context(ctx, -1, |ctx| {
        attr_value(ctx, op, i).map_or(-1, |v| attr_kind_code(&v))
    })
}

/// Read a signed-int attribute into `*out`. Returns false if the attribute is a
/// different kind or on error.
///
/// # Safety
/// `ctx` must be a valid context handle; `out` must be null or a writable `int64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_int(
    ctx: *const tir::Context,
    op: u32,
    i: usize,
    out: *mut i64,
) -> bool {
    with_context(ctx, false, |ctx| match attr_value(ctx, op, i) {
        Some(AttributeValue::Int(v)) if !out.is_null() => {
            unsafe { *out = v };
            true
        }
        Some(_) => {
            set_error("attribute is not a signed integer");
            false
        }
        None => false,
    })
}

/// Read an unsigned-int attribute into `*out`. Returns false on kind mismatch or error.
///
/// # Safety
/// `ctx` must be a valid context handle; `out` must be null or a writable `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_uint(
    ctx: *const tir::Context,
    op: u32,
    i: usize,
    out: *mut u64,
) -> bool {
    with_context(ctx, false, |ctx| match attr_value(ctx, op, i) {
        Some(AttributeValue::UInt(v)) if !out.is_null() => {
            unsafe { *out = v };
            true
        }
        Some(_) => {
            set_error("attribute is not an unsigned integer");
            false
        }
        None => false,
    })
}

/// Read a floating-point attribute (`F32` or `F64`) into `*out`. Returns false
/// on kind mismatch or error.
///
/// # Safety
/// `ctx` must be a valid context handle; `out` must be null or a writable `double`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_float(
    ctx: *const tir::Context,
    op: u32,
    i: usize,
    out: *mut f64,
) -> bool {
    with_context(ctx, false, |ctx| {
        let value = match attr_value(ctx, op, i) {
            Some(AttributeValue::F32(v)) => v as f64,
            Some(AttributeValue::F64(v)) => v,
            Some(_) => {
                set_error("attribute is not a float");
                return false;
            }
            None => return false,
        };
        if out.is_null() {
            return false;
        }
        unsafe { *out = value };
        true
    })
}

/// Read a boolean attribute into `*out`. Returns false on kind mismatch or error.
///
/// # Safety
/// `ctx` must be a valid context handle; `out` must be null or a writable `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_bool(
    ctx: *const tir::Context,
    op: u32,
    i: usize,
    out: *mut bool,
) -> bool {
    with_context(ctx, false, |ctx| match attr_value(ctx, op, i) {
        Some(AttributeValue::Bool(v)) if !out.is_null() => {
            unsafe { *out = v };
            true
        }
        Some(_) => {
            set_error("attribute is not a bool");
            false
        }
        None => false,
    })
}

/// Value of a string attribute as an owned C string, or null on kind mismatch or
/// error. Free with `tir_string_free`.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_string(
    ctx: *const tir::Context,
    op: u32,
    i: usize,
) -> *mut c_char {
    with_context(ctx, std::ptr::null_mut(), |ctx| {
        match attr_value(ctx, op, i) {
            Some(AttributeValue::Str(s)) => into_cstring(s),
            Some(_) => {
                set_error("attribute is not a string");
                std::ptr::null_mut()
            }
            None => std::ptr::null_mut(),
        }
    })
}

/// Type id of a type attribute, or [`TIR_INVALID_ID`] on kind mismatch or error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_type(ctx: *const tir::Context, op: u32, i: usize) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| match attr_value(ctx, op, i) {
        Some(AttributeValue::Type(t)) => t.number(),
        Some(_) => {
            set_error("attribute is not a type");
            TIR_INVALID_ID
        }
        None => TIR_INVALID_ID,
    })
}

/// Block id of a block-reference attribute, or [`TIR_INVALID_ID`] on kind
/// mismatch or error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_op_attribute_block(
    ctx: *const tir::Context,
    op: u32,
    i: usize,
) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| match attr_value(ctx, op, i) {
        Some(AttributeValue::Block(b)) => b.number(),
        Some(_) => {
            set_error("attribute is not a block reference");
            TIR_INVALID_ID
        }
        None => TIR_INVALID_ID,
    })
}
