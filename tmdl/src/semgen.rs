//! What the SMT-LIB and BTOR2 emitters share: the walk over a lowered value
//! graph, with the width, signedness and evaluation-order rules stated once,
//! and the helpers both resolve an instruction's operands with.
//!
//! A backend supplies the vocabulary — how a constant, a binary operator or a
//! slice is spelled — and [`emit`] decides what is spelled, so the two solver
//! formats cannot drift apart on the semantics of a `sext` or a shift.

use std::collections::HashMap;

use tir_graph::{Dag, NodeId};
use tir_symbolic::lang::{SmtTemplate, SymKind, SymPayload, scalar_op};
use tir_symbolic::sem::ValueId;

use crate::Type;
use crate::ast;
use crate::utils::{
    EncodingShape, parse_literal_value, resolve_isa_param_values, resolve_operand_widths,
    resolve_operands_for_instruction, resolve_params_for_instruction,
};

/// A graph of value terms, as [`crate::sem_expr_state`] lowers a behavior to.
pub(crate) trait ValueDag: Dag<Node = SymKind, Leaf = SymPayload<ValueId>> {}
impl<G: Dag<Node = SymKind, Leaf = SymPayload<ValueId>>> ValueDag for G {}

/// One solver format's spelling of the terms [`emit`] builds. Every operand
/// handed to a binary operator, comparison or `ite` already has the width the
/// result takes; the shift amount already has the left operand's width.
pub(crate) trait TermBackend {
    type Val: Clone;

    fn width(&self, value: &Self::Val) -> u32;
    fn signed(&self, value: &Self::Val) -> bool;
    /// Whether the backend can spell `kind` at all; a term it cannot spell is
    /// rejected before any of its operands are emitted.
    fn supports(&self, kind: SymKind) -> bool;
    fn constant(&mut self, width: u32, value: u64, signed: bool) -> Self::Val;
    fn symbol(&mut self, id: u32) -> Option<Self::Val>;
    fn binary(&mut self, kind: SymKind, lhs: Self::Val, rhs: Self::Val, signed: bool) -> Self::Val;
    fn compare(&mut self, kind: SymKind, lhs: Self::Val, rhs: Self::Val) -> Self::Val;
    fn shift(
        &mut self,
        kind: SymKind,
        lhs: Self::Val,
        amount: Self::Val,
        signed: bool,
    ) -> Self::Val;
    fn unary(&mut self, kind: SymKind, value: Self::Val) -> Self::Val;
    fn concat(&mut self, high: Self::Val, low: Self::Val) -> Self::Val;
    fn widen(&mut self, value: Self::Val, target: u32, signed: bool) -> Self::Val;
    fn slice(&mut self, value: Self::Val, high: u32, low: u32) -> Self::Val;
    fn ite(
        &mut self,
        condition: Self::Val,
        then: Self::Val,
        otherwise: Self::Val,
        signed: bool,
    ) -> Self::Val;
    fn as_bool(&mut self, value: Self::Val) -> Self::Val;
    /// A term outside the shared vocabulary (memory, atomics, clamping):
    /// `emit` yields the backend's spelling of an operand.
    fn special<G: ValueDag>(
        &mut self,
        graph: &G,
        node: NodeId,
        emit: &mut dyn FnMut(&mut Self, NodeId) -> Option<Self::Val>,
    ) -> Option<Self::Val>;

    /// Both operands at their common width.
    fn coerce(&mut self, lhs: Self::Val, rhs: Self::Val) -> (Self::Val, Self::Val) {
        let width = self.width(&lhs).max(self.width(&rhs));
        let (ls, rs) = (self.signed(&lhs), self.signed(&rhs));
        (self.widen(lhs, width, ls), self.widen(rhs, width, rs))
    }

    /// Exactly `target` bits: widened when narrower, truncated when wider.
    fn fit(&mut self, value: Self::Val, target: u32) -> Self::Val {
        if self.width(&value) > target {
            self.slice(value, target - 1, 0)
        } else {
            let signed = self.signed(&value);
            self.widen(value, target, signed)
        }
    }
}

