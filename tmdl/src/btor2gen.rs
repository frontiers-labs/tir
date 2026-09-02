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
    EncodingShape, behavior_uses_todo, get_encoding_shapes, isa_param_values, item_supports_isa,
    parse_literal_value, resolve_params_for_instruction,
};
use tir_graph::NodeId;
use tir_symbolic::lang::SymKind as ExprKind;

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

/// BTOR2's spelling of the shared term vocabulary.
struct Btor2Term<'a, 'b> {
    b: &'a mut Btor2,
    r: &'a Resolver<'b>,
}

impl crate::semgen::TermBackend for Btor2Term<'_, '_> {
    type Val = Bv;

    fn width(&self, value: &Bv) -> u32 {
        value.width
    }

    fn signed(&self, value: &Bv) -> bool {
        value.signed
    }

    fn supports(&self, kind: ExprKind) -> bool {
        use ExprKind::*;
        matches!(
            kind,
            Symbol
                | Constant
                | Add
                | Sub
                | Mul
                | Div
                | UDiv
                | Or
                | And
                | Xor
                | Eq
                | Ne
                | Lt
                | Gt
                | Ge
                | ULt
                | ULe
                | UGt
                | UGe
                | ShiftLeft
                | ShiftRightLogic
                | ShiftRightArithmetic
                | Bitcast
                | Not
                | If
                | ZExt
                | SExt
                | Extract
                | Log2Ceil
                | Clamp
        )
    }

    fn constant(&mut self, width: u32, value: u64, signed: bool) -> Bv {
        Bv {
            signed,
            ..self.b.constant(width, value)
        }
    }

    fn symbol(&mut self, id: u32) -> Option<Bv> {
        self.r.resolve(id)
    }

    fn binary(&mut self, kind: ExprKind, lhs: Bv, rhs: Bv, signed: bool) -> Bv {
        let operator = match kind {
            ExprKind::Add => "add",
            ExprKind::Sub => "sub",
            ExprKind::Mul => "mul",
            ExprKind::Div => "sdiv",
            ExprKind::UDiv => "udiv",
            ExprKind::Or => "or",
            ExprKind::And => "and",
            ExprKind::Xor => "xor",
            _ => unreachable!("binary operator the backend declared unsupported"),
        };
        self.b.binary(operator, lhs, rhs, signed)
    }

    fn compare(&mut self, kind: ExprKind, lhs: Bv, rhs: Bv) -> Bv {
        let operator = match kind {
            ExprKind::Eq => "eq",
            ExprKind::Ne => "neq",
            ExprKind::Lt => "slt",
            ExprKind::Gt => "sgt",
            ExprKind::Ge => "sgte",
            ExprKind::ULt => "ult",
            ExprKind::ULe => "ulte",
            ExprKind::UGt => "ugt",
            ExprKind::UGe => "ugte",
            _ => unreachable!("comparison the backend declared unsupported"),
        };
        self.b.compare(operator, lhs, rhs)
    }

    fn shift(&mut self, kind: ExprKind, lhs: Bv, amount: Bv, signed: bool) -> Bv {
        let operator = match kind {
            ExprKind::ShiftLeft => "sll",
            ExprKind::ShiftRightLogic => "srl",
            _ => "sra",
        };
        self.b.binary(operator, lhs, amount, signed)
    }

    fn unary(&mut self, _kind: ExprKind, value: Bv) -> Bv {
        self.b.not(value)
    }

    fn concat(&mut self, high: Bv, low: Bv) -> Bv {
        self.b.concat(high, low)
    }

    fn widen(&mut self, value: Bv, target: u32, signed: bool) -> Bv {
        self.b.widen(value, target, signed)
    }

    fn slice(&mut self, value: Bv, high: u32, low: u32) -> Bv {
        self.b.slice(value, high, low)
    }

    fn ite(&mut self, condition: Bv, then: Bv, otherwise: Bv, signed: bool) -> Bv {
        self.b.ite(condition, then, otherwise, signed)
    }

    fn as_bool(&mut self, value: Bv) -> Bv {
        self.b.as_bool(value)
    }

