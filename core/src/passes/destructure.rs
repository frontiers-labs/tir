use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct DestructurePass;

impl DestructurePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(DestructurePass, "destructure");

impl Pass for DestructurePass {
    fn name(&self) -> &'static str {
        "destructure"
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
