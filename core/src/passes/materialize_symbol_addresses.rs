use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct MaterializeSymbolAddressesPass;

impl MaterializeSymbolAddressesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(
    MaterializeSymbolAddressesPass,
    "materialize-symbol-addresses"
);

impl Pass for MaterializeSymbolAddressesPass {
    fn name(&self) -> &'static str {
        "materialize-symbol-addresses"
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
