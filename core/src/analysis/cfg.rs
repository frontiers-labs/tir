//! Queries over completed CFG analyses for one operation subtree.

use crate::{Context, OpId, PassError};

pub use super::generated::CfgAnalyzeResult as ScopeFacts;

/// Read CFG facts for `root` and retain the tables for the lifetime of the result.
pub fn scope(context: &Context, root: OpId) -> Result<ScopeFacts, PassError> {
    let revision = context.subtree_revision(root);
    let mut engine = context.egraph();
    super::generated::cfg_analyze(
        engine.database_mut(),
        revision,
        super::generated::OperationId::from_raw(root.number()),
    )
    .map_err(|error| PassError::InvalidRuleSet(error.to_string()))
}
