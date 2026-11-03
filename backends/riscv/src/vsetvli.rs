use tir::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct InsertVsetvliPass;
impl InsertVsetvliPass {
    pub fn new() -> Self {
        Self
    }
}
tir::register_pass!(InsertVsetvliPass, "riscv-insert-vsetvli");
impl Pass for InsertVsetvliPass {
    fn name(&self) -> &'static str {
        "riscv-insert-vsetvli"
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
