//! The gate between overloadable TIR and object code: an object file names one
//! symbol per name, so by lowering time every overload must have been mangled
//! to a name of its own.

use crate::analysis::{AnalysisManager, PreservedAnalyses};
use crate::symbol_table::format_symbol;
use crate::{
    Context, OperationRef, Pass, PassError, PassTarget, Rewriter, SymbolTable, builtin::ModuleOp,
};

#[derive(Default)]
pub struct CheckUniqueSymbolsPass;

impl CheckUniqueSymbolsPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(CheckUniqueSymbolsPass, "check-unique-symbols");

impl Pass for CheckUniqueSymbolsPass {
    fn name(&self) -> &'static str {
        "check-unique-symbols"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<ModuleOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        let table = analyses.get::<SymbolTable>(context, op.op().id);
        let mut overloaded: Vec<_> = table
            .names()
            .filter(|name| table.lookup(name).len() > 1)
            .collect();
        overloaded.sort_unstable();

        if let Some(name) = overloaded.first() {
            let candidates: Vec<_> = table
                .lookup(name)
                .iter()
                .map(|entry| format_symbol(context, name, entry.signature.as_deref()))
                .collect();
            return Err(PassError::InvalidRuleSet(format!(
                "symbol '@{name}' is still overloaded: {}",
                candidates.join(", ")
            )));
        }
        Ok(PreservedAnalyses::all())
    }
}
