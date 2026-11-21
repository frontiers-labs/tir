use crate::attributes::AttributeValue as Attr;
use crate::{Context, Error, OpId, RegionId, Sym, TypeId};
use crate::{OpHandle, OpSchema, ValueId};
use signature::Argument;
use std::fmt;

mod control;
mod fields;
pub(crate) mod signature;
mod structure;

/// Failure to typecheck the live IR in a context.
#[derive(Debug)]
pub enum TypeCheckError {
    IllTyped {
        first: TypeDiagnostic,
        rest: Vec<TypeDiagnostic>,
    },
    QueryFailed(Error),
}

/// An operation and the typing obligation it failed.
#[derive(Debug)]
pub struct TypeDiagnostic {
    pub operation: OpId,
    pub failure: TypeFailure,
    detail: String,
}

impl TypeDiagnostic {
    /// The failed obligation as reported by the typing program.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// The reason an operation failed typechecking.
#[derive(Debug)]
pub enum TypeFailure {
    MissingContract,
    MissingInput(OperandIndex),
    Mismatch {
        site: ObligationSite,
        actual: TypeId,
        expected: String,
    },
    Unsatisfied(ObligationSite),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperandIndex(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResultIndex(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObligationSite {
    Operand(OperandIndex),
    Result(ResultIndex),
    Attribute(Sym),
    Region(RegionId),
    Signature,
}

impl fmt::Display for TypeCheckError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IllTyped { first, rest } => {
                write!(
                    fmt,
                    "operation {} failed typechecking: {}",
                    first.operation.number(),
                    first.detail()
                )?;
                for diagnostic in rest {
                    write!(
                        fmt,
                        "\noperation {} failed typechecking: {}",
                        diagnostic.operation.number(),
                        diagnostic.detail()
                    )?;
                }
                Ok(())
            }
            Self::QueryFailed(error) => write!(fmt, "IR typing query failed: {error}"),
        }
    }
}

impl std::error::Error for TypeCheckError {}

#[derive(Default)]
struct Failures(Vec<&'static str>);

impl Failures {
    fn require(&mut self, valid: bool, label: &'static str) {
        if !valid && !self.0.contains(&label) {
            self.0.push(label);
        }
    }
}

struct Groups<'a> {
    spans: Vec<&'a [Option<TypeId>]>,
    complete: bool,
}

impl<'a> Groups<'a> {
    fn new(
        fields: &[crate::FieldSchema],
        ports: &'a [Option<TypeId>],
        segments: Option<&[Attr]>,
        result: bool,
    ) -> Self {
        let segmented = !result && fields.iter().any(|f| f.variadic);
        let required = |field: &crate::FieldSchema| !field.variadic && !field.ty.starts_with('?');
        let mut after = fields.iter().filter(|f| required(f)).count();
        let mut remaining = ports;
        let mut spans = Vec::with_capacity(fields.len());
        for (i, field) in fields.iter().enumerate() {
            after -= usize::from(required(field));
            let n = if segmented {
                let Some(Attr::UInt(n)) = segments.and_then(|s| s.get(i)) else {
                    break;
                };
                let Ok(n) = usize::try_from(*n) else { break };
                n
            } else if required(field) {
                1
            } else {
                let available = remaining
                    .len()
                    .saturating_sub(if result { after } else { 0 });
                if field.variadic {
                    available
                } else {
                    available.min(1)
                }
            };
            let Some((group, tail)) = remaining.split_at_checked(n) else {
                break;
            };
            spans.push(group);
            remaining = tail;
        }
        let complete = remaining.is_empty()
            && spans.len() == fields.len()
            && (!segmented || segments.is_some_and(|s| s.len() == fields.len()));
        Self { spans, complete }
    }

    fn one(&self, index: usize) -> Option<TypeId> {
        match self.spans.get(index)? {
            [Some(ty)] => Some(*ty),
            _ => None,
        }
    }

    fn list(&self, index: usize) -> Option<Vec<TypeId>> {
        self.spans.get(index)?.iter().copied().collect()
    }

    fn lists(&self) -> Option<Vec<Vec<TypeId>>> {
        self.complete.then(|| {
            self.spans
                .iter()
                .map(|span| span.iter().copied().collect())
                .collect()
        })?
    }
}

fn arguments<'a>(
    fields: &[crate::FieldSchema],
    groups: &'a [Vec<TypeId>],
) -> Option<Vec<Argument<'a>>> {
    fields
        .iter()
        .zip(groups)
        .map(|(field, group)| match group.as_slice() {
            items if field.variadic => Some(Argument::Many(items)),
            [] if field.ty.starts_with('?') => Some(Argument::Optional(None)),
            &[ty] if field.ty.starts_with('?') => Some(Argument::Optional(Some(ty))),
            &[ty] => Some(Argument::Fixed(ty)),
            _ => None,
        })
        .collect()
}

struct Check<'a> {
    ctx: &'a Context,
    op: OpHandle,
    kind: tir_egglog::NativeIdentity,
    schema: Option<&'static OpSchema>,
    inputs: Groups<'a>,
    outputs: Groups<'a>,
}

