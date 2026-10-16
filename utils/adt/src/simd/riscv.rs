//! Kernels for the RISC-V V extension, in assembly: stable Rust can neither
//! enable `v` for a function nor name its intrinsics. Each loop asks `vsetvli`
//! how many rows this pass takes, which also covers the last, partial vector,
//! except the sorted intersection, which hands the rest of its lists to the
//! plain merge.

use std::arch::asm;
use std::sync::OnceLock;

use super::{HASH_MUL, Kernels, scalar};

pub(super) static RVV: Kernels = Kernels {
    name: "rvv",
    select_eq,
    find_same,
    select_moved,
    hash_mix,
    hash_finish,
    intersect_sorted,
    and_words,
};

pub(super) fn has_vector() -> bool {
    const AT_HWCAP: usize = 16;
    const HWCAP_V: usize = 1 << (b'v' - b'a');
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        let Ok(auxv) = std::fs::read("/proc/self/auxv") else {
            return false;
        };
        let (words, _) = auxv.as_chunks::<{ size_of::<usize>() }>();
        let (pairs, _) = words.as_chunks::<2>();
        pairs.iter().any(|&[key, value]| {
            usize::from_ne_bytes(key) == AT_HWCAP && usize::from_ne_bytes(value) & HWCAP_V != 0
        })
    })
}

// In every loop `{n}` counts the rows left and `{vl}` the rows this pass
// takes. v0 is the mask register.

unsafe fn select_eq(column: *const u32, len: usize, value: u32, out: *mut u32) -> usize {
    let found: usize;
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 4f",
            "vsetvli {vl}, {n}, e32, m1, ta, ma",
            "vle32.v v8, ({p})",
            "vmseq.vx v0, v8, {v}",
            "vcpop.m {t}, v0",
            "beqz {t}, 3f",
            // Pack the rows of the hits to the front and store that many.
            "vid.v v9",
            "vadd.vx v9, v9, {i}",
            "vcompress.vm v10, v9, v0",
            "vsetvli zero, {t}, e32, m1, ta, ma",
            "vse32.v v10, ({o})",
            "add {k}, {k}, {t}",
            "slli {t}, {t}, 2",
            "add {o}, {o}, {t}",
            "3:",
            "add {i}, {i}, {vl}",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {p}, {p}, {t}",
            "j 2b",
            "4:",
            ".option pop",
            p = inout(reg) column => _,
            n = inout(reg) len => _,
            v = in(reg) value,
            o = inout(reg) out => _,
            i = inout(reg) 0usize => _,
            k = inout(reg) 0usize => found,
            vl = out(reg) _,
            t = out(reg) _,
            out("v0") _, out("v8") _, out("v9") _, out("v10") _,
            options(nostack),
        );
    }
    found
}

unsafe fn find_same(a: *const u32, b: *const u32, len: usize) -> usize {
    let row: usize;
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 4f",
            "vsetvli {vl}, {n}, e32, m1, ta, ma",
            "vle32.v v8, ({a})",
            "vle32.v v9, ({b})",
            "vmseq.vv v0, v8, v9",
            "vfirst.m {t}, v0",
            "bgez {t}, 3f",
            "add {i}, {i}, {vl}",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {a}, {a}, {t}",
            "add {b}, {b}, {t}",
            "j 2b",
            "3:",
            "add {i}, {i}, {t}",
            "4:",
            ".option pop",
            a = inout(reg) a => _,
            b = inout(reg) b => _,
            n = inout(reg) len => _,
            i = inout(reg) 0usize => row,
            vl = out(reg) _,
            t = out(reg) _,
            out("v0") _, out("v8") _, out("v9") _,
            options(nostack, readonly),
        );
    }
    row
}

unsafe fn select_moved(
    column: *const u32,
    map: *const u32,
    map_len: usize,
    len: usize,
    out: *mut u32,
) -> usize {
    let found: usize;
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 4f",
            // Masked-off lanes of the gather keep what v10 held.
            "vsetvli {vl}, {n}, e32, m1, ta, mu",
            "vle32.v v8, ({c})",
            // A cell outside the map reads as its complement, which differs.
            "vnot.v v10, v8",
            "vmsltu.vx v0, v8, {l}",
            "vsll.vi v9, v8, 2",
            "vluxei32.v v10, ({m}), v9, v0.t",
            "vmsne.vv v0, v10, v8",
            "vcpop.m {t}, v0",
            "beqz {t}, 3f",
            // Pack the rows of the hits to the front and store that many.
            "vid.v v9",
            "vadd.vx v9, v9, {i}",
            "vcompress.vm v11, v9, v0",
            "vsetvli zero, {t}, e32, m1, ta, ma",
            "vse32.v v11, ({o})",
            "add {k}, {k}, {t}",
            "slli {t}, {t}, 2",
            "add {o}, {o}, {t}",
            "3:",
            "add {i}, {i}, {vl}",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {c}, {c}, {t}",
            "j 2b",
            "4:",
            ".option pop",
            c = inout(reg) column => _,
            m = in(reg) map,
            l = in(reg) map_len,
            n = inout(reg) len => _,
            o = inout(reg) out => _,
            i = inout(reg) 0usize => _,
            k = inout(reg) 0usize => found,
            vl = out(reg) _,
            t = out(reg) _,
            out("v0") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            options(nostack),
        );
    }
    found
}

