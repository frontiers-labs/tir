//! Cachegrind client requests that bound the counted region of a function benchmark.
//!
//! The child process runs under `--instr-at-start=no`, so setup and teardown
//! outside these calls are not counted. Outside Valgrind both calls do nothing.
// `VG_USERREQ_TOOL_BASE('C', 'G')` from Valgrind's cachegrind.h (3.22 or newer).
const START_INSTRUMENTATION: usize = (b'C' as usize) << 24 | (b'G' as usize) << 16;
const STOP_INSTRUMENTATION: usize = START_INSTRUMENTATION + 1;

#[inline(always)]
pub fn start() {
    request(START_INSTRUMENTATION);
}

#[inline(always)]
pub fn stop() {
    request(STOP_INSTRUMENTATION);
}

/// Valgrind recognizes the rotate sequence, which leaves every register intact,
/// followed by a self-exchange. The argument block address and the default
/// result travel in the registers that valgrind.h assigns for each platform.
#[inline(always)]
fn request(code: usize) {
    let arguments = [code, 0, 0, 0, 0, 0];
    #[cfg(target_arch = "x86_64")]
    // SAFETY: outside Valgrind this is four rotations summing to 128 bits and a no-op exchange.
    unsafe {
        std::arch::asm!(
            "rol rdi, 3", "rol rdi, 13", "rol rdi, 61", "rol rdi, 51", "xchg rbx, rbx",
            inout("rdi") 0usize => _,
            in("rax") arguments.as_ptr(),
            inout("rdx") 0usize => _,
            options(nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: outside Valgrind this is four rotations summing to 128 bits and a no-op move.
    unsafe {
        std::arch::asm!(
            "ror x12, x12, #3", "ror x12, x12, #13", "ror x12, x12, #51", "ror x12, x12, #61",
            "orr x10, x10, x10",
            inout("x12") 0usize => _,
            in("x4") arguments.as_ptr(),
            inout("x3") 0usize => _,
            options(nostack),
        );
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: outside Valgrind these shifts write the zero register and the move is a no-op.
    unsafe {
        std::arch::asm!(
            ".option push", ".option norvc",
            "srli zero, zero, 3", "srli zero, zero, 13", "srli zero, zero, 51", "srli zero, zero, 61",
            "or a0, a0, a0",
            ".option pop",
            in("a4") arguments.as_ptr(),
            inout("a3") 0usize => _,
            options(nostack),
        );
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )))]
    let _ = arguments;
}
