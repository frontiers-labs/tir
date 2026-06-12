use std::collections::HashMap;
use std::io::Write;

use crate::Type;
use crate::ast;
use crate::error::TMDLError;
use crate::sem_expr_state;
use crate::utils::{
    get_encoding_arms, resolve_isa_param_values, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};
use tir::graph::{Dag, NodeId};

const REG_INDEX_WIDTH: u16 = 5;
const REG_VALUE_WIDTH: u16 = 64;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn generate_smtlib<'a>(
    dialect: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    writeln!(output, "{}", HEADER)?;
    build_state(files, &mut output)?;
    build_instructions(dialect, item_cache, files, &mut output)?;
    build_decoder(dialect, item_cache, files, &mut output)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// State (register file) declaration
// ---------------------------------------------------------------------------

fn is_pc_class(rc: &ast::RegisterClass) -> bool {
    rc.resolve_registers()
        .any(|r| r.traits.contains(&ast::RegisterTrait::ProgramCounter))
}

fn build_state(files: &[ast::File], output: &mut Box<dyn Write>) -> Result<(), TMDLError> {
    let all_classes = files
        .iter()
        .flat_map(|f| f.register_classes())
        .collect::<Vec<_>>();

    let array_class_names = all_classes
        .iter()
        .filter(|rc| !is_pc_class(rc))
        .map(|rc| rc.name.to_lowercase())
        .collect::<Vec<_>>();

    let mut fields = array_class_names
        .iter()
        .map(|name| {
            format!(
                "({} (Array (_ BitVec {}) (_ BitVec {})))",
                name, REG_INDEX_WIDTH, REG_VALUE_WIDTH
            )
        })
        .collect::<Vec<_>>();
    fields.push(format!("(pc (_ BitVec {}))", REG_VALUE_WIDTH));

    writeln!(
        output,
        "(declare-datatypes () ((TMDLState (mk-TMDLState {}))))",
        fields.join(" ")
    )?;

    for name in &array_class_names {
        writeln!(
            output,
            "\n(define-fun read_{name} ((st TMDLState) (r (_ BitVec {idx_width}))) (_ BitVec {val_width})\n  (ite (= r (_ bv0 {idx_width}))\n    (_ bv0 {val_width})\n    (select ({name} st) r)))",
            idx_width = REG_INDEX_WIDTH,
            val_width = REG_VALUE_WIDTH
        )?;

        let mut fields = Vec::new();
        for n2 in &array_class_names {
            if n2 == name {
                fields.push(format!("(store ({} st) r val)", n2));
            } else {
                fields.push(format!("({} st)", n2));
            }
        }
        fields.push("(pc st)".to_string());
        writeln!(
            output,
            "\n(define-fun write_{name} ((st TMDLState) (r (_ BitVec {idx_width})) (val (_ BitVec {val_width}))) TMDLState\n  (ite (= r (_ bv0 {idx_width}))\n    st\n    (mk-TMDLState {fields})))",
            idx_width = REG_INDEX_WIDTH,
            val_width = REG_VALUE_WIDTH,
            fields = fields.join(" ")
        )?;
    }

    let mut fields = array_class_names
        .iter()
        .map(|name| format!("({} st)", name))
        .collect::<Vec<_>>();
    fields.push("val".to_string());
    writeln!(
        output,
        "\n(define-fun write_pc ((st TMDLState) (val (_ BitVec {val_width}))) TMDLState\n  (mk-TMDLState {fields}))",
        val_width = REG_VALUE_WIDTH,
        fields = fields.join(" ")
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Instruction encoding and execution
// ---------------------------------------------------------------------------

fn build_instructions<'a>(
    dialect: &str,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    files: &'a [ast::File],
    output: &mut Box<dyn Write>,
) -> Result<(), TMDLError> {
    let mut instruction_variants = vec![];
    let mut encode_arms = vec![];
    let mut execute_arms = vec![];

    // `(class, register-name) -> encoding index` so register paths without a
    // numeric index (e.g. `PC::pc`) lower to a stable slot.
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
    let pc_classes: std::collections::HashSet<String> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .filter(|rc| is_pc_class(rc))
        .map(|rc| rc.name.to_lowercase())
        .collect();

    for i in files.iter().flat_map(|f| f.instructions()) {
        let name = i.name.to_lowercase();
        let uppercase_name = name.to_uppercase();

        let operands = resolve_operands_for_instruction(i, item_cache);
        let smt_operands = build_smt_operands(&operands);
        let smt_operands_joined = smt_operands.join(" ");
        let operand_params = if smt_operands_joined.is_empty() {
            "()".to_string()
        } else {
            format!("({smt_operands_joined})")
        };
        let execute_params = if smt_operands_joined.is_empty() {
            "((st TMDLState))".to_string()
        } else {
            format!("((st TMDLState) {smt_operands_joined})")
        };
        let smt_encoding = build_smt_encoding(item_cache, i, &operands);
        let smt_behavior =
            build_smt_behavior(item_cache, i, &operands, &register_index_map, &pc_classes);
        // Untranslatable behaviors (e.g. memory accesses) get an identity body
        // plus a machine-readable marker so verification tooling can tell
        // "proven unchanged" apart from "not modeled".
        let (smt_behavior, marker) = match smt_behavior {
            Some(b) => (b, String::new()),
            None => (
                "st".to_string(),
                format!("\n; UNSUPPORTED-BEHAVIOR: {}", name),
            ),
        };
        write!(output, "{}", marker)?;

        let operand_names = operands
            .iter()
            .map(|(k, _v)| k.to_lowercase())
            .collect::<Vec<_>>();
        let operand_list = operand_names.join(" ");

        writeln!(
            output,
            "\n(define-fun encode_{name} {operand_params} (_ BitVec 32)\n  {smt_encoding})\n\n(define-fun execute_{name} {execute_params} TMDLState\n  {smt_behavior})"
        )?;

        // SMT-LIB requires datatype accessor names to be unique within the
        // whole datatype.  Prefix each accessor with the instruction name so
        // that `ADD_rd` and `SUB_rd` don't collide.  Match arms use positional
        // pattern binding, so they are unaffected by this renaming.
        let variant_operands = operands
            .iter()
            .map(|(op_name, ty)| format!("({}_{} {})", name, op_name.to_lowercase(), smt_ty_of(ty)))
            .collect::<Vec<_>>()
            .join(" ");

        if variant_operands.is_empty() {
            instruction_variants.push(format!("({uppercase_name})"));
        } else {
            instruction_variants.push(format!("({uppercase_name} {variant_operands})"));
        }

        // Build ite-based dispatch arms using the prefixed accessor names.
        // Z3's SMT-LIB `match` does not support pattern variable binding, so
        // we use `(_ is VARIANT)` discriminators and named accessors instead.
        let accessor_args = operand_names
            .iter()
            .map(|op| format!("({name}_{op} instr)"))
            .collect::<Vec<_>>()
            .join(" ");

        if operand_list.is_empty() {
            // Nullary functions and constructors are referenced bare in SMT-LIB.
            encode_arms.push(format!("((_ is {uppercase_name}) instr) encode_{name}"));
            execute_arms.push(format!(
                "((_ is {uppercase_name}) instr) (execute_{name} state)"
            ));
        } else {
            encode_arms.push(format!(
                "((_ is {uppercase_name}) instr) (encode_{name} {accessor_args})"
            ));
            execute_arms.push(format!(
                "((_ is {uppercase_name}) instr) (execute_{name} state {accessor_args})"
            ));
        }
    }

    writeln!(
        output,
        "\n(declare-datatypes () ((TMDLInstr {})))",
        instruction_variants.join(" ")
    )?;

    // Fold arms into nested ites; the last instruction is the fallback.
    // encode_* and execute_* already exist at this point so the ite can call them.
    let encode_body = encode_arms
        .iter()
        .rev()
        .fold("(_ bv0 32)".to_string(), |else_branch, arm| {
            format!("(ite {} {})", arm, else_branch)
        });
    writeln!(
        output,
        "\n(define-fun encode_{dialect} ((instr TMDLInstr)) (_ BitVec 32)\n  {encode_body})"
    )?;

    let execute_body = execute_arms
        .iter()
        .rev()
        .fold("state".to_string(), |else_branch, arm| {
            format!("(ite {} {})", arm, else_branch)
        });
    writeln!(
        output,
        "\n(define-fun execute_{dialect} ((state TMDLState) (instr TMDLInstr)) TMDLState\n  {execute_body})"
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

fn build_smt_operands(operands: &[(String, Type)]) -> Vec<String> {
    operands
        .iter()
        .map(|(name, ty)| format!("({} {})", name.to_lowercase(), smt_ty_of(ty)))
        .collect()
}

fn smt_ty_of(ty: &Type) -> String {
    match ty {
        Type::Struct(_) => format!("(_ BitVec {REG_INDEX_WIDTH})"),
        Type::Bits(_) | Type::Integer => format!("(_ BitVec {REG_VALUE_WIDTH})"),
        Type::String => "String".to_string(),
        _ => unreachable!("HM type vars should not appear as operand types"),
    }
}

fn build_smt_encoding<'a>(
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    instruction: &'a ast::Instruction,
    operands: &[(String, Type)],
) -> String {
    let operands = operands.iter().cloned().collect::<HashMap<_, _>>();
    let params = resolve_params_for_instruction(instruction, item_cache);
    let encoding_arms = get_encoding_arms(instruction, item_cache);

    let mut pieces: Vec<(u16, String)> = Vec::new();
    for arm in &encoding_arms {
        let start = arm.start;
        let end = arm.end.unwrap_or(start);
        let width: u16 = end - start + 1;
        let high_bit = end;

        let piece = match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => render_lit_bitvec(width, li),
            ast::Expr::Ident(id) => {
                let name = &id.name;
                if let Some(ty) = operands.get(name) {
                    let vname = name.to_lowercase();
                    match ty {
                        Type::Struct(_) => cast_bv(&vname, REG_INDEX_WIDTH, width),
                        Type::Bits(_) | Type::Integer => cast_bv(&vname, REG_VALUE_WIDTH, width),
                        Type::String => zero_bv(width),
                        _ => unreachable!("HM type vars should not appear as operand types"),
                    }
                } else if let Some((pty, pval)) = params.get(name) {
                    match pval {
                        Some(ast::Expr::Lit(ast::Lit::Int(li))) => render_lit_bitvec(width, li),
                        _ => match pty {
                            Type::Bits(_) | Type::Integer => zero_bv(width),
                            _ => zero_bv(width),
                        },
                    }
                } else {
                    zero_bv(width)
                }
            }
            ast::Expr::Slice(s) => {
                let base_str = match &*s.base {
                    ast::Expr::Ident(id) => id.name.to_lowercase(),
                    _ => "(_ bv0 64)".to_string(),
                };
                format!("((_ extract {} {}) {})", s.end, s.start, base_str)
            }
            ast::Expr::IndexAccess(s) => {
                let base_str = match &*s.base {
                    ast::Expr::Ident(id) => id.name.to_lowercase(),
                    _ => "(_ bv0 64)".to_string(),
                };
                format!("((_ extract {} {}) {})", s.index, s.index, base_str)
            }
            _ => zero_bv(width),
        };

        pieces.push((high_bit, piece));
    }

    pieces.sort_by_key(|piece| std::cmp::Reverse(piece.0));

    let mut iter = pieces.into_iter().map(|(_, piece)| piece);
    iter.next()
        .map(|first| iter.fold(first, |acc, piece| format!("(concat {} {})", acc, piece)))
        .unwrap_or_else(|| "(_ bv0 32)".to_string())
}

// ---------------------------------------------------------------------------
// Behavior (execution semantics)
// ---------------------------------------------------------------------------

/// Sort of an emitted SMT expression. Mirrors the width/signedness tracking of
/// the sem-expr interpreter (`tir::sem_expr::exec`), which evaluates behaviors
/// over `APInt`s of varying width: every value is a bitvector of the
/// interpreter's width, except comparisons which stay `Bool` until they cross
/// back into arithmetic.
#[derive(Clone, Copy, PartialEq)]
enum SmtSort {
    Bool,
    Bv { width: u32, signed: bool },
}

#[derive(Clone)]
struct SmtVal {
    expr: String,
    sort: SmtSort,
}

impl SmtVal {
    fn bv(expr: String, width: u32, signed: bool) -> Self {
        SmtVal {
            expr,
            sort: SmtSort::Bv { width, signed },
        }
    }

    fn boolean(expr: String) -> Self {
        SmtVal {
            expr,
            sort: SmtSort::Bool,
        }
    }

    /// Comparison results materialize as width-1 integers, matching the
    /// interpreter's `APInt::new(1, ...)`.
    fn as_bv(&self) -> (String, u32, bool) {
        match &self.sort {
            SmtSort::Bool => (format!("(ite {} (_ bv1 1) (_ bv0 1))", self.expr), 1, false),
            SmtSort::Bv { width, signed } => (self.expr.clone(), *width, *signed),
        }
    }

    fn as_bool(&self) -> String {
        match &self.sort {
            SmtSort::Bool => self.expr.clone(),
            SmtSort::Bv { width, .. } => {
                format!("(distinct {} (_ bv0 {}))", self.expr, width)
            }
        }
    }
}

/// Mirror of `exec::widen`: sign-extend signed values, zero-extend unsigned
/// ones, no-op when already at least `target` wide.
fn widen_smt(expr: &str, width: u32, signed: bool, target: u32) -> String {
    if width >= target {
        expr.to_string()
    } else if signed {
        format!("((_ sign_extend {}) {})", target - width, expr)
    } else {
        format!("((_ zero_extend {}) {})", target - width, expr)
    }
}

/// Widen both operands to a common width, mirroring `exec::coerce_ints`.
fn coerce_smt(a: &SmtVal, b: &SmtVal) -> (String, String, u32, bool, bool) {
    let (ea, wa, sa) = a.as_bv();
    let (eb, wb, sb) = b.as_bv();
    let w = wa.max(wb);
    (
        widen_smt(&ea, wa, sa, w),
        widen_smt(&eb, wb, sb, w),
        w,
        sa,
        sb,
    )
}

enum SmtSymbolInfo {
    Register { class: String, number: u32 },
    Variable { name: String },
}

struct SmtSymbolResolver<'a> {
    symbols: HashMap<u32, SmtSymbolInfo>,
    operands: &'a HashMap<String, Type>,
    state_name: &'a str,
    pc_classes: &'a std::collections::HashSet<String>,
}

