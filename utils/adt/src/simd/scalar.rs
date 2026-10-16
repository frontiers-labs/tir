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
    intersect_sorted,
    and_words,
};

pub(super) unsafe fn select_eq(column: *const u32, len: usize, value: u32, out: *mut u32) -> usize {
    unsafe { select_eq_in(column, 0..len, value, out, 0) }
}

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

pub(super) unsafe fn intersect_sorted(
    a: *const u32,
    alen: usize,
    b: *const u32,
    blen: usize,
    out: *mut u32,
) -> usize {
    unsafe { intersect_sorted_within(a, alen, b, blen, out, alen.min(blen)) }
}

// The merge, writing at most `room` cells. A vector kernel hands it the rest of
// both lists with the room it has left, which keeps the kernel inside `out`
// even when the lists are not ascending.
pub(super) unsafe fn intersect_sorted_within(
    a: *const u32,
    alen: usize,
    b: *const u32,
    blen: usize,
    out: *mut u32,
    room: usize,
) -> usize {
    let (mut i, mut j, mut found) = (0, 0, 0);
    while i < alen && j < blen && found < room {
        let (x, y) = unsafe { (*a.add(i), *b.add(j)) };
        if x == y {
            unsafe { *out.add(found) = x };
            found += 1;
        }
        i += usize::from(x <= y);
        j += usize::from(y <= x);
    }
    found
}

pub(super) unsafe fn and_words(a: *mut u64, b: *const u64, len: usize) {
    for word in 0..len {
        unsafe { *a.add(word) &= *b.add(word) };
    }
}
