use tir::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct OpLoweringPass;
impl OpLoweringPass {
    pub fn new() -> Self {
        Self
    }
}
crate::register_pass!(OpLoweringPass, "op-lowering");
impl Pass for OpLoweringPass {
    fn name(&self) -> &'static str {
        "op-lowering"
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