    fn special<G: crate::semgen::ValueDag>(
        &mut self,
        graph: &G,
        node: NodeId,
        emit: &mut dyn FnMut(&mut Self, NodeId) -> Option<Bv>,
    ) -> Option<Bv> {
        if *graph.get_node(node) != ExprKind::Clamp {
            return None;
        }
        let mut child = |b: &mut Self, index: usize| emit(b, graph.children(node).nth(index)?);
        let input = child(self, 0)?;
        let (lt, gt) = if input.signed {
            ("slt", "sgt")
        } else {
            ("ult", "ugt")
        };
        let min = child(self, 1)?;
        let max = child(self, 2)?;
        let w = input.width.max(min.width).max(max.width);
        let input = self.b.widen(input, w, input.signed);
        let min = self.b.widen(min, w, false);
        let max = self.b.widen(max, w, false);
        let below = self.b.compare(lt, input, min);
        let above = self.b.compare(gt, input, max);
        let hi = self.b.ite(above, max, input, input.signed);
        Some(self.b.ite(below, min, hi, input.signed))
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
        crate::semgen::emit(
            &graph,
            root,
            &mut Btor2Term {
                b: &mut b,
                r: &resolver,
            },
        )
        .or_else(|| {
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
        // Writes to non-retirement fixed registers (status flags) are outside
        // the checked post-state: the property observes only the destination
        // operand and next_pc, so they pass through instead of failing the
        // instruction. Retirement-file fixed writes (e.g. rdx:rax) still fail:
        // the single-write post-state cannot express them.
        if let sem_expr_state::Destination::FixedRegister { class, .. } = destination
            && !self.ctx.pc_classes.contains(&class.to_lowercase())
            && !self.ctx.is_retirement_class(class)
        {
            return Some(*state);
        }

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

    fn bind(&self, value: NodeId, state: &PostState) -> Option<PostState> {
        // A binding writes no checked register, but the bound term is emitted
        // here so a term this backend cannot model fails the instruction
        // instead of silently disappearing from the check.
        self.emit_val(value)?;
        Some(*state)
    }

    fn value_effect(
        &self,
        _kind: tir_symbolic::lang::SymKind,
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

struct PreparedInstruction<'a> {
    instruction: &'a ast::Instruction,
    operands: Vec<(String, Type)>,
    behavior: sem_expr_state::BehaviorGraph,
    source_operands: Vec<String>,
    shapes: Vec<EncodingShape>,
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
        if rc.is_program_counter() {
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
                idx_width: crate::semgen::eval_class_param(rc, "ENCODING_LEN", &isa_params)
                    .unwrap_or(5) as u16,
                val_width: crate::semgen::eval_class_param(rc, "WIDTH", &isa_params)
                    .unwrap_or(xlen as i64) as u16,
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
        // An empty or `todo()` behavior declares no semantics to check: there is
        // nothing to lower, and the checker's property only covers modeled
        // instructions.
        if matches!(&instruction.behavior, ast::Expr::Block(block) if block.stmts.is_empty())
            || behavior_uses_todo(&instruction.behavior)
        {
            continue;
        }
        let shapes = get_encoding_shapes(instruction, item_cache);
        if shapes.is_empty() {
            continue;
        }
        let operands = crate::semgen::resolved_operands(&ctx.isa_params, instruction, item_cache);
        let numeric_params =
            crate::semgen::numeric_params(&ctx.isa_params, instruction, item_cache);
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
            shapes,
        });
    }
    let word_width = prepared
        .iter()
        .flat_map(|instruction| instruction.shapes.iter().map(|shape| shape.width_bits))
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
    for prepared in &prepared {
        let PreparedInstruction {
            instruction: inst,
            operands: operand_list,
            behavior: behavior_graph,
            source_operands,
            shapes,
        } = prepared;
        let operands: HashMap<String, Type> = operand_list.iter().cloned().collect();
        let source_positions: HashMap<String, usize> = source_operands
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, name)| (name, index))
            .collect();

        // A shape is a fixed bit map, so each is its own decode arm over the
        // same fully symbolic word: same behavior, its own fixed bits, operand
        // pieces and width.
        for (shape_index, shape) in shapes.iter().enumerate() {
            let width = shape.width_bits;
            let (guards, pieces) = decode_layout(shape, inst, item_cache, &operands);

            // Decode operand addresses and immediates. Register values used by the
            // behavior come from ordered source slots in operand declaration order.
            let mut operand_vals = HashMap::new();
            let mut operand_addrs = HashMap::new();
            let mut spare_guards: Vec<Bv> = Vec::new();
            for (name, ty) in operand_list {
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
                operands: operands.clone(),
                operand_vals,
                operand_addrs,
                behavior: behavior_graph,
                pc,
                b: RefCell::new(&mut b),
                failed: Cell::new(false),
            };
            let post = sem_expr_state::fold_behavior(behavior_graph, &init, &checker);
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
            let name = match shapes.len() {
                1 => inst.name.clone(),
                _ => format!("{}#{shape_index}", inst.name),
            };
            specs.push((name, guard, post));
        }
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