/// Fold a symbol-free subtree to a constant at the interpreter's widths.
/// Width expressions such as `log2Ceil(self.XLEN) - 1` reach the emitters
/// unfolded, so structural `Constant` matching is not enough.
pub(crate) fn eval_const(graph: &impl ValueDag, node: NodeId) -> Option<(u64, u32)> {
    let child = |index: usize| eval_const(graph, graph.children(node).nth(index)?);
    let arith = |f: fn(u64, u64) -> u64| -> Option<(u64, u32)> {
        let (a, wa) = child(0)?;
        let (b, wb) = child(1)?;
        let w = wa.max(wb);
        let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
        Some((f(a, b) & mask, w))
    };
    match graph.get_node(node) {
        SymKind::Constant => match graph.get_leaf_data(node)? {
            SymPayload::Int(i) => Some((i.to_u64(), i.width())),
            _ => None,
        },
        SymKind::Add => arith(u64::wrapping_add),
        SymKind::Sub => arith(u64::wrapping_sub),
        SymKind::Mul => arith(u64::wrapping_mul),
        SymKind::Log2Ceil => {
            let (v, w) = child(0)?;
            let r = if v <= 1 {
                0
            } else {
                64 - (v - 1).leading_zeros() as u64
            };
            Some((r, w))
        }
        _ => None,
    }
}

/// Spell `node` in the backend's vocabulary, or `None` where the term has no
/// model there. Operands are emitted left to right, before the operator, so a
/// backend numbering its nodes in emission order numbers them the same way
/// whatever the format.
pub(crate) fn emit<B: TermBackend>(
    graph: &impl ValueDag,
    node: NodeId,
    b: &mut B,
) -> Option<B::Val> {
    let kind = *graph.get_node(node);
    if !b.supports(kind) {
        return None;
    }
    let child_node = |index: usize| graph.children(node).nth(index);
    let const_child =
        |index: usize| -> Option<u64> { Some(eval_const(graph, child_node(index)?)?.0) };

    if let Some(op) = scalar_op(kind) {
        return emit_scalar_op(graph, node, kind, &op.smt, b);
    }

    match kind {
        SymKind::Symbol => match graph.get_leaf_data(node)? {
            SymPayload::SymbolId(id) => b.symbol(*id),
            _ => None,
        },
        SymKind::Constant => match graph.get_leaf_data(node)? {
            SymPayload::Int(i) => {
                let w = i.width();
                let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
                Some(b.constant(w, i.to_u64() & mask, i.is_signed()))
            }
            _ => None,
        },
        SymKind::If => {
            let condition = emit(graph, child_node(0)?, b)?;
            let condition = b.as_bool(condition);
            let (t, e) = (
                emit(graph, child_node(1)?, b)?,
                emit(graph, child_node(2)?, b)?,
            );
            let signed = b.signed(&t) || b.signed(&e);
            let (t, e) = b.coerce(t, e);
            Some(b.ite(condition, t, e, signed))
        }
        SymKind::ZExt | SymKind::SExt => {
            let x = emit(graph, child_node(0)?, b)?;
            let target = const_child(1)? as u32;
            if target < b.width(&x) {
                return None;
            }
            Some(b.widen(x, target, kind == SymKind::SExt))
        }
        SymKind::Bitcast => emit(graph, child_node(0)?, b),
        SymKind::Extract => emit_extract(graph, node, b),
        SymKind::Log2Ceil => {
            let (v, w) = eval_const(graph, node)?;
            Some(b.constant(w, v, false))
        }
        _ => b.special(graph, node, &mut |b, child| emit(graph, child, b)),
    }
}

