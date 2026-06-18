//! BTOR2 emission of a per-instruction reference *checker* for hardware model
//! checking (the riscv-formal / RVFI shape).
//!
//! Why a checker and not a full transition system: a pipelined implementation
//! and a single-step ISA model can only be compared by decoupling timing from
//! semantics. The implementation exposes a retirement interface — for each
//! committed instruction it reports `pc`, `insn`, the source register *values*
//! it read (`rs1_val`, `rs2_val`), and the destination it wrote (`rd_addr`,
//! `rd_we`, `rd_val`, `next_pc`). This model is the golden side: from `insn`,
//! `pc` and the reported source values it decodes the instruction and computes
//! the architectural post-state, then asserts the implementation's report
//! matches. The result is a purely combinational relation over the retirement
//! signals; composed with the implementation's own BTOR2 it becomes a miter a
//! BMC engine (btormc/Bitwuzla) drives to a counterexample.
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
use crate::sem_expr_state::{self, StateEmitter};
use crate::utils::{
    get_encoding_arms, isa_param_values, item_supports_isa, parse_literal_value,
    resolve_isa_param_values, resolve_operand_widths, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};
use tir::graph::{Dag, NodeId};
use tir::sem_expr::{ExprKind, ExprPayload, ExprPostGraph};

// ---------------------------------------------------------------------------
// Target context (register-file layout resolved against the ISA)
// ---------------------------------------------------------------------------

struct ClassInfo {
    idx_width: u16,
    val_width: u16,
    zero_index: Option<u16>,
}

struct Ctx<'a> {
    isa: &'a str,
    xlen: u16,
    classes: BTreeMap<String, ClassInfo>,
    pc_classes: std::collections::HashSet<String>,
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

// ---------------------------------------------------------------------------
// BTOR2 node builder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Bv {
    nid: u32,
    width: u32,
    signed: bool,
}

struct Btor2 {
    out: String,
    next: u32,
    sorts: BTreeMap<u32, u32>,
}

impl Btor2 {
    fn new() -> Self {
        Btor2 {
            out: String::new(),
            next: 0,
            sorts: BTreeMap::new(),
        }
    }

    fn line(&mut self, body: &str) -> u32 {
        self.next += 1;
        let nid = self.next;
        self.out.push_str(&format!("{} {}\n", nid, body));
        nid
    }

    fn sort(&mut self, width: u32) -> u32 {
        if let Some(s) = self.sorts.get(&width) {
            return *s;
        }
        let s = self.line(&format!("sort bitvec {}", width));
        self.sorts.insert(width, s);
        s
    }

    fn input(&mut self, width: u32, name: &str) -> Bv {
        let s = self.sort(width);
        let nid = self.line(&format!("input {} {}", s, name));
        Bv {
            nid,
            width,
            signed: false,
        }
    }

    fn konst(&mut self, width: u32, value: u64) -> Bv {
        let s = self.sort(width);
        let nid = self.line(&format!("constd {} {}", s, value));
        Bv {
            nid,
            width,
            signed: false,
        }
    }

    /// Width-preserving binary op (`add`, `and`, `sll`, ...).
    fn bin(&mut self, op: &str, a: Bv, b: Bv, signed: bool) -> Bv {
        debug_assert_eq!(a.width, b.width);
        let s = self.sort(a.width);
        let nid = self.line(&format!("{} {} {} {}", op, s, a.nid, b.nid));
        Bv {
            nid,
            width: a.width,
            signed,
        }
    }

    /// Comparison producing a 1-bit result.
    fn cmp(&mut self, op: &str, a: Bv, b: Bv) -> Bv {
        debug_assert_eq!(a.width, b.width);
        let s = self.sort(1);
        let nid = self.line(&format!("{} {} {} {}", op, s, a.nid, b.nid));
        Bv {
            nid,
            width: 1,
            signed: false,
        }
    }

    fn not(&mut self, a: Bv) -> Bv {
        let s = self.sort(a.width);
        let nid = self.line(&format!("not {} {}", s, a.nid));
        Bv {
            nid,
            width: a.width,
            signed: a.signed,
        }
    }

