//! The e-graph node label for semantic instruction selection, plus the small
//! shared helpers that read types and operand bindings off e-classes.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tir::{
    Context, TypeId, ValueId,
    builtin::IntegerType,
    sem::{SymKind, SymPayload},
};
use tir_adt::APInt;
use tir_symbolic::egraph::{EGraph, ENode, Id};

/// The semantic e-graph instruction selection operates over: e-classes of
/// equivalent semantic expressions for the values computed in a block.
pub type SemEGraph = EGraph<SemNode>;

/// An e-graph node label: the operator identity (kind/payload) plus the IR type of
/// the value it represents, and its operand e-classes carried inline (the
/// [`ENode`] contract). Hash-consing and pattern matching compare only the label
/// (kind/payload/type) and the canonical children.
///
/// `ty` is the result type for an op node, the value type for a leaf. `None` on a
/// *pattern* node means "match any type"; `None` on a *graph* node means the type
/// is unknown (e.g. an intermediate node of a multi-node semantic expansion). The
/// type is stored verbatim from the IR — no width is collapsed or normalized — so
/// every target can constrain on exactly the widths/classes it distinguishes
/// (x86/AArch64 8/16/32/64-bit forms, RISC-V word vs XLEN, vector element types,
/// floats), and untyped rules stay width-agnostic.
#[derive(Clone, Debug)]
pub struct SemNode {
    pub kind: SymKind,
    pub payload: Option<SemPayload>,
    pub ty: Option<TypeId>,
    pub children: Vec<Id>,
}

/// A node label payload: a semantic-expression payload, or an opaque marker for
/// an un-lowerable sub-expression. Each opaque leaf carries a unique serial so
/// two unrelated unknown computations never hash-cons into the same e-class.
#[derive(Clone, Debug, PartialEq)]
pub enum SemPayload {
    Expr(SymPayload<ValueId>),
    Opaque(u32),
}

/// Label equality, ignoring children — two e-nodes share an e-class iff their
/// labels are equal and their canonical children are equal (the [`ENode`] model).
impl PartialEq for SemNode {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.payload == other.payload && self.ty == other.ty
    }
}

impl Eq for SemNode {}

impl ENode for SemNode {
    fn children(&self) -> &[Id] {
        &self.children
    }

    fn children_mut(&mut self) -> &mut [Id] {
        &mut self.children
    }

    fn hash_cons(&self) -> u64 {
        let mut h = DefaultHasher::new();
        hash_label(self, &mut h);
        h.finish()
    }

    /// Operator/label equality, ignoring children: the kind, result type, and
    /// payload. A distinct opaque serial keeps memory effects and un-lowerable
    /// nodes from ever congruence-merging.
    fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

/// Hashes exactly the fields compared by [`SemNode`]'s label equality.
fn hash_label(node: &SemNode, state: &mut impl Hasher) {
    node.kind.hash(state);
    node.ty.hash(state);
    match &node.payload {
        None => 0u8.hash(state),
        Some(SemPayload::Expr(SymPayload::SymbolId(s))) => {
            1u8.hash(state);
            s.hash(state);
        }
        Some(SemPayload::Expr(SymPayload::Value(v))) => {
            2u8.hash(state);
            v.number().hash(state);
        }
        Some(SemPayload::Expr(SymPayload::Int(i))) => {
            3u8.hash(state);
            i.width().hash(state);
            i.is_signed().hash(state);
            i.to_u64().hash(state);
        }
        Some(SemPayload::Expr(SymPayload::Float(f))) => {
            4u8.hash(state);
            f.to_f64().to_bits().hash(state);
        }
        Some(SemPayload::Opaque(serial)) => {
            5u8.hash(state);
            serial.hash(state);
        }
    }
}

impl tir::graph::Matchable<Context> for SemNode {
    fn is_leaf(&self, ctx: &Context) -> bool {
        self.kind.is_leaf(ctx)
    }

    fn num_children(&self, ctx: &Context) -> usize {
        self.kind.num_children(ctx)
    }

    fn is_commutative(&self) -> bool {
        self.kind.is_commutative()
    }

    fn is_constant(&self) -> bool {
        self.kind == SymKind::Constant
    }

    fn matches_pattern(&self, pattern: &Self, _ctx: &Context) -> bool {
        if self.kind != pattern.kind {
            return false;
        }

        // A typed pattern node only matches a graph node of exactly that type;
        // an untyped pattern node (`ty == None`) is a type wildcard.
        if pattern.ty.is_some() && self.ty != pattern.ty {
            return false;
        }

        match (&self.payload, &pattern.payload) {
            (_, None) => true,
            (Some(actual), Some(expected)) => actual == expected,
            (None, Some(_)) => false,
        }
    }
}

/// The concrete operand a capture e-class resolves to.
pub(crate) enum Binding {
    Int(APInt),
    Value(ValueId),
}

/// Resolve one capture e-class to its operand binding: a constant immediate, then
/// an input value, then the IR value an intermediate result produces (looked up in
/// `class_value`, the map recording which class computes which op result). `None` if
/// the class carries no materializable operand. This is the single resolution rule
/// used by both match collection and emission.
pub(crate) fn class_binding(
    egraph: &SemEGraph,
    class_value: &HashMap<Id, ValueId>,
    class: Id,
) -> Option<Binding> {
    let nodes = egraph.nodes(class);
    if let Some(v) = nodes.iter().find_map(|n| match n.payload.as_ref() {
        Some(SemPayload::Expr(SymPayload::Int(v))) => Some(v),
        _ => None,
    }) {
        Some(Binding::Int(v.clone()))
    } else if let Some(v) = nodes.iter().find_map(|n| match n.payload.as_ref() {
        Some(SemPayload::Expr(SymPayload::Value(v))) => Some(*v),
        _ => None,
    }) {
        Some(Binding::Value(v))
    } else {
        class_value
            .get(&egraph.find(class))
            .copied()
            .map(Binding::Value)
    }
}

/// The integer bit-width of an IR type, or `None` if it is not an integer type.
pub(crate) fn type_width(context: &Context, ty: TypeId) -> Option<u32> {
    let data = context.get_type_data(ty);
    (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .map(IntegerType::width)
}

pub(crate) fn minimal_unsigned_apint(value: u64) -> APInt {
    let width = if value == 0 {
        1
    } else {
        64 - value.leading_zeros()
    };
    APInt::new(width, value)
}

pub(crate) fn template_node(
    kind: SymKind,
    payload: Option<SymPayload<ValueId>>,
    ty: Option<TypeId>,
) -> SemNode {
    SemNode {
        kind,
        payload: payload.map(SemPayload::Expr),
        ty,
        children: Vec::new(),
    }
}

/// Whether duplicating the class's computation is sound: every member is a pure
/// value expression, so two fused matches may each recompute it inside their
/// instruction. Memory effects are excluded — two reads of the same address are
/// not interchangeable across an intervening write.
pub(crate) fn class_is_pure(egraph: &SemEGraph, class: Id) -> bool {
    egraph
        .nodes(class)
        .iter()
        .all(|n| !matches!(n.kind, SymKind::LoadMemory | SymKind::StoreMemory))
}

/// The integer width of an e-class, taken from whichever member carries a known
/// integer type (the original IR node keeps its type; rewrite-introduced nodes are
/// left untyped).
pub(crate) fn class_width(ctx: &Context, egraph: &SemEGraph, class: Id) -> Option<u32> {
    egraph
        .nodes(class)
        .iter()
        .find_map(|n| n.ty.and_then(|ty| type_width(ctx, ty)))
}
