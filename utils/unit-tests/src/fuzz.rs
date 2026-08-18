//! Unit tests for `tir-fuzz`'s deterministic restructuring harness.

/// A bounded smoke campaign: enough shapes to cover branches, joins,
/// dispatch and irreducible loops.
#[test]
fn five_hundred_random_graphs_restructure() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..500 {
        let mut input = Vec::new();
        for _ in 0..16 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            input.push(state as u8);
        }
        tir_fuzz::restructure::check(&input);
    }
}
