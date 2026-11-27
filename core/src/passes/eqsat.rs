mod publish;
mod session;

#[allow(dead_code)] // The generator emits readers for every CDSL export.
pub(crate) mod generated {
    include!(concat!(env!("OUT_DIR"), "/eqsat.rs"));
}

pub use generated::{add_zero_rewrites, builtin_rewrites};

impl From<generated::OperationId> for OpId {
    fn from(id: generated::OperationId) -> Self {
        Self::from_number(id.raw())
    }
}

impl From<OpId> for generated::OperationId {
    fn from(id: OpId) -> Self {
        Self::from_raw(id.number())
    }
}

impl From<generated::ValueId> for ValueId {
    fn from(id: generated::ValueId) -> Self {
        Self::from_number(id.raw())
    }
}

impl<T> From<generated::ValueRef<T>> for ValueId {
    fn from(reference: generated::ValueRef<T>) -> Self {
        Self::from_number(reference.raw())
    }
}

use tir_egglog::RuleCollection;

use crate::{AnalysisManager, Context, OpId, Pass, PassError, ValueId};

/// Saturate equalities in a private snapshot of the current IR.
///
/// The pass driver itself lives in CDSL (`core/defs/eqsat/mod.cdsl`); Rust is
/// a thin wrapper constructing a `Session` and invoking the generated driver.
pub struct EqSatPass {
    rules: Rules,
}

enum Rules {
    Builtin(Vec<RuleCollection>),
    Custom(RuleCollection),
}

impl EqSatPass {
    /// Run the built-in EqSat rules.
    pub fn new() -> Self {
        Self::with_extra_rules([])
    }

    /// Run a custom rule collection. An empty collection performs no rewrites.
    pub fn with_ruleset(rules: RuleCollection) -> Self {
        Self {
            rules: Rules::Custom(rules),
        }
    }

    /// Run the built-in rules together with custom rule collections.
    pub fn with_extra_rules(rules: impl IntoIterator<Item = RuleCollection>) -> Self {
        Self {
            rules: Rules::Builtin(rules.into_iter().collect()),
        }
    }
}

impl Default for EqSatPass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(EqSatPass, "eqsat");

impl Pass for EqSatPass {
    fn name(&self) -> &'static str {
        "eqsat"
    }

    fn run(
        &mut self,
        root_id: OpId,
        context: &Context,
        _: &AnalysisManager,
    ) -> Result<(), PassError> {
        let mut session = session::Session::new(context)?;
        match &self.rules {
            Rules::Builtin(extras) => {
                generated::run_builtin_eqsat(&mut session, root_id.into(), extras);
            }
            Rules::Custom(rules) => {
                generated::run_custom_eqsat(&mut session, root_id.into(), rules);
            }
        }
        session.finish()
    }
}
