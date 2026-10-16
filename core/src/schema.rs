//! Link-time schema of every operation, contributed by the `operation!` macro
//! into [`OP_SCHEMAS`]. Lets tools enumerate dialects, ops, their operands,
//! results, attributes and interfaces without hand-written per-op code — for
//! example to generate bindings for other languages.

use std::fmt::Write;

use linkme::distributed_slice;

use crate::ty::{Kind, Param, TypeArg};
use crate::{Context, TypeId};

/// An operand or result: its name, its type in the op's scheme (`?` first
/// for an optional one, `*` for a variadic segment), and whether it is a
/// variadic segment.
pub struct FieldSchema {
    pub name: &'static str,
    pub ty: &'static str,
    pub variadic: bool,
}

/// A named attribute and its declared kind (e.g. `Int`, `Str`, `Type`).
pub struct AttrSchema {
    pub name: &'static str,
    pub ty: &'static str,
    /// For a `Predicate` attribute, the comparisons the op accepts. Empty when
    /// the kind has no vocabulary to restrict.
    pub vocabulary: &'static [crate::attributes::Predicate],
}

/// The declarative shape of one operation.
pub struct OpSchema {
    pub dialect: &'static str,
    pub name: &'static str,
    pub operands: &'static [FieldSchema],
    pub results: &'static [FieldSchema],
    pub attributes: &'static [AttrSchema],
    pub interfaces: &'static [&'static str],
    /// The signature scheme the operands and results are checked against;
    /// see [`tir_symbolic::scheme`].
    pub scheme: &'static str,
    /// The op declares a `sem:`.
    pub semantics: bool,
}

/// Link-time registry of every operation schema reachable in the final binary.
#[distributed_slice]
pub static OP_SCHEMAS: [OpSchema];

fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn fields_json(fields: &[FieldSchema], out: &mut String) {
    out.push('[');
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        escape(f.name, out);
        out.push_str(",\"type\":");
        escape(f.ty, out);
        out.push_str(",\"variadic\":");
        out.push_str(if f.variadic { "true" } else { "false" });
        out.push('}');
    }
    out.push(']');
}

/// Serialize all registered op schemas as a JSON array.
pub fn schema_json() -> String {
    let mut out = String::from("[");
    for (i, op) in OP_SCHEMAS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"dialect\":");
        escape(op.dialect, &mut out);
        out.push_str(",\"name\":");
        escape(op.name, &mut out);
        out.push_str(",\"operands\":");
        fields_json(op.operands, &mut out);
        out.push_str(",\"results\":");
        fields_json(op.results, &mut out);
        out.push_str(",\"attributes\":[");
        for (j, a) in op.attributes.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            escape(a.name, &mut out);
            out.push_str(",\"type\":");
            escape(a.ty, &mut out);
            out.push('}');
        }
        out.push_str("],\"interfaces\":[");
        for (j, n) in op.interfaces.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            escape(n, &mut out);
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

/// Build the type `dialect.name` from structured arguments, without any
/// textual form. Returns an error if no such constructor is registered or the
/// arguments do not fit it.
pub fn build_type(
    context: &Context,
    dialect: &str,
    name: &str,
    args: &[TypeArg],
) -> Result<TypeId, String> {
    let ctor = context
        .terms()
        .ctor_named(dialect, name)
        .ok_or_else(|| format!("unknown type '{dialect}.{name}'"))?;
    context.terms().try_intern(ctor, args.into())
}

fn param_kind_str(param: Param) -> Option<&'static str> {
    match param {
        Param::One(Kind::Nat) | Param::Opt(Kind::Nat) => Some("u64"),
        Param::One(Kind::Data | Kind::Ghost) | Param::Opt(Kind::Data | Kind::Ghost) => Some("type"),
        _ => None,
    }
}

/// Serialize the constructors of the default dialects as a JSON array. A
/// constructor with a parameter a binding cannot pass is left out.
pub fn type_schema_json() -> String {
    let context = Context::with_default_dialects();
    let mut out = String::from("[");
    let mut first = true;
    for ctor in context.terms().ctors() {
        let info = context.terms().ctor_info(ctor);
        let Some(kinds) = info
            .params
            .iter()
            .map(|(_, param)| param_kind_str(*param))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str("{\"dialect\":");
        escape(info.dialect, &mut out);
        out.push_str(",\"name\":");
        escape(info.name, &mut out);
        out.push_str(",\"params\":[");
        for (j, ((name, _), kind)) in info.params.iter().zip(kinds).enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            escape(name, &mut out);
            out.push_str(",\"kind\":");
            escape(kind, &mut out);
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}
