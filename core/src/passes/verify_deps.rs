use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct VerifyDepsPass;

impl VerifyDepsPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(VerifyDepsPass, "verify-deps");

impl Pass for VerifyDepsPass {
    fn name(&self) -> &'static str {
        "verify-deps"
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
