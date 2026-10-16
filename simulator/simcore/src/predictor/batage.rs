//! BATAGE: Bayesian TAGE (Pierre Michaud, "An Alternative TAGE-like Conditional
//! Branch Predictor", ACM TACO 15(3), 2018).
//!
//! BATAGE keeps TAGE's table geometry but replaces each tagged entry's up/down
//! counter with a *dual counter* `(n1, n0)` — the number of taken and not-taken
//! occurrences, each saturating at `nmax`. From `(n1, n0)` a Bayesian
//! confidence level (high / medium / low) is derived; the final prediction is
//! taken from the hitting entry with the *lowest* confidence value (highest
//! confidence), longest history breaking ties. This solves the cold-counter
//! problem without a statistical corrector. The `u` bit is gone: dual-counter
//! decay plus Controlled Allocation Throttling (CAT) manage entry lifetime.

use super::BranchPredictor;
use super::common::{Rng, TageParams, TaggedTables, sat_update};

const RNG_SEED: u64 = 0x9e37_79b9_7f4a_7c15;

/// Confidence values, low value = high confidence. A tag miss yields `MISS`,
/// which never wins selection against a real hit.
const CONF_HIGH: u8 = 0;
const MISS: u8 = 3;

#[derive(Clone, Copy, Default)]
struct Entry {
    tag: u16,
    n1: u8,
    n0: u8,
}

fn confidence(n1: u32, n0: u32) -> u8 {
    let medium = n1 == 2 * n0 + 1 || n0 == 2 * n1 + 1;
    let low = n1 < 2 * n0 + 1 && n0 < 2 * n1 + 1;
    2 * u8::from(low) + u8::from(medium)
}

fn mhat(n1: u32, n0: u32) -> f64 {
    (1.0 + n1.min(n0) as f64) / (2.0 + (n1 + n0) as f64)
}

struct Prediction {
    base_index: usize,
    indices: Vec<usize>,
    tags: Vec<u16>,
    hits: Vec<bool>,
    /// Confidence per slot, slot 0 = base (tagless), slots `1..=G` = tagged.
    confs: Vec<u8>,
    /// Direction per slot, aligned with `confs`.
    preds: Vec<bool>,
    /// Selected provider slot in `0..=G`.
    provider: usize,
    final_pred: bool,
}

pub struct Batage {
    params: TageParams,
    geo: TaggedTables<Entry>,
    nmax: u8,
    cat: i32,
    rng: Rng,
    pending: Option<Prediction>,
}

impl Batage {
    pub fn new(params: TageParams) -> Self {
        let nmax = ((1u16 << params.ctr_bits) - 1) as u8;
        Batage {
            geo: TaggedTables::new(&params),
            params,
            nmax,
            cat: 0,
            rng: Rng::new(RNG_SEED),
            pending: None,
        }
    }

    fn base_index(&self, pc: u64) -> usize {
        ((pc >> 2) & ((1u64 << self.params.log_base) - 1)) as usize
    }

    fn base_dual(x: i8) -> (u32, u32) {
        if x >= 0 {
            ((x as u32) + 2, 1)
        } else {
            (1, ((-x) as u32) + 1)
        }
    }

    fn dc_update(&self, e: &mut Entry, taken: bool) {
        if taken {
            if e.n1 < self.nmax {
                e.n1 += 1;
            } else if e.n0 > 0 {
                e.n0 -= 1;
            }
        } else if e.n0 < self.nmax {
            e.n0 += 1;
        } else if e.n1 > 0 {
            e.n1 -= 1;
        }
    }

    fn dc_decay(e: &mut Entry) {
        if e.n1 > e.n0 {
            e.n1 -= 1;
        }
        if e.n0 > e.n1 {
            e.n0 -= 1;
        }
    }
}

impl BranchPredictor for Batage {
    fn predict(&mut self, pc: u64, _target: u64) -> bool {
        let g = self.params.num_tables;
        let base_index = self.base_index(pc);
        let x = self.geo.base[base_index];
        let (bn1, bn0) = Self::base_dual(x);

        let mut indices = Vec::with_capacity(g);
        let mut tags = Vec::with_capacity(g);
        let mut hits = vec![false; g];
        let mut confs = vec![0u8; g + 1];
        let mut preds = vec![false; g + 1];
        confs[0] = confidence(bn1, bn0);
        preds[0] = x >= 0;

        for i in 0..g {
            let len = self.geo.hist_lengths[i];
            let idx = self.geo.history.index(pc, len, self.params.log_table);
            let tag = self.geo.history.tag(pc, len, self.params.tag_bits);
            let e = self.geo.tables[i][idx];
            let hit = e.tag == tag;
            hits[i] = hit;
            confs[i + 1] = if hit {
                confidence(u32::from(e.n1), u32::from(e.n0))
            } else {
                MISS
            };
            preds[i + 1] = e.n1 >= e.n0;
            indices.push(idx);
            tags.push(tag);
        }

        // Pick the entry with the smallest confidence value; on a tie the
        // longest history (largest slot) wins (Michaud §5.2).
        let mut provider = g;
        for i in (0..g).rev() {
            if confs[i] < confs[provider] {
                provider = i;
            }
        }
        let final_pred = preds[provider];

        self.pending = Some(Prediction {
            base_index,
            indices,
            tags,
            hits,
            confs,
            preds,
            provider,
            final_pred,
        });
        final_pred
    }

