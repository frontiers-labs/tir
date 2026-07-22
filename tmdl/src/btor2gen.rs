//! BTOR2 emission of a per-instruction reference checker for hardware model
//! checking through an architecture-neutral retirement interface.
//!
//! Why a checker and not a full transition system: a pipelined implementation
//! and a single-step ISA model can only be compared by decoupling timing from
//! semantics. The implementation exposes a retirement interface — for each
//! committed instruction it reports `pc`, `insn`, ordered source register
//! values, one architectural destination write, and `next_pc`. The model is the
//! golden side: it decodes `insn`, computes the architectural post-state, and
//! asserts that the implementation's report matches. Composed with the
//! implementation's own BTOR2, this relation becomes a miter for a BMC engine.
//!
//! Scope mirrors `verify-smt`: register-only instructions. Behaviors touching
//! memory or traps are not modeled and are dropped from the dispatch (the
//! property only fires on decoded, modeled instructions, so dropping cannot
//! produce a false counterexample).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use crate::Type;
use crate::ast;
use crate::error::TMDLError;
use crate::sem_expr_state;
use crate::utils::{
    get_encoding_arms, isa_param_values, item_supports_isa, parse_literal_value,
    resolve_isa_param_values, resolve_operand_widths, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};
use tir::graph::{Dag, NodeId};
use tir::sem::{SymKind as ExprKind, SymPayload as ExprPayload};

type ExprPostGraph = sem_expr_state::ValueGraph;

// ---------------------------------------------------------------------------
// Target context (register-file layout resolved against the ISA)
// ---------------------------------------------------------------------------

struct ClassInfo {
    idx_width: u16,
    val_width: u16,
    zero_index: Option<u16>,
    storage: String,
    architectural_integer: bool,
}

struct Ctx<'a> {
    isa: &'a str,
    xlen: u16,
    classes: BTreeMap<String, ClassInfo>,
    pc_classes: std::collections::HashSet<String>,
    retirement_storage: String,
    isa_params: HashMap<String, i64>,
}

impl Ctx<'_> {
    fn idx_width(&self, class: &str) -> u16 {
        self.classes
            .get(&class.to_lowercase())
            .map_or(5, |c| c.idx_width)
    }

    fn val_width(&self, class: &str) -> u16 {
        let class = class.to_lowercase();
        if self.pc_classes.contains(&class) {
            return self.xlen;
        }
        self.classes.get(&class).map_or(self.xlen, |c| c.val_width)
    }

    fn zero_index(&self, class: &str) -> Option<u16> {
        self.classes
            .get(&class.to_lowercase())
            .and_then(|c| c.zero_index)
    }

    fn is_retirement_class(&self, class: &str) -> bool {
        self.classes
            .get(&class.to_lowercase())
            .is_some_and(|info| info.storage == self.retirement_storage)
    }
}

fn eval_class_param(
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

fn is_pc_class(rc: &ast::RegisterClass) -> bool {
    rc.resolve_registers()
        .any(|r| r.traits.contains(&ast::RegisterTrait::ProgramCounter))
}

fn item_enabled<'a>(
    for_isas: &[String],
    isa: &str,
    enabled_isas: Option<&[String]>,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> bool {
    item_supports_isa(for_isas, isa, item_cache)
        && enabled_isas.is_none_or(|enabled| {
            for_isas
                .iter()
                .any(|candidate| enabled.iter().any(|name| name == candidate))
        })
}

// ---------------------------------------------------------------------------
// BTOR2 node builder
// ---------------------------------------------------------------------------

use tir_symbolic::btor2::{BitVec as Bv, Builder as Btor2};

// ---------------------------------------------------------------------------
// Expression lowering (mirror of smtlibgen::emit_sem_expr over BTOR2 nodes)
// ---------------------------------------------------------------------------

enum SymbolInfo {
    Register { class: String },
    Variable { name: String },
}

struct Resolver<'a> {
    symbols: HashMap<u32, SymbolInfo>,
    operands: &'a HashMap<String, Type>,
    /// Decoded operand values keyed by lowercase operand name: source register
    /// values (`rs1`, `rs2`) come from retirement inputs, immediates from the
    /// instruction word.
    operand_vals: &'a HashMap<String, Bv>,
    pc: Bv,
    ctx: &'a Ctx<'a>,
}

