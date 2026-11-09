//! The operation library ABI, checked after ordinary CDSL type checking.
use std::collections::BTreeMap;

use crate::CheckedModule;
use crate::core::Definition;
use crate::diagnostic::{Diagnostic, Phase};
use crate::types::Type;

/// A completed operation contract. Rust binding names are selected separately.
pub struct CheckedOperation<'a> {
    pub definition: &'a Definition,
    pub binding: &'a str,
    pub operands: Vec<(String, Type)>,
    pub result: Type,
    pub attributes: BTreeMap<String, Type>,
    pub environment: BTreeMap<String, Type>,
}

fn record(ty: &Type) -> Option<&BTreeMap<String, Type>> {
    match ty {
        Type::Record(fields, None) => Some(fields),
        Type::Named(payload) => record(payload),
        _ => None,
    }
}

/// Check the operation ABI against the inferred types of the actual record and functions.
/// The core compiler has no knowledge of these field names.
pub fn check_operation<'a>(
    module: &'a CheckedModule,
    binding: &'a str,
) -> Result<CheckedOperation<'a>, Diagnostic> {
    operation_contract(module, binding, true)
}
fn operation_contract<'a>(
    module: &'a CheckedModule,
    binding: &'a str,
    validate_callbacks: bool,
) -> Result<CheckedOperation<'a>, Diagnostic> {
    let definition = module
        .definition(binding)
        .ok_or_else(|| Diagnostic::new(binding, 0..0, Phase::Type, "unknown operation export"))?;
    contract_for_definition(definition, binding, validate_callbacks)
}
fn contract_for_definition<'a>(
    definition: &'a Definition,
    binding: &'a str,
    validate_callbacks: bool,
) -> Result<CheckedOperation<'a>, Diagnostic> {
    let error = |message: String| {
        Diagnostic::new(
            &definition.source,
            definition.term.span.clone(),
            Phase::Type,
            format!("operation `{binding}`: {message}"),
        )
    };
    let fields = record(&definition.scheme.ty)
        .ok_or_else(|| error("export must be a completed finite record".into()))?;
    let Some(Type::Description(signature)) = fields.get("signature") else {
        return Err(error("signature must be a checked type description".into()));
    };
    let Type::Function(operands, result) = signature.as_ref() else {
        return Err(error("signature must describe a function".into()));
    };
    let descriptions = |name: &str| -> Result<BTreeMap<String, Type>, Diagnostic> {
        let Some(ty) = fields.get(name) else {
            return Ok(BTreeMap::new());
        };
        let row = record(ty).ok_or_else(|| error(format!("{name} must be a finite record")))?;
        row.iter()
            .map(|(label, descriptor)| {
                let decoded = match descriptor {
                    Type::Description(ty) => ty.as_ref(),
                    Type::Record(parts, None) => {
                        if let Some(unknown) = parts.keys().find(|key| {
                            !["ty", "default", "codec", "vocabulary"].contains(&key.as_str())
                        }) {
                            return Err(error(format!(
                                "unknown {name}.{label} descriptor field `{unknown}`"
                            )));
                        }
                        let Some(Type::Description(ty)) = parts.get("ty") else {
                            return Err(error(format!(
                                "{name}.{label} requires a type description"
                            )));
                        };
                        if let Some(default) = parts.get("default")
                            && default != ty.as_ref()
                        {
                            return Err(error(format!(
                                "{name}.{label} default has type {default}, expected {ty}"
                            )));
                        }
                        if let Some(codec) = parts.get("codec")
                            && codec
                                != &Type::Nominal(
                                    "ops::Attribute".into(),
                                    vec![ty.as_ref().clone()],
                                )
                        {
                            return Err(error(format!(
                                "{name}.{label} codec does not describe {ty}"
                            )));
                        }
                        ty.as_ref()
                    }
                    _ => return Err(error(format!("{name}.{label} requires a type description"))),
                };
                if name == "attributes"
                    && !matches!(
                        decoded,
                        Type::Bool | Type::Integer | Type::Nat | Type::String
                    )
                    && !record(descriptor).is_some_and(|parts| parts.contains_key("codec"))
                {
                    return Err(error(format!(
                        "{name}.{label} requires an Attribute<{decoded}> codec"
                    )));
                }
                Ok((label.clone(), decoded.clone()))
            })
            .collect()
    };
    let allowed = [
        "signature",
        "attributes",
        "environment",
        "exec",
        "poison",
        "requires",
        "verify",
        "interfaces",
        "regions",
        "bindings",
        "assembly",
    ];
    if let Some(unknown) = fields.keys().find(|name| !allowed.contains(&name.as_str())) {
        return Err(error(format!("unknown operation field `{unknown}`")));
    }
    let nominal = |name: &str| Type::Nominal(format!("ops::{name}"), Vec::new());
    if let Some(verify) = fields.get("verify") {
        let expected = Type::Function(
            vec![
                ("operation".into(), nominal("Operation")),
                ("context".into(), nominal("Context")),
            ],
            Box::new(nominal("Validation")),
        );
        if !equivalent(verify, &expected) {
            return Err(error(format!(
                "verify has type {verify}, expected {expected}"
            )));
        }
    }
    if let Some(interfaces) = fields.get("interfaces")
        && interfaces != &Type::List(Box::new(nominal("Interface")))
    {
        return Err(error(
            "interfaces must be checked Interface host bindings".into(),
        ));
    }
    if let Some(regions) = fields.get("regions") {
        let row =
            record(regions).ok_or_else(|| error("regions must be keyed by their names".into()))?;
        for (name, region) in row {
            let descriptor =
                record(region).ok_or_else(|| error(format!("region `{name}` must be a record")))?;
            if descriptor.get("kind") != Some(&nominal("RegionKind"))
                || descriptor
                    .get("variadic")
                    .is_some_and(|ty| ty != &Type::Bool)
                || descriptor
                    .keys()
                    .any(|key| !["kind", "variadic"].contains(&key.as_str()))
            {
                return Err(error(format!(
                    "region `{name}` requires kind: RegionKind and optional variadic: Bool"
                )));
            }
        }
    }
    if let Some(assembly) = fields.get("assembly") {
        let assembly = record(assembly)
            .ok_or_else(|| error("assembly must be a print/parse binding record".into()))?;
        let library = |name: &str| Type::Nominal(format!("library::{name}"), Vec::new());
        let print = Type::Function(
            vec![
                ("operation".into(), nominal("Operation")),
                ("formatter".into(), library("Formatter")),
            ],
            Box::new(library("PrintResult")),
        );
        let parse = Type::Function(
            vec![
                ("parser".into(), library("Parser")),
                ("context".into(), nominal("Context")),
                ("definition".into(), library("Definition")),
            ],
            Box::new(library("ParseResult")),
        );
        if assembly.len() != 2
            || !assembly
                .get("print")
                .is_some_and(|ty| equivalent(ty, &print))
            || !assembly
                .get("parse")
                .is_some_and(|ty| equivalent(ty, &parse))
        {
            return Err(error(
                "assembly callbacks do not implement the print/parse ABI".into(),
            ));
        }
    }
    if let Some(bindings) = fields.get("bindings") {
        let bindings = record(bindings)
            .ok_or_else(|| error("bindings must be a typed binding record".into()))?;
        let term = Type::Nominal("library::BindTerm".into(), Vec::new());
        let chain = Type::Record(
            BTreeMap::from([
                ("name".into(), Type::String),
                ("terms".into(), Type::List(Box::new(term.clone()))),
            ]),
            None,
        );
        if bindings.get("kind") != Some(&Type::Nominal("library::BindKind".into(), Vec::new()))
            || bindings.get("predicate") != Some(&term)
            || bindings.get("chains") != Some(&Type::List(Box::new(chain)))
            || bindings
                .keys()
                .any(|key| !["kind", "predicate", "chains", "counted"].contains(&key.as_str()))
        {
            return Err(error(
                "bindings do not implement the BindKind/BindTerm chain ABI".into(),
            ));
        }
        if let Some(counted) = bindings.get("counted") {
            let expected = Type::Record(
                BTreeMap::from([
                    ("induction".into(), Type::Nat),
                    ("lb".into(), Type::String),
                    ("ub".into(), Type::String),
                    ("step".into(), Type::String),
                ]),
                None,
            );
            if validate_callbacks && counted != &expected {
                return Err(error(
                    "counted binding requires induction:Nat and lb/ub/step labels".into(),
                ));
            }
        }
    }
    let attributes = descriptions("attributes")?;
    let environment = descriptions("environment")?;
    let mut arguments = Vec::new();
    if !environment.is_empty() {
        arguments.push((
            "environment".into(),
            Type::Record(environment.clone(), None),
        ));
    }
    if !attributes.is_empty() {
        arguments.push(("self".into(), Type::Record(attributes.clone(), None)));
    }
    arguments.extend(operands.iter().cloned());
    let same_arguments = |actual: &[(String, Type)]| {
        actual.len() == arguments.len()
            && actual
                .iter()
                .zip(&arguments)
                .all(|((_, a), (_, b))| equivalent(a, b))
    };
    for (field, expected_result) in [
        ("exec", result.as_ref()),
        ("poison", &Type::Bool),
        ("requires", &Type::Bool),
    ] {
        if let Some(actual) = fields.get(field) {
            let Type::Function(parameters, actual_result) = actual else {
                return Err(error(format!(
                    "{field} must be a checked function or typed host model"
                )));
            };
            if validate_callbacks
                && (!same_arguments(parameters)
                    || !equivalent(actual_result.as_ref(), expected_result))
            {
                let expected = Type::Function(arguments.clone(), Box::new(expected_result.clone()));
                return Err(error(format!(
                    "{field} has type {actual}, expected {expected}"
                )));
            }
        }
    }
    Ok(CheckedOperation {
        definition,
        binding,
        operands: operands.clone(),
        result: result.as_ref().clone(),
        attributes,
        environment,
    })
}

