//! The e-graph over the semantic vocabulary, and the readings every consumer of
//! it takes off an e-class.
//!
//! The label is [`SemNode`]; the classes are read the same way wherever the
//! e-graph is built — the selection axioms binding on class widths, the
//! peephole rules. What is read here is the
//! vocabulary's own business: a class's width, its integer binding, the IR type
//! a semantic type is spelled as. What a *backend* makes of a class — a
//! register's type, a low-bit view of one — stays with the backend.

use tir_relational::{ClassId as Id, Engine};

use crate::builtin::{FloatType, IntegerType};
use crate::sem::{FloatFormat, SemNode, SemPayload, SemType, SymKind, SymPayload};
use crate::{Context, TypeId};
use tir_adt::APInt;

/// The semantic e-graph: e-classes of equivalent semantic expressions for the
/// values a region or a function computes.
pub type SemEGraph = Engine<SemNode>;

/// The constant a class is proven to hold: an integer literal member, or the
/// value the open assumption scope proves it evaluates to.
pub(crate) fn class_int_binding(egraph: &SemEGraph, class: Id) -> Option<APInt> {
    let int = |n: &SemNode| match &n.payload {
        Some(SemPayload::Expr(SymPayload::Int(v))) => Some(v.clone()),
        _ => None,
    };
    egraph
        .const_of(class)
        .and_then(int)
        .or_else(|| egraph.nodes(class).find_map(int))
}

/// An unsigned literal at its minimal width. Widths identify a constant class,
/// so the byte counts and metadata the memory vocabulary carries must be spelled
/// this one way wherever they are seeded.
pub(crate) fn minimal_unsigned_apint(value: u64) -> APInt {
    let width = if value == 0 {
        1
    } else {
        64 - value.leading_zeros()
    };
    APInt::new(width, value)
}

/// The negated comparison at the same operand order (`!(a < b)` is `a >= b`).
pub(crate) fn complement_comparison(kind: SymKind) -> Option<SymKind> {
    Some(match kind {
        SymKind::Eq => SymKind::Ne,
        SymKind::Ne => SymKind::Eq,
        SymKind::Lt => SymKind::Ge,
        SymKind::Ge => SymKind::Lt,
        SymKind::Gt => SymKind::Le,
        SymKind::Le => SymKind::Gt,
        SymKind::ULt => SymKind::UGe,
        SymKind::UGe => SymKind::ULt,
        SymKind::UGt => SymKind::ULe,
        SymKind::ULe => SymKind::UGt,
        _ => return None,
    })
}

/// Whether the kind is a boolean comparison.
pub(crate) fn is_comparison(kind: SymKind) -> bool {
    complement_comparison(kind).is_some()
}

/// The bit-width of an IR integer or float type, or `None` for any other type.
pub(crate) fn type_width(context: &Context, ty: TypeId) -> Option<u32> {
    if ty == TypeId::DEPENDENCY {
        return None;
    }
    let data = context.get_type_data(ty);
    let any = data.as_ref() as &dyn std::any::Any;
    any.downcast_ref::<IntegerType>()
        .map(IntegerType::width)
        .or_else(|| any.downcast_ref::<FloatType>().map(FloatType::bit_width))
}

/// The context-independent semantic type represented by an IR type. Register
/// classes are intentionally absent: this describes the value, not its storage.
pub(crate) fn semantic_type(context: &Context, ty: TypeId) -> Option<SemType> {
    if ty == TypeId::DEPENDENCY {
        return None;
    }
    let data = context.get_type_data(ty);
    let any = data.as_ref() as &dyn std::any::Any;
    any.downcast_ref::<IntegerType>()
        .map(|ty| SemType::bits(ty.width()))
        .or_else(|| {
            any.downcast_ref::<FloatType>()
                .map(|ty| SemType::Float(FloatFormat::new(ty.exp_width(), ty.mant_width())))
        })
}

pub(crate) fn ir_type(context: &Context, ty: &SemType) -> Option<TypeId> {
    use crate::sem::Width;
    match ty {
        SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width)) => {
            Some(IntegerType::new(context, *width))
        }
        SemType::Float(format) => match (&format.exponent, &format.mantissa) {
            (Width::Const(exponent), Width::Const(mantissa)) => {
                Some(FloatType::new(context, *exponent, *mantissa))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The integer width of an e-class, taken from whichever member carries a known
/// integer type.
pub(crate) fn class_width(ctx: &Context, egraph: &SemEGraph, class: Id) -> Option<u32> {
    egraph
        .nodes(class)
        .find_map(|n| n.ty.and_then(|ty| type_width(ctx, ty)))
}

/// A ground semantic type carried by any typed member of an e-class.
pub(crate) fn class_semantic_type(ctx: &Context, egraph: &SemEGraph, class: Id) -> Option<SemType> {
    egraph
        .nodes(class)
        .find_map(|node| node.ty.and_then(|ty| semantic_type(ctx, ty)))
}
