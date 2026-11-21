mod publish;

#[allow(dead_code)] // The generator emits readers for every CDSL export.
pub(crate) mod generated {
    include!(concat!(env!("OUT_DIR"), "/eqsat.rs"));
}

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

use tir_egglog::{Color, Database};

use crate::{
    AnalysisManager, Context, OpId, Pass, PassError, RegionId, ValueId, attributes::AttributeValue,
};

/// Saturate integer equalities in a private snapshot of the current IR.
#[derive(Default)]
pub struct EqSatPass;

#[linkme::distributed_slice(crate::PASSES)]
static EQSAT: crate::PassInfo = crate::PassInfo {
    name: "eqsat",
    ctor: parse_eqsat,
};

fn parse_eqsat(args: &str) -> Result<Box<dyn Pass>, String> {
    if args.is_empty() {
        Ok(Box::new(EqSatPass))
    } else {
        Err("pass 'eqsat' takes no arguments".into())
    }
}

fn rule_error(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(error.to_string())
}

impl Pass for EqSatPass {
    fn name(&self) -> &'static str {
        "eqsat"
    }

    fn run(
        &mut self,
        root_id: OpId,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let mut db = context.eqsat_database().map_err(rule_error)?;
        let mut program = snapshot(context, root_id, &mut db)?;
        program.analyze().map_err(rule_error)?;
        program.seed_assumptions().map_err(rule_error)?;
        let local = scoped_literals(&mut program)?;
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
                let site = use_site(owner, slot)?;
                Ok((site, ValueId::from(old), ValueId::from(new)))
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
        publish::publish(context, rebuild, edits, local, &redirected, &erase)?;
        if context.has_operation(root_id) {
            sweep(context, root_id)?;
        }
        Ok(())
    }
}

fn snapshot<'db>(
    context: &Context,
    root: OpId,
    db: &'db mut Database,
) -> Result<generated::Program<'db>, PassError> {
    let mut program = generated::Program::load(db).map_err(rule_error)?;
    program
        .insert_eq_sat_root((root.into(),))
        .map_err(rule_error)?;
    for kind in context.kinds_implementing::<dyn crate::ConstantLike>() {
        program
            .insert_eq_sat_constant_implementation((kind.into(),))
            .map_err(rule_error)?;
    }
    program.prepare().map_err(rule_error)?;
    layout_values(context, &mut program)?;
    Ok(program)
}

fn sweep(context: &Context, root: OpId) -> Result<(), PassError> {
    let mut db = context.eqsat_database().map_err(rule_error)?;
    let mut program = snapshot(context, root, &mut db)?;
    program.sweep().map_err(rule_error)?;
    let removals = program
        .eq_sat_dead()
        .map(|row| {
            row.map(|(id,)| context.get_op(OpId::from(id)))
                .map_err(rule_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    publish::erase_operations(context, removals)
}

fn scoped_literals(program: &mut generated::Program<'_>) -> Result<Vec<publish::Local>, PassError> {
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
        program.propagate_scoped().map_err(rule_error)?;
    }
    let mut edits = Vec::new();
    for color in std::iter::once(Color::BASE).chain(colors) {
        for row in program.eq_sat_scoped_edit_in(color) {
            let (owner, slot, old, bits) = row.map_err(rule_error)?;
            let site = use_site(owner, slot)?;
            let old = ValueId::from(old);
            edits.push((site, old, bits));
        }
    }
    Ok(edits)
}

fn use_site(owner: generated::UseOwner, slot: i64) -> Result<publish::UseSite, PassError> {
    let slot = usize::try_from(slot).map_err(rule_error)?;
    Ok(match owner {
        generated::UseOwner::Operand(op) => publish::UseSite::Operand(OpId::from(op), slot),
        generated::UseOwner::Yield(region) => publish::UseSite::Yield(
            RegionId::from_number(u32::try_from(region).map_err(rule_error)?),
            slot,
        ),
    })
}

fn layout_values(context: &Context, program: &mut generated::Program<'_>) -> Result<(), PassError> {
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
