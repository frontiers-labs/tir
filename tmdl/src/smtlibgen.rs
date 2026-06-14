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

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Register-file layout of one (non-PC) register class, resolved against the
/// target ISA's parameters.
struct ClassInfo {
    idx_width: u16,
    val_width: u16,
    /// Encoding index of a hardwired-zero register (RISC-V `x0`, AArch64
    /// `xzr`), if the class has one: reads yield 0 and writes are dropped.
    zero_index: Option<u16>,
    /// State field holding this class's registers. A derived class (AArch64
    /// `GPRsp : GPR`) aliases its base's physical file, so both classes
    /// read and write one array; only its accessors differ (`GPRsp` has no
    /// hardwired zero at slot 31, `GPR` does).
    storage: String,
}

struct SmtCtx<'a> {
    isa: &'a str,
    /// Register value width of the target ISA; immediates and the PC use it.
    xlen: u16,
    /// Lowercase class name -> layout. BTreeMap so the emitted state datatype
    /// has a deterministic field order.
    classes: BTreeMap<String, ClassInfo>,
    pc_classes: std::collections::HashSet<String>,
    isa_params: HashMap<String, i64>,
    /// The target ISA's trap-entry sequence, inlined at `trap(...)` calls.
    trap_handler: Option<&'a ast::TrapHandler>,
}

/// The trap handler of `isa` or the nearest one in its requires closure.
fn find_trap_handler<'a>(
    isa: &str,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<&'a ast::TrapHandler> {
    let mut pending = vec![isa.to_string()];
    let mut visited = std::collections::HashSet::new();
    while let Some(name) = pending.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(ast::Item::Isa(isa)) = item_cache.get(name.as_str()) else {
            continue;
        };
        if let Some(handler) = &isa.trap_handler {
            return Some(handler);
        }
        match &isa.requires {
            None => {}
            Some(ast::IsaRequirement::Single(parent)) => pending.push(parent.clone()),
            Some(ast::IsaRequirement::Any(parents)) | Some(ast::IsaRequirement::All(parents)) => {
                pending.extend(parents.iter().cloned());
            }
        }
    }
    None
}

/// Instruction operands with `bits<expr>` widths resolved for the target ISA
/// (the ISA's own parameter values win over the cross-ISA maximum, so an
/// instruction shared by RV32I and RV64I sees XLEN=32 on RV32I).
fn resolved_operands<'a>(
    ctx: &SmtCtx<'_>,
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, Type)> {
    let mut params = resolve_isa_param_values(inst, item_cache);
    params.extend(ctx.isa_params.iter().map(|(k, v)| (k.clone(), *v)));
    resolve_operand_widths(resolve_operands_for_instruction(inst, item_cache), &params)
}

impl SmtCtx<'_> {
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
}

/// Resolve a register-class parameter (`ENCODING_LEN`, `WIDTH`) to a number:
/// either a literal or a `self.PARAM` reference into the target ISA.
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

pub fn generate_smtlib<'a>(
    dialect: &str,
    isa: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    let isa_params = isa_param_values(isa, item_cache);
    let xlen = isa_params.get("XLEN").copied().unwrap_or(64) as u16;

    let mut classes = BTreeMap::new();
    let mut pc_classes = std::collections::HashSet::new();
    let base_of: HashMap<String, String> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .filter_map(|rc| {
            rc.base
                .as_ref()
                .map(|b| (rc.name.to_lowercase(), b.to_lowercase()))
        })
        .collect();
    let storage_of = |name: &str| {
        let mut current = name.to_string();
        while let Some(base) = base_of.get(&current) {
            if *base == current {
                break;
            }
            current = base.clone();
        }
        current
    };
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
            name.clone(),
            ClassInfo {
                idx_width: eval_class_param(rc, "ENCODING_LEN", &isa_params).unwrap_or(5) as u16,
                val_width: eval_class_param(rc, "WIDTH", &isa_params).unwrap_or(xlen as i64) as u16,
                zero_index: rc.hardwired_zero_register_index(),
                storage: storage_of(&name),
            },
        );
    }
    let ctx = SmtCtx {
        isa,
        xlen,
        classes,
        pc_classes,
        isa_params,
        trap_handler: find_trap_handler(isa, item_cache),
    };

    writeln!(output, "{}", HEADER)?;
    build_state(&ctx, &mut output)?;
    build_instructions(dialect, &ctx, item_cache, files, &mut output)?;
    build_decoder(dialect, &ctx, item_cache, files, &mut output)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// State (register file) declaration
