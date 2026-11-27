use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct AffineSchedulePass;

impl AffineSchedulePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(AffineSchedulePass, "affine");

impl Pass for AffineSchedulePass {
    fn name(&self) -> &'static str {
        "affine"
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
