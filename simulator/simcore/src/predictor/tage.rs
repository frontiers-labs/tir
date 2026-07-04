//! TAGE: TAgged GEometric-history-length branch predictor (Seznec & Michaud;
//! "A New Case for the TAGE Branch Predictor", MICRO'11).
//!
//! A base bimodal predictor is backed by tagged components indexed with
//! geometrically increasing global-history lengths. The prediction comes from
//! the matching component with the longest history (the *provider*); a tiny
//! `USE_ALT_ON_NA` meta-selector falls back to the second-longest match
//! (*altpred*) when the provider counter is weak (newly allocated). Tagged
//! entries are allocated on mispredictions and reclaimed via the useful (`u`)
//! bit, which is periodically reset.

use super::BranchPredictor;
use super::common::{GlobalHistory, TageParams, sat_update};

#[derive(Clone, Copy, Default)]
struct Entry {
    tag: u16,
    ctr: i8,
    u: u8,
}

/// State stashed at `predict` time and consumed at `update`, so the second pass
/// does not recompute indices, tags, or the provider/altpred selection. The
/// trace-driven engine always calls `update` immediately after `predict` for
/// the same branch, so a single slot suffices.
struct Prediction {
    base_index: usize,
    indices: Vec<usize>,
    tags: Vec<u16>,
    provider: Option<usize>,
    provider_pred: bool,
    provider_weak: bool,
    alt_pred: bool,
    final_pred: bool,
}

pub struct Tage {
    params: TageParams,
    hist_lengths: Vec<usize>,
    base: Vec<i8>,
    tables: Vec<Vec<Entry>>,
    history: GlobalHistory,
    /// 4-bit-style meta-counter selecting altpred on newly-allocated providers.
    use_alt_on_na: i8,
    ctr_lo: i8,
    ctr_hi: i8,
    u_max: u8,
    reset_tick: u64,
    pending: Option<Prediction>,
}

impl Tage {
    pub fn new(params: TageParams) -> Self {
        let hist_lengths = params.history_lengths();
        let base = vec![0i8; 1usize << params.log_base];
        let tables = vec![vec![Entry::default(); 1usize << params.log_table]; params.num_tables];
        let history = GlobalHistory::new(params.max_hist);
        let ctr_hi = (1i8 << (params.ctr_bits - 1)) - 1;
        let ctr_lo = -(1i8 << (params.ctr_bits - 1));
        let u_max = ((1u16 << params.u_bits) - 1) as u8;
        Tage {
            params,
            hist_lengths,
            base,
            tables,
            history,
            use_alt_on_na: 0,
            ctr_lo,
            ctr_hi,
            u_max,
            reset_tick: 0,
            pending: None,
        }
    }

    fn base_index(&self, pc: u64) -> usize {
        ((pc >> 2) & ((1u64 << self.params.log_base) - 1)) as usize
    }

    /// A counter is "weak" (looks newly allocated) in its two central states.
    fn weak(&self, ctr: i8) -> bool {
        ctr == 0 || ctr == -1
    }
}

impl BranchPredictor for Tage {
    fn predict(&mut self, pc: u64, _target: u64) -> bool {
        let base_index = self.base_index(pc);
        let base_pred = self.base[base_index] >= 0;

        let mut indices = Vec::with_capacity(self.params.num_tables);
        let mut tags = Vec::with_capacity(self.params.num_tables);
        let mut provider = None;
        let mut alt = None;
        for i in 0..self.params.num_tables {
            let len = self.hist_lengths[i];
            let idx = self.history.index(pc, len, self.params.log_table);
            let tag = self.history.tag(pc, len, self.params.tag_bits);
            if self.tables[i][idx].tag == tag {
                alt = provider;
                provider = Some(i);
            }
            indices.push(idx);
            tags.push(tag);
        }

        let provider_pred = provider.map_or(base_pred, |i| self.tables[i][indices[i]].ctr >= 0);
        let alt_pred = alt.map_or(base_pred, |i| self.tables[i][indices[i]].ctr >= 0);
        let provider_weak = provider.is_some_and(|i| self.weak(self.tables[i][indices[i]].ctr));

        let final_pred = match provider {
            Some(_) if provider_weak && self.use_alt_on_na >= 0 => alt_pred,
            Some(_) => provider_pred,
            None => base_pred,
        };

        self.pending = Some(Prediction {
            base_index,
            indices,
            tags,
            provider,
            provider_pred,
            provider_weak,
            alt_pred,
            final_pred,
        });
        final_pred
    }