impl SmtSymbolResolver<'_> {
    fn resolve(&self, symbol_id: u32) -> Option<SmtVal> {
        let symbol = self.symbols.get(&symbol_id)?;

        match symbol {
            SmtSymbolInfo::Register { class, number } => {
                let class = class.to_lowercase();
                if self.pc_classes.contains(&class) {
                    Some(SmtVal::bv(
                        format!("(pc {})", self.state_name),
                        REG_VALUE_WIDTH as u32,
                        false,
                    ))
                } else {
                    Some(SmtVal::bv(
                        format!(
                            "(read_{} {} (_ bv{} {}))",
                            class, self.state_name, number, REG_INDEX_WIDTH
                        ),
                        REG_VALUE_WIDTH as u32,
                        false,
                    ))
                }
            }
            SmtSymbolInfo::Variable { name } => match self.operands.get(name)? {
                Type::Struct(rc) => {
                    let rc = rc.to_lowercase();
                    if self.pc_classes.contains(&rc) {
                        Some(SmtVal::bv(
                            format!("(pc {})", self.state_name),
                            REG_VALUE_WIDTH as u32,
                            false,
                        ))
                    } else {
                        Some(SmtVal::bv(
                            format!("(read_{} {} {})", rc, self.state_name, name.to_lowercase()),
                            REG_VALUE_WIDTH as u32,
                            false,
                        ))
                    }
                }
                // Immediate operands are passed as zero-extended 64-bit
                // function parameters; their semantic width is the declared
                // field width, which `sext`/`zext` in behaviors rely on.
                Type::Bits(n) => {
                    let n = (*n as u32).min(REG_VALUE_WIDTH as u32);
                    if n == REG_VALUE_WIDTH as u32 {
                        Some(SmtVal::bv(name.to_lowercase(), n, false))
                    } else {
                        Some(SmtVal::bv(
                            format!("((_ extract {} 0) {})", n - 1, name.to_lowercase()),
                            n,
                            false,
                        ))
                    }
                }
                Type::Integer => Some(SmtVal::bv(
                    name.to_lowercase(),
                    REG_VALUE_WIDTH as u32,
                    false,
                )),
                _ => None,
            },
        }
    }
}

