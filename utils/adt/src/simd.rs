//! Loops over `u32` columns, written by hand for each instruction set and
//! picked once at run time: AVX-512 and AVX2 on x86-64, SVE2 and NEON on
//! AArch64, the V extension on RISC-V, and plain Rust everywhere else.
//!
//! Nothing here relies on the compiler vectorizing a loop. The x86-64 and NEON
//! kernels use intrinsics. Stable Rust has no intrinsics for the scalable
//! instruction sets, so the SVE2 and V kernels are assembly; for the same
//! reason V is detected from the kernel's hardware capability word rather than
//! through the standard library.
//!
//! The functions take whole columns and leave row-at-a-time decisions to the
//! caller.

use std::sync::OnceLock;

#[cfg(target_arch = "aarch64")]
mod arm;
#[cfg(target_arch = "riscv64")]
mod riscv;
mod scalar;
#[cfg(target_arch = "x86_64")]
mod x86;

/// One instruction set's kernels. Every pointer is to `len` readable cells
/// (writable for a `*mut`), and the instruction set must be available.
pub(crate) struct Kernels {
    name: &'static str,
    /// First row holding `value`, or `len`.
    find_eq: unsafe fn(column: *const u32, len: usize, value: u32) -> usize,
    /// First row where the two columns agree, or `len`.
    find_same: unsafe fn(a: *const u32, b: *const u32, len: usize) -> usize,
    /// Largest cell, or zero for no cells.
    max: unsafe fn(column: *const u32, len: usize) -> u32,
    /// `flags[row] |= (map[column[row]] != column[row])`. Every cell indexes
    /// `map`.
    mark_moved: unsafe fn(column: *const u32, map: *const u32, flags: *mut u32, len: usize),
    /// `hash[row] = (hash[row] ^ column[row]) * HASH_MUL`.
    hash_mix: unsafe fn(hash: *mut u32, column: *const u32, len: usize),
    /// `hash[row] ^= hash[row] >> 15`.
    hash_finish: unsafe fn(hash: *mut u32, len: usize),
}

/// The instruction sets this CPU has kernels for, widest first; the plain Rust
/// ones are last and always present.
fn available() -> Vec<&'static Kernels> {
    let mut sets: Vec<&'static Kernels> = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512f") {
            sets.push(&x86::AVX512);
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            sets.push(&x86::AVX2);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("sve2") {
            sets.push(&arm::SVE2);
        }
        sets.push(&arm::NEON);
    }
    #[cfg(target_arch = "riscv64")]
    if riscv::has_vector() {
        sets.push(&riscv::RVV);
    }
    sets.push(&scalar::SCALAR);
    sets
}

fn kernels() -> &'static Kernels {
    static BEST: OnceLock<&'static Kernels> = OnceLock::new();
    BEST.get_or_init(|| available()[0])
}

/// The instruction set the kernels run on, for a report.
pub fn backend() -> &'static str {
    kernels().name
}

/// Indexes are gathered as 32-bit byte offsets on some instruction sets, so a
/// gathered table is limited to this many cells.
const MAX_GATHER: usize = 1 << 29;

fn select_eq_with(set: &Kernels, column: &[u32], value: u32, out: &mut Vec<u32>) {
    let mut at = 0;
    while at < column.len() {
        // SAFETY: the pointer and length name the rest of `column`.
        at += unsafe { (set.find_eq)(column.as_ptr().add(at), column.len() - at, value) };
        // Hits come in runs where they are not rare, and a run is cheaper to
        // walk than to find one vector scan at a time.
        while column.get(at) == Some(&value) {
            out.push(at as u32);
            at += 1;
        }
    }
}

/// Append to `out` the rows of `column` holding `value`, ascending.
pub fn select_eq(column: &[u32], value: u32, out: &mut Vec<u32>) {
    select_eq_with(kernels(), column, value, out);
}

fn select_same_with(set: &Kernels, a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    assert_eq!(a.len(), b.len(), "columns of one table");
    let mut at = 0;
    while at < a.len() {
        // SAFETY: both pointers name the rest of two columns of equal length.
        at += unsafe { (set.find_same)(a.as_ptr().add(at), b.as_ptr().add(at), a.len() - at) };
        if at < a.len() {
            out.push(at as u32);
            at += 1;
        }
    }
}

/// Append to `out` the rows where two columns of one table agree, ascending.
pub fn select_same(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    select_same_with(kernels(), a, b, out);
}

