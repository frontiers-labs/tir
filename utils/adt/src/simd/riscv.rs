//! Kernels for the RISC-V V extension, in assembly: stable Rust can neither
//! enable `v` for a function nor name its intrinsics. Each loop asks `vsetvli`
//! how many rows this pass takes, which also covers the last, partial vector.

use std::arch::asm;
use std::sync::OnceLock;

use super::{HASH_MUL, Kernels};

pub(super) static RVV: Kernels = Kernels {
    name: "rvv",
    find_eq,
    find_same,
    max,
    mark_moved,
    hash_mix,
    hash_finish,
};

/// Whether the kernel reports the V extension. The standard library's
/// detection macro is unstable on this architecture, so this reads the
/// hardware capability word the kernel hands every process.
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

unsafe fn find_eq(column: *const u32, len: usize, value: u32) -> usize {
    let row: usize;
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 4f",
            "vsetvli {vl}, {n}, e32, m1, ta, ma",
            "vle32.v v8, ({p})",
            "vmseq.vx v0, v8, {v}",
            "vfirst.m {t}, v0",
            "bgez {t}, 3f",
            "add {i}, {i}, {vl}",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {p}, {p}, {t}",
            "j 2b",
            "3:",
            "add {i}, {i}, {t}",
            "4:",
            ".option pop",
            p = inout(reg) column => _,
            n = inout(reg) len => _,
            v = in(reg) value,
            i = inout(reg) 0usize => row,
            vl = out(reg) _,
            t = out(reg) _,
            out("v0") _, out("v8") _,
            options(nostack, readonly),
        );
    }
    row
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

unsafe fn max(column: *const u32, len: usize) -> u32 {
    let best: usize;
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 3f",
            "vsetvli {vl}, {n}, e32, m1, ta, ma",
            "vle32.v v8, ({p})",
            // Fold this pass into the running maximum, kept in a register.
            "vmv.s.x v9, {r}",
            "vredmaxu.vs v9, v8, v9",
            "vmv.x.s {r}, v9",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {p}, {p}, {t}",
            "j 2b",
            "3:",
            ".option pop",
            p = inout(reg) column => _,
            n = inout(reg) len => _,
            r = inout(reg) 0usize => best,
            vl = out(reg) _,
            t = out(reg) _,
            out("v8") _, out("v9") _,
            options(nostack, readonly),
        );
    }
    best as u32
}

unsafe fn mark_moved(column: *const u32, map: *const u32, flags: *mut u32, len: usize) {
    unsafe {
        asm!(
            ".option push",
            ".option arch, +v",
            "2:",
            "beqz {n}, 3f",
            // Masked-off lanes of the `vor` below keep their flag.
            "vsetvli {vl}, {n}, e32, m1, ta, mu",
            "vle32.v v8, ({c})",
            // Gather `map[cell]`: the index is a byte offset.
            "vsll.vi v9, v8, 2",
            "vluxei32.v v10, ({m}), v9",
            "vmsne.vv v0, v10, v8",
            "vle32.v v11, ({f})",
            "vor.vi v11, v11, 1, v0.t",
            "vse32.v v11, ({f})",
            "sub {n}, {n}, {vl}",
            "slli {t}, {vl}, 2",
            "add {c}, {c}, {t}",
            "add {f}, {f}, {t}",
            "j 2b",
            "3:",
            ".option pop",
            c = inout(reg) column => _,
            m = in(reg) map,
            f = inout(reg) flags => _,
            n = inout(reg) len => _,
            vl = out(reg) _,
            t = out(reg) _,
            out("v0") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            options(nostack),
        );
    }
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