/// Evaluate a symbol-free subtree to a constant, mirroring the interpreter's
/// width rules. Width expressions like `log2Ceil(self.XLEN) - 1` reach the
/// emitter unfolded, so structural `Constant` matching is not enough.
fn eval_const_subtree(graph: &tir::sem_expr::ExprPostGraph, node: NodeId) -> Option<(u64, u32)> {
    use tir::sem_expr::{ExprKind, ExprPayload};

    let child = |idx: usize| eval_const_subtree(graph, graph.children(node).nth(idx)?);
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
            let result = if v <= 1 {
                0u64
            } else {
                64 - (v - 1).leading_zeros() as u64
            };
            Some((result, w))
        }
        _ => None,
    }
}

fn emit_sem_expr(
    graph: &tir::sem_expr::ExprPostGraph,
    node: NodeId,
    resolver: &SmtSymbolResolver<'_>,
) -> Option<SmtVal> {
    use tir::sem_expr::{ExprKind, ExprPayload};

    let child_node = |idx: usize| graph.children(node).nth(idx);
    let child = |idx: usize| emit_sem_expr(graph, child_node(idx)?, resolver);
    let const_child =
        |idx: usize| -> Option<u64> { Some(eval_const_subtree(graph, child_node(idx)?)?.0) };
    // Result signedness `signed && signed` mirrors `APInt` binary ops.
    let arith = |op: &str| -> Option<SmtVal> {
        let (a, b, w, sa, sb) = coerce_smt(&child(0)?, &child(1)?);
        Some(SmtVal::bv(format!("({} {} {})", op, a, b), w, sa && sb))
    };
    let cmp = |op: &str| -> Option<SmtVal> {
        let (a, b, _, _, _) = coerce_smt(&child(0)?, &child(1)?);
        Some(SmtVal::boolean(format!("({} {} {})", op, a, b)))
    };
    // Result width is the left operand's width; the amount is reinterpreted at
    // that width, matching the interpreter's `amount.to_u64()`.
    let shift = |op: &str, signed: fn(bool) -> bool| -> Option<SmtVal> {
        let (lhs, wl, sl) = child(0)?.as_bv();
        let (amt, wamt, samt) = child(1)?.as_bv();
        let amt = if wamt > wl {
            format!("((_ extract {} 0) {})", wl - 1, amt)
        } else {
            widen_smt(&amt, wamt, samt, wl)
        };
        Some(SmtVal::bv(
            format!("({} {} {})", op, lhs, amt),
            wl,
            signed(sl),
        ))
    };

    match graph.get_node(node) {
        ExprKind::Symbol => match graph.get_leaf_data(node)? {
            ExprPayload::SymbolId(id) => resolver.resolve(*id),
            _ => None,
        },
        ExprKind::Constant => match graph.get_leaf_data(node)? {
            ExprPayload::Int(i) => {
                let w = i.width();
                let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
                Some(SmtVal::bv(
                    format!("(_ bv{} {})", i.to_u64() & mask, w),
                    w,
                    i.is_signed(),
                ))
            }
            _ => None,
        },
        ExprKind::Add => arith("bvadd"),
        ExprKind::Sub => arith("bvsub"),
        ExprKind::Mul => arith("bvmul"),
        ExprKind::Div => arith("bvsdiv"),
        ExprKind::UDiv => arith("bvudiv"),
        ExprKind::Eq => cmp("="),
        ExprKind::Ne => cmp("distinct"),
        ExprKind::Lt => cmp("bvslt"),
        ExprKind::Gt => cmp("bvsgt"),
        ExprKind::Ge => cmp("bvsge"),
        ExprKind::ULt => cmp("bvult"),
        ExprKind::ULe => cmp("bvule"),
        ExprKind::UGt => cmp("bvugt"),
        ExprKind::UGe => cmp("bvuge"),
        ExprKind::ShiftLeft => shift("bvshl", |s| s),
        ExprKind::ShiftRightLogic => shift("bvlshr", |_| false),
        // An arithmetic shift always treats its operand as signed, like the
        // interpreter, which forces the signedness flag before shifting.
        ExprKind::ShiftRightArithmetic => shift("bvashr", |_| true),
        ExprKind::Or => arith("bvor"),
        ExprKind::And => arith("bvand"),
        ExprKind::Xor => arith("bvxor"),
        ExprKind::If => {
            let cond = child(0)?.as_bool();
            let (t, e, w, st, se) = coerce_smt(&child(1)?, &child(2)?);
            Some(SmtVal::bv(
                format!("(ite {} {} {})", cond, t, e),
                w,
                st || se,
            ))
        }
        ExprKind::ZExt => {
            let (e, w, _) = child(0)?.as_bv();
            let target = const_child(1)? as u32;
            if target < w {
                return None;
            }
            Some(SmtVal::bv(
                widen_smt(&e, w, false, target),
                target.max(w),
                false,
            ))
        }
        ExprKind::SExt => {
            let (e, w, _) = child(0)?.as_bv();
            let target = const_child(1)? as u32;
            if target < w {
                return None;
            }
            Some(SmtVal::bv(
                widen_smt(&e, w, true, target),
                target.max(w),
                true,
            ))
        }
        ExprKind::Extract => {
            let (e, w, _) = child(0)?.as_bv();
            let high = const_child(1)? as u32;
            let low = const_child(2)? as u32;
            if high < low {
                return None;
            }
            let mul = child_node(0)?;
            if low >= w && matches!(graph.get_node(mul), ExprKind::Mul) {
                // `extract(a * b, 2N-1, N)` is the TMDL idiom for the high half
                // of a full multiply (e.g. RISC-V `mulh`); the interpreter
                // recomputes it as a signed full-width product.
                let m0 = emit_sem_expr(graph, graph.children(mul).next()?, resolver)?;
                let m1 = emit_sem_expr(graph, graph.children(mul).nth(1)?, resolver)?;
                let (a, b, wm, _, _) = coerce_smt(&m0, &m1);
                if high >= 2 * wm {
                    return None;
                }
                Some(SmtVal::bv(
                    format!(
                        "((_ extract {} {}) (bvmul ((_ sign_extend {}) {}) ((_ sign_extend {}) {})))",
                        high, low, wm, a, wm, b
                    ),
                    high - low + 1,
                    false,
                ))
            } else if high < w {
                Some(SmtVal::bv(
                    format!("((_ extract {} {}) {})", high, low, e),
                    high - low + 1,
                    false,
                ))
            } else {
                None
            }
        }
        ExprKind::Log2Ceil => {
            let (v, w) = eval_const_subtree(graph, node)?;
            Some(SmtVal::bv(format!("(_ bv{} {})", v, w), w, false))
        }
        ExprKind::Clamp => {
            let input = child(0)?;
            let (_, _, signed) = input.as_bv();
            let (lt, gt) = if signed {
                ("bvslt", "bvsgt")
            } else {
                ("bvult", "bvugt")
            };
            let (i1, min, w1, _, _) = coerce_smt(&input, &child(1)?);
            let (i2, max, w2, _, _) = coerce_smt(&input, &child(2)?);
            let w = w1.max(w2);
            let (i1, min, i2, max) = (
                widen_smt(&i1, w1, signed, w),
                widen_smt(&min, w1, false, w),
                widen_smt(&i2, w2, signed, w),
                widen_smt(&max, w2, false, w),
            );
            Some(SmtVal::bv(
                format!(
                    "(ite ({} {} {}) {} (ite ({} {} {}) {} {}))",
                    lt, i1, min, min, gt, i2, max, max, i1
                ),
                w,
                signed,
            ))
        }
        ExprKind::LoadMemory | ExprKind::StoreMemory | ExprKind::Sqrt | ExprKind::Fma => None,
    }
}