    fn ite(&mut self, cond: Bv, t: Bv, e: Bv, signed: bool) -> Bv {
        debug_assert_eq!(t.width, e.width);
        let s = self.sort(t.width);
        let nid = self.line(&format!("ite {} {} {} {}", s, cond.nid, t.nid, e.nid));
        Bv {
            nid,
            width: t.width,
            signed,
        }
    }

    fn ext(&mut self, op: &str, a: Bv, by: u32, signed: bool) -> Bv {
        if by == 0 {
            return Bv { signed, ..a };
        }
        let s = self.sort(a.width + by);
        let nid = self.line(&format!("{} {} {} {}", op, s, a.nid, by));
        Bv {
            nid,
            width: a.width + by,
            signed,
        }
    }

    fn slice(&mut self, a: Bv, high: u32, low: u32) -> Bv {
        let width = high - low + 1;
        if width == a.width {
            return a;
        }
        let s = self.sort(width);
        let nid = self.line(&format!("slice {} {} {} {}", s, a.nid, high, low));
        Bv {
            nid,
            width,
            signed: false,
        }
    }

    fn concat(&mut self, a: Bv, b: Bv) -> Bv {
        let width = a.width + b.width;
        let s = self.sort(width);
        let nid = self.line(&format!("concat {} {} {}", s, a.nid, b.nid));
        Bv {
            nid,
            width,
            signed: false,
        }
    }

    fn widen(&mut self, a: Bv, target: u32, signed: bool) -> Bv {
        if a.width >= target {
            return a;
        }
        let op = if signed { "sext" } else { "uext" };
        self.ext(op, a, target - a.width, signed)
    }

    fn fit(&mut self, a: Bv, target: u32) -> Bv {
        if a.width > target {
            self.slice(a, target - 1, 0)
        } else {
            self.widen(a, target, a.signed)
        }
    }

    fn coerce(&mut self, a: Bv, b: Bv) -> (Bv, Bv) {
        let w = a.width.max(b.width);
        (self.widen(a, w, a.signed), self.widen(b, w, b.signed))
    }

    /// Reduce any value to a 1-bit truth: nonzero -> 1.
    fn as_bool(&mut self, a: Bv) -> Bv {
        if a.width == 1 {
            return a;
        }
        let zero = self.konst(a.width, 0);
        self.cmp("neq", a, zero)
    }
}

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
            // A fixed non-PC register read is not part of the RVFI retirement
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
            Some(b.bin($op, x, y, signed))
        }};
    }
    macro_rules! cmp {
        ($op:expr) => {{
            let (x, y) = (ch!(0), ch!(1));
            let (x, y) = b.coerce(x, y);
            Some(b.cmp($op, x, y))
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
            Some(b.bin($op, lhs, amt, sgn(lhs.signed)))
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
                    ..b.konst(w, i.to_u64() & mask)
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
                let prod = b.bin("mul", m0, m1, true);
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
            Some(b.konst(w, v))
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
            let below = b.cmp(lt, input, min);
            let above = b.cmp(gt, input, max);
            let hi = b.ite(above, max, input, input.signed);
            Some(b.ite(below, min, hi, input.signed))
        }
        ExprKind::LoadMemory
        | ExprKind::StoreMemory
        | ExprKind::Sqrt
        | ExprKind::Fma
        | ExprKind::Loop
        | ExprKind::IndVar
        | ExprKind::Acc => None,
    }
}

// ---------------------------------------------------------------------------
// Per-instruction checker: decode + execute over retirement signals
// ---------------------------------------------------------------------------

/// Architectural post-state the checker computes for one decoded instruction.
#[derive(Clone, Copy)]
struct PostState {
    rd_we: Bv,
    rd_val: Bv,
    rd_addr: Bv,
    next_pc: Bv,
}

