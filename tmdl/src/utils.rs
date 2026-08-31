use core::fmt;
use std::collections::hash_map::{
    IntoIter as HashMapIntoIter, Iter as HashMapIter, IterMut as HashMapIterMut,
};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

use crate::Type;
use crate::ast::{self, Instruction, Item};

#[derive(PartialEq, Clone)]
pub struct StableHashMap<K: Eq + Hash, V: PartialEq>(HashMap<K, V>);

impl<K: Eq + Hash, V: PartialEq> Default for StableHashMap<K, V> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<K: Eq + Hash, V: PartialEq> Deref for StableHashMap<K, V> {
    type Target = HashMap<K, V>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<K: Eq + Hash, V: PartialEq> DerefMut for StableHashMap<K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<K: Eq + Hash, V: PartialEq> fmt::Debug for StableHashMap<K, V>
where
    K: Ord + fmt::Debug,
    V: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort_by_key(|(k, _)| *k);
        f.debug_map().entries(entries).finish()
    }
}

impl<K: Eq + Hash, V: PartialEq> From<HashMap<K, V>> for StableHashMap<K, V> {
    fn from(val: HashMap<K, V>) -> Self {
        StableHashMap(val)
    }
}

impl<K: Eq + Hash, V: PartialEq> FromIterator<(K, V)> for StableHashMap<K, V>
where
    K: Eq + Hash,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(HashMap::from_iter(iter))
    }
}

impl<K: Eq + Hash, V: PartialEq> IntoIterator for StableHashMap<K, V> {
    type Item = (K, V);
    type IntoIter = HashMapIntoIter<K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, K: Eq + Hash, V: PartialEq> IntoIterator for &'a StableHashMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = HashMapIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a, K: Eq + Hash, V: PartialEq> IntoIterator for &'a mut StableHashMap<K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = HashMapIterMut<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

/// Evaluate a `bits<expr>` width expression by lowering it to a semantic
/// expression and constant-folding it under `params` (the ISA parameter
/// values). `None` when the expression does not converge to a constant —
/// e.g. it references an unknown parameter or a register.
pub fn eval_bits_width(expr: &ast::Expr, params: &HashMap<String, i64>) -> Option<u16> {
    let mut graph = tir_symbolic::sem::SemGraph::<()>::new();
    let lowering = expr.lower_to_sema(&mut graph, params, &HashMap::new())?;
    if !lowering.variable_symbols.is_empty() || !lowering.register_symbols.is_empty() {
        return None;
    }
    match tir_symbolic::lang::execute(&graph, &[]) {
        tir_symbolic::lang::Value::Int(v) => u16::try_from(v.to_u64()).ok(),
        tir_symbolic::lang::Value::Float(_)
        | tir_symbolic::lang::Value::Iterator(_)
        | tir_symbolic::lang::Value::RawBits(_) => None,
    }
}

/// Resolve `Type::BitsExpr` operand types to concrete `Type::Bits` widths
/// under `params`. Panics on non-constant widths: sema rejects those first.
pub fn resolve_operand_widths(
    operands: Vec<(String, Type)>,
    params: &HashMap<String, i64>,
) -> Vec<(String, Type)> {
    operands
        .into_iter()
        .map(|(name, ty)| match ty {
            Type::BitsExpr(expr) => {
                let width = eval_bits_width(&expr, params).unwrap_or_else(|| {
                    panic!("width of operand '{name}' does not evaluate to a constant")
                });
                (name, Type::Bits(width))
            }
            other => (name, other),
        })
        .collect()
}

pub fn resolve_operands_for_instruction<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, Type)> {
    resolve_template_chain(inst, item_cache)
        .into_iter()
        .flat_map(|t| t.operands.iter())
        .chain(inst.operands.iter())
        .map(|op| (op.name.clone(), op.ty.clone()))
        .collect()
}

/// The value constraints an operand declares, as its consumers need them: the
/// alignment defaults to 1 when the operand declares none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperandConstraint {
    pub align: u32,
    pub nonzero: bool,
}

impl Default for OperandConstraint {
    fn default() -> Self {
        Self {
            align: 1,
            nonzero: false,
        }
    }
}