// ---------------------------------------------------------------------------

fn is_pc_class(rc: &ast::RegisterClass) -> bool {
    rc.resolve_registers()
        .any(|r| r.traits.contains(&ast::RegisterTrait::ProgramCounter))
}

fn build_state(ctx: &SmtCtx<'_>, output: &mut Box<dyn Write>) -> Result<(), TMDLError> {
    // Derived classes alias their base's array, so only storage-owning
    // classes contribute a state field.
    let arrays: Vec<&String> = ctx
        .classes
        .iter()
        .filter(|(name, info)| info.storage == **name)
        .map(|(name, _)| name)
        .collect();

    let mut fields = arrays
        .iter()
        .map(|name| {
            let info = &ctx.classes[*name];
            format!(
                "({} (Array (_ BitVec {}) (_ BitVec {})))",
                name, info.idx_width, info.val_width
            )
        })
        .collect::<Vec<_>>();
    fields.push(format!(
        "(mem (Array (_ BitVec {}) (_ BitVec 8)))",
        ctx.xlen
    ));
    fields.push(format!("(pc (_ BitVec {}))", ctx.xlen));

    writeln!(
        output,
        "(declare-datatypes () ((TMDLState (mk-TMDLState {}))))",
        fields.join(" ")
    )?;

    for (name, info) in &ctx.classes {
        let idx_width = info.idx_width;
        let val_width = info.val_width;
        let storage = &info.storage;
        let select = format!("(select ({storage} st) r)");
        let read_body = match info.zero_index {
            Some(z) => {
                format!("(ite (= r (_ bv{z} {idx_width}))\n    (_ bv0 {val_width})\n    {select})")
            }
            None => select,
        };
        writeln!(
            output,
            "\n(define-fun read_{name} ((st TMDLState) (r (_ BitVec {idx_width}))) (_ BitVec {val_width})\n  {read_body})",
        )?;

        let mut fields = Vec::new();
        for n2 in &arrays {
            if *n2 == storage {
                fields.push(format!("(store ({} st) r val)", n2));
            } else {
                fields.push(format!("({} st)", n2));
            }
        }
        fields.push("(mem st)".to_string());
        fields.push("(pc st)".to_string());
        let store = format!("(mk-TMDLState {})", fields.join(" "));
        let write_body = match info.zero_index {
            Some(z) => format!("(ite (= r (_ bv{z} {idx_width}))\n    st\n    {store})"),
            None => store,
        };
        writeln!(
            output,
            "\n(define-fun write_{name} ((st TMDLState) (r (_ BitVec {idx_width})) (val (_ BitVec {val_width}))) TMDLState\n  {write_body})",
        )?;
    }

    let mut fields = arrays
        .iter()
        .map(|name| format!("({} st)", name))
        .collect::<Vec<_>>();
    fields.push("(mem st)".to_string());
    fields.push("val".to_string());
    writeln!(
        output,
        "\n(define-fun write_pc ((st TMDLState) (val (_ BitVec {val_width}))) TMDLState\n  (mk-TMDLState {fields}))",
        val_width = ctx.xlen,
        fields = fields.join(" ")
    )?;

    // Byte-addressable little-endian memory accessors, one pair per access
    // width, mirroring the interpreter's `Memory` convention.
    let xlen = ctx.xlen;
    for bytes in MEM_ACCESS_BYTES {
        let val_width = bytes * 8;
        let byte_at = |i: u16| {
            if i == 0 {
                "(select (mem st) addr)".to_string()
            } else {
                format!("(select (mem st) (bvadd addr (_ bv{i} {xlen})))")
            }
        };
        let read_body = (0..bytes)
            .rev()
            .map(byte_at)
            .reduce(|acc, b| format!("(concat {} {})", acc, b))
            .expect("at least one byte");
        writeln!(
            output,
            "\n(define-fun read_mem_{bytes} ((st TMDLState) (addr (_ BitVec {xlen}))) (_ BitVec {val_width})\n  {read_body})",
        )?;

        let mut mem = "(mem st)".to_string();
        for i in 0..bytes {
            let slot = if i == 0 {
                "addr".to_string()
            } else {
                format!("(bvadd addr (_ bv{i} {xlen}))")
            };
            let byte = format!("((_ extract {} {}) val)", i * 8 + 7, i * 8);
            mem = format!("(store {} {} {})", mem, slot, byte);
        }
        let mut fields = arrays
            .iter()
            .map(|name| format!("({} st)", name))
            .collect::<Vec<_>>();
        fields.push(mem);
        fields.push("(pc st)".to_string());
        writeln!(
            output,
            "\n(define-fun write_mem_{bytes} ((st TMDLState) (addr (_ BitVec {xlen})) (val (_ BitVec {val_width}))) TMDLState\n  (mk-TMDLState {}))",
            fields.join(" ")
        )?;
    }

    Ok(())
}