/// Translate an instruction behavior into an SMT state-transition expression.
/// Returns `None` when the behavior uses constructs the SMT model cannot
/// express (e.g. memory accesses); callers must not pretend such instructions
/// have identity semantics.
fn build_smt_behavior<'a>(
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    instruction: &'a ast::Instruction,
    operands: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
    pc_classes: &std::collections::HashSet<String>,
) -> Option<String> {
    let operands = operands.iter().cloned().collect::<HashMap<_, _>>();
    let mut numeric_params: HashMap<String, i64> =
        resolve_isa_param_values(instruction, item_cache);
    numeric_params.extend(
        resolve_params_for_instruction(instruction, item_cache)
            .into_iter()
            .filter_map(|(name, (_ty, val))| match val {
                Some(ast::Expr::Lit(ast::Lit::Int(li))) => {
                    Some((name, parse_literal_value_u128(&li) as i64))
                }
                _ => None,
            }),
    );

    fn try_emit_sem_expr(
        e: &ast::Expr,
        operands: &HashMap<String, Type>,
        numeric_params: &HashMap<String, i64>,
        register_index_map: &HashMap<(String, String), u32>,
        pc_classes: &std::collections::HashSet<String>,
        state_name: &str,
    ) -> Option<SmtVal> {
        let mut graph = tir::sem_expr::ExprPostGraph::new();
        let lowering =
            e.lower_to_sema_with_registers(&mut graph, numeric_params, register_index_map)?;
        let mut symbols = HashMap::new();
        for (name, id) in &lowering.variable_symbols {
            symbols.insert(*id, SmtSymbolInfo::Variable { name: name.clone() });
        }
        for ((class, number), id) in &lowering.register_symbols {
            symbols.insert(
                *id,
                SmtSymbolInfo::Register {
                    class: class.clone(),
                    number: *number,
                },
            );
        }
        let resolver = SmtSymbolResolver {
            symbols,
            operands,
            state_name,
            pc_classes,
        };
        emit_sem_expr(&graph, lowering.root, &resolver)
    }

    let failed = std::cell::Cell::new(false);
    let emit_val = |e: &ast::Expr| {
        try_emit_sem_expr(
            e,
            &operands,
            &numeric_params,
            register_index_map,
            pc_classes,
            "st",
        )
        .or_else(|| {
            failed.set(true);
            None
        })
    };
    // `compile_to_state` only evaluates `if` conditions through this closure.
    let emit_cond = |e: &ast::Expr| {
        emit_val(e)
            .map(|v| v.as_bool())
            .unwrap_or_else(|| "false".to_string())
    };
    let emit_assign = |a: &ast::Assign, st_name: &str| {
        let rhs = emit_val(&a.value)?;
        let (expr, width, signed) = rhs.as_bv();
        let rhs = widen_smt(&expr, width, signed, REG_VALUE_WIDTH as u32);
        let dest_name = match &*a.dest {
            ast::Expr::Ident(id) => Some(id.name.as_str()),
            ast::Expr::Path(p) if p.remainder.len() == 1 => Some(p.remainder[0].as_str()),
            _ => None,
        };
        if dest_name == Some("pc") {
            Some(format!("(write_pc {} {})", st_name, rhs))
        } else if let Some(name) = dest_name {
            match operands.get(name) {
                Some(Type::Struct(rc)) if pc_classes.contains(&rc.to_lowercase()) => {
                    Some(format!("(write_pc {} {})", st_name, rhs))
                }
                Some(Type::Struct(rc)) => Some(format!(
                    "(write_{} {} {} {})",
                    rc.to_lowercase(),
                    st_name,
                    name.to_lowercase(),
                    rhs
                )),
                _ => None,
            }
        } else {
            None
        }
    };
    let emit_if = |cond: &str, then_state: &str, else_state: &str| {
        format!("(ite {} {} {})", cond, then_state, else_state)
    };
    let on_unsupported = |_: &ast::Expr| failed.set(true);
    let body = sem_expr_state::compile_to_state(
        &instruction.behavior,
        "st",
        &emit_cond,
        &emit_assign,
        &emit_if,
        &on_unsupported,
    );
    if failed.get() { None } else { Some(body) }
}

