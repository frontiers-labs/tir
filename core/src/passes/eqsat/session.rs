//! Thin runtime shim bridging CDSL v3's EqSat pass drivers to `tir_egglog`
//! and TIR's `Context` mutation/publishing layer.

use tir_egglog::{Color, Database, RuleCollection, Schedule};

use super::generated::{self, OperationId, Program, UseOwner};
use super::publish;
use crate::{Context, OpId, PassError, RegionId, ValueId, attributes::AttributeValue};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EditSummary;

pub(crate) struct Session<'ctx> {
    context: &'ctx Context,
    db: Database,
    scoped: Vec<publish::Local>,
    error: Option<PassError>,
}

impl<'ctx> Session<'ctx> {
    pub(super) fn new(context: &'ctx Context) -> Result<Self, PassError> {
        let db = context.eqsat_database().map_err(rule_error)?;
        Ok(Self {
            context,
            db,
            scoped: Vec::new(),
            error: None,
        })
    }

    pub(super) fn finish(self) -> Result<(), PassError> {
        match self.error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn run(
        &mut self,
        f: impl FnOnce(&Context, &mut Database, &mut Vec<publish::Local>) -> Result<(), PassError>,
    ) {
        if self.error.is_none()
            && let Err(err) = f(self.context, &mut self.db, &mut self.scoped)
        {
            self.error = Some(err);
        }
    }
}

fn rule_error(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(error.to_string())
}

fn analysis_error(error: Box<dyn std::error::Error>) -> tir_egglog::Error {
    tir_egglog::Error::Unsupported(error.to_string())
}

fn use_site(owner: UseOwner, slot: i64) -> Result<publish::UseSite, PassError> {
    let slot = usize::try_from(slot).map_err(rule_error)?;
    Ok(match owner {
        UseOwner::Operand(op) => publish::UseSite::Operand(OpId::from(op), slot),
        UseOwner::Yield(region) => publish::UseSite::Yield(
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
    session.run(|ctx, db, _| load_and_seed(ctx, root, db));
}

pub(super) fn run_stage(session: &mut Session<'_>, schedule: Schedule) {
    session.run(|_, db, _| {
        let mut program = Program::load(db).map_err(rule_error)?;
        program.run_schedule(&schedule).map_err(rule_error)?;
        Ok(())
    });
}

pub(super) fn resolve_layouts(session: &mut Session<'_>) {
    session.run(|context, db, _| {
        let mut program = Program::load(db).map_err(rule_error)?;
        resolve_layout_queries(context, &mut program)
    });
}

pub(super) fn saturate_with_masks(
    session: &mut Session<'_>,
    schedule: Schedule,
    phase: String,
    extras: &[RuleCollection],
    apply_masks: Schedule,
) {
    session.run(|_, db, _| {
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
            .run_schedule_with_after(&schedule, &phase, &mut refs, |db| {
                let result = generated::known_zeros(db).map_err(analysis_error)?;
                Program::load_with_tables(db, &result.bindings())
                    .map_err(analysis_error)?
                    .run_schedule(&apply_masks)
            })
            .map_err(rule_error)?;
        Ok(())
    });
}

pub(super) fn run_collection(session: &mut Session<'_>, rules: &RuleCollection, rounds: i64) {
    session.run(|_, db, _| {
        let mut program = Program::load(db).map_err(rule_error)?;
        let (mut loaded, name) = program.load_rules(rules).map_err(rule_error)?;
        let n = usize::try_from(rounds).unwrap_or(64);
        program
            .run_rules(&mut loaded, &Schedule::repeat(n, name))
            .map_err(rule_error)?;
        Ok(())
    });
}

pub(super) fn propagate_scoped(session: &mut Session<'_>, schedule: Schedule) {
    session.run(|_, db, scoped| {
        let mut program = Program::load(db).map_err(rule_error)?;
        let mut seeds = program
            .eq_sat_color_seed_classes(Color::BASE)
            .map(|row| row.map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        seeds.sort_unstable_by_key(|row| row.0);
        let mut colors = Vec::new();
        for group in seeds.chunk_by(|a, b| a.0 == b.0) {
            let Some(&(scope, _, _)) = group.first() else {
                continue;
            };
            let color = program.color(&[Color::BASE]);
            colors.push(color);
            program
                .insert_eq_sat_color_site_in(color, (scope,))
                .map_err(rule_error)?;
            for &(_, old, literal) in group {
                program
                    .insert_eq_sat_color_subject_in(color, (old,))
                    .map_err(rule_error)?;
                program.union_in(color, old, literal).map_err(rule_error)?;
            }
        }
        if !colors.is_empty() {
            program.rebuild().map_err(rule_error)?;
            program.run_schedule(&schedule).map_err(rule_error)?;
        }
        for color in std::iter::once(Color::BASE).chain(colors) {
            for row in program.eq_sat_scoped_edit_in(color) {
                let (owner, slot, old, bits) = row.map_err(rule_error)?;
                scoped.push((use_site(owner, slot)?, ValueId::from(old), bits));
            }
        }
        Ok(())
    });
}

pub(super) fn publish_rewrites(session: &mut Session<'_>) -> EditSummary {
    session.run(|context, db, scoped| {
        let program = Program::load(db).map_err(rule_error)?;
        let rebuild = program
            .eq_sat_definition()
            .map(|row| {
                let (op, result, form) = row.map_err(rule_error)?;
                Ok((OpId::from(op), ValueId::from(result), form))
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
        let mut redirected = program
            .eq_sat_redirect()
            .map(|row| row.map(|(old, _)| ValueId::from(old)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        let mut erase = program
            .eq_sat_erase()
            .map(|row| row.map(|(id,)| OpId::from(id)).map_err(rule_error))
            .collect::<Result<Vec<_>, _>>()?;
        redirected.sort_unstable();
        erase.sort_unstable();
        let local = std::mem::take(scoped);
        publish::publish(context, rebuild, edits, local, &redirected, &erase)
    });
    EditSummary
}

pub(super) fn sweep_dead(
    session: &mut Session<'_>,
    root: OperationId,
    prepare: Schedule,
    sweep: Schedule,
) -> EditSummary {
    session.run(|context, _, _| {
        let root_id = OpId::from(root);
        if !context.has_operation(root_id) {
            return Ok(());
        }
        let mut db = context.eqsat_database().map_err(rule_error)?;
        load_and_seed(context, root, &mut db)?;
        let mut program = Program::load(&mut db).map_err(rule_error)?;
        program.run_schedule(&prepare).map_err(rule_error)?;
        resolve_layout_queries(context, &mut program)?;
        program.run_schedule(&sweep).map_err(rule_error)?;
        let removals = program
            .eq_sat_dead()
            .map(|row| {
                row.map(|(id,)| context.get_op(OpId::from(id)))
                    .map_err(rule_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        publish::erase_operations(context, removals)
    });
    EditSummary
}

fn resolve_layout_queries(context: &Context, program: &mut Program<'_>) -> Result<(), PassError> {
    let requests = program
        .eq_sat_layout_query()
        .map(|row| row.map_err(rule_error))
        .collect::<Result<Vec<_>, _>>()?;
    for (key, kind) in requests {
        let id = OpId::from(key);
        if context.operation_kind(id) != Some(kind.declaration()) {
            return Err(PassError::InvalidRuleSet(
                "layout query has the wrong operation declaration".into(),
            ));
        }
        let Some(layout) = crate::layout::DataLayout::for_op(context, id) else {
            continue;
        };
        let op = context.get_op(id);
        if op.is::<crate::ptr::ops::StoreOp>()
            || op.is::<crate::ptr::ops::LoadOp>()
            || op.is::<crate::ptr::ops::PtrAddOp>()
        {
            if let Some(width) = layout.pointer_size().filter(|&width| width != 0) {
                program
                    .insert_eq_sat_pointer_size((key, i64::from(width)))
                    .map_err(rule_error)?;
            }
            if op.is::<crate::ptr::ops::StoreOp>()
                && let Some(data) = op.operands().first()
                && let Some(access) = layout.layout_of(context.get_value(*data).ty())
                && let Ok(size) = access.size.to_string().parse::<tir_egglog::sort::BigInt>()
                && let Ok(align) = access.align.to_string().parse::<tir_egglog::sort::BigInt>()
            {
                program
                    .insert_eq_sat_store_layout((key, size, align))
                    .map_err(rule_error)?;
            }
            if op.is::<crate::ptr::ops::PtrAddOp>()
                && let Some(offset) = op.operands().get(1)
                && let Some(ty) =
                    crate::builtin::IntegerType::view(context, context.get_value(*offset).ty())
                && layout.index_width().map(u64::from) == Some(ty.width())
            {
                program
                    .insert_eq_sat_layout_scoped((key,))
                    .map_err(rule_error)?;
            }
            continue;
        }
        let Some(AttributeValue::Type(ty)) = op.attr("ty") else {
            continue;
        };
        let answer = if op.is::<crate::layout::ops::SizeOfOp>() {
            layout.layout_of(ty).map(|value| value.size)
        } else if op.is::<crate::layout::ops::AlignOfOp>() {
            layout.layout_of(ty).map(|value| value.align)
        } else if op.is::<crate::layout::ops::OffsetOfOp>() {
            op.attr("field")
                .and_then(|field| layout.offset_in_bits(ty, field.as_str()?))
        } else {
            None
        };
        if let Some(bits) =
            answer.and_then(|value| value.to_string().parse::<tir_egglog::sort::BigInt>().ok())
        {
            program
                .insert_eq_sat_layout_value((key, bits))
                .map_err(rule_error)?;
        }
    }
    Ok(())
}
