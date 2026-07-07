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
/// equivalent semantic expressions for the values computed across the whole
/// function, shared by every block and covered per block inside per-block
/// assumption scopes.
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

    /// The operator index buckets by kind alone: a pattern template with a
    /// wildcard type/payload must find every class holding its kind (the
    /// [`ENode::op_key`] contract for [`ENode::matches_template`]).
    fn op_key(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.kind.hash(&mut h);
        h.finish()
    }

    /// Operator/label equality, ignoring children: the kind, result type, and
    /// payload. A distinct opaque serial keeps memory effects and un-lowerable
    /// nodes from ever congruence-merging.
    fn matches(&self, other: &Self) -> bool {
        self == other
    }

    /// Template matching: a typed template only matches a node of exactly that
    /// type, an untyped one (`ty == None`) any type; a payload of `None` is a
    /// wildcard, `Some` matches by equality.
    fn matches_template(&self, target: &Self) -> bool {
        if self.kind != target.kind {
            return false;
        }
        if self.ty.is_some() && target.ty != self.ty {
            return false;
        }
        match (&self.payload, &target.payload) {
            (None, _) => true,
            (Some(expected), Some(actual)) => expected == actual,
            (Some(_), None) => false,
        }
    }

    fn commutative(&self) -> bool {
        self.kind.is_commutative()
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

/// The constant a class is proven to hold, if any member is an integer literal.
pub(crate) fn class_int_binding(egraph: &SemEGraph, class: Id) -> Option<APInt> {
    egraph.nodes(class).iter().find_map(|n| match &n.payload {
        Some(SemPayload::Expr(SymPayload::Int(v))) => Some(v.clone()),
        _ => None,
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
        .iter()
        .find_map(|n| match n.payload.as_ref() {
            Some(SemPayload::Expr(SymPayload::Value(v))) => Some(*v),
            _ => None,
        })
        .or_else(|| {
            class_values
                .get(&egraph.find(class))
                .and_then(|values| values.first().copied())
        })
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
    let data = context.get_type_data(ty);
    let any = data.as_ref() as &dyn std::any::Any;
    any.downcast_ref::<IntegerType>()
        .map(IntegerType::width)
        .or_else(|| {
            any.downcast_ref::<tir::builtin::FloatType>()
                .map(tir::builtin::FloatType::bit_width)
        })
}

/// The IR type of a `width`-bit value computed by a `kind` node: the IEEE
/// binary format for the float kinds, an integer type otherwise.
pub(crate) fn type_for_kind_width(context: &Context, kind: SymKind, width: u32) -> Option<TypeId> {
    use tir::builtin::FloatType;
    match kind {
        SymKind::FAdd | SymKind::FSub | SymKind::FMul | SymKind::FDiv => match width {
            32 => Some(FloatType::f32(context)),
            64 => Some(FloatType::f64(context)),
            _ => None,
        },
        _ => Some(IntegerType::new(context, width)),
    }
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
    egraph.nodes(class).iter().all(|n| {
        !matches!(
            n.kind,
            SymKind::LoadMemory
                | SymKind::StoreMemory
                | SymKind::LoadReserved
                | SymKind::StoreConditional
                | SymKind::AtomicRmw
                | SymKind::Fence
        )
    })
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

/// Whether an e-class holds a float (`Some(true)`) or integer (`Some(false)`)
/// value, from whichever member carries one of those types. `None` when no
/// member's type says either (rewrite-introduced intermediates, pointers).
pub(crate) fn class_is_float(ctx: &Context, egraph: &SemEGraph, class: Id) -> Option<bool> {
    egraph.nodes(class).iter().find_map(|n| {
        let data = ctx.get_type_data(n.ty?);
        let any = data.as_ref() as &dyn std::any::Any;
        if any.downcast_ref::<tir::builtin::FloatType>().is_some() {
            Some(true)
        } else if any.downcast_ref::<IntegerType>().is_some() {
            Some(false)
        } else {
            None
        }
    })
}
