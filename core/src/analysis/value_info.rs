//! Cached IR value, type, purity, and control-safety analysis for one operation subtree.

use crate::{Context, OpId, PassError};

pub use super::generated::ValueInfoAnalyzeResult as ValueFacts;

/// Read cached value/type/purity facts for `root`.
pub fn analyze(context: &Context, root: OpId) -> Result<ValueFacts, PassError> {
    let revision = context.subtree_revision(root);
    let mut engine = context.egraph();
    super::generated::value_info_analyze(
        engine.database_mut(),
        revision,
        super::generated::OperationId::from_raw(root.number()),
    )
    .map_err(|error| PassError::InvalidRuleSet(error.to_string()))
}
