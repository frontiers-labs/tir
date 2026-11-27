//! Control-flow & structured control-flow simplification pass driven by
//! `core/defs/passes/control_simplify.cdsl` and cached `analysis::value_info`.

use crate::analysis::value_info;
use crate::passes::common::{UseSite, apply_edits, rule_error};
use crate::{Context, OpId, Pass, PassError, RegionId, ValueId};

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/control_simplify.rs"));
}

#[derive(Clone, Default)]
pub struct ControlSimplifyPass;

impl ControlSimplifyPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(ControlSimplifyPass, "control-simplify");

fn use_site(owner: generated::CtrlUseOwner, slot: i64) -> Result<UseSite, PassError> {
    let slot = usize::try_from(slot).map_err(rule_error)?;
    Ok(match owner {
        generated::CtrlUseOwner::Operand(op) => UseSite::Operand(OpId::from_number(op.raw()), slot),
        generated::CtrlUseOwner::Yield(region) => UseSite::Yield(
            RegionId::from_number(u32::try_from(region).map_err(rule_error)?),
            slot,
        ),
    })
}

impl Pass for ControlSimplifyPass {
    fn name(&self) -> &'static str {
        "control-simplify"
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
        program.run_control_simplify().map_err(rule_error)?;

        let aliases = program
            .ctrl_alias_edit()
            .map(|row| {
                let (owner, slot, old, new) = row.map_err(rule_error)?;
                Ok((
                    use_site(owner, slot)?,
                    ValueId::from_number(old.raw()),
                    ValueId::from_number(new.raw()),
                ))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        let consts = program
            .ctrl_const_edit()
            .map(|row| {
                let (owner, slot, old, bits) = row.map_err(rule_error)?;
                Ok((
                    use_site(owner, slot)?,
                    ValueId::from_number(old.raw()),
                    bits,
                ))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        let redirected = program
            .ctrl_redirect()
            .map(|row| {
                row.map(|(old, _)| ValueId::from_number(old.raw()))
                    .map_err(rule_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        drop(program);

        apply_edits(context, aliases, consts, redirected, Vec::new())
    }
}
