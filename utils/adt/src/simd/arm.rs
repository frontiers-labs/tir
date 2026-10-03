//! NEON and SVE2 kernels. NEON is written with intrinsics. SVE2 has none on
//! stable Rust, so those loops are assembly: each walks the column under a
//! `whilelo` predicate, which also covers the last, partial vector.

use std::arch::aarch64::*;
use std::arch::asm;

use super::{HASH_MUL, Kernels, scalar};

pub(super) static NEON: Kernels = Kernels {
    name: "neon",
    find_eq: neon::find_eq,
    find_same: neon::find_same,
    max: neon::max,
    mark_moved: neon::mark_moved,
    hash_mix: neon::hash_mix,
    hash_finish: neon::hash_finish,
};

pub(super) static SVE2: Kernels = Kernels {
    name: "sve2",
    find_eq: sve2::find_eq,
    find_same: sve2::find_same,
    max: sve2::max,
    mark_moved: sve2::mark_moved,
    hash_mix: sve2::hash_mix,
    hash_finish: sve2::hash_finish,
};

mod neon {
    use super::*;

    const LANES: usize = 4;

    pub(super) unsafe fn find_eq(column: *const u32, len: usize, value: u32) -> usize {
        let mut row = 0;
        unsafe {
            let wanted = vdupq_n_u32(value);
            while row + LANES <= len {
                if vmaxvq_u32(vceqq_u32(vld1q_u32(column.add(row)), wanted)) != 0 {
                    break;
                }
                row += LANES;
            }
            row + scalar::find_eq(column.add(row), len - row, value)
        }
    }

    pub(super) unsafe fn find_same(a: *const u32, b: *const u32, len: usize) -> usize {
        let mut row = 0;
        unsafe {
            while row + LANES <= len {
                let equal = vceqq_u32(vld1q_u32(a.add(row)), vld1q_u32(b.add(row)));
                if vmaxvq_u32(equal) != 0 {
                    break;
                }
                row += LANES;
            }
            row + scalar::find_same(a.add(row), b.add(row), len - row)
        }
    }

    pub(super) unsafe fn max(column: *const u32, len: usize) -> u32 {
        let mut row = 0;
        unsafe {
            let mut best = vdupq_n_u32(0);
            while row + LANES <= len {
                best = vmaxq_u32(best, vld1q_u32(column.add(row)));
                row += LANES;
            }
            vmaxvq_u32(best).max(scalar::max(column.add(row), len - row))
        }
    }

    pub(super) unsafe fn mark_moved(
        column: *const u32,
        map: *const u32,
        flags: *mut u32,
        len: usize,
    ) {
        let mut row = 0;
        unsafe {
            let one = vdupq_n_u32(1);
            while row + LANES <= len {
                let cells = vld1q_u32(column.add(row));
                // NEON has no gather: four loads, one vector compare.
                let now = [
                    *map.add(vgetq_lane_u32::<0>(cells) as usize),
                    *map.add(vgetq_lane_u32::<1>(cells) as usize),
                    *map.add(vgetq_lane_u32::<2>(cells) as usize),
                    *map.add(vgetq_lane_u32::<3>(cells) as usize),
                ];
                let moved = vbicq_u32(one, vceqq_u32(vld1q_u32(now.as_ptr()), cells));
                vst1q_u32(flags.add(row), vorrq_u32(vld1q_u32(flags.add(row)), moved));
                row += LANES;
            }
            scalar::mark_moved(column.add(row), map, flags.add(row), len - row);
        }
    }

    pub(super) unsafe fn hash_mix(hash: *mut u32, column: *const u32, len: usize) {
        let mut row = 0;
        unsafe {
            let mul = vdupq_n_u32(HASH_MUL);
            while row + LANES <= len {
                let mixed = veorq_u32(vld1q_u32(hash.add(row)), vld1q_u32(column.add(row)));
                vst1q_u32(hash.add(row), vmulq_u32(mixed, mul));
                row += LANES;
            }
            scalar::hash_mix(hash.add(row), column.add(row), len - row);
        }
    }

    pub(super) unsafe fn hash_finish(hash: *mut u32, len: usize) {
        let mut row = 0;
        unsafe {
            while row + LANES <= len {
                let value = vld1q_u32(hash.add(row));
                vst1q_u32(hash.add(row), veorq_u32(value, vshrq_n_u32::<15>(value)));
                row += LANES;
            }
            scalar::hash_finish(hash.add(row), len - row);
        }
    }
}

mod sve2 {
    use super::*;

    // In every loop `{i}` is the row, p0 the lanes of this vector that are
    // below `len`. `whilelo` sets the flags: `b.eq` when no lane is active,
    // `b.mi` when the first one is.

    #[target_feature(enable = "sve2")]
    pub(super) unsafe fn find_eq(column: *const u32, len: usize, value: u32) -> usize {
        let row: usize;
        unsafe {
            asm!(
                "mov {i}, #0",
                "dup z1.s, {v:w}",
                "whilelo p0.s, {i}, {n}",
                "b.eq 3f",
                "2:",
                "ld1w z0.s, p0/z, [{p}, {i}, lsl #2]",
                "cmpeq p1.s, p0/z, z0.s, z1.s",
                "b.ne 4f",
                "incw {i}",
                "whilelo p0.s, {i}, {n}",
                "b.mi 2b",
                "3:",
                "mov {i}, {n}",
                "b 5f",
                // Count the active lanes before the first hit.
                "4:",
                "brkb p1.b, p0/z, p1.b",
                "incp {i}, p1.s",
                "5:",
                p = in(reg) column,
                n = in(reg) len,
                v = in(reg) value,
                i = out(reg) row,
                out("v0") _, out("v1") _, out("p0") _, out("p1") _,
                options(nostack, readonly),
            );
        }
        row
    }

