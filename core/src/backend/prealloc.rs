use tir::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct TiedOperandLoweringPass;
impl TiedOperandLoweringPass {
    pub fn new() -> Self {
        Self
    }
}
crate::register_pass!(TiedOperandLoweringPass, "tied-operand-lowering");

impl Pass for TiedOperandLoweringPass {
    fn name(&self) -> &'static str {
        "tied-operand-lowering"
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
