//! Branch-direction predictors — the first swappable microarchitecture policy of
//! the dynamic engine. A predictor sees a conditional branch (its address and
//! resolved target) and guesses taken/not-taken; the timing model charges a
//! misprediction penalty when the guess is wrong. New predictors are added by
//! implementing [`BranchPredictor`] in Rust — no TMDL involved.

mod batage;
mod common;
mod tage;

pub use batage::Batage;
pub use common::TageParams;
pub use tage::Tage;

/// A conditional-branch direction predictor.
pub trait BranchPredictor {
    /// Predict whether the branch at `pc` with destination `target` is taken.
    fn predict(&mut self, pc: u64, target: u64) -> bool;

    /// Update the predictor with the resolved outcome. Static predictors ignore it.
    fn update(&mut self, pc: u64, target: u64, taken: bool) {
        let _ = (pc, target, taken);
    }

    fn name(&self) -> &'static str;
}

/// Always predicts not-taken — the simplest possible static predictor and a useful
/// baseline. Correct only on fall-through branches; mispredicts every taken branch
/// (so it is poor on loops).
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysNotTaken;

impl BranchPredictor for AlwaysNotTaken {
    fn predict(&mut self, _pc: u64, _target: u64) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "always-not-taken"
    }
}

/// Backward-taken / forward-not-taken (BTFN): the classic static loop heuristic. A
/// branch to an earlier address is a loop back-edge and predicted taken; a forward
/// branch (an `if`-style skip) is predicted not-taken. Mispredicts only the loop
/// exit, so it is far better than [`AlwaysNotTaken`] on loops.
#[derive(Debug, Default, Clone, Copy)]
pub struct BackwardTaken;

impl BranchPredictor for BackwardTaken {
    fn predict(&mut self, pc: u64, target: u64) -> bool {
        target < pc
    }

    fn name(&self) -> &'static str {
        "btfn"
    }
}

/// Construct a predictor by name, for CLI selection. `config` is a
/// `key=value,...` string applied to the tunable predictors (`tage` / `batage`),
/// empty for the parameter-free static ones. Returns an error message on an
/// unknown name or a bad config.
pub fn by_name(name: &str, config: &str) -> Result<Box<dyn BranchPredictor>, String> {
    let dynamic = |mut params: TageParams| -> Result<TageParams, String> {
        params.apply(config)?;
        Ok(params)
    };
    match name {
        "not-taken" | "always-not-taken" => Ok(Box::new(AlwaysNotTaken)),
        "btfn" | "backward-taken" => Ok(Box::new(BackwardTaken)),
        "tage" => Ok(Box::new(Tage::new(dynamic(TageParams::default())?))),
        "batage" => Ok(Box::new(Batage::new(dynamic(TageParams::default())?))),
        _ => Err(format!(
            "unknown predictor '{name}' (expected: not-taken, btfn, tage, batage)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