fn emit_scalar_op<B: TermBackend>(
    graph: &impl ValueDag,
    node: NodeId,
    kind: SymKind,
    template: &SmtTemplate,
    b: &mut B,
) -> Option<B::Val> {
    let child_node = |index: usize| graph.children(node).nth(index);
    match template {
        SmtTemplate::Binary(_) => {
            let (x, y) = (
                emit(graph, child_node(0)?, b)?,
                emit(graph, child_node(1)?, b)?,
            );
            // Result signedness `signed && signed` mirrors `APInt` binary ops.
            let signed = b.signed(&x) && b.signed(&y);
            let (x, y) = b.coerce(x, y);
            Some(b.binary(kind, x, y, signed))
        }
        SmtTemplate::Compare(_) => {
            let (x, y) = (
                emit(graph, child_node(0)?, b)?,
                emit(graph, child_node(1)?, b)?,
            );
            let (x, y) = b.coerce(x, y);
            Some(b.compare(kind, x, y))
        }
        // The result width is the left operand's; the amount is
        // reinterpreted at that width, matching the interpreter.
        SmtTemplate::Shift(_) => {
            let lhs = emit(graph, child_node(0)?, b)?;
            let amount = emit(graph, child_node(1)?, b)?;
            let width = b.width(&lhs);
            let amount = b.fit(amount, width);
            let signed = match kind {
                SymKind::ShiftRightArithmetic => true,
                SymKind::ShiftRightLogic => false,
                _ => b.signed(&lhs),
            };
            Some(b.shift(kind, lhs, amount, signed))
        }
        SmtTemplate::Unary(_) => {
            let x = emit(graph, child_node(0)?, b)?;
            Some(b.unary(kind, x))
        }
        SmtTemplate::Concat => {
            let high = emit(graph, child_node(0)?, b)?;
            let low = emit(graph, child_node(1)?, b)?;
            Some(b.concat(high, low))
        }
    }
}

fn emit_extract<B: TermBackend>(graph: &impl ValueDag, node: NodeId, b: &mut B) -> Option<B::Val> {
    let child_node = |index: usize| graph.children(node).nth(index);
    let const_child =
        |index: usize| -> Option<u64> { Some(eval_const(graph, child_node(index)?)?.0) };
    let x = emit(graph, child_node(0)?, b)?;
    let high = const_child(1)? as u32;
    let low = const_child(2)? as u32;
    if high < low {
        return None;
    }
    let mul = child_node(0)?;
    if low >= b.width(&x) && matches!(graph.get_node(mul), SymKind::Mul) {
        // `extract(a * b, 2N-1, N)` is the TMDL idiom for the high half
        // of a full multiply (RISC-V `mulh`); the interpreter recomputes
        // it as a signed double-width product.
        let m0 = emit(graph, graph.children(mul).next()?, b)?;
        let m1 = emit(graph, graph.children(mul).nth(1)?, b)?;
        let (m0, m1) = b.coerce(m0, m1);
        let wm = b.width(&m0);
        if high >= 2 * wm {
            return None;
        }
        let m0 = b.widen(m0, 2 * wm, true);
        let m1 = b.widen(m1, 2 * wm, true);
        let product = b.binary(SymKind::Mul, m0, m1, true);
        Some(b.slice(product, high, low))
    } else if high < b.width(&x) {
        Some(b.slice(x, high, low))
    } else {
        None
    }
}

/// Resolve a register-class parameter (`ENCODING_LEN`, `WIDTH`) to a number:
/// either a literal or a `self.PARAM` reference into the target ISA.
pub(crate) fn eval_class_param(
    rc: &ast::RegisterClass,
    name: &str,
    isa_params: &HashMap<String, i64>,
) -> Option<i64> {
    match rc.parameters.get(name)? {
        (_, Some(ast::Expr::Lit(ast::Lit::Int(li)))) => Some(parse_literal_value(li) as i64),
        (_, Some(ast::Expr::Field(f))) if matches!(&*f.base, ast::Expr::Ident(id) if id.name == "self") => {
            isa_params.get(f.member.as_str()).copied()
        }
        _ => None,
    }
}