    fn update(&mut self, _pc: u64, _target: u64, taken: bool) {
        let p = self.pending.take().expect("update without predict");
        let g = self.params.num_tables;
        let j = p.provider;

        // `ln` = longest hitting slot strictly shorter than the provider. The
        // tagless slot (0) always "hits", so it exists whenever j > 0.
        let ln = (0..j).rev().find(|&k| k == 0 || p.hits[k - 1]).unwrap_or(0);
        let lp_high = p.confs[j] == CONF_HIGH;
        let ln_high = p.confs[ln] == CONF_HIGH;
        let ln_correct = p.preds[ln] == taken;

        let update_slot = |this: &mut Self, slot: usize| {
            if slot == 0 {
                let e = &mut this.geo.base[p.base_index];
                *e = sat_update(*e, taken, this.geo.ctr_lo, this.geo.ctr_hi);
            } else {
                let idx = p.indices[slot - 1];
                let mut e = this.geo.tables[slot - 1][idx];
                this.dc_update(&mut e, taken);
                this.geo.tables[slot - 1][idx] = e;
            }
        };

        // Rule 1: hitting entries longer than the provider are always updated
        // (they were skipped because they are cold — warm them up).
        for i in 0..g {
            if p.hits[i] && i + 1 > j {
                update_slot(self, i + 1);
            }
        }

        // Rule 2: the provider.
        if j == 0 {
            update_slot(self, 0);
        } else if !lp_high || !ln_high || !ln_correct {
            update_slot(self, j);
        } else {
            // Provider is a confidently-correct entry backed by a confident
            // shorter entry: it is redundant, so decay it toward reclaimable.
            let idx = p.indices[j - 1];
            let mut e = self.geo.tables[j - 1][idx];
            Self::dc_decay(&mut e);
            self.geo.tables[j - 1][idx] = e;
        }

        // Rule 3: the next-shorter hitting entry, when the provider is a weak
        // tagged entry (improves accuracy slightly).
        if j > 0 && !lp_high {
            update_slot(self, ln);
        }

        if p.final_pred != taken {
            self.allocate(taken, &p);
        }

        self.geo.history.push(taken);
    }

    fn name(&self) -> &'static str {
        "batage"
    }
}

impl Batage {
    fn allocate(&mut self, taken: bool, p: &Prediction) {
        let g = self.params.num_tables;
        // Allocation-probability gate: `1` when `cat` is small, `1/minap` when
        // `cat` approaches `cat_max`.
        let r = self.rng.below(self.params.minap);
        let threshold = (self.cat as i64 * i64::from(self.params.minap)
            / (i64::from(self.params.cat_max) + 1)) as u32;
        if r < threshold {
            return;
        }

        // Longest hitting slot (the tagless slot 0 always hits).
        let lm = (0..g)
            .filter(|&i| p.hits[i])
            .map(|i| i + 1)
            .max()
            .unwrap_or(0);
        let skip = 1 + self.rng.below(self.params.skipmax) as usize; // [1, skipmax]
        let start = lm + skip;

        let mut mhc = 0i32;
        for slot in start..=g {
            let ti = slot - 1;
            let idx = p.indices[ti];
            let mut e = self.geo.tables[ti][idx];
            if confidence(u32::from(e.n1), u32::from(e.n0)) == CONF_HIGH {
                // Protected: decay with probability 1/pdec_recip.
                if self.rng.below(self.params.pdec_recip) == 0 {
                    Self::dc_decay(&mut e);
                    self.geo.tables[ti][idx] = e;
                }
                if mhat(u32::from(e.n1), u32::from(e.n0)) > 0.17 {
                    mhc += 1;
                }
            } else {
                // Not high confidence: take this entry.
                e.tag = p.tags[ti];
                if taken {
                    e.n1 = 1;
                    e.n0 = 0;
                } else {
                    e.n1 = 0;
                    e.n0 = 1;
                }
                self.geo.tables[ti][idx] = e;
                // CATR = 3/4: cat += 3 - 4*mhc, then saturate to [0, cat_max].
                self.cat = (self.cat + 3 - 4 * mhc).clamp(0, self.params.cat_max);
                break;
            }
        }
    }
}
