//! The kernels in plain Rust: the reference the others are tested against, and
//! what runs where no instruction set below applies.

use super::{HASH_MUL, Kernels};

pub(super) static SCALAR: Kernels = Kernels {
    name: "scalar",
    find_eq,
    find_same,
    max,
    mark_moved,
    hash_mix,
    hash_finish,
};

pub(super) unsafe fn find_eq(column: *const u32, len: usize, value: u32) -> usize {
    (0..len)
        .find(|&row| unsafe { *column.add(row) } == value)
        .unwrap_or(len)
}

pub(super) unsafe fn find_same(a: *const u32, b: *const u32, len: usize) -> usize {
    (0..len)
        .find(|&row| unsafe { *a.add(row) == *b.add(row) })
        .unwrap_or(len)
}

pub(super) unsafe fn max(column: *const u32, len: usize) -> u32 {
    (0..len)
        .map(|row| unsafe { *column.add(row) })
        .max()
        .unwrap_or(0)
}

pub(super) unsafe fn mark_moved(column: *const u32, map: *const u32, flags: *mut u32, len: usize) {
    for row in 0..len {
        unsafe {
            let cell = *column.add(row);
            *flags.add(row) |= u32::from(*map.add(cell as usize) != cell);
        }
    }
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