/// Memory access widths with dedicated SMT accessors.
const MEM_ACCESS_BYTES: [u16; 4] = [1, 2, 4, 8];

// ---------------------------------------------------------------------------
// Instruction encoding and execution
// ---------------------------------------------------------------------------

fn build_instructions<'a>(
    dialect: &str,
    ctx: &SmtCtx<'_>,
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

    for i in files.iter().flat_map(|f| f.instructions()) {
        if !item_supports_isa(&i.for_isas, ctx.isa, item_cache) {
            continue;
        }
        let name = i.name.to_lowercase();
        let uppercase_name = name.to_uppercase();

        let operands = resolved_operands(ctx, i, item_cache);
        let smt_operands = build_smt_operands(ctx, &operands);
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
        let smt_encoding = build_smt_encoding(ctx, item_cache, i, &operands);
        let smt_behavior = build_smt_behavior(ctx, item_cache, i, &operands, &register_index_map);
        // Untranslatable behaviors (e.g. memory accesses) get an identity body
        // plus a machine-readable marker so verification tooling can tell
        // "proven unchanged" apart from "not modeled".
        let (smt_behavior, marker, writes_pc) = match smt_behavior {
            Some((b, writes_pc)) => (b, String::new(), writes_pc),
            None => (
                "st".to_string(),
                format!("\n; UNSUPPORTED-BEHAVIOR: {}", name),
                false,
            ),
        };
        write!(output, "{}", marker)?;

        // Machine-readable operand inventory for verification tooling.
        let operand_meta = operands
            .iter()
            .map(|(op_name, ty)| {
                let kind = match ty {
                    Type::Struct(rc) => {
                        format!("reg:{}:{}", rc.to_lowercase(), ctx.idx_width(rc))
                    }
                    Type::Bits(n) => format!("bits:{}", n),
                    _ => "int".to_string(),
                };
                format!("{}:{}", op_name.to_lowercase(), kind)
            })
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(
            output,
            "\n; INSTRUCTION: {} writes-pc={} {}",
            name, writes_pc, operand_meta
        )?;

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
            .map(|(op_name, ty)| {
                format!(
                    "({}_{} {})",
                    name,
                    op_name.to_lowercase(),
                    smt_ty_of(ctx, ty)
                )
            })
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

fn build_smt_operands(ctx: &SmtCtx<'_>, operands: &[(String, Type)]) -> Vec<String> {
    operands
        .iter()
        .map(|(name, ty)| format!("({} {})", name.to_lowercase(), smt_ty_of(ctx, ty)))
        .collect()
}

fn smt_ty_of(ctx: &SmtCtx<'_>, ty: &Type) -> String {
    match ty {
        Type::Struct(rc) => format!("(_ BitVec {})", ctx.idx_width(rc)),
        Type::Bits(_) | Type::Integer => format!("(_ BitVec {})", ctx.xlen),
        Type::String => "String".to_string(),
        _ => unreachable!("HM type vars should not appear as operand types"),
    }
}

fn build_smt_encoding<'a>(
    ctx: &SmtCtx<'_>,
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
                        Type::Struct(rc) => cast_bv(&vname, ctx.idx_width(rc), width),
                        Type::Bits(_) | Type::Integer => cast_bv(&vname, ctx.xlen, width),
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

/// Coerce an expression to exactly `target` bits: widen when narrower,
/// truncate when wider. Register writes use it so a value computed at a wider
/// width still fits a narrow destination (e.g. a 1-bit PSTATE flag).
fn fit_smt(expr: &str, width: u32, signed: bool, target: u32) -> String {
    if width > target {
        format!("((_ extract {} 0) {})", target - 1, expr)
    } else {
        widen_smt(expr, width, signed, target)
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
    /// Let-bound variables (exception payloads), shadowing operands.
    locals: &'a HashMap<String, SmtVal>,
    state_name: &'a str,
    ctx: &'a SmtCtx<'a>,
}

impl SmtSymbolResolver<'_> {
    fn resolve(&self, symbol_id: u32) -> Option<SmtVal> {
        let symbol = self.symbols.get(&symbol_id)?;
        let ctx = self.ctx;

        match symbol {
            SmtSymbolInfo::Register { class, number } => {
                let class = class.to_lowercase();
                if ctx.pc_classes.contains(&class) {
                    Some(SmtVal::bv(
                        format!("(pc {})", self.state_name),
                        ctx.xlen as u32,
                        false,
                    ))
                } else {
                    Some(SmtVal::bv(
                        format!(
                            "(read_{} {} (_ bv{} {}))",
                            class,
                            self.state_name,
                            number,
                            ctx.idx_width(&class)
                        ),
                        ctx.val_width(&class) as u32,
                        false,
                    ))
                }
            }
            SmtSymbolInfo::Variable { name } if self.locals.contains_key(name) => {
                Some(self.locals[name].clone())
            }
            SmtSymbolInfo::Variable { name } => match self.operands.get(name)? {
                Type::Struct(rc) => {
                    let rc = rc.to_lowercase();
                    if ctx.pc_classes.contains(&rc) {
                        Some(SmtVal::bv(
                            format!("(pc {})", self.state_name),
                            ctx.xlen as u32,
                            false,
                        ))
                    } else {
                        Some(SmtVal::bv(
                            format!("(read_{} {} {})", rc, self.state_name, name.to_lowercase()),
                            ctx.val_width(&rc) as u32,
                            false,
                        ))
                    }
                }
                // Immediate operands are passed as zero-extended XLEN-wide
                // function parameters; their semantic width is the declared
                // field width, which `sext`/`zext` in behaviors rely on.
                Type::Bits(n) => {
                    let n = (*n as u32).min(ctx.xlen as u32);
                    if n == ctx.xlen as u32 {
                        Some(SmtVal::bv(name.to_lowercase(), n, false))
                    } else {
                        Some(SmtVal::bv(
                            format!("((_ extract {} 0) {})", n - 1, name.to_lowercase()),
                            n,
                            false,
                        ))
                    }
                }
                Type::Integer => Some(SmtVal::bv(name.to_lowercase(), ctx.xlen as u32, false)),
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
        ExprKind::Not => {
            let (e, w, s) = child(0)?.as_bv();
            Some(SmtVal::bv(format!("(bvnot {})", e), w, s))
        }
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
        ExprKind::LoadMemory => {
            let (addr, w, s) = child(0)?.as_bv();
            let bytes = const_child(1)? as u16;
            if !MEM_ACCESS_BYTES.contains(&bytes) {
                return None;
            }
            let xlen = resolver.ctx.xlen as u32;
            Some(SmtVal::bv(
                format!(
                    "(read_mem_{} {} {})",
                    bytes,
                    resolver.state_name,
                    fit_smt(&addr, w, s, xlen)
                ),
                bytes as u32 * 8,
                false,
            ))
        }
        // Stores are effect statements, handled by `BehaviorEmitter::store`.
        ExprKind::StoreMemory | ExprKind::Sqrt | ExprKind::Fma => None,
        // Loops are eliminated by `unroll_loops` before emission; a surviving one
        // has symbolic bounds, which SMT-LIB cannot express, so it is unsupported.
        ExprKind::Loop | ExprKind::IndVar | ExprKind::Acc => None,
        // Vector values have no scalar SMT-LIB encoding, so a vector map or lane
        // read is unsupported by the bit-vector backend.
        ExprKind::VectorMap | ExprKind::Lane => None,
        ExprKind::Map | ExprKind::Zip | ExprKind::IterConcat => None,
        ExprKind::Split | ExprKind::Reduce | ExprKind::Arg => None,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MemOpKind {
    Load,
    Store,
}

struct MemOp<'a> {
    kind: MemOpKind,
    addr: &'a ast::Expr,
    bytes: u64,
}

fn ast_int_lit(e: &ast::Expr) -> Option<u64> {
    match e {
        ast::Expr::Lit(ast::Lit::Int(li)) => Some(parse_literal_value(li)),
        _ => None,
    }
}

/// Memory operations on the no-trap path of `e`, in syntactic order. `None`
/// when an access size is not a literal (no exception condition can be built
/// for it). Nested try blocks own their accesses and are not descended into.
fn collect_mem_ops<'a>(e: &'a ast::Expr, out: &mut Vec<MemOp<'a>>) -> Option<()> {
    match e {
        ast::Expr::Call(c) => {
            for arg in &c.arguments {
                collect_mem_ops(arg, out)?;
            }
            let kind = match &*c.callee {
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Load) => Some(MemOpKind::Load),
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store) => Some(MemOpKind::Store),
                _ => None,
            };
            if let Some(kind) = kind {
                let addr = c.arguments.first()?;
                let bytes = ast_int_lit(c.arguments.get(1)?)?;
                out.push(MemOp { kind, addr, bytes });
            }
        }
        ast::Expr::Assign(a) => collect_mem_ops(&a.value, out)?,
        ast::Expr::Binary(b) => {
            collect_mem_ops(&b.lhs, out)?;
            collect_mem_ops(&b.rhs, out)?;
        }
        ast::Expr::Unary(u) => collect_mem_ops(&u.x, out)?,
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_mem_ops(stmt, out)?;
            }
        }
        ast::Expr::If(i) => {
            collect_mem_ops(&i.cond, out)?;
            collect_mem_ops(&i.then, out)?;
            if let Some(else_) = &i.else_ {
                collect_mem_ops(else_, out)?;
            }
        }
        ast::Expr::Slice(s) => collect_mem_ops(&s.base, out)?,
        ast::Expr::IndexAccess(i) => collect_mem_ops(&i.base, out)?,
        ast::Expr::Field(f) => collect_mem_ops(&f.base, out)?,
        ast::Expr::For(f) => {
            collect_mem_ops(&f.start, out)?;
            collect_mem_ops(&f.end, out)?;
            collect_mem_ops(&f.body, out)?;
        }
        ast::Expr::Lambda(l) => collect_mem_ops(&l.body, out)?,
        ast::Expr::Try(_)
        | ast::Expr::Ident(_)
        | ast::Expr::Path(_)
        | ast::Expr::Lit(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Invalid => {}
    }
    Some(())
}