    #[target_feature(enable = "sve2")]
    pub(super) unsafe fn find_same(a: *const u32, b: *const u32, len: usize) -> usize {
        let row: usize;
        unsafe {
            asm!(
                "mov {i}, #0",
                "whilelo p0.s, {i}, {n}",
                "b.eq 3f",
                "2:",
                "ld1w z0.s, p0/z, [{a}, {i}, lsl #2]",
                "ld1w z1.s, p0/z, [{b}, {i}, lsl #2]",
                "cmpeq p1.s, p0/z, z0.s, z1.s",
                "b.ne 4f",
                "incw {i}",
                "whilelo p0.s, {i}, {n}",
                "b.mi 2b",
                "3:",
                "mov {i}, {n}",
                "b 5f",
                "4:",
                "brkb p1.b, p0/z, p1.b",
                "incp {i}, p1.s",
                "5:",
                a = in(reg) a,
                b = in(reg) b,
                n = in(reg) len,
                i = out(reg) row,
                out("v0") _, out("v1") _, out("p0") _, out("p1") _,
                options(nostack, readonly),
            );
        }
        row
    }

    #[target_feature(enable = "sve2")]
    pub(super) unsafe fn max(column: *const u32, len: usize) -> u32 {
        let best: u32;
        unsafe {
            asm!(
                "mov {i}, #0",
                "dup z1.s, #0",
                "whilelo p0.s, {i}, {n}",
                "b.eq 3f",
                "2:",
                "ld1w z0.s, p0/z, [{p}, {i}, lsl #2]",
                "umax z1.s, p0/m, z1.s, z0.s",
                "incw {i}",
                "whilelo p0.s, {i}, {n}",
                "b.mi 2b",
                "3:",
                "ptrue p0.s",
                "umaxv s1, p0, z1.s",
                "fmov {r:w}, s1",
                p = in(reg) column,
                n = in(reg) len,
                i = out(reg) _,
                r = out(reg) best,
                out("v0") _, out("v1") _, out("p0") _,
                options(nostack, readonly),
            );
        }
        best
    }

    #[target_feature(enable = "sve2")]
    pub(super) unsafe fn mark_moved(
        column: *const u32,
        map: *const u32,
        flags: *mut u32,
        len: usize,
    ) {
        unsafe {
            asm!(
                "mov {i}, #0",
                "dup z3.s, #1",
                "whilelo p0.s, {i}, {n}",
                "b.eq 3f",
                "2:",
                "ld1w z0.s, p0/z, [{c}, {i}, lsl #2]",
                // Gather `map[cell]` for every active lane.
                "ld1w z1.s, p0/z, [{m}, z0.s, uxtw #2]",
                "cmpne p1.s, p0/z, z1.s, z0.s",
                "ld1w z2.s, p0/z, [{f}, {i}, lsl #2]",
                "orr z2.s, p1/m, z2.s, z3.s",
                "st1w z2.s, p0, [{f}, {i}, lsl #2]",
                "incw {i}",
                "whilelo p0.s, {i}, {n}",
                "b.mi 2b",
                "3:",
                c = in(reg) column,
                m = in(reg) map,
                f = in(reg) flags,
                n = in(reg) len,
                i = out(reg) _,
                out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("p0") _, out("p1") _,
                options(nostack),
            );
        }
    }

    #[target_feature(enable = "sve2")]
    pub(super) unsafe fn hash_mix(hash: *mut u32, column: *const u32, len: usize) {
        unsafe {
            asm!(
                "mov {i}, #0",
                "dup z2.s, {k:w}",
                "whilelo p0.s, {i}, {n}",
                "b.eq 3f",
                "2:",
                "ld1w z0.s, p0/z, [{h}, {i}, lsl #2]",
                "ld1w z1.s, p0/z, [{c}, {i}, lsl #2]",
                "eor z0.d, z0.d, z1.d",
                "mul z0.s, p0/m, z0.s, z2.s",
                "st1w z0.s, p0, [{h}, {i}, lsl #2]",
                "incw {i}",
                "whilelo p0.s, {i}, {n}",
                "b.mi 2b",
                "3:",
                h = in(reg) hash,
                c = in(reg) column,
                n = in(reg) len,
                k = in(reg) HASH_MUL,
                i = out(reg) _,
                out("v0") _, out("v1") _, out("v2") _, out("p0") _,
                options(nostack),
            );
        }
    }

    #[target_feature(enable = "sve2")]
    pub(super) unsafe fn hash_finish(hash: *mut u32, len: usize) {
        unsafe {
            asm!(
                "mov {i}, #0",
                "whilelo p0.s, {i}, {n}",
                "b.eq 3f",
                "2:",
                "ld1w z0.s, p0/z, [{h}, {i}, lsl #2]",
                "lsr z1.s, z0.s, #15",
                "eor z0.d, z0.d, z1.d",
                "st1w z0.s, p0, [{h}, {i}, lsl #2]",
                "incw {i}",
                "whilelo p0.s, {i}, {n}",
                "b.mi 2b",
                "3:",
                h = in(reg) hash,
                n = in(reg) len,
                i = out(reg) _,
                out("v0") _, out("v1") _, out("p0") _,
                options(nostack),
            );
        }
    }
}
