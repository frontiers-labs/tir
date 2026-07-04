//! Benchmarking-harness sketch: JIT a straight-line microkernel, then run it in
//! a host loop while a hardware performance counter is enabled, mirroring how a
//! generator would measure a candidate kernel.
//!
//! Run with: `cargo run -p tir-jit --example benchmark`
//! Reading the counter needs `perf_event_paranoid <= 2`; the example falls back
//! to wall-clock time when the counter is unavailable.

use std::time::Instant;

use tir_jit::Jit;

fn main() {
    // A straight-line arithmetic kernel — a stand-in for a generated candidate.
    let ir = r#"
        module {
          func @kernel(%0: !i64) -> !i64 {
            %1 = muli %0, %0 : !i64
            %2 = addi %1, %0 : !i64
            %c = constant {value = 3} : !i64
            %3 = muli %2, %c : !i64
            %4 = subi %3, %0 : !i64
            return %4
          }
          module_end
        }
    "#;

    let jit = Jit::host().expect("host target supported");
    let module = jit.compile(ir).expect("compile kernel");
    let kernel: extern "C" fn(i64) -> i64 = unsafe { module.get("kernel") }.expect("kernel symbol");

    const ITERS: u64 = 10_000_000;
    let counter = perf::InstructionCounter::open();

    let start = Instant::now();
    if let Some(c) = &counter {
        c.enable();
    }
    let mut acc: i64 = 0;
    for i in 0..ITERS as i64 {
        acc = acc.wrapping_add(kernel(i));
    }
    if let Some(c) = &counter {
        c.disable();
    }
    let elapsed = start.elapsed();

    // Keep `acc` observable so the loop is not optimized away.
    println!("checksum: {acc}");
    println!("iterations: {ITERS}");
    println!(
        "wall: {:.3} ms ({:.2} ns/call)",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_nanos() as f64 / ITERS as f64
    );
    match counter.map(|c| c.read()) {
        Some(insns) => println!(
            "instructions: {insns} ({:.2} per call)",
            insns as f64 / ITERS as f64
        ),
        None => println!("instructions: unavailable (raise perf_event_paranoid)"),
    }
}

/// A minimal `perf_event_open` wrapper for retired instructions. The libc crate
/// exposes only the syscall number, so the ABI struct and constants are defined
/// here directly.
mod perf {
    use std::os::fd::{FromRawFd, OwnedFd};

    // `_IO('$', n)` ioctl codes.
    const PERF_EVENT_IOC_ENABLE: u64 = 0x2400;
    const PERF_EVENT_IOC_DISABLE: u64 = 0x2401;
    const PERF_EVENT_IOC_RESET: u64 = 0x2403;

    const PERF_TYPE_HARDWARE: u32 = 0;
    const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
    const FLAG_DISABLED: u64 = 1 << 0;
    const FLAG_EXCLUDE_KERNEL: u64 = 1 << 5;
    const FLAG_EXCLUDE_HV: u64 = 1 << 6;

    /// The leading 64 bytes of `struct perf_event_attr` (`PERF_ATTR_SIZE_VER0`),
    /// which is all this counter configures.
    #[repr(C)]
    #[derive(Default)]
    struct PerfEventAttr {
        type_: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
    }

    pub struct InstructionCounter {
        fd: OwnedFd,
    }

    impl InstructionCounter {
        pub fn open() -> Option<Self> {
            let attr = PerfEventAttr {
                type_: PERF_TYPE_HARDWARE,
                size: std::mem::size_of::<PerfEventAttr>() as u32,
                config: PERF_COUNT_HW_INSTRUCTIONS,
                flags: FLAG_DISABLED | FLAG_EXCLUDE_KERNEL | FLAG_EXCLUDE_HV,
                ..Default::default()
            };

            let fd = unsafe {
                libc::syscall(
                    libc::SYS_perf_event_open,
                    &attr as *const _,
                    0,  // this thread
                    -1, // any cpu
                    -1, // no group
                    0u64,
                )
            };
            if fd < 0 {
                return None;
            }
            Some(Self {
                fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
            })
        }

        fn ioctl(&self, request: u64) {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::ioctl(self.fd.as_raw_fd(), request, 0);
            }
        }

        pub fn enable(&self) {
            self.ioctl(PERF_EVENT_IOC_RESET);
            self.ioctl(PERF_EVENT_IOC_ENABLE);
        }

        pub fn disable(&self) {
            self.ioctl(PERF_EVENT_IOC_DISABLE);
        }

        pub fn read(&self) -> u64 {
            use std::os::fd::AsRawFd;
            let mut value: u64 = 0;
            unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    &mut value as *mut u64 as *mut libc::c_void,
                    8,
                );
            }
            value
        }
    }
}
