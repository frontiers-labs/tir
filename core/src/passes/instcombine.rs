use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Default)]
pub struct InstCombineNodesPass;

impl InstCombineNodesPass {
    pub fn new() -> Self {
        Self
    }
    pub fn trusting() -> Self {
        Self
    }
    fn parse(args: &str) -> Result<Self, String> {
        match args {
            "" | "trusted" => Ok(Self),
            other => Err(format!("instcombine-nodes takes `trusted`, not `{other}`")),
        }
    }
}

crate::register_pass!(
    InstCombineNodesPass,
    "instcombine-nodes",
    InstCombineNodesPass::parse
);

impl Pass for InstCombineNodesPass {
    fn name(&self) -> &'static str {
        "instcombine-nodes"
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
