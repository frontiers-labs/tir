//! Shared environment for late, target-independent intrinsic expansion.

use crate::attributes::AttributeValue;
use crate::{Context, DataLayout, OpId, PassError, TargetEnv};

/// An operation whose implementation is chosen at the backend boundary.
///
/// Implementations use the ordinary [`Context`] rewrite APIs. Successful
/// expansion must remove the intrinsic, preserve its values and effects, and
/// produce ordinary operations for the remaining legalization and selection.
pub trait Intrinsic {
    fn expand(&self, context: &Context, env: &ExpansionEnv) -> Result<(), PassError>;
}

/// Facts in scope at an intrinsic. No target-specific lowering lives here.
pub struct ExpansionEnv {
    pub target: Option<TargetEnv>,
    pub data_layout: Option<DataLayout>,
}

impl ExpansionEnv {
    pub fn for_op(context: &Context, op: OpId) -> Self {
        Self {
            target: TargetEnv::for_op(context, op),
            data_layout: DataLayout::for_op(context, op),
        }
    }

    /// Optional dialect-owned fact from the scoped target environment.
    pub fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.target.as_ref()?.get(key)
    }
}
