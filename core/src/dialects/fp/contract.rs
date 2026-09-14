use std::collections::BTreeMap;

use crate::attributes::AttributeValue;
use crate::{Context, Error, TypeId};

use super::{ArithmeticSemantics, Semantics};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EvaluationContract {
    pub arithmetic: ArithmeticSemantics,
    pub permissions: TransformPermissions,
    pub assumptions: ScopedFacts,
    pub accuracy: AccuracyRequirement,
    pub result_formats: Vec<TypeId>,
    pub environment_epoch: u64,
    pub intermediate_exceptions: IntermediateExceptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransformPermissions {
    pub expression_contraction: bool,
    pub cross_statement_contraction: bool,
    pub reassociation: bool,
    pub reciprocal: bool,
    pub approved_approximation: bool,
    pub ignore_signed_zero: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScopedFacts {
    pub finite: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntermediateExceptions {
    Preserve,
    AllowContractedGroupRemoval,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AccuracyRequirement {
    Reference,
    CorrectlyRounded,
    Bounded(ErrorBound),
    ApprovedAlgorithms(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ErrorBound {
    pub reference: String,
    pub metric: String,
    pub maximum_bits: u64,
    pub finite_domain: String,
    pub zero: String,
    pub subnormal: String,
    pub exceptional: String,
}

impl EvaluationContract {
    pub fn parse_attribute(value: &AttributeValue) -> Result<Self, Error> {
        let fields = dict(value, "contract")?;
        exact(
            fields,
            &[
                "accuracy",
                "arithmetic",
                "assumptions",
                "environment_epoch",
                "intermediate_exceptions",
                "permissions",
                "result_formats",
            ],
        )?;
        let arithmetic = Semantics::parse_attribute(required(fields, "arithmetic")?)?
            .arithmetic()
            .copied()?;
        let permissions = dict(required(fields, "permissions")?, "permissions")?;
        exact(
            permissions,
            &[
                "approved_approximation",
                "cross_statement_contraction",
                "expression_contraction",
                "ignore_signed_zero",
                "reassociation",
                "reciprocal",
            ],
        )?;
        let assumptions = dict(required(fields, "assumptions")?, "assumptions")?;
        exact(assumptions, &["finite"])?;
        let formats = array(required(fields, "result_formats")?, "result_formats")?
            .iter()
            .map(|value| match value {
                AttributeValue::Type(ty) => Ok(*ty),
                _ => Err(invalid("contract result_formats entries must be types")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let intermediate_exceptions = match string(fields, "intermediate_exceptions")? {
            "preserve" => IntermediateExceptions::Preserve,
            "allow_contracted_group_removal" => IntermediateExceptions::AllowContractedGroupRemoval,
            _ => return Err(invalid("unknown intermediate exception contract")),
        };
        Ok(Self {
            arithmetic,
            permissions: TransformPermissions {
                expression_contraction: boolean(permissions, "expression_contraction")?,
                cross_statement_contraction: boolean(permissions, "cross_statement_contraction")?,
                reassociation: boolean(permissions, "reassociation")?,
                reciprocal: boolean(permissions, "reciprocal")?,
                approved_approximation: boolean(permissions, "approved_approximation")?,
                ignore_signed_zero: boolean(permissions, "ignore_signed_zero")?,
            },
            assumptions: ScopedFacts {
                finite: boolean(assumptions, "finite")?,
            },
            accuracy: parse_accuracy(required(fields, "accuracy")?)?,
            result_formats: formats,
            environment_epoch: unsigned(fields, "environment_epoch")?,
            intermediate_exceptions,
        })
    }

    pub(crate) fn print(
        &self,
        fmt: &mut crate::IRFormatter<'_>,
        context: &Context,
    ) -> Result<(), std::fmt::Error> {
        self.attribute().print(fmt, context)
    }

    fn attribute(&self) -> AttributeValue {
        let string = |value: &str| AttributeValue::Str(value.into());
        let dict = |entries: Vec<(&str, AttributeValue)>| {
            AttributeValue::Dict(Box::new(
                entries.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            ))
        };
        let arithmetic = Semantics::Arithmetic(self.arithmetic)
            .fields()
            .into_iter()
            .map(|(k, v)| (k.into(), string(v)))
            .collect();
        let p = self.permissions;
        dict(vec![
            ("accuracy", accuracy_attribute(&self.accuracy)),
            ("arithmetic", AttributeValue::Dict(Box::new(arithmetic))),
            (
                "assumptions",
                dict(vec![("finite", self.assumptions.finite.into())]),
            ),
            ("environment_epoch", self.environment_epoch.into()),
            (
                "intermediate_exceptions",
                string(match self.intermediate_exceptions {
                    IntermediateExceptions::Preserve => "preserve",
                    IntermediateExceptions::AllowContractedGroupRemoval => {
                        "allow_contracted_group_removal"
                    }
                }),
            ),
            (
                "permissions",
                dict(vec![
                    ("approved_approximation", p.approved_approximation.into()),
                    (
                        "cross_statement_contraction",
                        p.cross_statement_contraction.into(),
                    ),
                    ("expression_contraction", p.expression_contraction.into()),
                    ("ignore_signed_zero", p.ignore_signed_zero.into()),
                    ("reassociation", p.reassociation.into()),
                    ("reciprocal", p.reciprocal.into()),
                ]),
            ),
            (
                "result_formats",
                AttributeValue::Array(
                    self.result_formats
                        .iter()
                        .copied()
                        .map(AttributeValue::Type)
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ),
        ])
    }

    pub fn reference_is_admissible(&self) -> bool {
        matches!(self.accuracy, AccuracyRequirement::Reference)
    }
}

fn parse_accuracy(value: &AttributeValue) -> Result<AccuracyRequirement, Error> {
    let fields = dict(value, "accuracy")?;
    match string(fields, "kind")? {
        "reference" => {
            exact(fields, &["kind"])?;
            Ok(AccuracyRequirement::Reference)
        }
        "correctly_rounded" => {
            exact(fields, &["kind"])?;
            Ok(AccuracyRequirement::CorrectlyRounded)
        }
        "approved_algorithms" => {
            exact(fields, &["algorithms", "kind"])?;
            let names = array(required(fields, "algorithms")?, "algorithms")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("algorithm names must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(AccuracyRequirement::ApprovedAlgorithms(names))
        }
        "bounded" => {
            exact(
                fields,
                &[
                    "exceptional",
                    "finite_domain",
                    "kind",
                    "maximum_bits",
                    "metric",
                    "reference",
                    "subnormal",
                    "zero",
                ],
            )?;
            Ok(AccuracyRequirement::Bounded(ErrorBound {
                reference: string(fields, "reference")?.into(),
                metric: string(fields, "metric")?.into(),
                maximum_bits: unsigned(fields, "maximum_bits")?,
                finite_domain: string(fields, "finite_domain")?.into(),
                zero: string(fields, "zero")?.into(),
                subnormal: string(fields, "subnormal")?.into(),
                exceptional: string(fields, "exceptional")?.into(),
            }))
        }
        _ => Err(invalid("unknown accuracy requirement")),
    }
}

fn accuracy_attribute(value: &AccuracyRequirement) -> AttributeValue {
    let s = |v: &str| AttributeValue::Str(v.into());
    let d = |e: Vec<(&str, AttributeValue)>| {
        AttributeValue::Dict(Box::new(
            e.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        ))
    };
    match value {
        AccuracyRequirement::Reference => d(vec![("kind", s("reference"))]),
        AccuracyRequirement::CorrectlyRounded => d(vec![("kind", s("correctly_rounded"))]),
        AccuracyRequirement::ApprovedAlgorithms(names) => d(vec![
            (
                "algorithms",
                AttributeValue::Array(names.iter().map(|n| s(n)).collect::<Vec<_>>().into()),
            ),
            ("kind", s("approved_algorithms")),
        ]),
        AccuracyRequirement::Bounded(b) => d(vec![
            ("exceptional", s(&b.exceptional)),
            ("finite_domain", s(&b.finite_domain)),
            ("kind", s("bounded")),
            ("maximum_bits", b.maximum_bits.into()),
            ("metric", s(&b.metric)),
            ("reference", s(&b.reference)),
            ("subnormal", s(&b.subnormal)),
            ("zero", s(&b.zero)),
        ]),
    }
}

fn dict<'a>(
    value: &'a AttributeValue,
    name: &str,
) -> Result<&'a BTreeMap<String, AttributeValue>, Error> {
    match value {
        AttributeValue::Dict(v) => Ok(v),
        _ => Err(invalid(&format!("{name} must be a dictionary"))),
    }
}
fn array<'a>(value: &'a AttributeValue, name: &str) -> Result<&'a [AttributeValue], Error> {
    match value {
        AttributeValue::Array(v) => Ok(v),
        _ => Err(invalid(&format!("{name} must be an array"))),
    }
}
fn required<'a>(
    fields: &'a BTreeMap<String, AttributeValue>,
    name: &str,
) -> Result<&'a AttributeValue, Error> {
    fields
        .get(name)
        .ok_or_else(|| invalid(&format!("missing contract field '{name}'")))
}
fn string<'a>(fields: &'a BTreeMap<String, AttributeValue>, name: &str) -> Result<&'a str, Error> {
    required(fields, name)?
        .as_str()
        .ok_or_else(|| invalid(&format!("contract field '{name}' must be a string")))
}
fn boolean(fields: &BTreeMap<String, AttributeValue>, name: &str) -> Result<bool, Error> {
    match required(fields, name)? {
        AttributeValue::Bool(v) => Ok(*v),
        _ => Err(invalid(&format!(
            "contract field '{name}' must be a boolean"
        ))),
    }
}
fn unsigned(fields: &BTreeMap<String, AttributeValue>, name: &str) -> Result<u64, Error> {
    match required(fields, name)? {
        AttributeValue::UInt(v) => Ok(*v),
        AttributeValue::Int(v) if *v >= 0 => Ok(*v as u64),
        _ => Err(invalid(&format!(
            "contract field '{name}' must be unsigned"
        ))),
    }
}
fn exact(fields: &BTreeMap<String, AttributeValue>, names: &[&str]) -> Result<(), Error> {
    if fields.len() == names.len() && names.iter().all(|n| fields.contains_key(*n)) {
        Ok(())
    } else {
        Err(invalid("contract fields do not match the selected kind"))
    }
}
fn invalid(message: &str) -> Error {
    Error::VerificationError(message.into())
}
