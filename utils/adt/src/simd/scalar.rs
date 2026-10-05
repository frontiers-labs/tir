//! The kernels in plain Rust: the reference the others are tested against, and
//! what runs where no instruction set below applies.

use super::{HASH_MUL, Kernels};

pub(super) static SCALAR: Kernels = Kernels {
    name: "scalar",
    select_eq,
    find_same,
    select_moved,
    hash_mix,
    hash_finish,
};

pub(super) unsafe fn select_eq(column: *const u32, len: usize, value: u32, out: *mut u32) -> usize {
    unsafe { select_eq_in(column, 0..len, value, out, 0) }
}

/// [`select_eq`] over `rows` alone, writing from `out[found]` on; returns
/// the new count.
pub(super) unsafe fn select_eq_in(
    column: *const u32,
    rows: std::ops::Range<usize>,
    value: u32,
    out: *mut u32,
    mut found: usize,
) -> usize {
    for row in rows {
        if unsafe { *column.add(row) } == value {
            unsafe { *out.add(found) = row as u32 };
            found += 1;
        }
    }
    found
}

pub(super) unsafe fn find_same(a: *const u32, b: *const u32, len: usize) -> usize {
    (0..len)
        .find(|&row| unsafe { *a.add(row) == *b.add(row) })
        .unwrap_or(len)
}

pub(super) unsafe fn select_moved(
    column: *const u32,
    map: *const u32,
    map_len: usize,
    len: usize,
    out: *mut u32,
) -> usize {
    unsafe { select_moved_in(column, map, map_len, 0..len, out, 0) }
}

/// [`select_moved`] over `rows` alone, writing from `out[found]` on; returns
/// the new count.
pub(super) unsafe fn select_moved_in(
    column: *const u32,
    map: *const u32,
    map_len: usize,
    rows: std::ops::Range<usize>,
    out: *mut u32,
    mut found: usize,
) -> usize {
    for row in rows {
        let cell = unsafe { *column.add(row) };
        if cell as usize >= map_len || unsafe { *map.add(cell as usize) } != cell {
            unsafe { *out.add(found) = row as u32 };
            found += 1;
        }
    }
    found
}

pub(super) unsafe fn hash_mix(hash: *mut u32, column: *const u32, len: usize) {
    for row in 0..len {
        unsafe { *hash.add(row) = (*hash.add(row) ^ *column.add(row)).wrapping_mul(HASH_MUL) };
    }
}

pub(super) unsafe fn hash_finish(hash: *mut u32, len: usize) {
    for row in 0..len {
        unsafe { *hash.add(row) ^= *hash.add(row) >> 15 };
    }
}