/// Statement emitter folding a behavior into a TMDLState transition.
struct BehaviorEmitter<'a> {
    ctx: &'a SmtCtx<'a>,
    operands: &'a HashMap<String, Type>,
    numeric_params: &'a HashMap<String, i64>,
    register_index_map: &'a HashMap<(String, String), u32>,
    /// Exception payloads visible while a handler body is compiled.
    locals: std::cell::RefCell<HashMap<String, SmtVal>>,
    /// Uniquifies exception-payload `let` bindings across nested trys.
    let_counter: std::cell::Cell<usize>,
    /// Handler PC writes are trap entries, not architectural branches: they
    /// must not flip the instruction's `writes-pc` metadata.
    in_handler: std::cell::Cell<bool>,
    failed: std::cell::Cell<bool>,
    writes_pc: std::cell::Cell<bool>,
}

impl BehaviorEmitter<'_> {
    fn emit_val(&self, e: &ast::Expr) -> Option<SmtVal> {
        let mut graph = tir::sem_expr::ExprPostGraph::new();
        let lowering = e
            .lower_to_sema_with_registers(&mut graph, self.numeric_params, self.register_index_map)
            .or_else(|| {
                self.failed.set(true);
                None
            })?;
        // SMT-LIB has no iteration: unroll constant-bound loops to plain
        // expressions. Symbolic-bound loops survive and fail emission below.
        let (graph, root) = tir::sem_expr::unroll_loops(&graph, lowering.root);
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
        let locals = self.locals.borrow();
        let resolver = SmtSymbolResolver {
            symbols,
            operands: self.operands,
            locals: &locals,
            state_name: "st",
            ctx: self.ctx,
        };
        emit_sem_expr(&graph, root, &resolver).or_else(|| {
            self.failed.set(true);
            None
        })
    }
}

