//! Rust output for the ordinary type-definition library ABI.
use std::collections::BTreeMap;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, Path};

use crate::diagnostic::{Diagnostic, Phase};
use crate::types::Type;
use crate::{CheckedModule, Value};

fn nominal(name: &str) -> Type {
    Type::Nominal(format!("library::{name}"), Vec::new())
}

fn function(arguments: &[(&str, &str)], result: &str) -> Type {
    Type::Function(
        arguments
            .iter()
            .map(|(name, ty)| ((*name).into(), nominal(ty)))
            .collect(),
        Box::new(nominal(result)),
    )
}

fn constructor_type() -> Type {
    Type::Record(
        BTreeMap::from([
            ("dialect".into(), Type::String),
            ("name".into(), Type::String),
            (
                "params".into(),
                Type::List(Box::new(Type::Record(
                    BTreeMap::from([
                        ("name".into(), Type::String),
                        ("param".into(), nominal("Param")),
                    ]),
                    None,
                ))),
            ),
            ("kind".into(), nominal("Kind")),
            (
                "validate".into(),
                function(&[("terms", "Terms"), ("args", "TypeArgs")], "Validation"),
            ),
            (
                "carrier".into(),
                function(&[("terms", "Terms"), ("args", "TypeArgs")], "Carrier"),
            ),
        ]),
        None,
    )
}

fn spelling_type() -> Type {
    Type::Record(
        BTreeMap::from([
            (
                "ctor_id".into(),
                function(&[("context", "Context")], "CtorId"),
            ),
            ("keys".into(), Type::List(Box::new(Type::String))),
            (
                "parse".into(),
                function(
                    &[
                        ("mnemonic", "Mnemonic"),
                        ("parser", "Parser"),
                        ("context", "Context"),
                    ],
                    "ParseResult",
                ),
            ),
            (
                "print".into(),
                function(
                    &[
                        ("spellings", "Spellings"),
                        ("ty", "TypeId"),
                        ("formatter", "Formatter"),
                    ],
                    "PrintResult",
                ),
            ),
            ("variadic_tail".into(), Type::Bool),
        ]),
        None,
    )
}

fn equivalent(left: &Type, right: &Type) -> bool {
    match (left, right) {
        (Type::Function(a, ar), Type::Function(b, br)) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|((_, a), (_, b))| equivalent(a, b))
                && equivalent(ar, br)
        }
        (Type::Record(a, None), Type::Record(b, None)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(name, ty)| b.get(name).is_some_and(|other| equivalent(ty, other)))
        }
        (Type::List(a), Type::List(b)) | (Type::Named(a), Type::Named(b)) => equivalent(a, b),
        _ => left == right,
    }
}

fn diagnostic(module: &CheckedModule, binding: &str, message: impl Into<String>) -> Diagnostic {
    let definition = module.definition(binding);
    Diagnostic::new(
        definition.map_or(binding, |definition| definition.source.as_str()),
        definition.map_or(0..0, |definition| definition.term.span.clone()),
        Phase::Type,
        format!("type definition `{binding}`: {}", message.into()),
    )
}

fn checked_record(
    module: &CheckedModule,
    binding: &str,
    expected: Type,
) -> Result<Value, Diagnostic> {
    let definition = module
        .definition(binding)
        .ok_or_else(|| diagnostic(module, binding, "unknown export"))?;
    let category = definition
        .namespace
        .as_deref()
        .and_then(|namespace| module.nominal_definition(namespace));
    if !category.is_some_and(|namespace| namespace.category == nominal("TypeDefinitions")) {
        return Err(diagnostic(
            module,
            binding,
            "export must belong to a TypeDefinitions namespace",
        ));
    }
    let expected = Type::Named(Box::new(expected));
    if !equivalent(&definition.scheme.ty, &expected) {
        return Err(diagnostic(
            module,
            binding,
            format!("expected {expected}, found {}", definition.scheme.ty),
        ));
    }
    let Value::Named { payload, .. } = module.evaluate(binding)? else {
        return Err(diagnostic(
            module,
            binding,
            "export lost its checked identity",
        ));
    };
    Ok(*payload)
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, String> {
    let Value::Record(fields) = value else {
        return Err("expected a finite record".into());
    };
    fields
        .iter()
        .find_map(|(key, value)| (key == name).then_some(value))
        .ok_or_else(|| format!("missing field `{name}`"))
}

fn string(value: &Value) -> Result<&str, String> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err("expected a string".into()),
    }
}

fn list(value: &Value) -> Result<&[Value], String> {
    match value {
        Value::List(values) => Ok(values),
        _ => Err("expected a list".into()),
    }
}

