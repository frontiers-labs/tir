//! The compiler must be reproducible: the same input compiled by separate
//! processes must produce byte-identical assembly. Each process starts with a
//! different random hash state, so a single run cannot catch an ordering that
//! leaks that state into the output — only repeated processes can.

use std::path::PathBuf;

use super::link_support::compile_asm;

#[test]
fn assembly_is_identical_across_processes() {
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fcc/checks/Inputs/determinism.c");
    let expected = compile_asm(&source);
    for _ in 0..8 {
        assert_eq!(
            compile_asm(&source),
            expected,
            "assembly differs between runs of the same input"
        );
    }
}