/// The constraints of an instruction's operands, by name; a name the
/// instruction redeclares takes the instruction's constraints.
pub fn resolve_operand_constraints_for_instruction<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> HashMap<String, OperandConstraint> {
    resolve_template_chain(inst, item_cache)
        .into_iter()
        .flat_map(|t| t.operands.iter())
        .chain(inst.operands.iter())
        .map(|op| {
            (
                op.name.clone(),
                OperandConstraint {
                    align: op.align.unwrap_or(1),
                    nonzero: op.nonzero,
                },
            )
        })
        .collect()
}

pub fn resolve_template_chain<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<&'a ast::Template> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current_parent = inst.parent_template.as_deref();

    while let Some(parent_name) = current_parent {
        if !visited.insert(parent_name) {
            break;
        }
        match item_cache.get(parent_name).copied() {
            Some(ast::Item::Template(t)) => {
                chain.push(t);
                current_parent = t.parent_template.as_deref();
            }
            _ => break,
        }
    }

    chain.reverse();
    chain
}

pub fn resolve_effective_encoding_for_instruction<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<&'a ast::Expr> {
    // An `encoding { }` spells no bits, which is the same as declaring none:
    // the template's encoding, if any, still applies.
    let spelled = |encoding: &'a ast::Expr| match encoding {
        ast::Expr::Tuple(tuple) if tuple.elements.is_empty() => None,
        _ => Some(encoding),
    };
    inst.encoding.as_ref().and_then(spelled).or_else(|| {
        resolve_template_chain(inst, item_cache)
            .into_iter()
            .rev()
            .find_map(|t| t.encoding.as_ref().and_then(spelled))
    })
}

pub fn resolve_effective_asm_for_instruction<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<&'a ast::Expr> {
    inst.asm.as_ref().or_else(|| {
        resolve_template_chain(inst, item_cache)
            .into_iter()
            .rev()
            .find_map(|t| t.asm.as_ref())
    })
}

/// The scheduling-class membership in effect for `inst`: its own `schedule` block,
/// or the nearest one inherited from its template chain. Lets a family of
/// instructions share a class by declaring it once on their template.
pub fn resolve_effective_schedule_for_instruction<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<&'a ast::Schedule> {
    inst.schedule.as_ref().or_else(|| {
        resolve_template_chain(inst, item_cache)
            .into_iter()
            .rev()
            .find_map(|t| t.schedule.as_ref())
    })
}

/// One fixed bit map of an instruction word: the guard that selects it and the
/// bit ranges it spells (see [`crate::shapes`]). An unconditional encoding has
/// exactly one, with an always-true guard.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodingShape {
    pub guard: crate::shapes::Guard,
    pub arms: Vec<ast::EncodingArm>,
    pub width_bits: u16,
}

/// What an instruction's effective encoding expression resolves against. A
/// name the instruction redeclares takes the instruction's declaration, so the
/// context holds one entry per operand.
pub fn encoding_context<'a>(
    instruction: &'a Instruction,
    item_cache: &HashMap<&'a str, &'a Item>,
) -> crate::shapes::Context {
    let constraints = resolve_operand_constraints_for_instruction(instruction, item_cache);
    let isa_params = resolve_isa_param_values(instruction, item_cache);
    let mut operands: Vec<(String, Type, OperandConstraint)> = Vec::new();
    for (name, ty) in resolve_operands_for_instruction(instruction, item_cache) {
        let constraint = constraints.get(&name).copied().unwrap_or_default();
        match operands.iter_mut().find(|(held, _, _)| *held == name) {
            Some(held) => *held = (name, ty, constraint),
            None => operands.push((name, ty, constraint)),
        }
    }
    crate::shapes::Context {
        owner: instruction.name.clone(),
        operands,
        params: resolve_params_for_instruction(instruction, item_cache),
        classes: crate::encoding::register_classes_from_cache(item_cache),
        unit: crate::encoding::encoding_unit(&isa_params),
    }
}

