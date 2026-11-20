//! Undefined behavior the program states: an assumption, a freeze of
//! poison, and a refinement the solver checks.

// `%r = refine %x, %p`: `%r` is `%x`, and `%p` holds wherever `%x` is
// defined. The verifier proves it; see the semantic refinement checker.
pub use super::generated::{
    AssumeOp, AssumeOpBuilder, FreezeOp, FreezeOpBuilder, RefineOp, RefineOpBuilder, assume,
    freeze, refine,
};

impl RefineOp {
    /// The value refined.
    pub fn input(&self) -> crate::ValueId {
        self.0.operands()[0]
    }

    /// What holds of it.
    pub fn predicate(&self) -> crate::ValueId {
        self.0.operands()[1]
    }
}
