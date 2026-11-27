//! Thin runtime shim bridging CDSL v3's flat `Color::BASE` EqSat pass drivers
//! to `tir_egglog` and TIR's `Context` mutation/publishing layer.

use tir_egglog::{Database, RuleCollection, RunReport, Schedule};

use super::generated::{OperationId, Program, UseOwner};
use super::publish;
use crate::passes::common::{UseSite, resolve_layout_queries, rule_error};
use crate::{Context, OpId, PassError, RegionId, ValueId};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EditSummary;

pub(crate) struct Session<'ctx> {
    context: &'ctx Context,
    db: Database,
    error: Option<PassError>,
}

impl<'ctx> Session<'ctx> {
    pub(super) fn new(context: &'ctx Context) -> Result<Self, PassError> {
        let db = context.eqsat_database().map_err(rule_error)?;
        Ok(Self {
            context,
            db,
            error: None,
        })
    }

    pub(super) fn finish(self) -> Result<(), PassError> {
        match self.error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn run(&mut self, f: impl FnOnce(&Context, &mut Database) -> Result<(), PassError>) {
        if self.error.is_none()
            && let Err(err) = f(self.context, &mut self.db)
        {
            self.error = Some(err);
        }
    }
}

fn use_site(owner: UseOwner, slot: i64) -> Result<UseSite, PassError> {
    let slot = usize::try_from(slot).map_err(rule_error)?;
    Ok(match owner {
        UseOwner::Operand(op) => UseSite::Operand(OpId::from(op), slot),
        UseOwner::Yield(region) => UseSite::Yield(
            RegionId::from_number(u32::try_from(region).map_err(rule_error)?),
            slot,
        ),
    })
}

fn load_and_seed(context: &Context, root: OperationId, db: &mut Database) -> Result<(), PassError> {
    let mut program = Program::load(db).map_err(rule_error)?;
    program.insert_eq_sat_root((root,)).map_err(rule_error)?;
    for kind in context.kinds_implementing::<dyn crate::ConstantLike>() {
        program
            .insert_eq_sat_constant_implementation((kind.into(),))
            .map_err(rule_error)?;
    }
    Ok(())
}

pub(super) fn seed_root(session: &mut Session<'_>, root: OperationId) {
    session.run(|ctx, db| load_and_seed(ctx, root, db));
}

pub(super) fn run_stage(session: &mut Session<'_>, schedule: Schedule) {
    session.run(|_, db| {
        let mut program = Program::load(db).map_err(rule_error)?;
        program.run_schedule(&schedule).map_err(rule_error)?;
        Ok(())
    });
}

pub(super) fn resolve_layouts(session: &mut Session<'_>) {
    session.run(|context, db| {
        let mut program = Program::load(db).map_err(rule_error)?;
        let requests = program
            .eq_sat_layout_query()
            .map(|row| {
                let (key, kind) = row.map_err(rule_error)?;
                Ok((OpId::from(key), kind.declaration()))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        let answers = resolve_layout_queries(context, &requests)?;
        for (id, width) in answers.pointer_sizes {
            program
                .insert_eq_sat_pointer_size((id.into(), width))
                .map_err(rule_error)?;
        }
        for (id, size, align) in answers.store_layouts {
            program
                .insert_eq_sat_store_layout((id.into(), size, align))
                .map_err(rule_error)?;
        }
        for id in answers.scoped_ptradds {
            program
                .insert_eq_sat_layout_scoped((id.into(),))
                .map_err(rule_error)?;
        }
        for (id, bits) in answers.const_values {
            program
                .insert_eq_sat_layout_value((id.into(), bits))
                .map_err(rule_error)?;
        }
        Ok(())
    });
}

pub(super) fn saturate(
    session: &mut Session<'_>,
    schedule: Schedule,
    phase: String,
    extras: &[RuleCollection],
) {
    session.run(|_, db| {
        let mut program = Program::load(db).map_err(rule_error)?;
        let mut loaded = extras
            .iter()
            .map(|rules| program.load_rules(rules).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        let mut refs = loaded
            .iter_mut()
            .map(|(prog, name)| (prog, *name))
            .collect::<Vec<_>>();
        program
            .run_schedule_with_after(&schedule, &phase, &mut refs, |_| Ok(RunReport::default()))
            .map_err(rule_error)?;
        Ok(())
    });
}

pub(super) fn run_collection(session: &mut Session<'_>, rules: &RuleCollection, rounds: i64) {
    session.run(|_, db| {
        let mut program = Program::load(db).map_err(rule_error)?;
        let (mut loaded, name) = program.load_rules(rules).map_err(rule_error)?;
        let n = usize::try_from(rounds).unwrap_or(64);
        program
            .run_rules(&mut loaded, &Schedule::repeat(n, name))
            .map_err(rule_error)?;
        Ok(())
    });
}

pub(super) fn publish_rewrites(session: &mut Session<'_>) -> EditSummary {
    session.run(|context, db| {
        let program = Program::load(db).map_err(rule_error)?;
        let custom_defs = program
            .eq_sat_definition()
            .map(|row| {
                let (op, result, expr) = row.map_err(rule_error)?;
                Ok((OpId::from(op), ValueId::from(result), expr))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        let edits = program
            .eq_sat_alias_edit()
            .map(|row| {
                let (owner, slot, old, new) = row.map_err(rule_error)?;
                Ok((
                    use_site(owner, slot)?,
                    ValueId::from(old),
                    ValueId::from(new),
                ))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        let local = program
            .eq_sat_scoped_edit()
            .map(|row| {
                let (owner, slot, old, bits) = row.map_err(rule_error)?;
                Ok((use_site(owner, slot)?, ValueId::from(old), bits))
            })
            .collect::<Result<Vec<_>, PassError>>()?;
        let mut redirected = program
            .eq_sat_redirect()
            .map(|row| row.map(|(old, _)| ValueId::from(old)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        let mut erase = program
            .eq_sat_erase()
            .map(|row| row.map(|(id,)| OpId::from(id)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pinned_vals = program
            .eq_sat_pinned()
            .map(|row| row.map(|(v,)| ValueId::from(v)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pinned_ops = program
            .eq_sat_pinned_op()
            .map(|row| row.map(|(id,)| OpId::from(id)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        let live_ops = program
            .eq_sat_live()
            .map(|row| row.map(|(id,)| OpId::from(id)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        let mut lifted = program
            .expr_lifted()
            .map(|row| row.map(|(v,)| ValueId::from(v)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        for row in program.eq_sat_load_op() {
            row.map_err(rule_error)?;
        }
        drop(program);

        redirected.sort_unstable();
        erase.sort_unstable();
        pinned_vals.sort_unstable();
        pinned_ops.sort_unstable();
        lifted.sort_unstable();

        publish::publish(
            context,
            db,
            custom_defs,
            edits,
            local,
            &redirected,
            &erase,
            &pinned_vals,
            &pinned_ops,
            live_ops,
            &lifted,
        )
    });
    EditSummary
}
