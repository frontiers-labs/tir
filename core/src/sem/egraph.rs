//! The e-graph over the semantic vocabulary, and the readings every consumer of
//! it takes off an e-class.
//!
//! The label is [`SemNode`]; the classes are read the same way wherever the
//! e-graph is built — the selection axioms binding on class widths, the
//! peephole rules. What is read here is the vocabulary's own business: a
//! class's width, its integer binding. What a *backend* makes of a class — a
//! register's type, a low-bit view of one — stays with the backend.

use tir_egraph::{Carrier, Engine, Ref};

use crate::builtin::IntegerType;
use crate::ptr::PtrType;
use crate::sem::{SemNode, SemPayload, SymKind, SymPayload};
use crate::vector::VectorType;
use crate::{Context, TypeId};
use tir_adt::APInt;

/// The semantic e-graph: e-classes of equivalent semantic expressions for the
/// values a region or a function computes.
pub type SemEGraph = Engine<SemNode>;

/// The constant a value is proven to hold: an offset of its carrier's zero,
/// the value an open assumption scope proves it evaluates to, or an integer
/// literal too wide for a carrier among its class's rows.
pub(crate) fn class_int_binding(egraph: &SemEGraph, value: Ref) -> Option<APInt> {
    let int = |n: &SemNode| match &n.payload {
        Some(SemPayload::Expr(SymPayload::Int(v))) => Some(v.clone()),
        _ => None,
    };
    let value = egraph.find(value);
    egraph.const_of(value).and_then(|n| int(&n)).or_else(|| {
        (value.offset == 0)
            .then(|| egraph.nodes(value.class).find_map(int))
            .flatten()
    })
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
    APInt::new(u64::from(width), value)
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

/// The carrier a value of `ty` lives in: an integer of at most 64 bits, or a
/// pointer where a data layout gives its width. Anything else keeps no
/// offsets.
pub(crate) fn carrier_of(
    context: &Context,
    pointer_width: Option<u32>,
    ty: TypeId,
) -> Option<Carrier> {
    if context.is_state_type(ty) {
        return None;
    }
    if let Some(int) = IntegerType::view(context, ty) {
        return crate::sem::node::int_carrier(u32::try_from(int.width()).ok()?);
    }
    PtrType::view(context, ty)
        .and(pointer_width)
        .and_then(crate::sem::node::pointer_carrier)
}

// The width of a constant of the index type, which a length given to a vector
// operation has.
pub(crate) const LANES_WIDTH: u32 = 64;

// The count is held by the row as a child after its values.
pub(crate) fn counted_vector(context: &Context, ty: TypeId) -> Option<(TypeId, u32)> {
    let vector = VectorType::view(context, ty)?;
    let any = VectorType::dynamic(context, vector.element());
    Some((any, vector.length()?))
}

pub(crate) fn canonical_type(context: &Context, ty: TypeId) -> TypeId {
    if PtrType::view(context, ty).is_some() {
        return PtrType::opaque(context);
    }
    counted_vector(context, ty).map_or(ty, |(any, _)| any)
}