impl Check<'_> {
    fn signature(&self, schema: &OpSchema) -> Option<bool> {
        let inputs = self.inputs.lists()?;
        let outputs = self.outputs.lists()?;
        Some(
            schema
                .signature
                .substitution(
                    self.ctx.terms(),
                    &arguments(schema.operands, &inputs)?,
                    &arguments(schema.results, &outputs)?,
                )
                .is_some(),
        )
    }

    fn ty(&self, value: ValueId) -> Option<TypeId> {
        self.ctx
            .has_value(value)
            .then(|| self.ctx.get_value(value).ty())
    }

    fn check(&self, errors: &mut Failures) {
        if let Some(schema) = self.schema {
            errors.require(self.signature(schema) == Some(true), "signature");
            for field in schema.attributes {
                let (optional, kind) = field
                    .ty
                    .strip_prefix('?')
                    .map_or((false, field.ty), |ty| (true, ty));
                let valid = self.op.attr(field.name).map_or(optional, |attr| {
                    crate::operation::attr_type_matches(kind, &attr)
                        && (field.vocabulary.is_empty()
                            || matches!(attr, Attr::Predicate(p) if field.vocabulary.contains(&p)))
                });
                errors.require(valid, field.name);
            }
        } else {
            errors.require(false, "missing contract");
        }
        self.structure(errors);
        self.fields(errors);
        self.control(errors);
    }
}

fn port_types(
    ctx: &Context,
    op: OpId,
    results: bool,
    errors: &mut Failures,
) -> Vec<Option<TypeId>> {
    let id = i64::from(op.number());
    let rows: Vec<(i64, i64)> = if results {
        ctx.index_rows("Value", (&[2, 3], ("result", id)), &[4], [4, 1])
    } else {
        ctx.index_rows("Operand", (&[0], id), &[1], [1, 2])
    };
    let mut types = Vec::with_capacity(rows.len());
    let mut previous = None;
    for (slot, value) in rows {
        if results && previous == Some(slot) {
            errors.require(false, "duplicate result index");
        }
        previous = Some(slot);
        if slot < 0 {
            errors.require(false, "negative port index");
            continue;
        }
        if slot > types.len() as i64 {
            errors.require(false, "noncontiguous ports");
            continue;
        }
        if slot == types.len() as i64 {
            let ty = if results {
                Some(TypeId::from_number(value as u32))
            } else {
                let value = ValueId::from_number(value as u32);
                ctx.has_value(value).then(|| ctx.get_value(value).ty())
            };
            types.push(ty);
        }
    }
    types
}

/// Check all live operations at a completed edit boundary.
pub fn check_types(context: &Context) -> Result<(), TypeCheckError> {
    let operations: Vec<(i64, tir_egglog::NativeIdentity, i64)> = {
        let engine = context
            .typing_engine()
            .map_err(TypeCheckError::QueryFailed)?;
        let db = engine.database();
        let table = db
            .table("Operation")
            .map_err(|e| TypeCheckError::QueryFailed(Error::VerificationError(e.to_string())))?;
        db.rows(table)
            .map(|r| {
                (
                    engine.value_to_base(r[0]),
                    engine.value_to_base(r[1]),
                    engine.value_to_base(r[6]),
                )
            })
            .collect()
    };
    let mut diagnostics = Vec::new();
    for (id, kind, location) in operations {
        let id = OpId::from_number(id as u32);
        let op = context.get_op(id);
        if op.is::<crate::builtin::ModuleOp>()
            && let Err(error) = crate::SymbolTable::build(context, id).verify(context)
        {
            diagnostics.push((location, id.number(), error.to_string()));
        }
        let schema = context.schema_for_kind(kind);
        let mut errors = Failures::default();
        let inputs = port_types(context, id, false, &mut errors);
        let outputs = port_types(context, id, true, &mut errors);
        let segments = op.attr("operand_segment_sizes");
        let segments = match &segments {
            Some(Attr::Array(items)) => Some(&items[..]),
            _ => None,
        };
        let inputs = Groups::new(schema.map_or(&[], |s| s.operands), &inputs, segments, false);
        let outputs = Groups::new(schema.map_or(&[], |s| s.results), &outputs, None, true);
        let check = Check {
            ctx: context,
            op,
            kind,
            schema,
            inputs,
            outputs,
        };
        check.check(&mut errors);
        diagnostics.extend(
            errors
                .0
                .into_iter()
                .map(|label| (location, id.number(), label.to_owned())),
        );
    }
    diagnostics.sort_unstable();
    let mut diagnostics = diagnostics
        .into_iter()
        .map(|(_, id, label)| TypeDiagnostic {
            operation: OpId::from_number(id),
            failure: if label == "missing contract" {
                TypeFailure::MissingContract
            } else {
                TypeFailure::Unsatisfied(ObligationSite::Signature)
            },
            detail: label,
        });
    match diagnostics.next() {
        None => Ok(()),
        Some(first) => Err(TypeCheckError::IllTyped {
            first,
            rest: diagnostics.collect(),
        }),
    }
}
