use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

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
        _op: &OperationRef,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        Ok(())
    }
}