impl Resolver<'_> {
    fn resolve(&self, id: u32) -> Option<Bv> {
        match self.symbols.get(&id)? {
            SymbolInfo::Register { class, .. }
                if self.ctx.pc_classes.contains(&class.to_lowercase()) =>
            {
                Some(self.pc)
            }
            // A fixed non-PC register read is not part of the retirement
            // contract; reject so the instruction is dropped.
            SymbolInfo::Register { .. } => None,
            SymbolInfo::Variable { name } => match self.operands.get(name)? {
                Type::Struct(rc) if self.ctx.pc_classes.contains(&rc.to_lowercase()) => {
                    Some(self.pc)
                }
                _ => self.operand_vals.get(&name.to_lowercase()).copied(),
            },
        }
    }
}

/// Fold a symbol-free subtree to a constant (width expressions such as
/// `log2Ceil(self.XLEN) - 1` reach the emitter unfolded).
fn eval_const(graph: &ExprPostGraph, node: NodeId) -> Option<(u64, u32)> {
    let child = |idx: usize| eval_const(graph, graph.children(node).nth(idx)?);
    let arith = |f: fn(u64, u64) -> u64| -> Option<(u64, u32)> {
        let (a, wa) = child(0)?;
        let (b, wb) = child(1)?;
        let w = wa.max(wb);
        let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
        Some((f(a, b) & mask, w))
    };
    match graph.get_node(node) {
        ExprKind::Constant => match graph.get_leaf_data(node)? {
            ExprPayload::Int(i) => Some((i.to_u64(), i.width())),
            _ => None,
        },
        ExprKind::Add => arith(u64::wrapping_add),
        ExprKind::Sub => arith(u64::wrapping_sub),
        ExprKind::Mul => arith(u64::wrapping_mul),
        ExprKind::Log2Ceil => {
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

fn emit(graph: &ExprPostGraph, node: NodeId, r: &Resolver<'_>, b: &mut Btor2) -> Option<Bv> {
    let child_node = |idx: usize| graph.children(node).nth(idx);
    let const_child = |idx: usize| -> Option<u64> { Some(eval_const(graph, child_node(idx)?)?.0) };

    macro_rules! ch {
        ($i:expr) => {
            emit(graph, child_node($i)?, r, b)?
        };
    }
    macro_rules! arith {
        ($op:expr) => {{
            let (x, y) = (ch!(0), ch!(1));
            let signed = x.signed && y.signed;
            let (x, y) = b.coerce(x, y);
            Some(b.binary($op, x, y, signed))
        }};
    }
    macro_rules! cmp {
        ($op:expr) => {{
            let (x, y) = (ch!(0), ch!(1));
            let (x, y) = b.coerce(x, y);
            Some(b.compare($op, x, y))
        }};
    }
    // Result width is the left operand's; the amount is reinterpreted at that
    // width, matching the interpreter.
    macro_rules! shift {
        ($op:expr, $sgn:expr) => {{
            let lhs = ch!(0);
            let amt = ch!(1);
            let amt = b.fit(amt, lhs.width);
            let sgn: fn(bool) -> bool = $sgn;
            Some(b.binary($op, lhs, amt, sgn(lhs.signed)))
        }};
    }

    match graph.get_node(node) {
        ExprKind::Symbol => match graph.get_leaf_data(node)? {
            ExprPayload::SymbolId(id) => r.resolve(*id),
            _ => None,
        },
        ExprKind::Constant => match graph.get_leaf_data(node)? {
            ExprPayload::Int(i) => {
                let w = i.width();
                let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
                Some(Bv {
                    signed: i.is_signed(),
                    ..b.constant(w, i.to_u64() & mask)
                })
            }
            _ => None,
        },
        ExprKind::Add => arith!("add"),
        ExprKind::Sub => arith!("sub"),
        ExprKind::Mul => arith!("mul"),
        ExprKind::Div => arith!("sdiv"),
        ExprKind::UDiv => arith!("udiv"),
        ExprKind::Or => arith!("or"),
        ExprKind::And => arith!("and"),
        ExprKind::Xor => arith!("xor"),
        ExprKind::Eq => cmp!("eq"),
        ExprKind::Ne => cmp!("neq"),
        ExprKind::Lt => cmp!("slt"),
        ExprKind::Gt => cmp!("sgt"),
        ExprKind::Ge => cmp!("sgte"),
        ExprKind::ULt => cmp!("ult"),
        ExprKind::ULe => cmp!("ulte"),
        ExprKind::UGt => cmp!("ugt"),
        ExprKind::UGe => cmp!("ugte"),
        ExprKind::ShiftLeft => shift!("sll", |s| s),
        ExprKind::ShiftRightLogic => shift!("srl", |_| false),
        ExprKind::ShiftRightArithmetic => shift!("sra", |_| true),
        ExprKind::Bitcast => Some(ch!(0)),
        ExprKind::Not => {
            let x = ch!(0);
            Some(b.not(x))
        }
        ExprKind::If => {
            let cond = ch!(0);
            let cond = b.as_bool(cond);
            let (t, e) = (ch!(1), ch!(2));
            let signed = t.signed || e.signed;
            let (t, e) = b.coerce(t, e);
            Some(b.ite(cond, t, e, signed))
        }
        ExprKind::ZExt => {
            let x = ch!(0);
            let target = const_child(1)? as u32;
            if target < x.width {
                return None;
            }
            Some(b.widen(x, target, false))
        }
        ExprKind::SExt => {
            let x = ch!(0);
            let target = const_child(1)? as u32;
            if target < x.width {
                return None;
            }
            Some(b.widen(x, target, true))
        }
        ExprKind::Extract => {
            let high = const_child(1)? as u32;
            let low = const_child(2)? as u32;
            if high < low {
                return None;
            }
            let mul = child_node(0)?;
            if low >= ch!(0).width && matches!(graph.get_node(mul), ExprKind::Mul) {
                // `extract(a * b, 2N-1, N)`: high half of a signed full multiply
                // (RISC-V `mulh`). Recompute as a double-width signed product.
                let m0 = emit(graph, graph.children(mul).next()?, r, b)?;
                let m1 = emit(graph, graph.children(mul).nth(1)?, r, b)?;
                let (m0, m1) = b.coerce(m0, m1);
                let wm = m0.width;
                if high >= 2 * wm {
                    return None;
                }
                let m0 = b.widen(m0, 2 * wm, true);
                let m1 = b.widen(m1, 2 * wm, true);
                let prod = b.binary("mul", m0, m1, true);
                Some(b.slice(prod, high, low))
            } else {
                let x = ch!(0);
                if high >= x.width {
                    return None;
                }
                Some(b.slice(x, high, low))
            }
        }
        ExprKind::Log2Ceil => {
            let (v, w) = eval_const(graph, node)?;
            Some(b.constant(w, v))
        }
        ExprKind::Clamp => {
            let input = ch!(0);
            let (lt, gt) = if input.signed {
                ("slt", "sgt")
            } else {
                ("ult", "ugt")
            };
            let min = ch!(1);
            let max = ch!(2);
            let w = input.width.max(min.width).max(max.width);
            let input = b.widen(input, w, input.signed);
            let min = b.widen(min, w, false);
            let max = b.widen(max, w, false);
            let below = b.compare(lt, input, min);
            let above = b.compare(gt, input, max);
            let hi = b.ite(above, max, input, input.signed);
            Some(b.ite(below, min, hi, input.signed))
        }
        ExprKind::LoadMemory
        | ExprKind::StoreMemory
        | ExprKind::Sqrt
        | ExprKind::Fma
        | ExprKind::SRem
        | ExprKind::URem
        | ExprKind::Neg
        | ExprKind::Le
        | ExprKind::Xnor
        | ExprKind::Concat
        | ExprKind::FAdd
        | ExprKind::FSub
        | ExprKind::FMul
        | ExprKind::FDiv
        | ExprKind::SIToFP
        | ExprKind::UIToFP
        | ExprKind::FPToSI
        | ExprKind::FPToUI
        | ExprKind::Map
        | ExprKind::Zip
        | ExprKind::IterConcat
        | ExprKind::Split
        | ExprKind::Reduce
        | ExprKind::Arg
        | ExprKind::LoadReserved
        | ExprKind::StoreConditional
        | ExprKind::AtomicRmw
        | ExprKind::Fence
        | ExprKind::StateAssign
        | ExprKind::StateStore
        | ExprKind::StateStoreConditional
        | ExprKind::StateFence
        | ExprKind::StateTrap
        | ExprKind::StateBlock
        | ExprKind::StateIf
        | ExprKind::StateTry
        | ExprKind::StateHandler => None,
    }
}

// ---------------------------------------------------------------------------
// Per-instruction checker: decode + execute over retirement signals
// ---------------------------------------------------------------------------

/// Architectural post-state the checker computes for one decoded instruction.
#[derive(Clone, Copy)]
struct PostState {
    dst_we: Bv,
    dst_val: Bv,
    dst_addr: Bv,
    next_pc: Bv,
}

struct Checker<'a> {
    ctx: &'a Ctx<'a>,
    operands: HashMap<String, Type>,
    operand_vals: HashMap<String, Bv>,
    operand_addrs: HashMap<String, (Bv, String)>,
    behavior: &'a sem_expr_state::BehaviorGraph,
    pc: Bv,
    b: RefCell<&'a mut Btor2>,
    failed: Cell<bool>,
}

impl Checker<'_> {
    fn emit_val(&self, expression: NodeId) -> Option<Bv> {
        let mut symbols = HashMap::new();
        for (name, id) in &self.behavior.variable_symbols {
            symbols.insert(*id, SymbolInfo::Variable { name: name.clone() });
        }
        for ((class, _number), id) in &self.behavior.register_symbols {
            symbols.insert(
                *id,
                SymbolInfo::Register {
                    class: class.clone(),
                },
            );
        }
        let resolver = Resolver {
            symbols,
            operands: &self.operands,
            operand_vals: &self.operand_vals,
            pc: self.pc,
            ctx: self.ctx,
        };
        let (graph, root) = self.behavior.value_graph(expression)?;
        let mut b = self.b.borrow_mut();
        emit(&graph, root, &resolver, &mut b).or_else(|| {
            self.failed.set(true);
            None
        })
    }
}

impl sem_expr_state::BehaviorEmitter for Checker<'_> {
    type State = PostState;

    fn assign(
        &self,
        destination: &sem_expr_state::Destination,
        value: NodeId,
        state: &PostState,
    ) -> Option<PostState> {
        let value = self.emit_val(value)?;
        let xlen = self.ctx.xlen as u32;
        let mut b = self.b.borrow_mut();

        if let sem_expr_state::Destination::FixedRegister { class, .. } = destination
            && self.ctx.pc_classes.contains(&class.to_lowercase())
        {
            let next_pc = b.fit(value, xlen);
            return Some(PostState { next_pc, ..*state });
        }

        let name = match destination {
            sem_expr_state::Destination::Ident(name) => Some(name.as_str()),
            sem_expr_state::Destination::Path { members, .. } if members.len() == 1 => {
                Some(members[0].as_str())
            }
            _ => None,
        }?;

        if name == "pc" {
            let next_pc = b.fit(value, xlen);
            return Some(PostState { next_pc, ..*state });
        }
        match self.operands.get(name) {
            Some(Type::Struct(class)) if self.ctx.pc_classes.contains(&class.to_lowercase()) => {
                let next_pc = b.fit(value, xlen);
                Some(PostState { next_pc, ..*state })
            }
            Some(Type::Struct(class)) if !self.ctx.is_retirement_class(class) => None,
            Some(Type::Struct(class)) => {
                let dst_val = b.fit(value, self.ctx.val_width(class) as u32);
                let (dst_addr, class) = self.operand_addrs.get(name)?.clone();
                let dst_we = match self.ctx.zero_index(&class) {
                    Some(index) => {
                        let zero = b.constant(dst_addr.width, u64::from(index));
                        b.compare("neq", dst_addr, zero)
                    }
                    None => b.constant(1, 1),
                };
                Some(PostState {
                    dst_we,
                    dst_val,
                    dst_addr,
                    ..*state
                })
            }
            _ => None,
        }
    }

    fn value_effect(
        &self,
        _kind: tir::sem::SymKind,
        _value: NodeId,
        _state: &PostState,
    ) -> Option<PostState> {
        None
    }

    fn trap(
        &self,
        _arguments: &[NodeId],
        _params: &[String],
        _handler: Option<NodeId>,
        _state: &PostState,
        _fold: &dyn Fn(NodeId, &PostState) -> PostState,
    ) -> Option<PostState> {
        None
    }

    fn branch(
        &self,
        condition: NodeId,
        _entry_state: &PostState,
        then_state: &PostState,
        else_state: &PostState,
    ) -> PostState {
        let Some(condition) = self.emit_val(condition) else {
            self.failed.set(true);
            return *else_state;
        };
        let mut b = self.b.borrow_mut();
        let condition = b.as_bool(condition);
        PostState {
            dst_we: b.ite(condition, then_state.dst_we, else_state.dst_we, false),
            dst_val: b.ite(condition, then_state.dst_val, else_state.dst_val, false),
            dst_addr: b.ite(condition, then_state.dst_addr, else_state.dst_addr, false),
            next_pc: b.ite(condition, then_state.next_pc, else_state.next_pc, false),
        }
    }

    fn try_except(
        &self,
        _body: NodeId,
        _handlers: &[NodeId],
        _state: &PostState,
        _fold: &dyn Fn(NodeId, &PostState) -> PostState,
    ) -> Option<PostState> {
        None
    }

    fn unsupported(&self) {
        self.failed.set(true);
    }
}