struct Checker<'a> {
    ctx: &'a Ctx<'a>,
    operands: HashMap<String, Type>,
    operand_vals: HashMap<String, Bv>,
    /// Decoded destination index, for the `rd_addr` cross-check and the
    /// hardwired-zero mask. Present only for register write targets.
    operand_addrs: HashMap<String, (Bv, String)>,
    numeric_params: HashMap<String, i64>,
    register_index_map: &'a HashMap<(String, String), u32>,
    pc: Bv,
    b: RefCell<&'a mut Btor2>,
    states: RefCell<HashMap<String, PostState>>,
    counter: Cell<usize>,
    failed: Cell<bool>,
}

impl Checker<'_> {
    fn fresh(&self, st: PostState) -> String {
        let n = self.counter.get();
        self.counter.set(n + 1);
        let name = format!("s{}", n);
        self.states.borrow_mut().insert(name.clone(), st);
        name
    }

    fn get(&self, name: &str) -> PostState {
        self.states.borrow()[name]
    }

    fn emit_val(&self, e: &ast::Expr) -> Option<Bv> {
        let mut graph = ExprPostGraph::new();
        let lowering = e
            .lower_to_sema_with_registers(&mut graph, &self.numeric_params, self.register_index_map)
            .or_else(|| {
                self.failed.set(true);
                None
            })?;
        let mut symbols = HashMap::new();
        for (name, id) in &lowering.variable_symbols {
            symbols.insert(*id, SymbolInfo::Variable { name: name.clone() });
        }
        for ((class, _number), id) in &lowering.register_symbols {
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
        let mut b = self.b.borrow_mut();
        emit(&graph, lowering.root, &resolver, &mut b).or_else(|| {
            self.failed.set(true);
            None
        })
    }
}

impl StateEmitter for Checker<'_> {
    fn cond(&self, e: &ast::Expr) -> String {
        match self.emit_val(e) {
            Some(v) => {
                let mut b = self.b.borrow_mut();
                b.as_bool(v).nid.to_string()
            }
            None => "0".to_string(),
        }
    }

    fn assign(&self, a: &ast::Assign, st_name: &str) -> Option<String> {
        let value = self.emit_val(&a.value)?;
        let st = self.get(st_name);
        let dest = match &*a.dest {
            ast::Expr::Ident(id) => Some(id.name.as_str()),
            ast::Expr::Path(p) if p.remainder.len() == 1 => Some(p.remainder[0].as_str()),
            _ => None,
        }?;

        let xlen = self.ctx.xlen as u32;
        let mut b = self.b.borrow_mut();

        if dest == "pc" {
            let next_pc = b.fit(value, xlen);
            return Some(self.fresh(PostState { next_pc, ..st }));
        }
        match self.operands.get(dest) {
            Some(Type::Struct(rc)) if self.ctx.pc_classes.contains(&rc.to_lowercase()) => {
                let next_pc = b.fit(value, xlen);
                Some(self.fresh(PostState { next_pc, ..st }))
            }
            Some(Type::Struct(rc)) => {
                let rd_val = b.fit(value, self.ctx.val_width(rc) as u32);
                let (rd_addr, class) = self.operand_addrs.get(dest)?.clone();
                // A write to the hardwired-zero register is dropped.
                let rd_we = match self.ctx.zero_index(&class) {
                    Some(z) => {
                        let zc = b.konst(rd_addr.width, z as u64);
                        b.cmp("neq", rd_addr, zc)
                    }
                    None => b.konst(1, 1),
                };
                Some(self.fresh(PostState {
                    rd_we,
                    rd_val,
                    rd_addr,
                    ..st
                }))
            }
            _ => None,
        }
    }

    fn store(&self, _c: &ast::Call, _st: &str) -> Option<String> {
        None
    }

    fn trap(
        &self,
        _c: &ast::Call,
        _st: &str,
        _compile: &dyn Fn(&ast::Expr, &str) -> String,
    ) -> Option<String> {
        None
    }

    fn ite(&self, cond: &str, then_state: &str, else_state: &str) -> String {
        let cond_nid: u32 = cond.parse().unwrap_or(0);
        if cond_nid == 0 {
            return else_state.to_string();
        }
        let (t, e) = (self.get(then_state), self.get(else_state));
        let mut b = self.b.borrow_mut();
        let cond = Bv {
            nid: cond_nid,
            width: 1,
            signed: false,
        };
        let merged = PostState {
            rd_we: b.ite(cond, t.rd_we, e.rd_we, false),
            rd_val: b.ite(cond, t.rd_val, e.rd_val, false),
            rd_addr: b.ite(cond, t.rd_addr, e.rd_addr, false),
            next_pc: b.ite(cond, t.next_pc, e.next_pc, false),
        };
        drop(b);
        self.fresh(merged)
    }

    fn try_except(
        &self,
        _t: &ast::TryExcept,
        _st: &str,
        _body: &str,
        _compile: &dyn Fn(&ast::Expr, &str) -> String,
    ) -> Option<String> {
        None
    }

    fn unsupported(&self, _e: &ast::Expr) {
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
        return (b.konst(target_width as u32, 0), None);
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
            let gap = b.konst((expected_hi - op_hi) as u32, 0);
            push(b, &mut acc, gap);
        }
        let frag = b.slice(insn, *word_hi as u32, *word_lo as u32);
        push(b, &mut acc, frag);
        expected_hi = op_lo.saturating_sub(1);
    }
    let lowest = pieces.last().map(|p| p.0).unwrap_or(0);
    if lowest > 0 {
        let pad = b.konst(lowest as u32, 0);
        push(b, &mut acc, pad);
    }
    let raw = acc.unwrap();
    let target = target_width as u32;
    let guard = if raw.width > target {
        let spare = b.slice(raw, raw.width - 1, target);
        let zero = b.konst(spare.width, 0);
        Some(b.cmp("eq", spare, zero))
    } else {
        None
    };
    (b.fit(raw, target), guard)
}

