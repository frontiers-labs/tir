//! Semantic-expression substrate.
//!
//! The semantic graph, its interpreter, width inference, serialized wire format
//! and equivalence oracles all live in the `tir-symbolic` crate now; this module
//! re-exports them and adds the core-specific pieces: the graph annotated with
//! [`NodeMeta`], type-resolving replay, the `AsSemExpr` trait and constant
//! folding via [`Operation::semantic_expr`].

use crate::{NodeId, OpId, Operation, TypeId};

pub use tir_symbolic::lang::{
    AtomicRmwOp, BuildError, Continuation, MemOrdering, Memory, SCALAR_OPS, ScalarOp,
    SemBuilderHooks, SemExpr, SmtTemplate, StateAccessKind, StateFieldKind, StateFieldSchema,
    StateResourceKind, SymKind, SymPayload, TypeError, TypeUnifier, Value, WidthRule, build,
    canonicalize_for_selection, execute, execute_pure, execute_with_memory, infer_types,
    infer_widths, op_kind, op_name, parse, scalar_op, scalar_op_named,
};

pub use tir_symbolic::sem::{
    EquivalenceOracle, ExtendSemBytes, FuzzOracle, ProofOutcome, SemBlobBuilder, SemOp,
    SemPayloadDesc, SmtOracle, UnsupportedReason, confirm_bool_via_if,
    confirm_extension_via_shifts, decode_sem_ops, float_payload, int_payload,
};
pub(crate) use tir_symbolic::sem::{con, op};

pub mod fp_refinement;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeMeta {
    pub original_op: Option<OpId>,
    pub actual_type: Option<TypeId>,
}

/// The post-order graph core builds semantic expressions into: the shared
/// [`tir_symbolic::sem::SemGraph`] annotated with [`NodeMeta`], so a node can
/// carry its originating op and inferred type.
pub type SemGraph = tir_symbolic::sem::SemGraph<NodeMeta>;

/// [`ExtendSemBytes`] for graphs that carry [`NodeMeta`], resolving
/// [`SemOp::Typed`] widths against the context's interned integer types.
pub trait ExtendSemBytesTyped {
    fn extend_sem_bytes_typed(
        &mut self,
        context: &crate::Context,
        kinds: &[SymKind],
        blob: &[u8],
        offset: u32,
    ) -> NodeId;
}

impl ExtendSemBytesTyped for SemGraph {
    fn extend_sem_bytes_typed(
        &mut self,
        context: &crate::Context,
        kinds: &[SymKind],
        blob: &[u8],
        offset: u32,
    ) -> NodeId {
        self.extend_sem_bytes_with(kinds, blob, offset, |g, node, width| {
            g.annotation_mut(node).actual_type =
                Some(crate::builtin::IntegerType::new(context, u64::from(width)));
        })
    }
}

/// The definedness condition of a partial integer operation, materialized as new
/// nodes in `g` over the operation's own operands. The partial kinds are the
/// IR-level division/remainder ops, undefined at a zero divisor (and, for the
/// signed forms, at the `MIN / -1` overflow point). Returns the condition's root,
/// or `None` when `node`'s kind is total. `widths` must give every existing
/// node's bit width (e.g. from [`infer_widths`]); the operand width sizes the
/// emitted constants.
///
/// This is the metadata the guard-relaxation gate consumes: a guarded target
/// instruction may be relaxed to the pure partial op exactly where the op is
/// defined, so `D(pattern)` is the antecedent of the relaxation obligation.
pub fn definedness_condition(
    g: &mut SemGraph,
    node: NodeId,
    widths: &[Option<u32>],
) -> Option<NodeId> {
    let signed = match *g.get_node(node) {
        SymKind::Div | SymKind::SRem => true,
        SymKind::UDiv | SymKind::URem => false,
        _ => return None,
    };
    let operands: Vec<NodeId> = g.children(node).collect();
    let [lhs, rhs] = operands.as_slice() else {
        return None;
    };
    let (lhs, rhs) = (*lhs, *rhs);
    let width = widths.get(rhs.index()).copied().flatten()?;

    let zero = con(g, 0, width);
    let nonzero = op(g, SymKind::Ne, &[rhs, zero]);
    if !signed {
        return Some(nonzero);
    }
    let min = con(g, 1u64 << (width - 1), width);
    let all_ones = con(g, ones_mask(width), width);
    let is_min = op(g, SymKind::Eq, &[lhs, min]);
    let is_neg_one = op(g, SymKind::Eq, &[rhs, all_ones]);
    let overflow = op(g, SymKind::And, &[is_min, is_neg_one]);
    let no_overflow = op(g, SymKind::Not, &[overflow]);
    Some(op(g, SymKind::And, &[nonzero, no_overflow]))
}

fn ones_mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Build an operation's semantic expression into `g`, returning its root. Unlike
/// [`Operation::semantic_expr`], the expression must build.
pub trait AsSemExpr: Operation {
    fn convert(&self, g: &mut SemGraph) -> NodeId;
}

/// Fold an operation over constant operand `values` by evaluating its declared
/// semantic expression. Returns `None` for ops without one. This backs the
/// `ConstantFold` impl the `operation!` macro derives from `sem`.
pub fn fold_with_sem(op: &dyn Operation, values: &[Value]) -> Option<Value> {
    let mut graph = SemGraph::new();
    op.semantic_expr(&mut graph)?;
    Some(execute(&graph, values))
}

// ── APInt boundary helpers ──────────────────────────────────────────────────
//
// These let TMDL-generated backend code construct and consume sem values without
// naming `tir-adt` directly.

/// A signed integer interpreter value of the given width.
pub fn int_value_signed(width: u32, value: i64) -> Value {
    Value::Int(tir_adt::APInt::new_signed(u64::from(width), value))
}

/// An unsigned integer interpreter value of the given width.
pub fn int_value(width: u32, value: u64) -> Value {
    Value::Int(tir_adt::APInt::new(u64::from(width), value))
}

/// Wrap a machine-register `APInt` (e.g. from `MachineContext::read_register`) as
/// an interpreter value.
pub fn value_from_register(v: tir_adt::APInt) -> Value {
    Value::Int(v)
}

/// Wrap raw register byte lanes (e.g. a vector register from
/// `MachineContext::read_register_bits`) as an interpreter value; behaviors then
/// split it into lanes.
pub fn value_from_raw_bits(v: tir_adt::RawBits) -> Value {
    Value::RawBits(v)
}
