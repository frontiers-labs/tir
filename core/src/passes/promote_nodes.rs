use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct PromoteNodesPass;

impl PromoteNodesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(PromoteNodesPass, "promote-nodes");

impl Pass for PromoteNodesPass {
    fn name(&self) -> &'static str {
        "promote-nodes"
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
