//! Link-time schema of every operation, contributed by the `operation!` macro
//! into [`OP_SCHEMAS`]. Lets tools enumerate dialects, ops, their operands,
//! results, attributes and interfaces without hand-written per-op code — for
//! example to generate bindings for other languages.

use std::fmt::Write;

use linkme::distributed_slice;

use crate::{Context, TypeId};

/// An operand or result: its name, type-constraint name, and whether it is a
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
}

/// The declarative shape of one operation.
pub struct OpSchema {
    pub dialect: &'static str,
    pub name: &'static str,
    pub operands: &'static [FieldSchema],
    pub results: &'static [FieldSchema],
    pub attributes: &'static [AttrSchema],
    pub interfaces: &'static [&'static str],
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

/// Kind of a type-constructor parameter, as captured by `#[derive(TirType)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeParamKind {
    U32,
    U64,
    I64,
    Bool,
    Type,
}

/// A named parameter of a type constructor.
pub struct TypeParam {
    pub name: &'static str,
    pub kind: TypeParamKind,
}

/// A concrete argument passed to a type constructor through [`build_type`].
#[derive(Debug, Clone, Copy)]
pub enum TypeArg {
    U32(u32),
    U64(u64),
    I64(i64),
    Bool(bool),
    Type(TypeId),
}

/// The declarative shape of one type, plus a builder that constructs it from
/// arguments. Contributed by `#[derive(TirType)]`.
pub struct TypeSchema {
    pub dialect: &'static str,
    pub name: &'static str,
    pub params: &'static [TypeParam],
    pub build: fn(&Context, &[TypeArg]) -> Result<TypeId, String>,
}

/// Link-time registry of every type schema reachable in the final binary.
#[distributed_slice]
pub static TYPE_SCHEMAS: [TypeSchema];

/// Build a type by dialect-qualified name from structured arguments, without any
/// textual form. Returns an error if no such type is registered or the
/// arguments do not match.
pub fn build_type(
    context: &Context,
    dialect: &str,
    name: &str,
    args: &[TypeArg],
) -> Result<TypeId, String> {
    let schema = TYPE_SCHEMAS
        .iter()
        .find(|s| s.dialect == dialect && s.name == name)
        .ok_or_else(|| format!("unknown type '{dialect}.{name}'"))?;
    (schema.build)(context, args)
}

fn param_kind_str(kind: TypeParamKind) -> &'static str {
    match kind {
        TypeParamKind::U32 => "u32",
        TypeParamKind::U64 => "u64",
        TypeParamKind::I64 => "i64",
        TypeParamKind::Bool => "bool",
        TypeParamKind::Type => "type",
    }
}

/// Serialize all registered type schemas as a JSON array.
pub fn type_schema_json() -> String {
    let mut out = String::from("[");
    for (i, ty) in TYPE_SCHEMAS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"dialect\":");
        escape(ty.dialect, &mut out);
        out.push_str(",\"name\":");
        escape(ty.name, &mut out);
        out.push_str(",\"params\":[");
        for (j, p) in ty.params.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            escape(p.name, &mut out);
            out.push_str(",\"kind\":");
            escape(param_kind_str(p.kind), &mut out);
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}
