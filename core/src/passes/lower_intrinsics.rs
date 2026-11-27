use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct LowerIntrinsicsPass;

impl LowerIntrinsicsPass {
    pub fn new() -> Self {
        Self
    }
    pub fn for_target(_target: &dyn crate::backend::TargetMachine) -> Self {
        Self
    }
}

crate::register_pass!(LowerIntrinsicsPass, "lower-intrinsics");

impl Pass for LowerIntrinsicsPass {
    fn name(&self) -> &'static str {
        "lower-intrinsics"
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
