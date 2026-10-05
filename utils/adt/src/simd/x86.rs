//! AVX2 and AVX-512 kernels. Each loop handles whole vectors and hands the
//! remainder to the plain kernel, except where AVX-512 masks the last vector.

use std::arch::x86_64::*;

use super::{HASH_MUL, Kernels, scalar};

/// Write `row` plus the position of each bit set in `hits` to `out` from
/// `found` on, and return the new count. Hits are rare, so a loop over bits
/// beats packing them with a vector instruction.
#[inline(always)]
unsafe fn push_hits(mut hits: u64, row: usize, out: *mut u32, mut found: usize) -> usize {
    while hits != 0 {
        unsafe { *out.add(found) = (row + hits.trailing_zeros() as usize) as u32 };
        found += 1;
        hits &= hits - 1;
    }
    found
}

pub(super) static AVX2: Kernels = Kernels {
    name: "avx2",
    select_eq: avx2::select_eq,
    find_same: avx2::find_same,
    select_moved: avx2::select_moved,
    hash_mix: avx2::hash_mix,
    hash_finish: avx2::hash_finish,
};

pub(super) static AVX512: Kernels = Kernels {
    name: "avx512",
    select_eq: avx512::select_eq,
    find_same: avx512::find_same,
    select_moved: avx512::select_moved,
    hash_mix: avx512::hash_mix,
    hash_finish: avx512::hash_finish,
};

mod avx2 {
    use super::*;

    const LANES: usize = 8;

    #[target_feature(enable = "avx2")]
    unsafe fn load(p: *const u32) -> __m256i {
        unsafe { _mm256_loadu_si256(p.cast()) }
    }