impl sem_expr_state::StateEmitter for BehaviorEmitter<'_> {
    fn cond(&self, e: &ast::Expr) -> String {
        self.emit_val(e)
            .map(|v| v.as_bool())
            .unwrap_or_else(|| "false".to_string())
    }

    fn assign(&self, a: &ast::Assign, st_name: &str) -> Option<String> {
        let ctx = self.ctx;
        let rhs = self.emit_val(&a.value)?;
        let (expr, width, signed) = rhs.as_bv();
        let fit = |target: u16| fit_smt(&expr, width, signed, target as u32);
        let write_pc = || {
            if !self.in_handler.get() {
                self.writes_pc.set(true);
            }
            format!("(write_pc {} {})", st_name, fit(ctx.xlen))
        };
        let dest_name = match &*a.dest {
            ast::Expr::Ident(id) => Some(id.name.as_str()),
            ast::Expr::Path(p) if p.remainder.len() == 1 => Some(p.remainder[0].as_str()),
            _ => None,
        };
        if dest_name == Some("pc") {
            return Some(write_pc());
        }
        if let Some(name) = dest_name {
            match self.operands.get(name) {
                Some(Type::Struct(rc)) if ctx.pc_classes.contains(&rc.to_lowercase()) => {
                    return Some(write_pc());
                }
                Some(Type::Struct(rc)) => {
                    return Some(format!(
                        "(write_{} {} {} {})",
                        rc.to_lowercase(),
                        st_name,
                        name.to_lowercase(),
                        fit(ctx.val_width(rc))
                    ));
                }
                _ => {}
            }
        }
        // Writes to a fixed register named by class path (`GPR::x30`,
        // `PSTATE::n`).
        if let ast::Expr::Path(p) = &*a.dest
            && p.remainder.len() == 1
            && let Some(idx) = self
                .register_index_map
                .get(&(p.base.clone(), p.remainder[0].clone()))
        {
            let class = p.base.to_lowercase();
            return Some(format!(
                "(write_{} {} (_ bv{} {}) {})",
                class,
                st_name,
                idx,
                ctx.idx_width(&class),
                fit(ctx.val_width(&class))
            ));
        }
        None
    }

    fn store(&self, c: &ast::Call, st_name: &str) -> Option<String> {
        let bytes = ast_int_lit(c.arguments.get(1)?)? as u16;
        if !MEM_ACCESS_BYTES.contains(&bytes) {
            return None;
        }
        let xlen = self.ctx.xlen as u32;
        let (addr, wa, sa) = self.emit_val(c.arguments.first()?)?.as_bv();
        let (val, wv, sv) = self.emit_val(c.arguments.get(2)?)?.as_bv();
        Some(format!(
            "(write_mem_{} {} {} {})",
            bytes,
            st_name,
            fit_smt(&addr, wa, sa, xlen),
            fit_smt(&val, wv, sv, bytes as u32 * 8)
        ))
    }

    fn trap(
        &self,
        c: &ast::Call,
        st_name: &str,
        compile: &dyn Fn(&ast::Expr, &str) -> String,
    ) -> Option<String> {
        let handler = self.ctx.trap_handler?;
        let xlen = self.ctx.xlen as u32;
        // Bind handler parameters to the call arguments; missing trailing
        // arguments (ecall has no tval) read as zero.
        let mut shadowed = Vec::new();
        for (i, param) in handler.params.iter().enumerate() {
            let value = match c.arguments.get(i) {
                Some(arg) => self.emit_val(arg)?,
                None => SmtVal::bv(format!("(_ bv0 {})", xlen), xlen, false),
            };
            shadowed.push((
                param.clone(),
                self.locals.borrow_mut().insert(param.clone(), value),
            ));
        }
        let state = compile(&handler.body, st_name);
        for (param, previous) in shadowed {
            let mut locals = self.locals.borrow_mut();
            match previous {
                Some(value) => locals.insert(param, value),
                None => locals.remove(&param),
            };
        }
        Some(state)
    }

    fn ite(&self, cond: &str, then_state: &str, else_state: &str) -> String {
        format!("(ite {} {} {})", cond, then_state, else_state)
    }

    fn try_except(
        &self,
        t: &ast::TryExcept,
        st_name: &str,
        body_state: &str,
        compile: &dyn Fn(&ast::Expr, &str) -> String,
    ) -> Option<String> {
        let mut ops = Vec::new();
        collect_mem_ops(&t.body, &mut ops)?;
        // Exception conditions are evaluated against the entry state, which
        // is only sound while at most one access can raise.
        if ops.len() > 1 {
            return None;
        }
        let op = ops.first();
        let xlen = self.ctx.xlen;
        let var = format!("exc_addr{}", self.let_counter.get());
        let mut arms: Vec<(String, String)> = Vec::new();
        for handler in &t.handlers {
            let wanted = match handler.kind.as_str() {
                "misaligned_load" => MemOpKind::Load,
                "misaligned_store" => MemOpKind::Store,
                // Unknown kinds are already a sema diagnostic.
                _ => return None,
            };
            let Some(op) = op.filter(|o| o.kind == wanted) else {
                continue;
            };
            // A byte access never misaligns: the clause is statically dead.
            if op.bytes <= 1 {
                continue;
            }
            if !op.bytes.is_power_of_two() {
                return None;
            }
            let cond = format!(
                "(distinct (bvand {var} (_ bv{} {xlen})) (_ bv0 {xlen}))",
                op.bytes - 1
            );
            if let Some(binding) = &handler.binding {
                self.locals
                    .borrow_mut()
                    .insert(binding.clone(), SmtVal::bv(var.clone(), xlen as u32, false));
            }
            let was_in_handler = self.in_handler.replace(true);
            let state = compile(&handler.body, st_name);
            self.in_handler.set(was_in_handler);
            if let Some(binding) = &handler.binding {
                self.locals.borrow_mut().remove(binding);
            }
            arms.push((cond, state));
        }
        if arms.is_empty() {
            return Some(body_state.to_string());
        }
        let op = op.expect("a live clause implies an access");
        let (addr, wa, sa) = self.emit_val(op.addr)?.as_bv();
        let addr = fit_smt(&addr, wa, sa, xlen as u32);
        self.let_counter.set(self.let_counter.get() + 1);
        let folded = arms
            .into_iter()
            .rev()
            .fold(body_state.to_string(), |else_state, (cond, state)| {
                format!("(ite {} {} {})", cond, state, else_state)
            });
        Some(format!("(let (({} {})) {})", var, addr, folded))
    }

    fn unsupported(&self, _: &ast::Expr) {
        self.failed.set(true);
    }
}

