//! What instruction selection reads off an e-class beyond the vocabulary's own
//! readings ([`tir::sem::egraph`]): the framework's value model (a low-bit view
//! of a register) and the purity a fused match needs.

use tir::{
    Context,
    sem::{
        SemNode, SemType, SymKind,
        egraph::{SemEGraph, class_int_binding, class_semantic_type},
    },
};
use tir_adt::APInt;
use tir_relational::{ClassId as Id, Ref, RowId};

/// The rows of `value`'s class that are `value` itself, not the class at
/// another offset.
pub(crate) fn rows_at(egraph: &SemEGraph, value: Ref) -> impl Iterator<Item = RowId> + '_ {
    rows_at_where(egraph, value, |_| true)
}

/// [`rows_at`] narrowed to the rows whose node `keep` accepts, which is asked
/// first: reading where a row sits costs a find.
pub(crate) fn rows_at_where<'a>(
    egraph: &'a SemEGraph,
    value: Ref,
    keep: impl Fn(&SemNode) -> bool + 'a,
) -> impl Iterator<Item = RowId> + 'a {
    let value = egraph.find(value);
    egraph
        .rows(value.class)
        .filter(move |&row| keep(egraph.node(row)) && egraph.value(row) == value)
}

/// If `value` is a low-bit truncation `Extract(v, hi, 0)`, its operand `v`.
/// Such a value *is* the low `hi+1` bits of `v`'s register — the framework's
/// value model (a width-n value occupies the low n bits, upper bits undefined) —
/// so it computes nothing: consumers read `v`'s register directly. No
/// materializer, no instruction, and no cross-width union (the i32 view and any
/// explicit i64 widening stay distinct classes, kept apart by the width matcher).
pub(crate) fn low_extract_source(egraph: &SemEGraph, value: Ref) -> Option<Ref> {
    let view = |node: &SemNode| node.kind == SymKind::Extract && node.children.len() == 3;
    rows_at_where(egraph, value, view).find_map(|row| {
        let children = egraph.children(row);
        (class_int_binding(egraph, children[2])
            .as_ref()
            .map(APInt::to_u64)
            == Some(0))
        .then_some(children[0])
    })
}

/// The value whose tile defines the register a low-extract view re-reads:
/// `value` itself unless it is a chain of low-bit truncations.
pub(crate) fn chase_low_extract(egraph: &SemEGraph, value: Ref) -> Ref {
    let mut value = egraph.find(value);
    while let Some(source) = low_extract_source(egraph, value) {
        value = source;
    }
    value
}

/// Whether `value` is a low-bit truncation (see [`low_extract_source`]).
pub(crate) fn is_low_extract_view(egraph: &SemEGraph, value: Ref) -> bool {
    low_extract_source(egraph, value).is_some()
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

/// Whether duplicating the computation of `value` is sound: every row at it
/// is a pure value expression (see [`class_is_pure`]). A value no row is at is
/// its class plus an offset, an addition over a register the match reads, so
/// it is pure whatever its class holds.
pub(crate) fn is_pure(egraph: &SemEGraph, value: Ref) -> bool {
    let impure = |node: &SemNode| match &node.kind {
        tir::sem::Kind::Sym(kind) => !kind_is_pure(*kind),
        tir::sem::Kind::Ir(_) => false,
    };
    rows_at_where(egraph, value, impure).next().is_none()
}

pub(crate) fn is_identity_effect(egraph: &SemEGraph, value: Ref) -> bool {
    rows_at_where(egraph, value, |node| node.kind == SymKind::FPEffect).any(|row| {
        let children = egraph.children(row);
        children[0] == children[1]
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
        SymKind::FPEffect
            | SymKind::LoadMemory
            | SymKind::StoreMemory
            | SymKind::LoadReserved
            | SymKind::StoreConditional
            | SymKind::AtomicRmw
            | SymKind::Fence
            | SymKind::StateAssign
            | SymKind::StateStore
            | SymKind::StateStoreConditional
            | SymKind::StateFence
            | SymKind::StateTrap
            | SymKind::StateBlock
            | SymKind::StateIf
            | SymKind::StateTry
            | SymKind::StateHandler
    )
}

/// The semantic type a register must hold for an e-class. Pointers preserve
/// their IR type in the graph, but use the target data layout's pointer width at
/// an instruction's register boundary; a class no type names occupies a
/// register of its carrier's width.
pub(crate) fn class_register_type(
    ctx: &Context,
    egraph: &SemEGraph,
    class: Id,
    pointer_width: Option<u32>,
) -> Option<SemType> {
    class_semantic_type(ctx, egraph, class)
        .or_else(|| {
            let width = pointer_width?;
            egraph
                .nodes(class)
                .any(|node| {
                    node.ty
                        .filter(|ty| !ctx.is_state_type(*ty))
                        .is_some_and(|ty| {
                            let data = ctx.get_type_data(ty);
                            (data.as_ref() as &dyn std::any::Any)
                                .downcast_ref::<tir::ptr::PtrType>()
                                .is_some()
                        })
                })
                .then(|| SemType::bits(width))
        })
        // A carrier's zero holds the constants of every type of its width, and
        // what it occupies is a register of that width.
        .or_else(|| egraph.width(class).map(SemType::bits))
}

/// The width of the register an e-class occupies, when its type fixes one.
pub(crate) fn class_register_width(
    ctx: &Context,
    egraph: &SemEGraph,
    class: Id,
    pointer_width: Option<u32>,
) -> Option<u32> {
    use tir::sem::Width;
    match class_register_type(ctx, egraph, class, pointer_width)? {
        SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width)) => Some(width),
        SemType::Float(format) => match (format.exponent, format.mantissa) {
            (Width::Const(exponent), Width::Const(mantissa)) => Some(1 + exponent + mantissa),
            _ => None,
        },
        _ => None,
    }
}
