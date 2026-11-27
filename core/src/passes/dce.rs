//! Dead code elimination pass driven by `core/defs/passes/dce.cdsl` and cached
//! `analysis::value_info` facts.

use crate::analysis::value_info;
use crate::passes::common::{erase_operations, rule_error};
use crate::{Context, OpId, Pass, PassError};

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/dce.rs"));
}

#[derive(Clone, Default)]
pub struct DeadCodeEliminationPass;

impl DeadCodeEliminationPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(DeadCodeEliminationPass, "dce");

impl Pass for DeadCodeEliminationPass {
    fn name(&self) -> &'static str {
        "dce"
    }

    fn run(
        &mut self,
        op: OpId,
        context: &Context,
        _: &crate::AnalysisManager,
    ) -> Result<(), PassError> {
        if !context.has_operation(op) {
            return Ok(());
        }
        let facts = value_info::analyze(context, op)?;
        let bindings = facts.bindings();
        let tables = bindings.iter().map(|&(_, t)| t).collect::<Vec<_>>();
        let (mut db, cloned) = context
            .eqsat_database_with_tables(&tables)
            .map_err(rule_error)?;
        let inputs = bindings
            .iter()
            .zip(&cloned)
            .map(|(&(name, _), table)| (name, table))
            .collect::<Vec<_>>();
        let mut program =
            generated::Program::load_with_tables(&mut db, &inputs).map_err(rule_error)?;
        program.run_dce().map_err(rule_error)?;
        let removals = program
            .dce_dead()
            .map(|row| {
                row.map(|(id,)| context.get_op(OpId::from_number(id.raw())))
                    .map_err(rule_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        drop(program);
        erase_operations(context, removals)
    }
}
