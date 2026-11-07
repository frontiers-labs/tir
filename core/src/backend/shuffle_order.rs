use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct ShuffleMachineOrderPass;

impl ShuffleMachineOrderPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(ShuffleMachineOrderPass, "shuffle-machine-order");

impl Pass for ShuffleMachineOrderPass {
    fn name(&self) -> &'static str {
        "shuffle-machine-order"
    }
    fn run(
        &mut self,
        _op: OpId,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        Ok(())
    }
}
