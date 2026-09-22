//! Expand operations through their shared intrinsic interface.

use crate::analysis::AnalysisManager;
use crate::attributes::AttributeValue;
use crate::backend::TargetMachine;
use crate::{Context, Intrinsic, OperationRef, Pass, PassError, TargetEnv};

#[derive(Clone, Default)]
pub struct LowerIntrinsicsPass {
    target: Option<AttributeValue>,
}

impl LowerIntrinsicsPass {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use target facts as defaults beneath scoped IR environment entries.
    pub fn for_target(target: &dyn TargetMachine) -> Self {
        let mut entries = match target.target_env() {
            Some(AttributeValue::Dict(entries)) => *entries,
            _ => Default::default(),
        };
        entries
            .entry("memory_scalar_bytes".to_string())
            .or_insert_with(|| {
                AttributeValue::Array(
                    target
                        .unaligned_scalar_bytes()
                        .iter()
                        .map(|width| AttributeValue::UInt(u64::from(*width)))
                        .collect(),
                )
            });
        Self {
            target: Some(AttributeValue::Dict(Box::new(entries))),
        }
    }
}

crate::register_pass!(LowerIntrinsicsPass, "lower-intrinsics");

impl Pass for LowerIntrinsicsPass {
    fn name(&self) -> &'static str {
        "lower-intrinsics"
    }

    fn run(
        &mut self,
        operation: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if let Some(intrinsic) = operation.op().clone().as_interface::<dyn Intrinsic>() {
            let env =
                TargetEnv::for_op_with_default(context, operation.op().id, self.target.as_ref());
            intrinsic.expand(context, env.as_ref())?;
            if operation.op().is_live() {
                return Err(PassError::InvalidRuleSet(
                    "intrinsic expansion did not remove its operation".into(),
                ));
            }
        }
        Ok(())
    }
}
