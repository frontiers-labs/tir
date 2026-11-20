use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct LowerPtrDisjointPass;

impl LowerPtrDisjointPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(LowerPtrDisjointPass, "lower-ptr-disjoint");

impl Pass for LowerPtrDisjointPass {
    fn name(&self) -> &'static str {
        "lower-ptr-disjoint"
    }
    fn run(
        &mut self,
        _op: &OperationRef,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        Ok(())
    }
}
