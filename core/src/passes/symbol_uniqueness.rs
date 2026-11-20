use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct CheckUniqueSymbolsPass;

impl CheckUniqueSymbolsPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(CheckUniqueSymbolsPass, "check-unique-symbols");

impl Pass for CheckUniqueSymbolsPass {
    fn name(&self) -> &'static str {
        "check-unique-symbols"
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
