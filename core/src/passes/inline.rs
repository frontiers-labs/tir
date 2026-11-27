use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineBudget {
    pub max_ops: u32,
    pub per_constant_arg: u32,
}

impl std::str::FromStr for InlineBudget {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (max_ops, per_constant_arg) = text
            .split_once(',')
            .ok_or_else(|| "inline takes '<max_ops,per_constant_arg>'".to_string())?;
        let number = |field: &str| {
            field
                .trim()
                .parse()
                .map_err(|_| format!("invalid inline budget '{field}'"))
        };
        Ok(Self {
            max_ops: number(max_ops)?,
            per_constant_arg: number(per_constant_arg)?,
        })
    }
}

#[derive(Clone, Default)]
pub struct InlinePass;
impl InlinePass {
    pub fn new(_budget: InlineBudget) -> Self {
        Self
    }
    pub fn parse(args: &str) -> Result<Self, String> {
        args.parse().map(Self::new)
    }
}
crate::register_pass!(InlinePass, "inline", InlinePass::parse);
impl Pass for InlinePass {
    fn name(&self) -> &'static str {
        "inline"
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