// ---------------------------------------------------------------------------
// Decode: reconstruct operands and the match guard from the instruction word
// ---------------------------------------------------------------------------

type Pieces = HashMap<String, Vec<(u16, u16, u16, u16)>>;

/// Collect fixed-field guards and per-operand bit pieces from the encoding,
/// mirroring `smtlibgen::build_decoder`.
fn decode_layout(
    instruction: &ast::Instruction,
    item_cache: &HashMap<&str, &ast::Item>,
    operands: &HashMap<String, Type>,
) -> (Vec<(u16, u16, u128)>, Pieces) {
    let params = resolve_params_for_instruction(instruction, item_cache);
    let mut guards = Vec::new();
    let mut pieces: Pieces = HashMap::new();

    for arm in get_encoding_arms(instruction, item_cache) {
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
                        .push((s.start, s.end, word_lo, word_hi));
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
            _ => {}
        }
    }
    (guards, pieces)
}

/// Reconstruct one operand from its word pieces, zero-filling gaps, then fit to
/// `target_width`. When the encoding field is wider than the operand (e.g. the
/// RV32 shift-immediate `shamt` occupies a 6-bit field but is 5 bits), the
/// spare high bits are reserved-zero in the architecture; the returned guard
/// (1-bit, true when they are zero) constrains decode to reject the otherwise
/// illegal encodings the hardware rejects.
fn decode_operand(
    b: &mut Btor2,
    insn: Bv,
    mut pieces: Vec<(u16, u16, u16, u16)>,
    target_width: u16,
) -> (Bv, Option<Bv>) {
    if pieces.is_empty() {
        return (b.constant(target_width as u32, 0), None);
    }
    pieces.sort_by_key(|p| std::cmp::Reverse(p.1));
    let mut acc: Option<Bv> = None;
    let push = |b: &mut Btor2, acc: &mut Option<Bv>, frag: Bv| {
        *acc = Some(match acc.take() {
            Some(a) => b.concat(a, frag),
            None => frag,
        });
    };

    let mut expected_hi = pieces[0].1;
    for (op_lo, op_hi, word_lo, word_hi) in &pieces {
        if *op_hi < expected_hi {
            let gap = b.constant((expected_hi - op_hi) as u32, 0);
            push(b, &mut acc, gap);
        }
        let frag = b.slice(insn, *word_hi as u32, *word_lo as u32);
        push(b, &mut acc, frag);
        expected_hi = op_lo.saturating_sub(1);
    }
    let lowest = pieces.last().map(|p| p.0).unwrap_or(0);
    if lowest > 0 {
        let pad = b.constant(lowest as u32, 0);
        push(b, &mut acc, pad);
    }
    let raw = acc.unwrap();
    let target = target_width as u32;
    let guard = if raw.width > target {
        let spare = b.slice(raw, raw.width - 1, target);
        let zero = b.constant(spare.width, 0);
        Some(b.compare("eq", spare, zero))
    } else {
        None
    };
    (b.fit(raw, target), guard)
}

