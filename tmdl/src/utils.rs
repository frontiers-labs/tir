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
    let mut graph = tir::sem_expr::ExprPostGraph::new();
    let lowering = expr.lower_to_sema(&mut graph, params)?;
    if !lowering.variable_symbols.is_empty() || !lowering.register_symbols.is_empty() {
        return None;
    }
    match tir::sem_expr::execute(&graph, &[]) {
        tir::sem_expr::Value::Int(v) => u16::try_from(v.to_u64()).ok(),
        tir::sem_expr::Value::Float(_) | tir::sem_expr::Value::Vector(_) => None,
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
        .map(|(name, ty)| (name.clone(), ty.clone()))
        .chain(
            inst.operands
                .iter()
                .map(|(name, ty)| (name.clone(), ty.clone())),
        )
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
) -> &'a [ast::EncodingArm] {
    if !inst.encoding.is_empty() {
        return &inst.encoding;
    }
    resolve_template_chain(inst, item_cache)
        .into_iter()
        .rev()
        .find(|t| !t.encoding.is_empty())
        .map(|t| t.encoding.as_slice())
        .unwrap_or(&[])
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

pub fn get_encoding_arms<'a>(
    instruction: &'a Instruction,
    item_cache: &HashMap<&'a str, &'a Item>,
) -> Vec<ast::EncodingArm> {
    resolve_effective_encoding_for_instruction(instruction, item_cache).to_vec()
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
    let mut pending: Vec<&str> = for_isas.iter().map(String::as_str).collect();
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(isa_name) = pending.pop() {
        if isa_name == target {
            return true;
        }
        if !visited.insert(isa_name) {
            continue;
        }
        let Some(ast::Item::Isa(isa)) = item_cache.get(isa_name) else {
            continue;
        };
        match &isa.requires {
            None => {}
            Some(ast::IsaRequirement::Single(parent)) => pending.push(parent),
            Some(ast::IsaRequirement::Any(parents)) | Some(ast::IsaRequirement::All(parents)) => {
                pending.extend(parents.iter().map(String::as_str));
            }
        }
    }
    false
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
