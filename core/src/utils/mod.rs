pub use tir_adt::{APFloat, APInt, RawBits};

/// splitmix64, so one seed gives one sequence on every host. The oracles that
/// re-linearize a block share it: a divergence has to reproduce from its seed
/// alone.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The seed the `TIR_SHUFFLE_SEED` environment variable names, or zero.
    /// One compiler process compiles one program, so the default already gives
    /// every program an order of its own; setting it sweeps several orders over
    /// one corpus.
    pub fn from_environment() -> Self {
        Self::new(
            std::env::var("TIR_SHUFFLE_SEED")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        )
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}
