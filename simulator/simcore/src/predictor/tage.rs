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
use super::common::{TageParams, TaggedTables, sat_update};

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
    geo: TaggedTables<Entry>,
    /// 4-bit-style meta-counter selecting altpred on newly-allocated providers.
    use_alt_on_na: i8,
    u_max: u8,
    reset_tick: u64,
    pending: Option<Prediction>,
}

impl Tage {
    pub fn new(params: TageParams) -> Self {
        let u_max = ((1u16 << params.u_bits) - 1) as u8;
        Tage {
            geo: TaggedTables::new(&params),
            params,
            use_alt_on_na: 0,
            u_max,
            reset_tick: 0,
            pending: None,
        }
    }

    fn base_index(&self, pc: u64) -> usize {
        ((pc >> 2) & ((1u64 << self.params.log_base) - 1)) as usize
    }

    fn weak(&self, ctr: i8) -> bool {
        ctr == 0 || ctr == -1
    }
}

impl BranchPredictor for Tage {
    fn predict(&mut self, pc: u64, _target: u64) -> bool {
        let base_index = self.base_index(pc);
        let base_pred = self.geo.base[base_index] >= 0;

        let mut indices = Vec::with_capacity(self.params.num_tables);
        let mut tags = Vec::with_capacity(self.params.num_tables);
        let mut provider = None;
        let mut alt = None;
        for i in 0..self.params.num_tables {
            let len = self.geo.hist_lengths[i];
            let idx = self.geo.history.index(pc, len, self.params.log_table);
            let tag = self.geo.history.tag(pc, len, self.params.tag_bits);
            if self.geo.tables[i][idx].tag == tag {
                alt = provider;
                provider = Some(i);
            }
            indices.push(idx);
            tags.push(tag);
        }

        let provider_pred = provider.map_or(base_pred, |i| self.geo.tables[i][indices[i]].ctr >= 0);
        let alt_pred = alt.map_or(base_pred, |i| self.geo.tables[i][indices[i]].ctr >= 0);
        let provider_weak = provider.is_some_and(|i| self.weak(self.geo.tables[i][indices[i]].ctr));

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
                    let e = &mut self.geo.tables[i][idx];
                    e.u = if p.provider_pred == taken {
                        (e.u + 1).min(self.u_max)
                    } else {
                        e.u.saturating_sub(1)
                    };
                }
                let e = &mut self.geo.tables[i][idx];
                e.ctr = sat_update(e.ctr, taken, self.geo.ctr_lo, self.geo.ctr_hi);
            }
            None => {
                let e = &mut self.geo.base[p.base_index];
                *e = sat_update(*e, taken, self.geo.ctr_lo, self.geo.ctr_hi);
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
                    if self.geo.tables[i][idx].u == 0 {
                        self.geo.tables[i][idx] = Entry {
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
                        self.geo.tables[i][idx].u = self.geo.tables[i][idx].u.saturating_sub(1);
                    }
                }
            }
        }

        // Graceful periodic reset of useful bits: halve them so dead entries
        // eventually become reclaimable without wiping fresh ones.
        self.reset_tick += 1;
        if self.reset_tick >= self.params.u_reset_period {
            self.reset_tick = 0;
            for table in &mut self.geo.tables {
                for e in table {
                    e.u >>= 1;
                }
            }
        }

        self.geo.history.push(taken);
    }

    fn name(&self) -> &'static str {
        "tage"
    }
}
