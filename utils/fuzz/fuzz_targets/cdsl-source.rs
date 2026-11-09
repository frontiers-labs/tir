#![no_main]

use libfuzzer_sys::fuzz_target;
use tir_cdsl::{compile, EvaluationLimits, SourceMap};

fuzz_target!(|data: &[u8]| {
    if data.len() > 16 * 1024 {
        return;
    }
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let mut input = source.splitn(3, '\0');
    let root = input.next().unwrap_or("");
    let library = input.next().unwrap_or("");
    let entry = input.next().unwrap_or("main");
    let mut sources = SourceMap::new("fuzz.cdsl", root);
    sources.insert("library", library);
    if let Ok(checked) = compile(&sources) {
        let _ = checked.evaluate_with_limits(
            entry,
            EvaluationLimits {
                max_depth: 128,
                max_steps: 10_000,
            },
        );
    }
});