fn equivalent(left: &Type, right: &Type) -> bool {
    match (left, right) {
        (Type::Function(a, ar), Type::Function(b, br)) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|((_, a), (_, b))| equivalent(a, b))
                && equivalent(ar, br)
        }
        _ => left == right,
    }
}

/// Whether a checked declaration belongs to a Dialect namespace.
pub fn is_operation(module: &CheckedModule, binding: &str) -> bool {
    module
        .definition(binding)
        .and_then(|definition| definition.namespace.as_deref())
        .and_then(|namespace| module.nominal_definition(namespace))
        .is_some_and(|namespace| {
            namespace.category == Type::Nominal("ops::Dialect".into(), Vec::new())
        })
        && matches!(module.type_of(binding), Some(Type::Named(_)))
}

fn requirement(binding: &str, definition: &Definition) -> Result<(String, Type), Diagnostic> {
    let operation = contract_for_definition(definition, binding, false)?;
    let mut arguments = Vec::new();
    if !operation.environment.is_empty() {
        arguments.push((
            "env".into(),
            Type::Record(operation.environment.clone(), None),
        ));
    }
    if !operation.attributes.is_empty() {
        arguments.push((
            "self".into(),
            Type::Record(operation.attributes.clone(), None),
        ));
    }
    arguments.extend(operation.operands.iter().cloned());
    let mut fields = record(&operation.definition.scheme.ty)
        .cloned()
        .ok_or_else(|| {
            Diagnostic::new(binding, 0..0, Phase::Type, "operation record disappeared")
        })?;
    if let Some(Type::Record(bindings, None)) = fields.get_mut("bindings")
        && bindings.contains_key("counted")
    {
        bindings.insert(
            "counted".into(),
            Type::Record(
                BTreeMap::from([
                    ("induction".into(), Type::Nat),
                    ("lb".into(), Type::String),
                    ("ub".into(), Type::String),
                    ("step".into(), Type::String),
                ]),
                None,
            ),
        );
    }
    for (name, result) in [
        ("exec", &operation.result),
        ("poison", &Type::Bool),
        ("requires", &Type::Bool),
    ] {
        if fields.contains_key(name) {
            fields.insert(
                name.into(),
                Type::Function(arguments.clone(), Box::new(result.clone())),
            );
        }
    }
    Ok((
        binding.into(),
        Type::Named(Box::new(Type::Record(fields, None))),
    ))
}
/// Compile and complete the operation ABI before admitting unresolved receiver inference.
pub fn compile_operations(sources: &crate::SourceMap) -> Result<CheckedModule, Vec<Diagnostic>> {
    crate::compile_with_requirements(sources, |definitions, nominals| {
        definitions
            .iter()
            .filter(|(_, definition)| {
                definition
                    .namespace
                    .as_deref()
                    .and_then(|name| nominals.get(name))
                    .is_some_and(|namespace| {
                        namespace.category == Type::Nominal("ops::Dialect".into(), Vec::new())
                    })
            })
            .map(|(name, definition)| requirement(name, definition).map_err(|error| vec![error]))
            .collect()
    })
}
/// Complete an already checked module through the same library ABI.
pub fn complete_operations(module: CheckedModule) -> Result<CheckedModule, Vec<Diagnostic>> {
    let requirements = module
        .bindings()
        .filter(|(name, _)| is_operation(&module, name))
        .map(|(name, _)| {
            module
                .definition(name)
                .ok_or_else(|| {
                    vec![Diagnostic::new(
                        name,
                        0..0,
                        Phase::Type,
                        "operation definition disappeared",
                    )]
                })
                .and_then(|definition| requirement(name, definition).map_err(|error| vec![error]))
        })
        .collect::<Result<Vec<_>, _>>()?;
    module.complete(&requirements)
}