/// The instruction's encoding shapes, each as instruction-word bit ranges laid
/// out in the encoding units its ISA declares (see [`crate::encoding`]).
pub fn get_encoding_shapes<'a>(
    instruction: &'a Instruction,
    item_cache: &HashMap<&'a str, &'a Item>,
) -> Vec<EncodingShape> {
    let Some(encoding) = resolve_effective_encoding_for_instruction(instruction, item_cache) else {
        return Vec::new();
    };
    let ctx = encoding_context(instruction, item_cache);
    let (shapes, _) = crate::shapes::expand(encoding, &ctx);
    shapes
        .into_iter()
        .filter_map(|shape| {
            let width_bits = shape.fields.iter().map(|field| field.width).sum();
            Some(EncodingShape {
                arms: crate::encoding::encoding_arms(&shape.fields, ctx.unit)?,
                guard: shape.guard,
                width_bits,
            })
        })
        .collect()
}

/// The bit ranges of an instruction's first encoding shape. The emitters lower
/// one fixed bit map per instruction; a guarded encoding has several, and the
/// encoder that picks between them by guard is not wired up yet.
pub fn first_encoding_shape_arms<'a>(
    instruction: &'a Instruction,
    item_cache: &HashMap<&'a str, &'a Item>,
) -> Vec<ast::EncodingArm> {
    get_encoding_shapes(instruction, item_cache)
        .into_iter()
        .next()
        .map(|shape| shape.arms)
        .unwrap_or_default()
}

/// The instruction's encoding size in bytes, widest shape first. With no
/// encoding (a text-only pseudo-ISA) there is no binary width; report 0 bytes
/// rather than the 32-bit default assumed for real ISAs.
pub fn encoding_width_bytes<'a>(
    instruction: &'a Instruction,
    item_cache: &HashMap<&'a str, &'a Item>,
) -> u64 {
    let widest = get_encoding_shapes(instruction, item_cache)
        .iter()
        .map(|shape| shape.width_bits)
        .max()
        .unwrap_or(0);
    u64::from(u32::from(widest).div_ceil(8))
}

pub fn resolve_params_for_instruction<'a>(
    inst: &'a ast::Instruction,
    cache: &HashMap<&'a str, &'a ast::Item>,
) -> HashMap<String, (Type, Option<ast::Expr>)> {
    resolve_template_chain(inst, cache)
        .into_iter()
        .flat_map(|t| t.params.iter())
        .map(|(name, value)| (name.clone(), value.clone()))
        .chain(
            inst.params
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        )
        .collect()
}

/// ISA parameters referenced via `self.PARAM` (e.g. `XLEN`). They are not
/// instruction/template params, so they survive lowering as unbound symbols.
/// Extension ISAs (e.g. `RVM`) inherit parameters from the base ISAs in their
/// `requires` closure. An instruction may span ISAs that define the same
/// parameter with different values (RV32I/RV64I `XLEN`); pick the widest so
/// 64-bit execution is correct.
pub fn resolve_isa_param_values<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> HashMap<String, i64> {
    let mut acc: HashMap<String, i64> = HashMap::new();
    let mut pending: Vec<&str> = inst.for_isas.iter().map(String::as_str).collect();
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(isa_name) = pending.pop() {
        if !visited.insert(isa_name) {
            continue;
        }
        let Some(ast::Item::Isa(isa)) = item_cache.get(isa_name) else {
            continue;
        };
        for (name, (_ty, value)) in isa.parameters.iter() {
            if let Some(ast::Expr::Lit(ast::Lit::Int(li))) = value {
                let v = parse_literal_value(li) as i64;
                acc.entry(name.clone())
                    .and_modify(|e| *e = (*e).max(v))
                    .or_insert(v);
            }
        }
        match &isa.requires {
            None => {}
            Some(ast::IsaRequirement::Single(parent)) => pending.push(parent),
            Some(ast::IsaRequirement::Any(parents)) | Some(ast::IsaRequirement::All(parents)) => {
                pending.extend(parents.iter().map(String::as_str));
            }
        }
    }
    acc
}

