//! What instruction selection reads off an e-class beyond the vocabulary's own
//! readings ([`tir::sem::egraph`]): the framework's value model (a low-bit view
//! of a register), the register a class is carried in, and the purity a fused
//! match needs.

use std::collections::HashMap;

use tir::{
    Context, ValueId,
    sem::{
        SemType, SymKind,
        egraph::{SemEGraph, class_int_binding, class_semantic_type},
    },
};
use tir_adt::APInt;
use tir_relational::{ClassId as Id, Label as ENode};

/// If the class is a low-bit truncation `Extract(v, hi, 0)`, its operand class
/// `v`. Such a value *is* the low `hi+1` bits of `v`'s register — the framework's
/// value model (a width-n value occupies the low n bits, upper bits undefined) —
/// so it computes nothing: consumers read `v`'s register directly. No
/// materializer, no instruction, and no cross-width union (the i32 view and any
/// explicit i64 widening stay distinct classes, kept apart by the width matcher).
pub(crate) fn low_extract_source(egraph: &SemEGraph, class: Id) -> Option<Id> {
    egraph.nodes(class).find_map(|n| {
        (n.kind == SymKind::Extract
            && n.children().len() == 3
            && class_int_binding(egraph, egraph.find(n.children()[2]))
                .as_ref()
                .map(APInt::to_u64)
                == Some(0))
        .then(|| egraph.find(n.children()[0]))
    })
}

/// Whether the class is a low-bit truncation (see [`low_extract_source`]).
pub(crate) fn is_low_extract_view(egraph: &SemEGraph, class: Id) -> bool {
    low_extract_source(egraph, class).is_some()
}

pub(crate) fn low_extract_width(egraph: &SemEGraph, class: Id) -> Option<u32> {
    egraph.nodes(class).find_map(|node| {
        if node.kind != SymKind::Extract || node.children().len() != 3 {
            return None;
        }
        let hi = class_int_binding(egraph, egraph.find(node.children()[1]))?.to_u64();
        let lo = class_int_binding(egraph, egraph.find(node.children()[2]))?.to_u64();
        (lo == 0).then(|| u32::try_from(hi + 1).ok()).flatten()
    })
}

/// The register value carrying a class: an input value, then the first IR value
/// the class computes (from `class_values`, the map recording which values a
/// class stands for). The representative feeds cost-model approximation only.
pub(crate) fn class_value_binding(
    egraph: &SemEGraph,
    class_values: &HashMap<Id, Vec<ValueId>>,
    class: Id,
) -> Option<ValueId> {
    egraph
        .nodes(class)
        .find_map(|n| match n.payload.as_ref() {
            Some(tir::sem::SemPayload::Expr(tir::sem::SymPayload::Value(v))) => Some(*v),
            _ => None,
        })
        .or_else(|| {
            class_values
                .get(&egraph.find(class))
                .and_then(|values| values.first().copied())
        })
}

/// Whether duplicating the class's computation is sound: every member is a pure
/// value expression, so two fused matches may each recompute it inside their
/// instruction. Memory effects are excluded — two reads of the same address are
/// not interchangeable across an intervening write.
///
/// An operation identity ([`tir::sem::Kind::Ir`]) is pure: it is seeded only for
/// an op with no memory effect. A gated-SSA merge is not — it is the schedule,
/// not a value expression.
pub(crate) fn class_is_pure(egraph: &SemEGraph, class: Id) -> bool {
    egraph.nodes(class).all(|n| match &n.kind {
        tir::sem::Kind::Sym(kind) => kind_is_pure(*kind),
        tir::sem::Kind::Ir(_) => true,
    })
}

/// Whether a term of this kind names an access to memory, and therefore carries
/// the state chain it reads as its last operand — the arity
/// `super::builder::SemDagBuilder::build_memory_effect` spells and the one
/// `super::pattern` compiles a rule's memory node up to.
pub(crate) fn is_memory_kind(kind: SymKind) -> bool {
    matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory)
}

/// Whether the kind is a pure value expression (see [`class_is_pure`]).
pub(crate) fn kind_is_pure(kind: SymKind) -> bool {
    !matches!(
        kind,
        SymKind::LoadMemory
            | SymKind::StoreMemory
            | SymKind::LoadReserved
            | SymKind::StoreConditional
            | SymKind::AtomicRmw
            | SymKind::Fence
    )
}

/// The semantic type a register must hold for an e-class. Pointers preserve
/// their IR type in the graph, but use the target data layout's pointer width at
/// an instruction's register boundary.
pub(crate) fn class_register_type(
    ctx: &Context,
    egraph: &SemEGraph,
    class: Id,
    pointer_width: Option<u32>,
) -> Option<SemType> {
    class_semantic_type(ctx, egraph, class).or_else(|| {
        let width = pointer_width?;
        egraph
            .nodes(class)
            .any(|node| {
                node.ty.is_some_and(|ty| {
                    let data = ctx.get_type_data(ty);
                    (data.as_ref() as &dyn std::any::Any)
                        .downcast_ref::<tir::ptr::PtrType>()
                        .is_some()
                })
            })
            .then(|| SemType::bits(width))
    })
}