// ---------------------------------------------------------------------------
// Decoder (instruction word → TMDLInstr)
// ---------------------------------------------------------------------------

fn build_decoder<'a>(
    dialect: &str,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    files: &'a [ast::File],
    output: &mut Box<dyn Write>,
) -> Result<(), TMDLError> {
    let instructions: Vec<&ast::Instruction> =
        files.iter().flat_map(|f| f.instructions()).collect();
    if instructions.is_empty() {
        return Ok(());
    }

    let mut arms: Vec<(String, String)> = vec![];

    for i in &instructions {
        let name_upper = i.name.to_uppercase();
        let operand_list = resolve_operands_for_instruction(i, item_cache);
        let operands: HashMap<String, Type> = operand_list.iter().cloned().collect();
        let params = resolve_params_for_instruction(i, item_cache);
        let encoding_arms = get_encoding_arms(i, item_cache);

        // For each operand: collect (op_lo, op_hi, word_lo, word_hi) pieces.
        let mut operand_pieces: HashMap<String, Vec<(u16, u16, u16, u16)>> = HashMap::new();
        let mut guards: Vec<String> = vec![];

        for arm in &encoding_arms {
            let word_lo = arm.start;
            let word_hi = arm.end.unwrap_or(arm.start);
            let word_width = word_hi - word_lo + 1;

            match &arm.value {
                ast::Expr::Lit(ast::Lit::Int(li)) => {
                    let val = parse_literal_value_u128(li);
                    guards.push(format!(
                        "(= ((_ extract {} {}) word) (_ bv{} {}))",
                        word_hi, word_lo, val, word_width
                    ));
                }
                ast::Expr::Ident(id) => {
                    let name = &id.name;
                    if operands.contains_key(name) {
                        // The entire word field holds bits [0..word_width-1] of the operand.
                        operand_pieces.entry(name.clone()).or_default().push((
                            0,
                            word_width - 1,
                            word_lo,
                            word_hi,
                        ));
                    } else if let Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) =
                        params.get(name)
                    {
                        let val = parse_literal_value_u128(li);
                        guards.push(format!(
                            "(= ((_ extract {} {}) word) (_ bv{} {}))",
                            word_hi, word_lo, val, word_width
                        ));
                    }
                    // Unresolved param: no guard emitted (treated as don't-care).
                }
                ast::Expr::Slice(s) => {
                    if let ast::Expr::Ident(id) = &*s.base
                        && operands.contains_key(&id.name)
                    {
                        operand_pieces
                            .entry(id.name.clone())
                            .or_default()
                            .push((s.start, s.end, word_lo, word_hi));
                    }
                }
                ast::Expr::IndexAccess(s) => {
                    if let ast::Expr::Ident(id) = &*s.base
                        && operands.contains_key(&id.name)
                    {
                        operand_pieces
                            .entry(id.name.clone())
                            .or_default()
                            .push((s.index, s.index, word_lo, word_hi));
                    }
                }
                _ => {}
            }
        }

        let guard = match guards.len() {
            0 => "true".to_string(),
            1 => guards.remove(0),
            _ => format!("(and {})", guards.join(" ")),
        };

        // Build the constructor arguments in operand declaration order.
        let constructor_args: Vec<String> = operand_list
            .iter()
            .map(|(op_name, op_ty)| {
                let target_width = match op_ty {
                    Type::Struct(_) => REG_INDEX_WIDTH,
                    _ => REG_VALUE_WIDTH,
                };

                let Some(mut pieces) = operand_pieces.remove(op_name) else {
                    return zero_bv(target_width);
                };

                // Sort pieces by op_hi descending so the concat builds high→low.
                pieces.sort_by_key(|piece| std::cmp::Reverse(piece.1));

                // Reconstruct the operand from its pieces, filling any gaps
                // between non-contiguous slices with zero bits.
                // `expected_hi` tracks the next op bit we expect; it starts at
                // the top bit of the highest piece and steps downward.
                let mut fragments: Vec<String> = vec![];
                let mut raw_width: u16 = 0;
                let mut expected_hi = pieces[0].1;

                for (op_lo, op_hi, word_lo, word_hi) in &pieces {
                    // Fill any gap between the previous piece and this one.
                    if *op_hi < expected_hi {
                        let gap = expected_hi - op_hi; // bits [expected_hi..op_hi+1]
                        fragments.push(zero_bv(gap));
                        raw_width += gap;
                    }
                    fragments.push(format!("((_ extract {} {}) word)", word_hi, word_lo));
                    raw_width += op_hi - op_lo + 1;
                    expected_hi = op_lo.saturating_sub(1);
                }
                // Fill any gap below the lowest piece (bits [op_lo-1..0]).
                let lowest_op_lo = pieces.last().map(|(lo, _, _, _)| *lo).unwrap_or(0);
                if lowest_op_lo > 0 {
                    fragments.push(zero_bv(lowest_op_lo));
                    raw_width += lowest_op_lo;
                }

                let raw = fragments
                    .into_iter()
                    .reduce(|acc, f| format!("(concat {} {})", acc, f))
                    .unwrap_or_else(|| zero_bv(target_width));

                cast_bv_smt(&raw, raw_width, target_width)
            })
            .collect();

        let constructor = if constructor_args.is_empty() {
            name_upper.clone()
        } else {
            format!("({name_upper} {})", constructor_args.join(" "))
        };
        arms.push((guard, constructor));
    }

    // Build a fallback: the first instruction with all-zero operands.
    let first = &instructions[0];
    let first_ops = resolve_operands_for_instruction(first, item_cache);
    let fallback = {
        let zeros: Vec<String> = first_ops
            .iter()
            .map(|(_, ty)| {
                zero_bv(match ty {
                    Type::Struct(_) => REG_INDEX_WIDTH,
                    _ => REG_VALUE_WIDTH,
                })
            })
            .collect();
        if zeros.is_empty() {
            first.name.to_uppercase()
        } else {
            format!("({} {})", first.name.to_uppercase(), zeros.join(" "))
        }
    };

    // Fold arms into nested ites, first arm wins.
    let body = arms
        .iter()
        .rev()
        .fold(fallback, |else_branch, (guard, then_branch)| {
            format!("(ite {}\n    {}\n    {})", guard, then_branch, else_branch)
        });

    writeln!(
        output,
        "\n(define-fun decode_{dialect} ((word (_ BitVec 32))) TMDLInstr\n  {})",
        body
    )?;

    writeln!(
        output,
        "\n(define-fun execute_by_word_{dialect} ((state TMDLState) (word (_ BitVec 32))) TMDLState\n  (execute_{dialect} state (decode_{dialect} word)))"
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Bitvector rendering helpers
// ---------------------------------------------------------------------------

fn render_lit_bitvec(width: u16, lit: &ast::LitInt) -> String {
    let value = parse_literal_value_u128(lit);
    format!("(_ bv{} {})", value, width)
}

fn zero_bv(width: u16) -> String {
    format!("(_ bv0 {})", width)
}

/// SMT-lib needs the full u128 range for large bitvector literals.
fn parse_literal_value_u128(lit: &ast::LitInt) -> u128 {
    let v = lit.value();
    if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u128::from_str_radix(hex, 16).unwrap_or(0)
    } else if let Some(bin) = v.strip_prefix("0b") {
        u128::from_str_radix(bin, 2).unwrap_or(0)
    } else {
        v.parse::<u128>().unwrap_or(0)
    }
}

fn cast_bv(name: &str, from_width: u16, to_width: u16) -> String {
    cast_bv_smt(name, from_width, to_width)
}

/// Like `cast_bv` but accepts an arbitrary SMT-LIB expression instead of a
/// plain identifier.  When `from_width == to_width` the expression is returned
/// as-is; otherwise it is wrapped in `zero_extend` or `extract`.
fn cast_bv_smt(expr: &str, from_width: u16, to_width: u16) -> String {
    match from_width.cmp(&to_width) {
        std::cmp::Ordering::Equal => expr.to_string(),
        std::cmp::Ordering::Less => {
            format!("((_ zero_extend {}) {})", to_width - from_width, expr)
        }
        std::cmp::Ordering::Greater => {
            format!("((_ extract {} 0) {})", to_width - 1, expr)
        }
    }
}

// AUFDTBV: Arrays, Uninterpreted Functions, Datatypes (for TMDLInstr),
// BitVectors.  Use ALL as an alias that Z3 and CVC5 both accept.
const HEADER: &str = "; Automatically generated by TMDL compiler\n(set-logic ALL)\n";
