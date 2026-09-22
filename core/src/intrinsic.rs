//! Late, target-independent intrinsic expansion.

use crate::{Context, PassError, TargetEnv};

/// An operation whose implementation is chosen at the backend boundary.
///
/// Implementations use the ordinary [`Context`] rewrite APIs. Successful
/// expansion must remove the intrinsic, preserve its values and effects, and
/// produce ordinary operations for the remaining legalization and selection.
pub trait Intrinsic {
    fn expand(&self, context: &Context, env: Option<&TargetEnv>) -> Result<(), PassError>;
}
