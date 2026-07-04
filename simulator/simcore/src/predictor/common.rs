//! Shared building blocks for the TAGE and BATAGE tagged-geometric-history
//! predictors: tunable parameters, a global branch-history register with XOR
//! folding, the geometric history-length series, and a deterministic PRNG.
//!
//! TAGE and BATAGE share the same table geometry (a base predictor plus tagged
//! components indexed by geometrically increasing global-history lengths); they
//! differ only in the tagged-entry payload and the predict/update algorithms.
//! Everything common lives here so both predictors stay in sync.

/// Tunable parameters shared by TAGE and BATAGE. Exposed on the CLI via
/// `--predictor-config key=value,...` so measurements can sweep the predictor
/// geometry without recompiling.
#[derive(Debug, Clone)]
pub struct TageParams {
    /// Number of tagged components (history tables).
    pub num_tables: usize,
    /// Shortest tagged history length, `L(1)`.
    pub min_hist: usize,
    /// Longest tagged history length, `L(num_tables)`.
    pub max_hist: usize,
    /// log2 of the base table's entry count.
    pub log_base: u32,
    /// log2 of each tagged table's entry count.
    pub log_table: u32,
    /// Partial-tag width in bits (<= 16).
    pub tag_bits: u32,
    /// Counter width. TAGE: the signed prediction counter is `ctr_bits` wide.
    /// BATAGE: each dual counter `n1`/`n0` is `ctr_bits` wide, so `nmax` is
    /// `2^ctr_bits - 1`.
    pub ctr_bits: u32,

    // --- TAGE only ---
    /// Useful-bit width in the tagged entry.
    pub u_bits: u32,
    /// Branches between graceful (halving) resets of the useful bits.
    pub u_reset_period: u64,

    // --- BATAGE only ---
    /// Controlled-Allocation-Throttling saturation ceiling.
    pub cat_max: i32,
    /// Minimum allocation probability is `1/minap`.
    pub minap: u32,
    /// Maximum random table skip when searching for an allocation victim.
    pub skipmax: u32,
    /// Decay probability of a protected high-confidence entry is `1/pdec_recip`.
    pub pdec_recip: u32,
}

impl Default for TageParams {
    fn default() -> Self {
        // A ~mid-size configuration in the spirit of the reference predictors:
        // 12 tagged tables, (4..640) geometric history, 1K entries per tagged
        // table, an 8K base table, 12-bit tags, 3-bit counters.
        TageParams {
            num_tables: 12,
            min_hist: 4,
            max_hist: 640,
            log_base: 13,
            log_table: 10,
            tag_bits: 12,
            ctr_bits: 3,
            u_bits: 2,
            u_reset_period: 1 << 19,
            cat_max: 8192,
            minap: 4,
            skipmax: 2,
            pdec_recip: 4,
        }
    }
}

impl TageParams {
    /// Apply `key=value,...` overrides parsed from `--predictor-config`.
    pub fn apply(&mut self, spec: &str) -> Result<(), String> {
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (key, value) = entry
                .split_once('=')
                .ok_or_else(|| format!("bad predictor config '{entry}' (expected key=value)"))?;
            let key = key.trim();
            let value = value.trim();
            let usize_val = || value.parse::<usize>().map_err(|_| bad(key, value));
            let u32_val = || value.parse::<u32>().map_err(|_| bad(key, value));
            let u64_val = || value.parse::<u64>().map_err(|_| bad(key, value));
            let i32_val = || value.parse::<i32>().map_err(|_| bad(key, value));
            match key {
                "tables" => self.num_tables = usize_val()?,
                "min_hist" => self.min_hist = usize_val()?,
                "max_hist" => self.max_hist = usize_val()?,
                "log_base" => self.log_base = u32_val()?,
                "log_table" => self.log_table = u32_val()?,
                "tag_bits" => self.tag_bits = u32_val()?,
                "ctr_bits" => self.ctr_bits = u32_val()?,
                "u_bits" => self.u_bits = u32_val()?,
                "u_reset_period" => self.u_reset_period = u64_val()?,
                "cat_max" => self.cat_max = i32_val()?,
                "minap" => self.minap = u32_val()?,
                "skipmax" => self.skipmax = u32_val()?,
                "pdec_recip" => self.pdec_recip = u32_val()?,
                _ => return Err(format!("unknown predictor parameter '{key}'")),
            }
        }
        self.validate()
    }

    fn validate(&self) -> Result<(), String> {
        if self.num_tables == 0 {
            return Err("tables must be >= 1".into());
        }
        if self.min_hist == 0 || self.min_hist > self.max_hist {
            return Err("require 1 <= min_hist <= max_hist".into());
        }
        if !(1..=16).contains(&self.tag_bits) {
            return Err("tag_bits must be in 1..=16".into());
        }
        if !(2..=8).contains(&self.ctr_bits) {
            return Err("ctr_bits must be in 2..=8".into());
        }
        if self.log_table == 0 || self.log_table > 24 || self.log_base == 0 || self.log_base > 24 {
            return Err("log_base/log_table must be in 1..=24".into());
        }
        if self.minap == 0 || self.skipmax == 0 || self.pdec_recip == 0 {
            return Err("minap, skipmax and pdec_recip must be >= 1".into());
        }
        Ok(())
    }

    /// Geometric history-length series `L(i)` for the tagged tables, shortest
    /// first: `L(1) = min_hist`, `L(num_tables) = max_hist`.
    pub fn history_lengths(&self) -> Vec<usize> {
        let n = self.num_tables;
        if n == 1 {
            return vec![self.min_hist];
        }
        let (lo, hi) = (self.min_hist as f64, self.max_hist as f64);
        (0..n)
            .map(|i| {
                let ratio = (hi / lo).powf(i as f64 / (n - 1) as f64);
                (lo * ratio + 0.5) as usize
            })
            .collect()
    }
}