fn mark_moved_with(set: &Kernels, column: &[u32], map: &[u32], flags: &mut [u32]) {
    assert_eq!(column.len(), flags.len(), "a flag per row");
    assert!(map.len() <= MAX_GATHER, "the map is within gather range");
    // SAFETY: the pointer and length name `column`.
    let max = unsafe { (set.max)(column.as_ptr(), column.len()) };
    assert!(
        column.is_empty() || (max as usize) < map.len(),
        "every cell has an entry in the map"
    );
    // SAFETY: the three slices are as long as stated, and the check above put
    // every cell inside `map`.
    unsafe {
        (set.mark_moved)(
            column.as_ptr(),
            map.as_ptr(),
            flags.as_mut_ptr(),
            column.len(),
        )
    };
}

/// Set `flags[row]` to one where `map` sends the cell of `column` at `row`
/// somewhere else. Flags already set stay set, so several columns accumulate
/// into one "this row moved", which [`select_eq`] then lists. Panics if a cell
/// is outside `map`.
pub fn mark_moved(column: &[u32], map: &[u32], flags: &mut [u32]) {
    mark_moved_with(kernels(), column, map, flags);
}

const HASH_SEED: u32 = 0x811c_9dc5;
pub(crate) const HASH_MUL: u32 = 0x9e37_79b1;

/// The hash [`hash_rows`] gives the row whose cells are `cells`.
#[inline]
pub fn hash_row(cells: &[u32]) -> u32 {
    let mut hash = HASH_SEED;
    for &cell in cells {
        hash = (hash ^ cell).wrapping_mul(HASH_MUL);
    }
    hash ^ (hash >> 15)
}

fn hash_rows_with(set: &Kernels, columns: &[&[u32]], out: &mut [u32]) {
    out.fill(HASH_SEED);
    for column in columns {
        assert_eq!(column.len(), out.len(), "columns of one table");
        // SAFETY: both slices are `out.len()` long.
        unsafe { (set.hash_mix)(out.as_mut_ptr(), column.as_ptr(), out.len()) };
    }
    // SAFETY: the pointer and length name `out`.
    unsafe { (set.hash_finish)(out.as_mut_ptr(), out.len()) };
}

/// Hash every row of a table whose key columns are `columns`, into `out`.
pub fn hash_rows(columns: &[&[u32]], out: &mut [u32]) {
    hash_rows_with(kernels(), columns, out);
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn the_plain_kernels_are_always_available() {
        assert_eq!(available().last().map(|set| set.name), Some("scalar"));
    }

    proptest! {
        /// Every instruction set this machine has agrees with a model written
        /// without any kernel, at lengths on both sides of a vector.
        #[test]
        fn every_instruction_set_agrees_with_the_model(
            rows in prop::collection::vec((0u32..5, 0u32..5, any::<u32>()), 0..200),
            map in prop::collection::vec(0u32..5, 5),
        ) {
            let a: Vec<u32> = rows.iter().map(|row| row.0).collect();
            let b: Vec<u32> = rows.iter().map(|row| row.1).collect();
            let wide: Vec<u32> = rows.iter().map(|row| row.2).collect();
            let all = 0..rows.len() as u32;
            let equal: Vec<u32> = all.clone().filter(|&row| a[row as usize] == 3).collect();
            let same: Vec<u32> = all.clone().filter(|&row| a[row as usize] == b[row as usize]).collect();
            let moved: Vec<u32> = a.iter().zip(&b)
                .map(|(&x, &y)| u32::from(map[x as usize] != x || map[y as usize] != y))
                .collect();
            let hashes: Vec<u32> = rows.iter().map(|&(x, y, z)| hash_row(&[x, y, z])).collect();
            for set in available() {
                let mut found = Vec::new();
                select_eq_with(set, &a, 3, &mut found);
                prop_assert_eq!(&found, &equal, "select_eq on {}", set.name);

                found.clear();
                select_same_with(set, &a, &b, &mut found);
                prop_assert_eq!(&found, &same, "select_same on {}", set.name);

                let mut flags = vec![0; rows.len()];
                mark_moved_with(set, &a, &map, &mut flags);
                mark_moved_with(set, &b, &map, &mut flags);
                prop_assert_eq!(&flags, &moved, "mark_moved on {}", set.name);

                let mut out = vec![0; rows.len()];
                hash_rows_with(set, &[&a, &b, &wide], &mut out);
                prop_assert_eq!(&out, &hashes, "hash_rows on {}", set.name);

                // SAFETY: the pointer and length name `wide`.
                let max = unsafe { (set.max)(wide.as_ptr(), wide.len()) };
                prop_assert_eq!(max, wide.iter().copied().max().unwrap_or(0), "max on {}", set.name);
            }
        }
    }

    #[test]
    #[should_panic(expected = "every cell has an entry in the map")]
    fn mark_moved_refuses_a_cell_outside_the_map() {
        mark_moved(&[0, 5], &[0, 1], &mut [0, 0]);
    }
}
