use tir::{AnalysisManager, Context, OpId, Pass, PassError};
#[derive(Clone, Default)]
pub struct RaiseLoopsPass;
impl RaiseLoopsPass {
    pub fn new() -> Self {
        Self
    }
}
tir::register_pass!(RaiseLoopsPass, "raise-loops");
impl Pass for RaiseLoopsPass {
    fn name(&self) -> &'static str {
        "raise-loops"
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