fn build_guard(b: &mut Btor2, insn: Bv, guards: &[(u16, u16, u128)]) -> Bv {
    let mut acc: Option<Bv> = None;
    for (hi, lo, val) in guards {
        let field = b.slice(insn, *hi as u32, *lo as u32);
        let k = b.konst(field.width, *val as u64);
        let eq = b.cmp("eq", field, k);
        acc = Some(match acc {
            Some(a) => b.bin("and", a, eq, false),
            None => eq,
        });
    }
    acc.unwrap_or_else(|| b.konst(1, 1))
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

pub fn generate_btor2<'a>(
    isa: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    let isa_params = isa_param_values(isa, item_cache);
    let xlen = isa_params.get("XLEN").copied().unwrap_or(64) as u16;

    let mut classes = BTreeMap::new();
    let mut pc_classes = std::collections::HashSet::new();
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        if !item_supports_isa(&rc.for_isas, isa, item_cache) {
            continue;
        }
        let name = rc.name.to_lowercase();
        if is_pc_class(rc) {
            pc_classes.insert(name);
            continue;
        }
        classes.insert(
            name,
            ClassInfo {
                idx_width: eval_class_param(rc, "ENCODING_LEN", &isa_params).unwrap_or(5) as u16,
                val_width: eval_class_param(rc, "WIDTH", &isa_params).unwrap_or(xlen as i64) as u16,
                zero_index: rc.hardwired_zero_register_index(),
            },
        );
    }
    let ctx = Ctx {
        isa,
        xlen,
        classes,
        pc_classes,
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

    let mut b = Btor2::new();
    b.out.push_str("; TMDL RVFI-style instruction checker\n");
    let x = xlen as u32;
    // The retirement `rd_addr` indexes the integer register file (the class
    // with a hardwired-zero slot, RISC-V `x0`); CSR/PC classes are out of scope.
    let idx_w = ctx
        .classes
        .values()
        .find(|c| c.zero_index.is_some())
        .or_else(|| ctx.classes.values().next())
        .map_or(5, |c| c.idx_width as u32);

    // Retirement interface inputs.
    let insn = b.input(32, "insn");
    let pc = b.input(x, "pc");
    let rs1_val = b.input(x, "rs1_val");
    let rs2_val = b.input(x, "rs2_val");
    let rd_addr_impl = b.input(idx_w, "rd_addr");
    let rd_we_impl = b.input(1, "rd_we");
    let rd_val_impl = b.input(x, "rd_val");
    let next_pc_impl = b.input(x, "next_pc");
    let valid = b.input(1, "valid");

    let four = b.konst(x, 4);
    let pc_plus4 = b.bin("add", pc, four, false);

    let mut specs: Vec<(Bv, PostState)> = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        if !item_supports_isa(&inst.for_isas, ctx.isa, item_cache) {
            continue;
        }
        let behavior = &inst.behavior;
        let operand_list = resolved_operands(&ctx, inst, item_cache);
        let operands: HashMap<String, Type> = operand_list.iter().cloned().collect();
        let (guards, pieces) = decode_layout(inst, item_cache, &operands);

        // Decode the values the behavior consumes: source registers come from
        // retirement inputs, immediates from the word; destination indices are
        // decoded for the `rd_addr` check.
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
                    operand_vals.insert(
                        lname,
                        match name.as_str() {
                            "rs1" => rs1_val,
                            "rs2" => rs2_val,
                            _ => continue,
                        },
                    );
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
            guard = b.bin("and", guard, sg, false);
        }

        let init = PostState {
            rd_we: b.konst(1, 0),
            rd_val: b.konst(x, 0),
            rd_addr: b.konst(idx_w, 0),
            next_pc: pc_plus4,
        };

        let checker = Checker {
            ctx: &ctx,
            operands,
            operand_vals,
            operand_addrs,
            numeric_params: ctx.isa_params.clone(),
            register_index_map: &register_index_map,
            pc,
            b: RefCell::new(&mut b),
            states: RefCell::new(HashMap::new()),
            counter: Cell::new(0),
            failed: Cell::new(false),
        };
        let init_handle = checker.fresh(init);
        let final_handle = sem_expr_state::compile_to_state(behavior, &init_handle, &checker);
        if checker.failed.get() {
            continue;
        }
        let post = checker.get(&final_handle);
        drop(checker);
        // rd_addr is decoded at idx_width; the retirement input may be wider.
        let post = PostState {
            rd_addr: b.fit(post.rd_addr, idx_w),
            ..post
        };
        specs.push((guard, post));
    }

    // Fold per-instruction specs into one selected post-state; unmatched words
    // default to a fall-through with no register write.
    let no_we = b.konst(1, 0);
    let zero_val = b.konst(x, 0);
    let zero_addr = b.konst(idx_w, 0);
    let mut legal = b.konst(1, 0);
    let mut spec = PostState {
        rd_we: no_we,
        rd_val: zero_val,
        rd_addr: zero_addr,
        next_pc: pc_plus4,
    };
    for (guard, post) in specs.iter().rev() {
        spec = PostState {
            rd_we: b.ite(*guard, post.rd_we, spec.rd_we, false),
            rd_val: b.ite(*guard, post.rd_val, spec.rd_val, false),
            rd_addr: b.ite(*guard, post.rd_addr, spec.rd_addr, false),
            next_pc: b.ite(*guard, post.next_pc, spec.next_pc, false),
        };
        legal = b.bin("or", legal, *guard, false);
    }

    // Mismatch, split per field so a model checker reports which one diverged.
    // A write to the hardwired-zero register is architecturally a no-op, so mask
    // the implementation's write-enable with `rd_addr != 0` before comparing.
    let zero_addr2 = b.konst(idx_w, 0);
    let rd_nonzero = b.cmp("neq", rd_addr_impl, zero_addr2);
    let impl_we_eff = b.bin("and", rd_we_impl, rd_nonzero, false);
    let we_bad = b.cmp("neq", impl_we_eff, spec.rd_we);
    let val_ne = b.cmp("neq", rd_val_impl, spec.rd_val);
    let val_bad = b.bin("and", spec.rd_we, val_ne, false);
    let addr_ne = b.cmp("neq", rd_addr_impl, spec.rd_addr);
    let addr_bad = b.bin("and", spec.rd_we, addr_ne, false);
    let pc_bad = b.cmp("neq", next_pc_impl, spec.next_pc);

    // Observable spec/impl values for counterexample triage (ignored by BMC).
    b.line(&format!("output {} decode_legal", legal.nid));
    b.line(&format!("output {} impl_rd_we_eff", impl_we_eff.nid));
    b.line(&format!("output {} spec_rd_we", spec.rd_we.nid));
    b.line(&format!("output {} spec_rd_val", spec.rd_val.nid));
    b.line(&format!("output {} spec_rd_addr", spec.rd_addr.nid));
    b.line(&format!("output {} spec_next_pc", spec.next_pc.nid));

    let gated = b.bin("and", valid, legal, false);
    for (cond, name) in [
        (we_bad, "rd_we_mismatch"),
        (val_bad, "rd_val_mismatch"),
        (addr_bad, "rd_addr_mismatch"),
        (pc_bad, "next_pc_mismatch"),
    ] {
        let g = b.bin("and", gated, cond, false);
        b.line(&format!("bad {} {}", g.nid, name));
    }

    output.write_all(b.out.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit(source: &str, isa: &str) -> String {
        let (tokens, errs) = crate::lex(source);
        assert!(errs.is_empty(), "lex errors");
        let (file, errs) = crate::parse(source, &tokens, "test");
        assert!(errs.is_empty(), "parse errors");
        let mut files = vec![file.unwrap()];
        crate::ast::resolve_register_class_inheritance(&mut files);
        assert!(crate::sema_analyze(&files).is_empty(), "sema errors");
        assert!(crate::type_check(&files).1.is_empty(), "typeck errors");
        let item_cache: HashMap<&str, &ast::Item> = files
            .iter()
            .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
            .collect();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("tmdl_btor2_{nanos}.btor2"));
        generate_btor2(
            isa,
            &files,
            &item_cache,
            Box::new(std::fs::File::create(&path).unwrap()),
        )
        .unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        out
    }

    /// Every operand reference must point at an already-defined node, ids must
    /// be sequential, and the model must end in exactly one `bad` property —
    /// the invariants a BTOR2 consumer relies on.
    fn assert_valid(model: &str) {
        let mut defined = std::collections::HashSet::new();
        let mut sorts = std::collections::HashSet::new();
        let mut expect = 1u32;
        let mut bads = 0;
        for line in model.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            let p: Vec<&str> = line.split_whitespace().collect();
            let nid: u32 = p[0].parse().unwrap();
            assert_eq!(nid, expect, "non-sequential id at: {line}");
            expect += 1;
            let op = p[1];
            // Node-id operand positions per opcode; sorts and immediates excluded.
            let refs: &[usize] = match op {
                "sort" => {
                    sorts.insert(nid);
                    &[]
                }
                "input" | "constd" | "const" | "one" | "zero" | "ones" => &[],
                "output" => &[2],
                "bad" => {
                    bads += 1;
                    &[2]
                }
                "slice" | "sext" | "uext" | "not" => &[3],
                "ite" => &[3, 4, 5],
                _ => &[3, 4],
            };
            if !matches!(op, "sort" | "bad" | "output") {
                assert!(
                    sorts.contains(&p[2].parse().unwrap()),
                    "sort ref at: {line}"
                );
            }
            for &i in refs {
                let r: u32 = p[i].parse().unwrap();
                assert!(defined.contains(&r), "undefined ref {r} at: {line}");
            }
            defined.insert(nid);
        }
        assert!(bads >= 1, "at least one bad property expected");
    }

    const SPEC: &str = include_str!("../checks/Inputs/smtlib.tmdl");

    #[test]
    fn checker_is_structurally_valid() {
        assert_valid(&emit(SPEC, "TestIsa"));
    }

    #[test]
    fn exposes_retirement_interface_and_property() {
        let m = emit(SPEC, "TestIsa");
        for name in [
            "insn", "pc", "rs1_val", "rs2_val", "rd_addr", "rd_we", "rd_val", "next_pc", "valid",
        ] {
            assert!(
                m.lines()
                    .any(|l| l.contains(" input ") && l.ends_with(&format!(" {name}"))),
                "missing retirement input `{name}`"
            );
        }
        assert!(
            m.lines().any(|l| l.contains(" bad ")),
            "missing bad property"
        );
    }
}