/// Instruction operands with `bits<expr>` widths resolved for the target ISA:
/// the ISA's own parameter values win over the cross-ISA maximum, so an
/// instruction shared by RV32I and RV64I sees XLEN=32 on RV32I.
pub(crate) fn resolved_operands<'a>(
    isa_params: &HashMap<String, i64>,
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, Type)> {
    let mut params = resolve_isa_param_values(inst, item_cache);
    params.extend(isa_params.iter().map(|(k, v)| (k.clone(), *v)));
    resolve_operand_widths(resolve_operands_for_instruction(inst, item_cache), &params)
}

/// The numeric parameters a behavior is lowered under: the instruction's ISA
/// parameters, overridden by the target ISA's own values, then its literal
/// template parameters.
pub(crate) fn numeric_params<'a>(
    isa_params: &HashMap<String, i64>,
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> HashMap<String, i64> {
    let mut params = resolve_isa_param_values(inst, item_cache);
    params.extend(isa_params.iter().map(|(k, v)| (k.clone(), *v)));
    params.extend(
        resolve_params_for_instruction(inst, item_cache)
            .into_iter()
            .filter_map(|(name, (_ty, value))| match value {
                Some(ast::Expr::Lit(ast::Lit::Int(li))) => {
                    Some((name, parse_literal_value(&li) as i64))
                }
                _ => None,
            }),
    );
    params
}

/// Per operand, the `(operand_lo, operand_hi, word_lo, word_hi)` bit runs an
/// encoding spells it in.
pub(crate) type Pieces = HashMap<String, Vec<(u16, u16, u16, u16)>>;

/// The fixed fields a shape's decoder tests, as `(word_hi, word_lo, value)`,
/// and the pieces each operand is reassembled from.
pub(crate) fn decode_layout(
    shape: &EncodingShape,
    instruction: &ast::Instruction,
    item_cache: &HashMap<&str, &ast::Item>,
    operands: &HashMap<String, Type>,
) -> (Vec<(u16, u16, u128)>, Pieces) {
    let params = resolve_params_for_instruction(instruction, item_cache);
    let mut guards = Vec::new();
    let mut pieces: Pieces = HashMap::new();

    for arm in &shape.arms {
        let word_lo = arm.start;
        let word_hi = arm.end.unwrap_or(arm.start);
        match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => {
                guards.push((word_hi, word_lo, parse_literal_value(li) as u128));
            }
            ast::Expr::Ident(id) => {
                if operands.contains_key(&id.name) {
                    let w = word_hi - word_lo;
                    pieces
                        .entry(id.name.clone())
                        .or_default()
                        .push((0, w, word_lo, word_hi));
                } else if let Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) =
                    params.get(&id.name)
                {
                    guards.push((word_hi, word_lo, parse_literal_value(li) as u128));
                }
            }
            ast::Expr::Slice(s) => {
                if let ast::Expr::Ident(id) = &*s.base
                    && operands.contains_key(&id.name)
                {
                    pieces
                        .entry(id.name.clone())
                        .or_default()
                        .push((s.lo, s.hi, word_lo, word_hi));
                }
            }
            ast::Expr::IndexAccess(s) => {
                if let ast::Expr::Ident(id) = &*s.base
                    && operands.contains_key(&id.name)
                {
                    pieces
                        .entry(id.name.clone())
                        .or_default()
                        .push((s.index, s.index, word_lo, word_hi));
                }
            }
            // `x as bits<n>`: the operand's low n bits, in the n bits the arm
            // spells.
            ast::Expr::Cast(cast) => {
                if let ast::Expr::Ident(id) = &*cast.x
                    && operands.contains_key(&id.name)
                {
                    pieces.entry(id.name.clone()).or_default().push((
                        0,
                        word_hi - word_lo,
                        word_lo,
                        word_hi,
                    ));
                }
            }
            _ => {}
        }
    }
    (guards, pieces)
}