/// True when an item declared `for [for_isas]` is part of the `target` ISA:
/// either `target` is listed directly, or a listed extension ISA reaches
/// `target` through its `requires` closure (e.g. `RVM requires [RV32I | RV64I]`
/// makes RVM instructions part of both RV32I and RV64I targets).
pub fn item_supports_isa<'a>(
    for_isas: &[String],
    target: &str,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> bool {
    fn supports<'a>(
        isa_name: &str,
        target: &str,
        item_cache: &HashMap<&'a str, &'a ast::Item>,
        visiting: &mut HashSet<String>,
    ) -> bool {
        if isa_name == target {
            return true;
        }
        if !visiting.insert(isa_name.to_string()) {
            return false;
        }
        let result = match item_cache.get(isa_name) {
            Some(ast::Item::Isa(isa)) => match &isa.requires {
                None => false,
                Some(ast::IsaRequirement::Single(parent)) => {
                    supports(parent, target, item_cache, visiting)
                }
                Some(ast::IsaRequirement::Any(parents)) => parents
                    .iter()
                    .any(|parent| supports(parent, target, item_cache, visiting)),
                Some(ast::IsaRequirement::All(parents)) => parents
                    .iter()
                    .all(|parent| supports(parent, target, item_cache, visiting)),
            },
            _ => false,
        };
        visiting.remove(isa_name);
        result
    }

    for_isas
        .iter()
        .any(|isa| supports(isa, target, item_cache, &mut HashSet::new()))
}

/// Parameter values visible from `target`: its own parameters and those
/// inherited through its `requires` closure, nearest definition winning.
pub fn isa_param_values<'a>(
    target: &str,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> HashMap<String, i64> {
    let mut acc: HashMap<String, i64> = HashMap::new();
    let mut pending: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    pending.push_back(target);
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(isa_name) = pending.pop_front() {
        if !visited.insert(isa_name) {
            continue;
        }
        let Some(ast::Item::Isa(isa)) = item_cache.get(isa_name) else {
            continue;
        };
        for (name, (_ty, value)) in isa.parameters.iter() {
            if let Some(ast::Expr::Lit(ast::Lit::Int(li))) = value {
                acc.entry(name.clone())
                    .or_insert(parse_literal_value(li) as i64);
            }
        }
        match &isa.requires {
            None => {}
            Some(ast::IsaRequirement::Single(parent)) => pending.push_back(parent),
            Some(ast::IsaRequirement::Any(parents)) | Some(ast::IsaRequirement::All(parents)) => {
                pending.extend(parents.iter().map(String::as_str));
            }
        }
    }
    acc
}

pub fn parse_literal_value(lit: &ast::LitInt) -> u64 {
    let v = lit.value();
    if let Some(stripped) = v.strip_prefix("0b") {
        u64::from_str_radix(stripped, 2).unwrap_or(0)
    } else if let Some(stripped) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u64::from_str_radix(stripped, 16).unwrap_or(0)
    } else {
        v.parse::<u64>().unwrap_or(0)
    }
}

/// Whether a behavior invokes the `todo()` builtin anywhere: its semantics are
/// unmodeled, so it generates no selection rules and its `execute()` traps.
pub fn behavior_uses_todo(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::BuiltinFunction(ast::BuiltinFunction::Todo) => true,
        ast::Expr::Ident(_) | ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) => false,
        ast::Expr::Path(_) | ast::Expr::Tuple(_) | ast::Expr::Invalid => false,
        ast::Expr::Assign(a) => behavior_uses_todo(&a.dest) || behavior_uses_todo(&a.value),
        ast::Expr::Let(l) => behavior_uses_todo(&l.value),
        ast::Expr::Binary(b) => behavior_uses_todo(&b.lhs) || behavior_uses_todo(&b.rhs),
        ast::Expr::Unary(u) => behavior_uses_todo(&u.x),
        ast::Expr::Block(b) => b.stmts.iter().any(behavior_uses_todo),
        ast::Expr::Call(c) => {
            behavior_uses_todo(&c.callee) || c.arguments.iter().any(behavior_uses_todo)
        }
        ast::Expr::Field(f) => behavior_uses_todo(&f.base),
        ast::Expr::If(i) => {
            behavior_uses_todo(&i.cond)
                || behavior_uses_todo(&i.then)
                || i.else_.as_ref().is_some_and(|e| behavior_uses_todo(e))
        }
        ast::Expr::IndexAccess(i) => behavior_uses_todo(&i.base),
        ast::Expr::Slice(s) => behavior_uses_todo(&s.base),
        ast::Expr::Cast(c) => behavior_uses_todo(&c.x) || behavior_uses_todo(&c.width),
        ast::Expr::Try(t) => {
            behavior_uses_todo(&t.body) || t.handlers.iter().any(|h| behavior_uses_todo(&h.body))
        }
        ast::Expr::Lambda(l) => behavior_uses_todo(&l.body),
    }
}

