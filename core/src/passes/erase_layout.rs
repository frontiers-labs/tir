use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct EraseLayoutPass;

impl EraseLayoutPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(EraseLayoutPass, "erase-layout");

impl Pass for EraseLayoutPass {
    fn name(&self) -> &'static str {
        "erase-layout"
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
