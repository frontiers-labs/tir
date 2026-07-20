//! Semantic-expression substrate.
//!
//! The semantic graph, its interpreter, width inference and selection
//! canonicalization all live in the `tir-symbolic` crate's `lang` module now;
//! this module re-exports them and adds the core-specific pieces: the post-order
//! graph alias core builds into, the `AsSemExpr` trait, and constant folding via
//! [`Operation::semantic_expr`].

use crate::graph::{MutDag, NodeId, NodeMeta, PostOrderDag};
use crate::{Operation, ValueId};

pub use tir_symbolic::lang::{
    AtomicRmwOp, BuildError, FloatFormat, MemOrdering, Memory, SCALAR_OPS, ScalarOp,
    SemBuilderHooks, SemExpr, SemType, SmtTemplate, SymKind, SymPayload, TypeError, TypeUnifier,
    TypeVar, Value, Width, WidthRule, WidthVar, build, canonicalize_for_selection, execute,
    execute_with_memory, infer_types, infer_widths, op_kind, op_name, parse, scalar_op,
    scalar_op_named,
};

mod discover;
#[cfg(debug_assertions)]
pub(crate) use discover::sym;
pub use discover::{
    EquivalenceOracle, FuzzOracle, SmtOracle, confirm_bool_via_if, confirm_extension_via_shifts,
};
pub(crate) use discover::{con, op};

/// The post-order graph core builds semantic expressions into: [`SymKind`] nodes
/// over `SymPayload<ValueId>` leaves, annotated with [`NodeMeta`] so a node can
/// carry its originating op and inferred type.
pub type SemGraph = PostOrderDag<SymKind, SymPayload<ValueId>, NodeMeta>;

/// Build an operation's semantic expression into any graph backend. Unlike
/// [`Operation::semantic_expr`] (nailed to [`SemGraph`] so it stays `dyn`-callable),
/// this is generic, so isel and TMDL can lower into their own graph stores.
pub trait AsSemExpr: Operation {
    fn convert(
        &self,
        g: &mut impl MutDag<Node = SymKind, Leaf = SymPayload<ValueId>, Annotation = NodeMeta>,
    ) -> NodeId;
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

/// An integer payload literal for graph construction (`signed` picks the
/// constructor); used by TMDL codegen in place of a raw `APInt`.
pub fn int_payload(width: u32, value: u64, signed: bool) -> SymPayload<ValueId> {
    let v = if signed {
        tir_adt::APInt::new_signed(width, value as i64)
    } else {
        tir_adt::APInt::new(width, value)
    };
    SymPayload::Int(v)
}

/// A float payload literal for graph construction.
pub fn float_payload(value: f64) -> SymPayload<ValueId> {
    SymPayload::Float(tir_adt::APFloat::from_f64(value))
}

/// A signed integer interpreter value of the given width.
pub fn int_value_signed(width: u32, value: i64) -> Value {
    Value::Int(tir_adt::APInt::new_signed(width, value))
}

/// An unsigned integer interpreter value of the given width.
pub fn int_value(width: u32, value: u64) -> Value {
    Value::Int(tir_adt::APInt::new(width, value))
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

/// Convert an interpreter integer back to a machine-register `APInt` for write-back.
pub fn register_from_int(v: tir_adt::APInt) -> tir_adt::APInt {
    v
}