unsafe fn hash_mix(hash: *mut u32, column: *const u32, len: usize) {
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 3f",
            "vsetvli {vl}, {n}, e32, m1, ta, ma",
            "vle32.v v8, ({h})",
            "vle32.v v9, ({c})",
            "vxor.vv v8, v8, v9",
            "vmul.vx v8, v8, {k}",
            "vse32.v v8, ({h})",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {h}, {h}, {t}",
            "add {c}, {c}, {t}",
            "j 2b",
            "3:",
            ".option pop",
            h = inout(reg) hash => _,
            c = inout(reg) column => _,
            n = inout(reg) len => _,
            k = in(reg) HASH_MUL as usize,
            vl = out(reg) _,
            t = out(reg) _,
            out("v8") _, out("v9") _,
            options(nostack),
        );
    }
}

unsafe fn hash_finish(hash: *mut u32, len: usize) {
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 3f",
            "vsetvli {vl}, {n}, e32, m1, ta, ma",
            "vle32.v v8, ({h})",
            "vsrl.vi v9, v8, 15",
            "vxor.vv v8, v8, v9",
            "vse32.v v8, ({h})",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {h}, {h}, {t}",
            "j 2b",
            "3:",
            ".option pop",
            h = inout(reg) hash => _,
            n = inout(reg) len => _,
            vl = out(reg) _,
            t = out(reg) _,
            out("v8") _, out("v9") _,
            options(nostack),
        );
    }
}

// The block compare of `x86::avx2::intersect_sorted` over whole vectors of
// `{w}` lanes: `vslide1down` feeds the first lane of `b` back in at the top, so
// `{w}` slides rotate every lane of `b` under every lane of `a`. `{na}` and
// `{nb}` count the rows left in `a` and `b`, `{room}` the cells `out` holds.
unsafe fn intersect_sorted(
    a: *const u32,
    alen: usize,
    b: *const u32,
    blen: usize,
    out: *mut u32,
) -> usize {
    let room = alen.min(blen);
    let (a_rest, b_rest, out_rest, alen_rest, blen_rest, room_rest);
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "vsetvli {w}, zero, e32, m1, ta, ma",
            "slli {s}, {w}, 2",
            "2:",
            "bltu {na}, {w}, 4f",
            "bltu {nb}, {w}, 4f",
            "bltu {room}, {w}, 4f",
            "vle32.v v8, ({a})",
            "vle32.v v9, ({b})",
            "vmclr.m v0",
            "mv {t}, {w}",
            "3:",
            "vmseq.vv v10, v8, v9",
            "vmor.mm v0, v0, v10",
            "vmv.x.s {x}, v9",
            "vslide1down.vx v9, v9, {x}",
            "addi {t}, {t}, -1",
            "bnez {t}, 3b",
            "vcompress.vm v11, v8, v0",
            "vcpop.m {t}, v0",
            "vsetvli zero, {t}, e32, m1, ta, ma",
            "vse32.v v11, ({o})",
            "vsetvli zero, {w}, e32, m1, ta, ma",
            "sub {room}, {room}, {t}",
            "slli {t}, {t}, 2",
            "add {o}, {o}, {t}",
            // Step past the vector that ends lower, or both on a tie.
            "add {t}, {a}, {s}",
            "lwu {x}, -4({t})",
            "add {u}, {b}, {s}",
            "lwu {y}, -4({u})",
            "bltu {y}, {x}, 5f",
            "mv {a}, {t}",
            "sub {na}, {na}, {w}",
            "5:",
            "bltu {x}, {y}, 2b",
            "mv {b}, {u}",
            "sub {nb}, {nb}, {w}",
            "j 2b",
            "4:",
            ".option pop",
            a = inout(reg) a => a_rest,
            b = inout(reg) b => b_rest,
            o = inout(reg) out => out_rest,
            na = inout(reg) alen => alen_rest,
            nb = inout(reg) blen => blen_rest,
            room = inout(reg) room => room_rest,
            w = out(reg) _,
            s = out(reg) _,
            t = out(reg) _,
            u = out(reg) _,
            x = out(reg) _,
            y = out(reg) _,
            out("v0") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            options(nostack),
        );
        room - room_rest
            + scalar::intersect_sorted_within(
                a_rest, alen_rest, b_rest, blen_rest, out_rest, room_rest,
            )
    }
}

unsafe fn and_words(a: *mut u64, b: *const u64, len: usize) {
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 3f",
            "vsetvli {vl}, {n}, e64, m1, ta, ma",
            "vle64.v v8, ({a})",
            "vle64.v v9, ({b})",
            "vand.vv v8, v8, v9",
            "vse64.v v8, ({a})",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 3",
            "add {a}, {a}, {t}",
            "add {b}, {b}, {t}",
            "j 2b",
            "3:",
            ".option pop",
            a = inout(reg) a => _,
            b = inout(reg) b => _,
            n = inout(reg) len => _,
            vl = out(reg) _,
            t = out(reg) _,
            out("v8") _, out("v9") _,
            options(nostack),
        );
    }
}
