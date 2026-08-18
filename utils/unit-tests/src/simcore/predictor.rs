use tir_sim::predictor::*;

#[test]
fn always_not_taken_never_predicts_taken() {
    let mut p = AlwaysNotTaken;
    assert!(!p.predict(0x100, 0x80)); // backward
    assert!(!p.predict(0x100, 0x180)); // forward
}

#[test]
fn btfn_predicts_backward_taken_forward_not_taken() {
    let mut p = BackwardTaken;
    // Backward branch (loop back-edge) → taken.
    assert!(p.predict(0x100, 0x80));
    assert!(p.predict(0x80000010, 0x80000004));
    // Forward branch (skip) → not taken.
    assert!(!p.predict(0x100, 0x180));
    // A branch to itself is not "backward".
    assert!(!p.predict(0x100, 0x100));
}

#[test]
fn by_name_selects_predictors() {
    assert_eq!(by_name("not-taken", "").unwrap().name(), "always-not-taken");
    assert_eq!(by_name("btfn", "").unwrap().name(), "btfn");
    assert_eq!(by_name("tage", "").unwrap().name(), "tage");
    assert_eq!(by_name("batage", "").unwrap().name(), "batage");
    assert!(by_name("nope", "").is_err());
}

#[test]
fn by_name_applies_and_validates_config() {
    assert!(by_name("tage", "tables=8,max_hist=1000").is_ok());
    assert!(by_name("batage", "ctr_bits=4,cat_max=4096").is_ok());
    assert!(by_name("tage", "bogus=1").is_err());
    assert!(by_name("batage", "tables=0").is_err());
}

use tir_sim::predictor::TageParams;

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
fn apply_rejects_unknown_and_malformed() {
    let mut p = TageParams::default();
    assert!(p.apply("tables=8,max_hist=1000").is_ok());
    assert_eq!(p.num_tables, 8);
    assert_eq!(p.max_hist, 1000);
    assert!(p.apply("bogus=1").is_err());
    assert!(p.apply("tables=xyz").is_err());
    assert!(p.apply("min_hist=2000").is_err()); // min > max
}

mod batage {
    use tir_sim::predictor::{Batage, BranchPredictor, TageParams};

    fn small() -> Batage {
        Batage::new(TageParams {
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

    fn steady_mispredicts(p: &mut Batage, pattern: &[bool], iters: usize) -> usize {
        let pc = 0x4000u64;
        let mut last = 0;
        for it in 0..iters {
            last = 0;
            for &taken in pattern {
                let target = pc.wrapping_sub(4);
                let pred = p.predict(pc, target);
                if pred != taken && it == iters - 1 {
                    last += 1;
                }
                p.update(pc, target, taken);
            }
        }
        last
    }

    #[test]
    fn learns_long_periodic_pattern() {
        let mut p = small();
        let pattern = [true, true, false, true, false, false, true, false];
        let miss = steady_mispredicts(&mut p, &pattern, 200);
        assert_eq!(miss, 0, "BATAGE should learn the periodic pattern exactly");
    }

    #[test]
    fn predicts_biased_branch() {
        let mut p = small();
        let taken = [true; 16];
        assert_eq!(steady_mispredicts(&mut p, &taken, 50), 0);
    }
}

mod tage {
    use tir_sim::predictor::{BranchPredictor, Tage, TageParams};

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
