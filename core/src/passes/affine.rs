use crate::{AnalysisManager, Context, OpId, Pass, PassError};

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
        _op: OpId,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        Ok(())
    }
}
