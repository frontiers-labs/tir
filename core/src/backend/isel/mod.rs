use tir::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Default)]
pub struct InstructionSelectPass;
impl InstructionSelectPass {
    pub fn new() -> Self {
        Self
    }
}
crate::register_pass!(InstructionSelectPass, "instruction-select");
impl Pass for InstructionSelectPass {
    fn name(&self) -> &'static str {
        "instruction-select"
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
