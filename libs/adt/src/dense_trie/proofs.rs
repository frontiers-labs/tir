use super::{Simd, WINDOW, window};

fn check_window<S: Simd>(simd: S) {
    let cells: [u32; WINDOW] = kani::any();
    let len: u32 = kani::any();
    let value: u32 = kani::any();
    let low: u32 = kani::any();
    let high: u32 = kani::any();
    kani::assume(0 < len && len <= WINDOW as u32);
    kani::assume(low <= high && high <= len);

    // Every sorted window has these partitions. Cells within each partition
    // and all padding remain arbitrary, so the property also covers unsorted
    // cells whose relation to this query has the same partitions.
    for (i, &cell) in cells.iter().enumerate() {
        let i = i as u32;
        if i < low {
            kani::assume(cell < value);
        } else if i < high {
            kani::assume(cell == value);
        } else if i < len {
            kani::assume(cell > value);
        }
    }

    let found = window(simd, &cells, len, value);
    assert_eq!(found, low..high);
    kani::cover!(len == WINDOW as u32);
    kani::cover!(found.start < found.end);
    kani::cover!(found.start == found.end);
}

#[kani::proof]
#[kani::unwind(65)]
fn fallback_window() {
    check_window(fearless_simd::Fallback::new());
}

#[cfg(target_arch = "x86_64")]
#[kani::proof]
#[kani::unwind(65)]
fn sse2_window() {
    // SAFETY: this proof assumes the SSE2 ISA is available.
    check_window(unsafe { fearless_simd::x86::Sse2::assume_supported() });
}

#[cfg(target_arch = "x86_64")]
#[kani::proof]
#[kani::unwind(65)]
fn avx512_window() {
    // SAFETY: this proof assumes the Ice Lake AVX-512 ISA is available.
    check_window(unsafe { fearless_simd::x86::Avx512::assume_supported() });
}

// Kani 0.68 cannot verify the SSE4.2 and AVX2 paths: their unsigned compares
// reach the unsupported simd_select intrinsic. They are outside this suite.
