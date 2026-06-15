//! Generic IR construction and mutation. Operations are built textually via
//! [`tir_parse_op`] (operand refs resolve by numeric value id, so any dialect
//! works without per-op code); blocks and regions are created and edited
//! structurally. All entities are addressed by raw `u32` ids.

use std::ffi::c_char;
use std::sync::Arc;

use tir::{Block, BlockId, OpId, RegionId, TypeId};

use crate::{TIR_INVALID_ID, set_error, str_from_raw, with_context};

/// Fetch a block by raw id, or set an error and return `None`.
fn block_checked(ctx: &tir::Context, block: u32) -> Option<Arc<Block>> {
    let bid = BlockId::from_number(block);
    if ctx.has_block(bid) {
        Some(ctx.get_block(bid))
    } else {
        set_error(format!("no block with id {block}"));
        None
    }
}

/// Require that op `op` exists, setting an error otherwise.
fn op_exists(ctx: &tir::Context, op: u32) -> bool {
    if ctx.has_operation(OpId::from_number(op)) {
        true
    } else {
        set_error(format!("no operation with id {op}"));
        false
    }
}

/// Parse a single detached operation from `src` (`len` bytes), returning its id
/// or [`TIR_INVALID_ID`] on error. Operand references resolve by numeric value
/// id (e.g. `%5` is value 5), so operands can be wired to existing values; the
/// op's results get fresh value ids (queryable with `tir_op_result`). The op is
/// not placed in any block until inserted with `tir_block_append_op`.
///
/// # Safety
/// `ctx` must be a valid context handle; `src` must be null or readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_parse_op(
    ctx: *const tir::Context,
    src: *const c_char,
    len: usize,
) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let Some(src) = (unsafe { str_from_raw(src, len) }) else {
            return TIR_INVALID_ID;
        };
        match tir::parse::ir::parse_op(ctx, src) {
            Ok(op) => op.id().number(),
            Err((span, err)) => {
                set_error(format!("failed to parse op at byte {}: {err:?}", span.0));
                TIR_INVALID_ID
            }
        }
    })
}

/// Create an empty region, returning its id.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_region_create(ctx: *const tir::Context) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| ctx.create_region().id().number())
}

/// Create a block with `n` arguments of the given type ids, returning its id.
///
/// # Safety
/// `ctx` must be a valid context handle; `arg_types` must be null or readable
/// for `n` `uint32_t`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_create(
    ctx: *const tir::Context,
    arg_types: *const u32,
    n: usize,
) -> u32 {
    with_context(ctx, TIR_INVALID_ID, |ctx| {
        let types: &[u32] = if n == 0 {
            &[]
        } else if arg_types.is_null() {
            set_error("null arg_types with nonzero count");
            return TIR_INVALID_ID;
        } else {
            unsafe { std::slice::from_raw_parts(arg_types, n) }
        };
        let args = types
            .iter()
            .map(|&t| ctx.create_value(TypeId::from_number(t), None))
            .collect();
        ctx.create_block(args).id().number()
    })
}

/// Append block `block` to region `region`. Returns false on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_region_append_block(
    ctx: *const tir::Context,
    region: u32,
    block: u32,
) -> bool {
    with_context(ctx, false, |ctx| {
        let rid = RegionId::from_number(region);
        if !ctx.has_region(rid) {
            set_error(format!("no region with id {region}"));
            return false;
        }
        if block_checked(ctx, block).is_none() {
            return false;
        }
        ctx.get_region(rid).add_block(BlockId::from_number(block));
        true
    })
}

/// Append op `op` to the end of block `block`. Returns false on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_append_op(
    ctx: *const tir::Context,
    block: u32,
    op: u32,
) -> bool {
    with_context(ctx, false, |ctx| {
        let Some(block) = block_checked(ctx, block) else {
            return false;
        };
        if !op_exists(ctx, op) {
            return false;
        }
        block.insert(block.len(), OpId::from_number(op));
        true
    })
}

/// Insert op `op` into block `block` at `index`. Returns false on error,
/// including when `index` exceeds the block length.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_insert_op(
    ctx: *const tir::Context,
    block: u32,
    index: usize,
    op: u32,
) -> bool {
    with_context(ctx, false, |ctx| {
        let Some(block) = block_checked(ctx, block) else {
            return false;
        };
        if !op_exists(ctx, op) {
            return false;
        }
        if index > block.len() {
            set_error(format!("insert index {index} exceeds block length"));
            return false;
        }
        block.insert(index, OpId::from_number(op));
        true
    })
}

/// Remove op `op` from block `block`. Returns false if the op was not present
/// or on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_remove_op(
    ctx: *const tir::Context,
    block: u32,
    op: u32,
) -> bool {
    with_context(ctx, false, |ctx| {
        let Some(block) = block_checked(ctx, block) else {
            return false;
        };
        if block.remove_op(OpId::from_number(op)) {
            true
        } else {
            set_error(format!("op {op} is not in block {}", block.id().number()));
            false
        }
    })
}

/// Replace op `old` with op `new` in block `block`, preserving position.
/// Returns false if `old` was not present or on error.
///
/// # Safety
/// `ctx` must be a valid context handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tir_block_replace_op(
    ctx: *const tir::Context,
    block: u32,
    old: u32,
    new: u32,
) -> bool {
    with_context(ctx, false, |ctx| {
        let Some(block) = block_checked(ctx, block) else {
            return false;
        };
        if !op_exists(ctx, new) {
            return false;
        }
        if block.replace_op(OpId::from_number(old), OpId::from_number(new)) {
            true
        } else {
            set_error(format!("op {old} is not in block {}", block.id().number()));
            false
        }
    })
}
