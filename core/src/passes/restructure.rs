use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct RestructureNodesPass;

impl RestructureNodesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(RestructureNodesPass, "restructure-nodes");

impl Pass for RestructureNodesPass {
    fn name(&self) -> &'static str {
        "restructure-nodes"
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
