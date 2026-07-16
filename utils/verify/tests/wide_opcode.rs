use std::path::PathBuf;

#[test]
fn execute_accepts_an_opcode_wider_than_u64() {
    let inputs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/inputs");
    let verifier = tir_verify::Verifier::load(
        &inputs.join("wide_opcode.ir"),
        &inputs.join("wide_opcode.toml"),
        &[],
        "footprint",
        1,
        1,
        false,
    )
    .unwrap();
    let word = 1_u128 << 64;

    let traces = verifier.execute(&[word], &[80]).unwrap();

    assert!(traces.contains_key(&word));
}