fn build_guard(b: &mut Btor2, insn: Bv, guards: &[(u16, u16, u128)]) -> Bv {
    let mut acc: Option<Bv> = None;
    for (hi, lo, val) in guards {
        let field = b.slice(insn, *hi as u32, *lo as u32);
        let k = b.constant(field.width, *val as u64);
        let eq = b.compare("eq", field, k);
        acc = Some(match acc {
            Some(a) => b.binary("and", a, eq, false),
            None => eq,
        });
    }
    acc.unwrap_or_else(|| b.constant(1, 1))
}

// ---------------------------------------------------------------------------
// Top-level emission
// ---------------------------------------------------------------------------

fn resolved_operands(
    ctx: &Ctx<'_>,
    inst: &ast::Instruction,
    item_cache: &HashMap<&str, &ast::Item>,
) -> Vec<(String, Type)> {
    let mut params = resolve_isa_param_values(inst, item_cache);
    params.extend(ctx.isa_params.iter().map(|(k, v)| (k.clone(), *v)));
    resolve_operand_widths(resolve_operands_for_instruction(inst, item_cache), &params)
}

struct PreparedInstruction<'a> {
    instruction: &'a ast::Instruction,
    operands: Vec<(String, Type)>,
    behavior: sem_expr_state::BehaviorGraph,
    source_operands: Vec<String>,
    width: u16,
}

