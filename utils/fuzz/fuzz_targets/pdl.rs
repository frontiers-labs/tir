#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 32 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    if let Ok(input) = std::str::from_utf8(data) {
        let _ = tir_pdl::compile_to_rust(input);
    }
});
