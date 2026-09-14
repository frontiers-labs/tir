//! Semantic-expression substrate.
//!
//! The semantic graph, its interpreter, width inference, serialized wire format
//! and equivalence oracles all live in the `tir-symbolic` crate now; this module
//! re-exports them and adds the core-specific pieces: the graph annotated with
//! [`NodeMeta`], type-resolving replay, the `AsSemExpr` trait and constant
//! folding via [`Operation::semantic_expr`].

use crate::graph::{Dag, MutDag, NodeId, NodeMeta};
use crate::{Operation, ValueId};

pub use tir_symbolic::lang::{
    AtomicRmwOp, BuildError, FloatFormat, MemOrdering, Memory, SCALAR_OPS, ScalarOp,
    SemBuilderHooks, SemExpr, SemType, SmtTemplate, StateAccessKind, StateFieldKind,
    StateFieldSchema, StateResourceKind, SymKind, SymPayload, TypeError, TypeUnifier, TypeVar,
    Value, Width, WidthRule, WidthVar, build, canonicalize_for_selection, execute,
    execute_with_memory, infer_types, infer_widths, op_kind, op_name, parse, scalar_op,
    scalar_op_named,
};

pub use tir_symbolic::sem::{
    EquivalenceOracle, ExtendSemBytes, FuzzOracle, ProofOutcome, SemBlobBuilder, SemOp,
    SemPayloadDesc, SmtOracle, UnsupportedReason, confirm_bool_via_if,
    confirm_extension_via_shifts, decode_sem_ops, float_payload, int_payload,
};
pub(crate) use tir_symbolic::sem::{con, op, sym};