fn instruction_width(
    instruction: &ast::Instruction,
    item_cache: &HashMap<&str, &ast::Item>,
) -> Option<u16> {
    get_encoding_arms(instruction, item_cache)
        .into_iter()
        .map(|arm| arm.end.unwrap_or(arm.start) + 1)
        .max()
}

pub fn generate_btor2<'a>(
    isa: &str,
    enabled_isas: Option<&[String]>,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    let isa_params = isa_param_values(isa, item_cache);
    let xlen = isa_params.get("XLEN").copied().unwrap_or(64) as u16;

    let mut classes = BTreeMap::new();
    let mut pc_classes = std::collections::HashSet::new();
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        if !item_enabled(&rc.for_isas, isa, enabled_isas, item_cache) {
            continue;
        }
        let name = rc.name.to_lowercase();
        if is_pc_class(rc) {
            pc_classes.insert(name);
            continue;
        }
        let architectural_integer = !rc.resolve_registers().any(|register| {
            register.traits.iter().any(|trait_| {
                matches!(
                    trait_,
                    ast::RegisterTrait::StatusFlag
                        | ast::RegisterTrait::Float
                        | ast::RegisterTrait::Polymorphic
                )
            })
        });
        let storage = rc
            .file
            .as_ref()
            .or(rc.base.as_ref())
            .unwrap_or(&rc.name)
            .to_lowercase();
        classes.insert(
            name,
            ClassInfo {
                idx_width: eval_class_param(rc, "ENCODING_LEN", &isa_params).unwrap_or(5) as u16,
                val_width: eval_class_param(rc, "WIDTH", &isa_params).unwrap_or(xlen as i64) as u16,
                zero_index: rc.hardwired_zero_register_index(),
                storage,
                architectural_integer,
            },
        );
    }
    let retirement_storage = classes
        .iter()
        .filter(|(name, class)| {
            **name == class.storage
                && class.val_width == xlen
                && class.idx_width > 0
                && class.architectural_integer
        })
        .min_by_key(|(_, class)| class.idx_width)
        .map(|(_, class)| class.storage.clone())
        .unwrap_or_else(|| "gpr".to_string());
    let ctx = Ctx {
        isa,
        xlen,
        classes,
        pc_classes,
        retirement_storage,
        isa_params,
    };

    let register_index_map: HashMap<(String, String), u32> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .flat_map(|rc| {
            let class = rc.name.clone();
            rc.register_indices()
                .into_iter()
                .map(move |(name, idx)| ((class.clone(), name), u32::from(idx)))
        })
        .collect();

    let mut prepared = Vec::new();
    for instruction in files.iter().flat_map(|file| file.instructions()) {
        if !item_enabled(&instruction.for_isas, ctx.isa, enabled_isas, item_cache) {
            continue;
        }
        if matches!(&instruction.behavior, ast::Expr::Block(block) if block.stmts.is_empty()) {
            continue;
        }
        let Some(width) = instruction_width(instruction, item_cache) else {
            continue;
        };
        let operands = resolved_operands(&ctx, instruction, item_cache);
        let mut numeric_params = resolve_isa_param_values(instruction, item_cache);
        numeric_params.extend(
            ctx.isa_params
                .iter()
                .map(|(name, value)| (name.clone(), *value)),
        );
        numeric_params.extend(
            resolve_params_for_instruction(instruction, item_cache)
                .into_iter()
                .filter_map(|(name, (_ty, value))| match value {
                    Some(ast::Expr::Lit(ast::Lit::Int(literal))) => {
                        Some((name, parse_literal_value(&literal) as i64))
                    }
                    _ => None,
                }),
        );
        let Some(behavior) = sem_expr_state::lower_behavior(
            &instruction.behavior,
            None,
            &numeric_params,
            &ctx.isa_params,
            &register_index_map,
        ) else {
            continue;
        };
        let source_operands = operands
            .iter()
            .filter_map(|(name, ty)| match ty {
                Type::Struct(class)
                    if ctx.is_retirement_class(class)
                        && behavior.variable_symbols.contains_key(name) =>
                {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect();
        prepared.push(PreparedInstruction {
            instruction,
            operands,
            behavior,
            source_operands,
            width,
        });
    }
    let word_width = prepared
        .iter()
        .map(|instruction| instruction.width)
        .max()
        .unwrap_or(8);
    let source_count = prepared
        .iter()
        .map(|instruction| instruction.source_operands.len())
        .max()
        .unwrap_or(0);

    let mut b = Btor2::new();
    b.comment("TMDL retirement checker");
    let x = xlen as u32;
    // Destination addresses index the target's primary architectural integer
    // register file. Other register files are outside this checker contract.
    let idx_w = ctx
        .classes
        .get(&ctx.retirement_storage)
        .map_or(5, |class| class.idx_width as u32);

    // Retirement interface inputs.
    let insn = b.input(word_width as u32, "insn");
    let pc = b.input(x, "pc");
    let source_values: Vec<Bv> = (0..source_count)
        .map(|index| b.input(x, &format!("src{index}_val")))
        .collect();
    let dst_addr_impl = b.input(idx_w, "dst_addr");
    let dst_we_impl = b.input(1, "dst_we");
    let dst_val_impl = b.input(x, "dst_val");
    let next_pc_impl = b.input(x, "next_pc");
    let valid = b.input(1, "valid");

    let mut specs: Vec<(String, Bv, PostState)> = Vec::new();
    for prepared in prepared {
        let PreparedInstruction {
            instruction: inst,
            operands: operand_list,
            behavior: behavior_graph,
            source_operands,
            width,
        } = prepared;
        let operands: HashMap<String, Type> = operand_list.iter().cloned().collect();
        let (guards, pieces) = decode_layout(inst, item_cache, &operands);
        let source_positions: HashMap<String, usize> = source_operands
            .into_iter()
            .enumerate()
            .map(|(index, name)| (name, index))
            .collect();

        // Decode operand addresses and immediates. Register values used by the
        // behavior come from ordered source slots in operand declaration order.
        let mut operand_vals = HashMap::new();
        let mut operand_addrs = HashMap::new();
        let mut spare_guards: Vec<Bv> = Vec::new();
        for (name, ty) in &operand_list {
            let lname = name.to_lowercase();
            match ty {
                Type::Struct(rc) if ctx.pc_classes.contains(&rc.to_lowercase()) => {}
                Type::Struct(rc) => {
                    let (addr, guard) = decode_operand(
                        &mut b,
                        insn,
                        pieces.get(name).cloned().unwrap_or_default(),
                        ctx.idx_width(rc),
                    );
                    spare_guards.extend(guard);
                    operand_addrs.insert(lname.clone(), (addr, rc.clone()));
                    if let Some(index) = source_positions.get(name) {
                        operand_vals.insert(
                            lname,
                            b.fit(source_values[*index], ctx.val_width(rc) as u32),
                        );
                    }
                }
                Type::Bits(n) => {
                    let (v, guard) = decode_operand(
                        &mut b,
                        insn,
                        pieces.get(name).cloned().unwrap_or_default(),
                        *n,
                    );
                    spare_guards.extend(guard);
                    operand_vals.insert(lname, v);
                }
                _ => {}
            }
        }

        let mut guard = build_guard(&mut b, insn, &guards);
        for sg in spare_guards {
            guard = b.binary("and", guard, sg, false);
        }
        if width < word_width {
            let high = b.slice(insn, word_width as u32 - 1, width as u32);
            let zero = b.constant(high.width, 0);
            let high_clear = b.compare("eq", high, zero);
            guard = b.binary("and", guard, high_clear, false);
        }

        let step = b.constant(x, u64::from(width.div_ceil(8)));
        let fallthrough = b.binary("add", pc, step, false);

        let init = PostState {
            dst_we: b.constant(1, 0),
            dst_val: b.constant(x, 0),
            dst_addr: b.constant(idx_w, 0),
            next_pc: fallthrough,
        };

        let checker = Checker {
            ctx: &ctx,
            operands,
            operand_vals,
            operand_addrs,
            behavior: &behavior_graph,
            pc,
            b: RefCell::new(&mut b),
            failed: Cell::new(false),
        };
        let post = sem_expr_state::fold_behavior(&behavior_graph, &init, &checker);
        if checker.failed.get() {
            continue;
        }
        drop(checker);
        // Normalize destination views to the retirement ABI widths. Narrow
        // register views report the value written through that view, zero
        // extended to XLEN; they do not report preserved backing-file bits.
        let post = PostState {
            dst_addr: b.fit(post.dst_addr, idx_w),
            dst_val: if post.dst_val.width > x {
                b.slice(post.dst_val, x - 1, 0)
            } else {
                b.widen(post.dst_val, x, false)
            },
            ..post
        };
        specs.push((inst.name.clone(), guard, post));
    }

    for (name, _, _) in &specs {
        b.comment(&format!("modeled {name}"));
    }

    // Fold per-instruction specs into one selected post-state. The unmatched
    // value is unobservable because every property is gated by `legal`.
    let no_we = b.constant(1, 0);
    let zero_val = b.constant(x, 0);
    let zero_addr = b.constant(idx_w, 0);
    let mut legal = b.constant(1, 0);
    let mut spec = PostState {
        dst_we: no_we,
        dst_val: zero_val,
        dst_addr: zero_addr,
        next_pc: pc,
    };
    for (_, guard, post) in specs.iter().rev() {
        spec = PostState {
            dst_we: b.ite(*guard, post.dst_we, spec.dst_we, false),
            dst_val: b.ite(*guard, post.dst_val, spec.dst_val, false),
            dst_addr: b.ite(*guard, post.dst_addr, spec.dst_addr, false),
            next_pc: b.ite(*guard, post.next_pc, spec.next_pc, false),
        };
        legal = b.binary("or", legal, *guard, false);
    }

    // Mismatch, split per field so a model checker reports which one diverged.
    // `dst_we` reports an architectural write, so writes discarded by a
    // hardwired-zero destination must be reported as disabled by the DUT.
    let we_bad = b.compare("neq", dst_we_impl, spec.dst_we);
    let val_ne = b.compare("neq", dst_val_impl, spec.dst_val);
    let val_bad = b.binary("and", spec.dst_we, val_ne, false);
    let addr_ne = b.compare("neq", dst_addr_impl, spec.dst_addr);
    let addr_bad = b.binary("and", spec.dst_we, addr_ne, false);
    let pc_bad = b.compare("neq", next_pc_impl, spec.next_pc);

    // Observable spec/impl values for counterexample triage (ignored by BMC).
    b.output(legal, "decode_legal");
    b.output(dst_we_impl, "impl_dst_we");
    b.output(spec.dst_we, "spec_dst_we");
    b.output(spec.dst_val, "spec_dst_val");
    b.output(spec.dst_addr, "spec_dst_addr");
    b.output(spec.next_pc, "spec_next_pc");

    let gated = b.binary("and", valid, legal, false);
    for (cond, name) in [
        (we_bad, "dst_we_mismatch"),
        (val_bad, "dst_val_mismatch"),
        (addr_bad, "dst_addr_mismatch"),
        (pc_bad, "next_pc_mismatch"),
    ] {
        let g = b.binary("and", gated, cond, false);
        b.bad(g, name);
    }

    output.write_all(b.as_str().as_bytes())?;
    Ok(())
}