/// Translate an instruction behavior into an SMT state-transition expression.
/// Returns `None` when the behavior uses constructs the SMT model cannot
/// express (e.g. `trap()`); callers must not pretend such instructions have
/// identity semantics.
fn build_smt_behavior<'a>(
    ctx: &SmtCtx<'_>,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    instruction: &'a ast::Instruction,
    operands: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<(String, bool)> {
    let operands = operands.iter().cloned().collect::<HashMap<_, _>>();
    let mut numeric_params: HashMap<String, i64> =
        resolve_isa_param_values(instruction, item_cache);
    // The target ISA's own values win over the cross-ISA maximum (an
    // instruction shared by RV32I and RV64I must see XLEN=32 on RV32I).
    numeric_params.extend(ctx.isa_params.iter().map(|(k, v)| (k.clone(), *v)));
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

    let emitter = BehaviorEmitter {
        ctx,
        operands: &operands,
        numeric_params: &numeric_params,
        register_index_map,
        locals: Default::default(),
        let_counter: Default::default(),
        in_handler: Default::default(),
        failed: Default::default(),
        writes_pc: Default::default(),
    };
    let behavior = instruction.behavior.expand_loops(&numeric_params);
    let body = sem_expr_state::compile_to_state(&behavior, "st", &emitter);
    if emitter.failed.get() {
        None
    } else {
        Some((body, emitter.writes_pc.get()))
    }
}

// ---------------------------------------------------------------------------
// Decoder (instruction word → TMDLInstr)
// ---------------------------------------------------------------------------

fn build_decoder<'a>(
    dialect: &str,
    ctx: &SmtCtx<'_>,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    files: &'a [ast::File],
    output: &mut Box<dyn Write>,
) -> Result<(), TMDLError> {
    let instructions: Vec<&ast::Instruction> = files
        .iter()
        .flat_map(|f| f.instructions())
        .filter(|i| item_supports_isa(&i.for_isas, ctx.isa, item_cache))
        .collect();
    if instructions.is_empty() {
        return Ok(());
    }

    let mut arms: Vec<(String, String)> = vec![];

    for i in &instructions {
        let name_upper = i.name.to_uppercase();
        let operand_list = resolved_operands(ctx, i, item_cache);
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
                    Type::Struct(rc) => ctx.idx_width(rc),
                    _ => ctx.xlen,
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
    let first_ops = resolved_operands(ctx, first, item_cache);
    let fallback = {
        let zeros: Vec<String> = first_ops
            .iter()
            .map(|(_, ty)| {
                zero_bv(match ty {
                    Type::Struct(rc) => ctx.idx_width(rc),
                    _ => ctx.xlen,
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