pub(crate) mod axioms;
pub mod fp_refinement;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CounterexampleBinding {
    pub name: String,
    pub bits: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counterexample {
    pub bindings: Vec<CounterexampleBinding>,
    pub lhs_bits: Option<String>,
    pub rhs_bits: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleProofResult {
    Proven,
    Admitted,
    Disproven {
        counterexample: Option<Counterexample>,
    },
    Unsupported {
        reason: UnsupportedReason,
    },
}

impl RuleProofResult {
    pub fn is_proven(&self) -> bool {
        matches!(self, Self::Proven)
    }
}

/// Report the proof outcome of every PDL rule at `width` bits for each of its
/// width names. Equality rules with SMT proofs use the solver. Refinements with
/// contract proofs use their structural checker and report admission.
pub fn prove_rules(source: &str, width: u64) -> Result<Vec<(String, RuleProofResult)>, String> {
    let file = tir_pdl::compile(source).map_err(|diagnostics| {
        diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    file.items
        .iter()
        .filter_map(|item| match item {
            tir_pdl::Item::Rule(rule) => Some(rule.as_ref()),
            _ => None,
        })
        .map(|rule| {
            let result = match (rule.kind, rule.proof()) {
                (tir_pdl::RuleKind::Refinement, tir_pdl::Proof::Contract) => {
                    if rule_has_shaped_float(rule) {
                        RuleProofResult::Unsupported {
                            reason: UnsupportedReason::UnsupportedObligation(
                                "shaped floating-point refinements are not supported".into(),
                            ),
                        }
                    } else {
                        match fp_refinement::check_contraction_rule(rule) {
                            Ok(()) => RuleProofResult::Admitted,
                            Err(reason) => RuleProofResult::Unsupported {
                                reason: UnsupportedReason::UnsupportedObligation(format!(
                                    "structural refinement check failed: {reason:?}"
                                )),
                            },
                        }
                    }
                }
                (tir_pdl::RuleKind::Refinement, tir_pdl::Proof::Smt) => {
                    RuleProofResult::Unsupported {
                        reason: UnsupportedReason::UnsupportedObligation(
                            "SMT proofs for refinements are not supported; use `proof contract` for checked admission".into(),
                        ),
                    }
                }
                (_, tir_pdl::Proof::Trusted) => RuleProofResult::Unsupported {
                    reason: UnsupportedReason::UnsupportedObligation(
                        "trusted rules have no checked proof".into(),
                    ),
                },
                (_, tir_pdl::Proof::Definitional) => RuleProofResult::Unsupported {
                    reason: UnsupportedReason::UnsupportedObligation(
                        "definitional rules have no solver model".into(),
                    ),
                },
                (_, tir_pdl::Proof::Contract) => RuleProofResult::Unsupported {
                    reason: UnsupportedReason::UnsupportedObligation(
                        "contract proofs only apply to refinements".into(),
                    ),
                },
                (_, tir_pdl::Proof::Smt) => match axioms::pdl::axiom_from_rule(rule) {
                    Ok(axiom) => {
                        let widths = vec![width; axiom.width_names.len()];
                        match axiom.prove_outcome(&widths) {
                            ProofOutcome::Proven => RuleProofResult::Proven,
                            ProofOutcome::Disproven { model, values } => {
                                RuleProofResult::Disproven {
                                    counterexample: Some(axiom.counterexample(&model, values)),
                                }
                            }
                            ProofOutcome::Unsupported(reason) => {
                                RuleProofResult::Unsupported { reason }
                            }
                        }
                    }
                    Err(reason) => RuleProofResult::Unsupported {
                        reason: UnsupportedReason::UnsupportedObligation(reason),
                    },
                },
            };
            Ok((rule.name.clone(), result))
        })
        .collect()
}

fn rule_has_shaped_float(rule: &tir_pdl::Rule) -> bool {
    fn term_has_shaped_float(term: &tir_pdl::Term) -> bool {
        if matches!(term.ty, Some(tir_pdl::Type::ShapedFloat { .. })) {
            return true;
        }
        match &term.kind {
            tir_pdl::TermKind::Binder {
                ty: Some(tir_pdl::BindingType::Type(tir_pdl::Type::ShapedFloat { .. })),
                ..
            } => true,
            tir_pdl::TermKind::Operation {
                operands,
                dependencies,
                ..
            } => operands
                .iter()
                .chain(dependencies)
                .any(term_has_shaped_float),
            tir_pdl::TermKind::Keep(inner) => term_has_shaped_float(inner),
            _ => false,
        }
    }

    term_has_shaped_float(&rule.lhs) || term_has_shaped_float(&rule.rhs)
}
pub(crate) mod egraph;
pub mod node;
pub(crate) mod rewrites;
pub use egraph::SemEGraph;
pub use node::{IrOp, Kind, Prov, SemNode, SemPayload, template_node};
pub use rewrites::{SaturationLimits, Theory};

/// The post-order graph core builds semantic expressions into: the shared
/// [`tir_symbolic::sem::SemGraph`] annotated with [`NodeMeta`], so a node can
/// carry its originating op and inferred type.
pub type SemGraph = tir_symbolic::sem::SemGraph<NodeMeta>;

/// [`ExtendSemBytes`] for graphs that carry [`NodeMeta`], resolving
/// [`SemOp::Typed`] widths against the context's interned integer types.
pub trait ExtendSemBytesTyped:
    MutDag<Node = SymKind, Leaf = SymPayload<ValueId>, Annotation = NodeMeta> + Sized
{
    fn extend_sem_bytes_typed(
        &mut self,
        context: &crate::Context,
        kinds: &[SymKind],
        blob: &[u8],
        offset: u32,
    ) -> NodeId {
        use crate::graph::MetaMutDag as _;
        self.extend_sem_bytes_with(kinds, blob, offset, |g, node, width| {
            g.set_actual_type(node, crate::builtin::IntegerType::new(context, width));
        })
    }
}

impl<G: MutDag<Node = SymKind, Leaf = SymPayload<ValueId>, Annotation = NodeMeta>>
    ExtendSemBytesTyped for G
{
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