fn host<'a>(value: &Value, hosts: &'a BTreeMap<String, Expr>) -> Result<&'a Expr, String> {
    let Value::Foreign { identity, .. } = value else {
        return Err("callback must reference a named typed host binding".into());
    };
    let name = identity.strip_prefix("main::").unwrap_or(identity);
    hosts
        .get(name)
        .ok_or_else(|| format!("missing Rust binding for checked host `{name}`"))
}

fn kind(value: &Value, runtime: &Path) -> Result<TokenStream, String> {
    let Value::Constructor(name, fields) = value else {
        return Err("kind must be ordinary Kind data".into());
    };
    let Some(name) = name.strip_prefix("library::Kind::") else {
        return Err("kind must come from library::Kind".into());
    };
    if !fields.is_empty() || !matches!(name, "Data" | "Ghost" | "Nat" | "Sym" | "Bool") {
        return Err("unknown Kind value".into());
    }
    let name = format_ident!("{name}");
    Ok(quote! { #runtime::Kind::#name })
}

fn parameter(value: &Value, runtime: &Path) -> Result<TokenStream, String> {
    let Value::Constructor(name, fields) = value else {
        return Err("parameter must be ordinary Param data".into());
    };
    match (name.strip_prefix("library::Param::"), fields.as_slice()) {
        (Some(name @ ("One" | "Opt" | "List")), [value]) => {
            let name = format_ident!("{name}");
            let kind = kind(value, runtime)?;
            Ok(quote! { #runtime::Param::#name(#kind) })
        }
        (Some(name @ ("Fields" | "Any")), []) => {
            let name = format_ident!("{name}");
            Ok(quote! { #runtime::Param::#name })
        }
        _ => Err("unknown Param value".into()),
    }
}

/// Emit the existing Rust `TypeCtor` expression from a checked constructor record.
/// `runtime` names the module exporting TypeCtor, Kind, Param, Terms and TypeArg.
pub fn emit_type_ctor(
    module: &CheckedModule,
    binding: &str,
    hosts: &BTreeMap<String, Expr>,
    runtime: &Path,
) -> Result<TokenStream, Diagnostic> {
    let value = checked_record(module, binding, constructor_type())?;
    let emit = || -> Result<TokenStream, String> {
        let dialect = string(field(&value, "dialect")?)?;
        let name = string(field(&value, "name")?)?;
        let params = list(field(&value, "params")?)?
            .iter()
            .map(|value| {
                let name = string(field(value, "name")?)?;
                let param = parameter(field(value, "param")?, runtime)?;
                Ok(quote! { (#name, #param) })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let kind = kind(field(&value, "kind")?, runtime)?;
        let validate = host(field(&value, "validate")?, hosts)?;
        let carrier = host(field(&value, "carrier")?, hosts)?;
        Ok(quote! {{
            let validate: fn(&#runtime::Terms, &[#runtime::TypeArg]) -> Result<(), String> = #validate;
            let carrier: fn(&#runtime::Terms, &[#runtime::TypeArg]) -> Option<#runtime::TypeArg> = #carrier;
            #runtime::TypeCtor { dialect: #dialect, name: #name, params: &[#(#params),*], kind: #kind, validate, carrier }
        }})
    };
    emit().map_err(|message| diagnostic(module, binding, message))
}

/// Emit the existing Rust `TypeSpelling` expression and check its host pointer ABI.
/// The constructor provider runs against the supplied context, preserving local IDs.
pub fn emit_type_spelling(
    module: &CheckedModule,
    binding: &str,
    hosts: &BTreeMap<String, Expr>,
    context: &Expr,
    runtime: &Path,
) -> Result<TokenStream, Diagnostic> {
    let value = checked_record(module, binding, spelling_type())?;
    let emit = || -> Result<TokenStream, String> {
        let ctor_id = host(field(&value, "ctor_id")?, hosts)?;
        let keys = list(field(&value, "keys")?)?
            .iter()
            .map(string)
            .collect::<Result<Vec<_>, _>>()?;
        let parse = host(field(&value, "parse")?, hosts)?;
        let print = host(field(&value, "print")?, hosts)?;
        let Value::Bool(variadic_tail) = field(&value, "variadic_tail")? else {
            return Err("variadic_tail must be Bool".into());
        };
        Ok(quote! {{
            let ctor_id: fn(&#runtime::Context) -> #runtime::CtorId = #ctor_id;
            let parse: #runtime::TypeParser = #parse;
            let print: #runtime::TypePrinter = #print;
            #runtime::TypeSpelling { ctor: ctor_id(#context), keys: &[#(#keys),*], parse, print, variadic_tail: #variadic_tail }
        }})
    };
    emit().map_err(|message| diagnostic(module, binding, message))
}
