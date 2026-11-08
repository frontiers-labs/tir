use crate::{AnalysisManager, Context, OperationRef, Pass, PassError};

#[derive(Clone, Default)]
pub struct MachineBlockLayoutPass;

impl MachineBlockLayoutPass {
    pub fn new(_format: crate::backend::binary::ObjectFormatInfo) -> Self {
        Self
    }
    fn parse(arguments: &str) -> Result<Self, String> {
        let target = arguments.trim();
        if target.is_empty() {
            return Err(
                "machine-block-layout requires a target, for example machine-block-layout<x86_64>"
                    .into(),
            );
        }
        let target = crate::backend::select_target(target, None, None)?;
        let format = target
            .object_format()
            .ok_or_else(|| "target does not support object emission".to_string())?;
        Ok(Self::new(format))
    }
}

crate::register_pass!(
    MachineBlockLayoutPass,
    "machine-block-layout",
    MachineBlockLayoutPass::parse
);

impl Pass for MachineBlockLayoutPass {
    fn name(&self) -> &'static str {
        "machine-block-layout"
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