    fn update(&mut self, _pc: u64, _target: u64, taken: bool) {
        let p = self.pending.take().expect("update without predict");

        // Update the provider counter (or the base predictor if none matched),
        // the meta-selector, and the useful bit.
        match p.provider {
            Some(i) => {
                let idx = p.indices[i];
                if p.provider_weak && p.provider_pred != p.alt_pred {
                    // The provider was newly allocated and disagreed with altpred:
                    // learn whether altpred was the better call.
                    self.use_alt_on_na = if p.alt_pred == taken {
                        (self.use_alt_on_na + 1).min(7)
                    } else {
                        (self.use_alt_on_na - 1).max(-8)
                    };
                }
                if p.provider_pred != p.alt_pred {
                    let e = &mut self.tables[i][idx];
                    e.u = if p.provider_pred == taken {
                        (e.u + 1).min(self.u_max)
                    } else {
                        e.u.saturating_sub(1)
                    };
                }
                let e = &mut self.tables[i][idx];
                e.ctr = sat_update(e.ctr, taken, self.ctr_lo, self.ctr_hi);
            }
            None => {
                let e = &mut self.base[p.base_index];
                *e = sat_update(*e, taken, self.ctr_lo, self.ctr_hi);
            }
        }

        // Allocate on a misprediction into a table with a longer history than
        // the provider, reusing a non-useful (`u == 0`) entry.
        if p.final_pred != taken {
            let start = p.provider.map_or(0, |i| i + 1);
            if start < self.params.num_tables {
                let mut allocated = false;
                for i in start..self.params.num_tables {
                    let idx = p.indices[i];
                    if self.tables[i][idx].u == 0 {
                        self.tables[i][idx] = Entry {
                            tag: p.tags[i],
                            ctr: if taken { 0 } else { -1 },
                            u: 0,
                        };
                        allocated = true;
                        break;
                    }
                }
                if !allocated {
                    // No room: age the candidates so an allocation succeeds soon.
                    for i in start..self.params.num_tables {
                        let idx = p.indices[i];
                        self.tables[i][idx].u = self.tables[i][idx].u.saturating_sub(1);
                    }
                }
            }
        }

        // Graceful periodic reset of useful bits: halve them so dead entries
        // eventually become reclaimable without wiping fresh ones.
        self.reset_tick += 1;
        if self.reset_tick >= self.params.u_reset_period {
            self.reset_tick = 0;
            for table in &mut self.tables {
                for e in table {
                    e.u >>= 1;
                }
            }
        }

        self.history.push(taken);
    }

    fn name(&self) -> &'static str {
        "tage"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Tage {
        Tage::new(TageParams {
            num_tables: 4,
            min_hist: 2,
            max_hist: 32,
            log_base: 8,
            log_table: 8,
            tag_bits: 10,
            ctr_bits: 3,
            ..Default::default()
        })
    }

    /// Drive `p` over `iters` repetitions of `pattern` and return the
    /// misprediction count of the final repetition (steady state).
    fn steady_mispredicts(p: &mut Tage, pattern: &[bool], iters: usize) -> usize {
        let pc = 0x4000u64;
        let mut last = 0;
        for it in 0..iters {
            last = 0;
            for (k, &taken) in pattern.iter().enumerate() {
                // Distinct PCs would defeat history correlation; a single branch
                // whose direction follows a long pattern is the TAGE sweet spot.
                let target = pc.wrapping_sub(4);
                let pred = p.predict(pc, target);
                if pred != taken && it == iters - 1 {
                    last += 1;
                }
                let _ = k;
                p.update(pc, target, taken);
            }
        }
        last
    }

    #[test]
    fn learns_long_periodic_pattern() {
        let mut p = small();
        // A period-8 pattern: unpredictable by a single bimodal counter, but a
        // TAGE component with history >= 8 nails it.
        let pattern = [true, true, false, true, false, false, true, false];
        let miss = steady_mispredicts(&mut p, &pattern, 200);
        assert_eq!(miss, 0, "TAGE should learn the periodic pattern exactly");
    }

    #[test]
    fn predicts_biased_branch() {
        let mut p = small();
        let taken = [true; 16];
        let miss = steady_mispredicts(&mut p, &taken, 50);
        assert_eq!(miss, 0);
    }
}
