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
    /// Write the rows holding `value` to `out`, ascending, and return how
    /// many there are. `out` has room for `len`.
    select_eq: unsafe fn(column: *const u32, len: usize, value: u32, out: *mut u32) -> usize,
    /// First row where the two columns agree, or `len`.
    find_same: unsafe fn(a: *const u32, b: *const u32, len: usize) -> usize,
    /// Write the rows whose cell is `map_len` or more, or which `map` sends
    /// somewhere else, to `out`, ascending, and return how many there are.
    /// `map` is `map_len` cells long, at most [`MAX_GATHER`]; `out` has room
    /// for `len`.
    select_moved: unsafe fn(
        column: *const u32,
        map: *const u32,
        map_len: usize,
        len: usize,
        out: *mut u32,
    ) -> usize,
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
    out.reserve(column.len());
    let len = out.len();
    // SAFETY: the pointer and length name `column`, and `out` has room for a
    // row per cell past its end; the kernel initialized the rows it counted.
    unsafe {
        let found = (set.select_eq)(
            column.as_ptr(),
            column.len(),
            value,
            out.as_mut_ptr().add(len),
        );
        out.set_len(len + found);
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

fn select_moved_with(set: &Kernels, column: &[u32], map: &[u32], out: &mut Vec<u32>) {
    assert!(map.len() <= MAX_GATHER, "the map is within gather range");
    out.reserve(column.len());
    let len = out.len();
    // SAFETY: the pointers and lengths name `column` and `map`, the kernel
    // reads `map` only below its length, and `out` has room for a row per
    // cell past its end; the kernel initialized the rows it counted.
    unsafe {
        let found = (set.select_moved)(
            column.as_ptr(),
            map.as_ptr(),
            map.len(),
            column.len(),
            out.as_mut_ptr().add(len),
        );
        out.set_len(len + found);
    }
}

/// Append to `out` the rows of `column` whose cell `map` sends somewhere else,
/// or which lie outside `map`, ascending.
pub fn select_moved(column: &[u32], map: &[u32], out: &mut Vec<u32>) {
    select_moved_with(kernels(), column, map, out);
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
            // `select_eq` appends.
            let equal: Vec<u32> = std::iter::once(7).chain(all.clone().filter(|&row| a[row as usize] == 3)).collect();
            let same: Vec<u32> = all.clone().filter(|&row| a[row as usize] == b[row as usize]).collect();
            // `wide` reaches past the map; `short` cuts the map below some cells.
            let short = &map[..3];
            let moved = |column: &[u32], map: &[u32]| (0..column.len() as u32)
                .filter(|&row| map.get(column[row as usize] as usize) != Some(&column[row as usize]))
                .collect::<Vec<u32>>();
            let all_moved = [&a, &wide].map(|column| [&map[..], short].map(|map| moved(column, map)));
            let hashes: Vec<u32> = rows.iter().map(|&(x, y, z)| hash_row(&[x, y, z])).collect();
            for set in available() {
                let mut found = vec![7];
                select_eq_with(set, &a, 3, &mut found);
                prop_assert_eq!(&found, &equal, "select_eq on {}", set.name);

                found.clear();
                select_same_with(set, &a, &b, &mut found);
                prop_assert_eq!(&found, &same, "select_same on {}", set.name);

                let found = [&a, &wide].map(|column| [&map[..], short].map(|map| {
                    let mut found = Vec::new();
                    select_moved_with(set, column, map, &mut found);
                    found
                }));
                prop_assert_eq!(found, all_moved.clone(), "select_moved on {}", set.name);

                let mut out = vec![0; rows.len()];
                hash_rows_with(set, &[&a, &b, &wide], &mut out);
                prop_assert_eq!(&out, &hashes, "hash_rows on {}", set.name);
            }
        }
    }
}
