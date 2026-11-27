//! Link-time schema of every operation in [`OP_SCHEMAS`].
//! Lets tools enumerate dialects, ops, their operands,
//! results, attributes and interfaces without hand-written per-op code — for
//! example to generate bindings for other languages.

use std::fmt::Write;

use linkme::distributed_slice;

use crate::ty::{Kind, Param, TypeArg};
use crate::{Context, TypeId};

pub use crate::typecheck::signature::{Argument, BinderKind, Pattern, Port, Signature};

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

/// The operands, results, and attributes of one operation form.
pub struct OpLayout {
    pub operands: &'static [FieldSchema],
    pub results: &'static [FieldSchema],
    pub attributes: &'static [AttrSchema],
}

/// The input or output group named by a CDSL type constraint.
#[derive(Clone, Copy)]
pub enum ConstraintPort {
    Input(usize),
    Output(usize),
}

/// Additional type obligations that depend on scoped or cross-port facts.
#[derive(Clone, Copy)]
pub enum TypeConstraint {
    EqualWidth(ConstraintPort, ConstraintPort, &'static str),
    PointerWidth(ConstraintPort, &'static str),
    EqualType(ConstraintPort, ConstraintPort, &'static str),
    TupleElements(ConstraintPort, ConstraintPort),
    TupleProjection(ConstraintPort, &'static str, ConstraintPort),
    StateFan(ConstraintPort, ConstraintPort, &'static str),
    NonFunction(ConstraintPort, &'static str),
    FunctionDefinition(ConstraintPort, &'static str),
    FunctionReturn(ConstraintPort),
    FunctionCall(ConstraintPort, ConstraintPort, ConstraintPort),
    ModuleScope(&'static str),
    RegionExits(usize, &'static [&'static str], &'static str, &'static str),
    ExitRole(&'static str),
    OrderedLoop(ConstraintPort, &'static str),
    SwitchCases(ConstraintPort),
}

/// The declarative shape of one operation.
pub struct OpSchema {
    pub declaration: fn() -> tir_egglog::NativeIdentity,
    pub bind: fn(&mut tir_egglog::Database) -> Result<(), tir_egglog::Error>,
    pub construct: fn(crate::OpHandle) -> Box<dyn crate::Operation>,
    pub dialect: &'static str,
    pub name: &'static str,
    pub operands: &'static [FieldSchema],
    pub results: &'static [FieldSchema],
    pub attributes: &'static [AttrSchema],
    pub interfaces: &'static [&'static str],
    pub cost: u32,
    pub regions: &'static [(crate::RegionKind, bool)],
    pub binding_inputs: Option<(usize, usize)>,
    pub counted_groups: Option<(usize, usize, usize)>,
    /// The signature scheme the operands and results are checked against;
    /// see [`tir_symbolic::scheme`].
    pub scheme: &'static str,
    /// Type patterns and quantified binders from the CDSL signature.
    pub signature: &'static Signature,
    pub type_constraints: &'static [TypeConstraint],
    /// The assembly form, when the fields above describe the pre-allocation form.
    pub assembly: Option<OpLayout>,
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

fn layout_json(layout: &OpLayout, out: &mut String) {
    out.push_str("\"operands\":");
    fields_json(layout.operands, out);
    out.push_str(",\"results\":");
    fields_json(layout.results, out);
    out.push_str(",\"attributes\":[");
    for (i, attr) in layout.attributes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        escape(attr.name, out);
        out.push_str(",\"type\":");
        escape(attr.ty, out);
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
        let primary = OpLayout {
            operands: op.operands,
            results: op.results,
            attributes: op.attributes,
        };
        out.push(',');
        layout_json(&primary, &mut out);
        if let Some(assembly) = &op.assembly {
            out.push_str(",\"assembly\":{");
            layout_json(assembly, &mut out);
            out.push('}');
        }
        out.push_str(",\"interfaces\":[");
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