    /// One bit per lane, set where the lanes are equal.
    #[target_feature(enable = "avx2")]
    fn equal(a: __m256i, b: __m256i) -> u32 {
        _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_cmpeq_epi32(a, b))) as u32
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn select_eq(
        column: *const u32,
        len: usize,
        value: u32,
        out: *mut u32,
    ) -> usize {
        let wanted = _mm256_set1_epi32(value as i32);
        let (mut row, mut found) = (0, 0);
        // Four vectors a step, with one test for the lot.
        while row + 4 * LANES <= len {
            let hits = (0..4).fold(0, |hits, at| {
                let cells = unsafe { load(column.add(row + at * LANES)) };
                hits | u64::from(equal(cells, wanted)) << (at * LANES)
            });
            found = unsafe { push_hits(hits, row, out, found) };
            row += 4 * LANES;
        }
        while row + LANES <= len {
            let hits = equal(unsafe { load(column.add(row)) }, wanted);
            found = unsafe { push_hits(hits.into(), row, out, found) };
            row += LANES;
        }
        unsafe { scalar::select_eq_in(column, row..len, value, out, found) }
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn find_same(a: *const u32, b: *const u32, len: usize) -> usize {
        let mut row = 0;
        while row + LANES <= len {
            let hits = unsafe { equal(load(a.add(row)), load(b.add(row))) };
            if hits != 0 {
                return row + hits.trailing_zeros() as usize;
            }
            row += LANES;
        }
        row + unsafe { scalar::find_same(a.add(row), b.add(row), len - row) }
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn select_moved(
        column: *const u32,
        map: *const u32,
        map_len: usize,
        len: usize,
        out: *mut u32,
    ) -> usize {
        if map_len == 0 {
            return unsafe { scalar::select_moved(column, map, map_len, len, out) };
        }
        // `map_len` is within gather range, so the largest index fits a lane.
        let last = _mm256_set1_epi32(map_len as i32 - 1);
        let (mut row, mut found) = (0, 0);
        while row + LANES <= len {
            let cells = unsafe { load(column.add(row)) };
            let inside = _mm256_cmpeq_epi32(_mm256_min_epu32(cells, last), cells);
            // A lane outside the map keeps its cell, and is reported below.
            let now = unsafe { _mm256_mask_i32gather_epi32::<4>(cells, map.cast(), cells, inside) };
            let stays = _mm256_and_si256(_mm256_cmpeq_epi32(now, cells), inside);
            let moved = !_mm256_movemask_ps(_mm256_castsi256_ps(stays)) as u32 & 0xff;
            found = unsafe { push_hits(moved.into(), row, out, found) };
            row += LANES;
        }
        unsafe { scalar::select_moved_in(column, map, map_len, row..len, out, found) }
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn hash_mix(hash: *mut u32, column: *const u32, len: usize) {
        let mul = _mm256_set1_epi32(HASH_MUL as i32);
        let mut row = 0;
        while row + LANES <= len {
            unsafe {
                let mixed = _mm256_xor_si256(load(hash.add(row)), load(column.add(row)));
                _mm256_storeu_si256(hash.add(row).cast(), _mm256_mullo_epi32(mixed, mul));
            }
            row += LANES;
        }
        unsafe { scalar::hash_mix(hash.add(row), column.add(row), len - row) };
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn hash_finish(hash: *mut u32, len: usize) {
        let mut row = 0;
        while row + LANES <= len {
            unsafe {
                let value = load(hash.add(row));
                let folded = _mm256_xor_si256(value, _mm256_srli_epi32::<15>(value));
                _mm256_storeu_si256(hash.add(row).cast(), folded);
            }
            row += LANES;
        }
        unsafe { scalar::hash_finish(hash.add(row), len - row) };
    }
}

mod avx512 {
    use super::*;

    const LANES: usize = 16;

    /// The mask selecting the lanes of the vector at `row` that lie below
    /// `len`. A masked load does not touch memory behind a clear lane, so the
    /// last, partial vector goes through the same code as the others.
    fn lanes(row: usize, len: usize) -> __mmask16 {
        let left = len - row;
        if left >= LANES {
            u16::MAX
        } else {
            (1u16 << left) - 1
        }
    }

    #[target_feature(enable = "avx512f")]
    unsafe fn load(mask: __mmask16, p: *const u32) -> __m512i {
        unsafe { _mm512_maskz_loadu_epi32(mask, p.cast()) }
    }

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn select_eq(
        column: *const u32,
        len: usize,
        value: u32,
        out: *mut u32,
    ) -> usize {
        let wanted = _mm512_set1_epi32(value as i32);
        let (mut row, mut found) = (0, 0);
        // Four whole vectors a step, with one test for the lot: the masks the
        // last vector needs would lengthen the loop by half.
        while row + 4 * LANES <= len {
            let hits = (0..4).fold(0, |hits, at| {
                let cells = unsafe { _mm512_loadu_si512(column.add(row + at * LANES).cast()) };
                hits | u64::from(_mm512_cmpeq_epi32_mask(cells, wanted)) << (at * LANES)
            });
            found = unsafe { push_hits(hits, row, out, found) };
            row += 4 * LANES;
        }
        while row < len {
            let mask = lanes(row, len);
            let cells = unsafe { load(mask, column.add(row)) };
            let hits = _mm512_mask_cmpeq_epi32_mask(mask, cells, wanted);
            found = unsafe { push_hits(hits.into(), row, out, found) };
            row += LANES;
        }
        found
    }

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn find_same(a: *const u32, b: *const u32, len: usize) -> usize {
        let mut row = 0;
        while row < len {
            let mask = lanes(row, len);
            let (left, right) = unsafe { (load(mask, a.add(row)), load(mask, b.add(row))) };
            let hits = _mm512_mask_cmpeq_epi32_mask(mask, left, right);
            if hits != 0 {
                return row + hits.trailing_zeros() as usize;
            }
            row += LANES;
        }
        len
    }

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn select_moved(
        column: *const u32,
        map: *const u32,
        map_len: usize,
        len: usize,
        out: *mut u32,
    ) -> usize {
        // `map_len` is within gather range, so it fits a lane.
        let bound = _mm512_set1_epi32(map_len as i32);
        let (mut row, mut found) = (0, 0);
        while row < len {
            let mask = lanes(row, len);
            let cells = unsafe { load(mask, column.add(row)) };
            let inside = _mm512_mask_cmplt_epu32_mask(mask, cells, bound);
            let now = unsafe { _mm512_mask_i32gather_epi32::<4>(cells, inside, cells, map.cast()) };
            let moved = _mm512_mask_cmpneq_epi32_mask(mask, now, cells) | (mask & !inside);
            found = unsafe { push_hits(moved.into(), row, out, found) };
            row += LANES;
        }
        found
    }

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn hash_mix(hash: *mut u32, column: *const u32, len: usize) {
        let mul = _mm512_set1_epi32(HASH_MUL as i32);
        let mut row = 0;
        while row < len {
            let mask = lanes(row, len);
            unsafe {
                let mixed =
                    _mm512_xor_si512(load(mask, hash.add(row)), load(mask, column.add(row)));
                _mm512_mask_storeu_epi32(
                    hash.add(row).cast(),
                    mask,
                    _mm512_mullo_epi32(mixed, mul),
                );
            }
            row += LANES;
        }
    }

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn hash_finish(hash: *mut u32, len: usize) {
        let mut row = 0;
        while row < len {
            let mask = lanes(row, len);
            unsafe {
                let value = load(mask, hash.add(row));
                let folded = _mm512_xor_si512(value, _mm512_srli_epi32::<15>(value));
                _mm512_mask_storeu_epi32(hash.add(row).cast(), mask, folded);
            }
            row += LANES;
        }
    }
}