fn bad(key: &str, value: &str) -> String {
    format!("bad value '{value}' for predictor parameter '{key}'")
}

/// Global branch-history register: the sequence of resolved directions, most
/// recent first. Only the last `max_len` bits are retained, since no tagged
/// table indexes with a longer history.
pub struct GlobalHistory {
    bits: std::collections::VecDeque<u8>,
    max_len: usize,
}

impl GlobalHistory {
    pub fn new(max_len: usize) -> Self {
        GlobalHistory {
            bits: std::collections::VecDeque::with_capacity(max_len + 1),
            max_len,
        }
    }

    /// Record a resolved branch direction.
    pub fn push(&mut self, taken: bool) {
        self.bits.push_front(taken as u8);
        if self.bits.len() > self.max_len {
            self.bits.pop_back();
        }
    }

    /// XOR-fold the most recent `len` history bits into a `width`-bit value.
    /// This is the standard TAGE compression of a long history into a table
    /// index or tag.
    fn fold(&self, len: usize, width: u32) -> u64 {
        let mut acc = 0u64;
        let mut pos = 0u32;
        for k in 0..len.min(self.bits.len()) {
            acc ^= u64::from(self.bits[k]) << pos;
            pos += 1;
            if pos == width {
                pos = 0;
            }
        }
        acc
    }

    /// Table index for a tagged component: the program counter mixed with the
    /// folded history of length `len`, masked to `log_size` bits.
    pub fn index(&self, pc: u64, len: usize, log_size: u32) -> usize {
        let pc = pc >> 2; // instruction-aligned addresses
        let h = self.fold(len, log_size);
        ((pc ^ (pc >> log_size) ^ h) & ((1u64 << log_size) - 1)) as usize
    }

    /// Partial tag for a tagged component. Two folds of different widths
    /// decorrelate the tag from the index and cut false matches.
    pub fn tag(&self, pc: u64, len: usize, tag_bits: u32) -> u16 {
        let pc = pc >> 2;
        let h1 = self.fold(len, tag_bits);
        let h2 = self.fold(len, tag_bits.saturating_sub(1).max(1));
        ((pc ^ h1 ^ (h2 << 1)) & ((1u64 << tag_bits) - 1)) as u16
    }
}

/// Deterministic xorshift64 PRNG. Fixed-seeded so every simulation of a given
/// trace is reproducible (BATAGE allocation/decay and the TAGE useful-bit reset
/// consume randomness).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1) // keep the state non-zero
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform value in `0..n` (`n` must be non-zero).
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % u64::from(n)) as u32
    }
}

/// Saturating up/down update of a signed counter toward `taken`.
pub fn sat_update(ctr: i8, taken: bool, lo: i8, hi: i8) -> i8 {
    if taken {
        (ctr + 1).min(hi)
    } else {
        (ctr - 1).max(lo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometric_series_spans_min_to_max() {
        let p = TageParams {
            num_tables: 6,
            min_hist: 4,
            max_hist: 640,
            ..Default::default()
        };
        let l = p.history_lengths();
        assert_eq!(l.len(), 6);
        assert_eq!(l[0], 4);
        assert_eq!(*l.last().unwrap(), 640);
        // Monotonically non-decreasing.
        assert!(l.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn history_retains_only_max_len() {
        let mut h = GlobalHistory::new(3);
        for _ in 0..10 {
            h.push(true);
        }
        assert_eq!(h.bits.len(), 3);
    }

    #[test]
    fn apply_rejects_unknown_and_malformed() {
        let mut p = TageParams::default();
        assert!(p.apply("tables=8,max_hist=1000").is_ok());
        assert_eq!(p.num_tables, 8);
        assert_eq!(p.max_hist, 1000);
        assert!(p.apply("bogus=1").is_err());
        assert!(p.apply("tables=xyz").is_err());
        assert!(p.apply("min_hist=2000").is_err()); // min > max
    }
}
