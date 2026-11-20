use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct ResolveFpPass;
impl ResolveFpPass {
    pub fn new() -> Self {
        Self
    }
    fn parse(arguments: &str) -> Result<Self, String> {
        if arguments.trim().is_empty() {
            return Err("resolve-fp requires a target, for example resolve-fp<rv64ifd>".into());
        }
        crate::backend::select_target(arguments.trim(), None, None)?;
        Ok(Self)
    }
}
crate::register_pass!(ResolveFpPass, "resolve-fp", ResolveFpPass::parse);
impl Pass for ResolveFpPass {
    fn name(&self) -> &'static str {
        "resolve-fp"
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
