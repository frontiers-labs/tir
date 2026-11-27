//! Memory optimization pass (store-to-load forwarding, dead-store elimination,
//! and private stack slot elimination) driven by `core/defs/passes/memopt.cdsl`,
//! `analysis::memory`, and cached `analysis::value_info`.

use crate::analysis::{memory, value_info};
use crate::passes::common::{UseSite, apply_edits, erase_operations, rule_error};
use crate::{Context, OpId, Pass, PassError, RegionId, ValueId};

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/memopt.rs"));
}

#[derive(Clone, Default)]
pub struct MemoryOptPass;

impl MemoryOptPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(MemoryOptPass, "memopt");

fn use_site(owner: generated::MemUseOwner, slot: i64) -> Result<UseSite, PassError> {
    let slot = usize::try_from(slot).map_err(rule_error)?;
    Ok(match owner {
        generated::MemUseOwner::Operand(op) => UseSite::Operand(OpId::from_number(op.raw()), slot),
        generated::MemUseOwner::Yield(region) => UseSite::Yield(
            RegionId::from_number(u32::try_from(region).map_err(rule_error)?),
            slot,
        ),
    })
}

impl Pass for MemoryOptPass {
    fn name(&self) -> &'static str {
        "memopt"
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

        memory::populate(context, &mut db, &inputs)?;

        let mut program =
            generated::Program::load_with_tables(&mut db, &inputs).map_err(rule_error)?;
        program.run_memopt().map_err(rule_error)?;

        let aliases = program
            .mem_alias_edit()
            .map(|row| {
                let (owner, slot, old, new) = row.map_err(rule_error)?;
                Ok((
                    use_site(owner, slot)?,
                    ValueId::from_number(old.raw()),
                    ValueId::from_number(new.raw()),
                ))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        let redirected = program
            .mem_redirect()
            .map(|row| {
                row.map(|(old, _)| ValueId::from_number(old.raw()))
                    .map_err(rule_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let erase = program
            .mem_erase()
            .map(|row| {
                row.map(|(id,)| OpId::from_number(id.raw()))
                    .map_err(rule_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let dead = program
            .mem_dead()
            .map(|row| {
                row.map(|(id,)| context.get_op(OpId::from_number(id.raw())))
                    .map_err(rule_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        drop(program);

        apply_edits(context, aliases, Vec::new(), redirected, erase)?;
        erase_operations(context, dead)
    }
}