/// The data memory an instruction's behavior touches, as `(reads, writes)`,
/// from the memory builtins it invokes anywhere in its body.
pub fn behavior_memory_effects(expr: &ast::Expr) -> (bool, bool) {
    let mut effects = (false, false);
    visit_exprs(expr, &mut |e| {
        let ast::Expr::Call(call) = e else { return };
        let ast::Expr::BuiltinFunction(builtin) = call.callee.as_ref() else {
            return;
        };
        match builtin {
            ast::BuiltinFunction::Load | ast::BuiltinFunction::LoadReserved => effects.0 = true,
            ast::BuiltinFunction::Store | ast::BuiltinFunction::StoreConditional => {
                effects.1 = true
            }
            ast::BuiltinFunction::AtomicRmw => effects = (true, true),
            _ => {}
        }
    });
    effects
}

/// Apply `f` to `expr` and every sub-expression of it, outermost first.
pub(crate) fn visit_exprs<'a>(expr: &'a ast::Expr, f: &mut dyn FnMut(&'a ast::Expr)) {
    f(expr);
    match expr {
        ast::Expr::Ident(_)
        | ast::Expr::Lit(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Path(_)
        | ast::Expr::Invalid => {}
        ast::Expr::Assign(a) => {
            visit_exprs(&a.dest, f);
            visit_exprs(&a.value, f);
        }
        ast::Expr::Let(l) => {
            if let Some(width) = &l.width {
                visit_exprs(width, f);
            }
            visit_exprs(&l.value, f);
        }
        ast::Expr::Binary(b) => {
            visit_exprs(&b.lhs, f);
            visit_exprs(&b.rhs, f);
        }
        ast::Expr::Unary(u) => visit_exprs(&u.x, f),
        ast::Expr::Block(b) => b.stmts.iter().for_each(|stmt| visit_exprs(stmt, f)),
        ast::Expr::Call(c) => {
            visit_exprs(&c.callee, f);
            c.arguments.iter().for_each(|arg| visit_exprs(arg, f));
        }
        ast::Expr::Field(field) => visit_exprs(&field.base, f),
        ast::Expr::If(i) => {
            visit_exprs(&i.cond, f);
            visit_exprs(&i.then, f);
            if let Some(els) = &i.else_ {
                visit_exprs(els, f);
            }
        }
        ast::Expr::IndexAccess(i) => visit_exprs(&i.base, f),
        ast::Expr::Slice(s) => visit_exprs(&s.base, f),
        ast::Expr::Cast(c) => {
            visit_exprs(&c.x, f);
            visit_exprs(&c.width, f);
        }
        ast::Expr::Try(t) => {
            visit_exprs(&t.body, f);
            t.handlers.iter().for_each(|h| visit_exprs(&h.body, f));
        }
        ast::Expr::Lambda(l) => visit_exprs(&l.body, f),
        ast::Expr::Tuple(t) => t.elements.iter().for_each(|e| visit_exprs(e, f)),
    }
}

/// Substitute every `let` binding into its uses and drop the `let` statements.
/// Selection patterns are matched against the expression a binding stands for,
/// so they must not see the name; execution keeps the bindings, which is where
/// the single-evaluation guarantee lives.
pub fn inline_let_bindings(expr: &ast::Expr) -> ast::Expr {
    fn inline(expr: &ast::Expr, bindings: &mut HashMap<String, ast::Expr>) -> ast::Expr {
        match expr {
            ast::Expr::Ident(id) => bindings
                .get(&id.name)
                .cloned()
                .unwrap_or_else(|| expr.clone()),
            ast::Expr::Block(b) => {
                let outer = bindings.clone();
                let stmts = b
                    .stmts
                    .iter()
                    .filter_map(|stmt| match stmt {
                        ast::Expr::Let(l) => {
                            let value = inline(&l.value, bindings);
                            bindings.insert(l.name.clone(), value);
                            None
                        }
                        other => Some(inline(other, bindings)),
                    })
                    .collect();
                *bindings = outer;
                ast::Expr::Block(ast::Block {
                    stmts,
                    last_expr_return: b.last_expr_return,
                    span: b.span,
                })
            }
            // A `let` outside a block binds nothing that follows it.
            ast::Expr::Let(l) => inline(&l.value, bindings),
            other => map_child_exprs(other, &mut |child| inline(child, bindings)),
        }
    }

    inline(expr, &mut HashMap::new())
}

/// Rebuild `expr` with `f` applied to each immediate child. Leaves are
/// returned unchanged.
pub(crate) fn map_child_exprs(
    expr: &ast::Expr,
    f: &mut dyn FnMut(&ast::Expr) -> ast::Expr,
) -> ast::Expr {
    match expr {
        ast::Expr::Assign(a) => ast::Expr::Assign(ast::Assign {
            dest: a.dest.clone(),
            value: Box::new(f(&a.value)),
            span: a.span,
        }),
        ast::Expr::Let(l) => ast::Expr::Let(ast::Let {
            name: l.name.clone(),
            width: l.width.as_ref().map(|w| Box::new(f(w))),
            value: Box::new(f(&l.value)),
            span: l.span,
        }),
        ast::Expr::Binary(b) => ast::Expr::Binary(ast::Binary {
            lhs: Box::new(f(&b.lhs)),
            rhs: Box::new(f(&b.rhs)),
            op: b.op.clone(),
            span: b.span,
        }),
        ast::Expr::Unary(u) => ast::Expr::Unary(ast::Unary {
            x: Box::new(f(&u.x)),
            op: u.op.clone(),
            span: u.span,
        }),
        ast::Expr::Block(b) => ast::Expr::Block(ast::Block {
            stmts: b.stmts.iter().map(&mut *f).collect(),
            last_expr_return: b.last_expr_return,
            span: b.span,
        }),
        ast::Expr::Call(c) => ast::Expr::Call(ast::Call {
            callee: Box::new(f(&c.callee)),
            arguments: c.arguments.iter().map(&mut *f).collect(),
            span: c.span,
        }),
        ast::Expr::Field(field) => ast::Expr::Field(ast::Field {
            base: Box::new(f(&field.base)),
            member: field.member.clone(),
            span: field.span,
        }),
        ast::Expr::If(i) => ast::Expr::If(ast::If {
            cond: Box::new(f(&i.cond)),
            then: Box::new(f(&i.then)),
            else_: i.else_.as_ref().map(|e| Box::new(f(e))),
            span: i.span,
        }),
        ast::Expr::IndexAccess(i) => ast::Expr::IndexAccess(ast::IndexAccess {
            base: Box::new(f(&i.base)),
            index: i.index,
            span: i.span,
        }),
        ast::Expr::Slice(s) => ast::Expr::Slice(ast::Slice {
            base: Box::new(f(&s.base)),
            hi: s.hi,
            lo: s.lo,
            span: s.span,
        }),
        ast::Expr::Cast(c) => ast::Expr::Cast(ast::Cast {
            x: Box::new(f(&c.x)),
            width: Box::new(f(&c.width)),
            span: c.span,
        }),
        ast::Expr::Try(t) => ast::Expr::Try(ast::TryExcept {
            body: Box::new(f(&t.body)),
            handlers: t
                .handlers
                .iter()
                .map(|h| ast::ExceptClause {
                    kind: h.kind.clone(),
                    binding: h.binding.clone(),
                    body: f(&h.body),
                    span: h.span,
                })
                .collect(),
            span: t.span,
        }),
        ast::Expr::Lambda(l) => ast::Expr::Lambda(ast::Lambda {
            params: l.params.clone(),
            body: Box::new(f(&l.body)),
            span: l.span,
        }),
        ast::Expr::Tuple(t) => ast::Expr::Tuple(ast::Tuple {
            elements: t.elements.iter().map(&mut *f).collect(),
            span: t.span,
        }),
        ast::Expr::Ident(_)
        | ast::Expr::Path(_)
        | ast::Expr::Lit(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Invalid => expr.clone(),
    }
}
