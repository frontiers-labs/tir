//! AVX2 and AVX-512 kernels. Each loop handles whole vectors and hands the
//! remainder to the plain kernel, except where AVX-512 masks the last vector.

use std::arch::x86_64::*;

use super::{HASH_MUL, Kernels, scalar};

pub(super) static AVX2: Kernels = Kernels {
    name: "avx2",
    find_eq: avx2::find_eq,
    find_same: avx2::find_same,
    max: avx2::max,
    mark_moved: avx2::mark_moved,
    hash_mix: avx2::hash_mix,
    hash_finish: avx2::hash_finish,
};

pub(super) static AVX512: Kernels = Kernels {
    name: "avx512",
    find_eq: avx512::find_eq,
    find_same: avx512::find_same,
    max: avx512::max,
    mark_moved: avx512::mark_moved,
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
    pub(super) unsafe fn find_eq(column: *const u32, len: usize, value: u32) -> usize {
        let wanted = _mm256_set1_epi32(value as i32);
        let mut row = 0;
        while row + LANES <= len {
            let hits = equal(unsafe { load(column.add(row)) }, wanted);
            if hits != 0 {
                return row + hits.trailing_zeros() as usize;
            }
            row += LANES;
        }
        row + unsafe { scalar::find_eq(column.add(row), len - row, value) }
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
    pub(super) unsafe fn max(column: *const u32, len: usize) -> u32 {
        let mut best = _mm256_setzero_si256();
        let mut row = 0;
        while row + LANES <= len {
            best = _mm256_max_epu32(best, unsafe { load(column.add(row)) });
            row += LANES;
        }
        let mut lanes = [0u32; LANES];
        unsafe { _mm256_storeu_si256(lanes.as_mut_ptr().cast(), best) };
        let tail = unsafe { scalar::max(column.add(row), len - row) };
        lanes.into_iter().fold(tail, u32::max)
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn mark_moved(
        column: *const u32,
        map: *const u32,
        flags: *mut u32,
        len: usize,
    ) {
        let one = _mm256_set1_epi32(1);
        let mut row = 0;
        while row + LANES <= len {
            unsafe {
                let cells = load(column.add(row));
                let now = _mm256_i32gather_epi32::<4>(map.cast(), cells);
                let moved = _mm256_andnot_si256(_mm256_cmpeq_epi32(now, cells), one);
                let merged = _mm256_or_si256(load(flags.add(row)), moved);
                _mm256_storeu_si256(flags.add(row).cast(), merged);
            }
            row += LANES;
        }
        unsafe { scalar::mark_moved(column.add(row), map, flags.add(row), len - row) };
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
    pub(super) unsafe fn find_eq(column: *const u32, len: usize, value: u32) -> usize {
        let wanted = _mm512_set1_epi32(value as i32);
        let mut row = 0;
        while row < len {
            let mask = lanes(row, len);
            let cells = unsafe { load(mask, column.add(row)) };
            let hits = _mm512_mask_cmpeq_epi32_mask(mask, cells, wanted);
            if hits != 0 {
                return row + hits.trailing_zeros() as usize;
            }
            row += LANES;
        }
        len
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
    pub(super) unsafe fn max(column: *const u32, len: usize) -> u32 {
        let mut best = _mm512_setzero_si512();
        let mut row = 0;
        while row < len {
            best = _mm512_max_epu32(best, unsafe { load(lanes(row, len), column.add(row)) });
            row += LANES;
        }
        _mm512_reduce_max_epu32(best)
    }

    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn mark_moved(
        column: *const u32,
        map: *const u32,
        flags: *mut u32,
        len: usize,
    ) {
        let one = _mm512_set1_epi32(1);
        let mut row = 0;
        while row < len {
            let mask = lanes(row, len);
            unsafe {
                let cells = load(mask, column.add(row));
                let now = _mm512_mask_i32gather_epi32::<4>(cells, mask, cells, map.cast());
                let moved = _mm512_mask_cmpneq_epi32_mask(mask, now, cells);
                let merged = _mm512_mask_or_epi32(cells, moved, load(mask, flags.add(row)), one);
                _mm512_mask_storeu_epi32(flags.add(row).cast(), moved, merged);
            }
            row += LANES;
        }
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
