//! Building an [`Axiom`] from a PDL rule.
//!
//! PDL is the one rule language; this is where its AST meets the prover. The two
//! vocabularies stay distinct by syntax: `#name` is a semantic operator, which is
//! what an axiom is written in, and `dialect.op` is an op identity, which the
//! prover can only read through the op's `sem:` declaration.

use tir_pdl::{
    BinaryOp, BindingType, Expr, ExprKind, Operator, Proof, Term, TermKind, Type, UnaryOp, Width,
};

use super::{
    AxNode, Axiom, ConstWidth, Guard_, ProofObligation, ValueGuard, WidthBinding, WidthExpr,
    contains_kind, holes_of, intern, references,
};
use crate::sem::{SymKind, op_kind};

/// Every rule in a PDL source the prover reads: the `smt` and `trusted` ones.
/// `definitional` rules are laws of an algebra it has no model for, and are
/// skipped.
pub(crate) fn axioms_from_pdl(source: &str) -> Result<Vec<Axiom>, String> {
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
            tir_pdl::Item::Rule(rule) if rule.proof() != Proof::Definitional => Some(rule.as_ref()),
            _ => None,
        })
        .map(axiom_from_rule)
        .collect()
}

/// The state a rule's terms are read against: the width names in declaration
/// order and the typed binders they belong to.
struct Scope {
    width_names: Vec<String>,
    vars: Vec<(String, WidthBinding)>,
    const_vars: Vec<usize>,
}

impl Scope {
    /// Width names are interned in the order the left-hand side declares them,
    /// then the root's: that order is the proof memo key.
    fn new(rule: &tir_pdl::Rule) -> Result<Self, String> {
        let mut scope = Scope {
            width_names: Vec::new(),
            vars: Vec::new(),
            const_vars: Vec::new(),
        };
        scope.declare(&rule.lhs)?;
        Ok(scope)
    }

    fn declare(&mut self, term: &Term) -> Result<(), String> {
        match &term.kind {
            TermKind::Operation {
                operands,
                dependencies,
                ..
            } => {
                for operand in operands.iter().chain(dependencies) {
                    self.declare(operand)?;
                }
            }
            TermKind::Binder { name, ty: Some(ty) } => {
                let (width, is_const) = match ty {
                    BindingType::Type(Type::Integer(width)) => (width_binding(width, self)?, false),
                    BindingType::Constant(Some(width)) => (self.width_expr_binding(width)?, true),
                    BindingType::Constant(None) => {
                        return Err(format!("constant binder `{name}` needs a width"));
                    }
                    BindingType::Type(Type::Named(name)) => {
                        return Err(format!("type group `{name}` has no width"));
                    }
                };
                if is_const {
                    self.const_vars.push(self.vars.len());
                }
                self.vars.push((name.clone(), width));
            }
            _ => {}
        }
        if let Some(Type::Integer(width)) = &term.ty {
            width_binding(width, self)?;
        }
        Ok(())
    }

    fn width_expr_binding(&mut self, expr: &Expr) -> Result<WidthBinding, String> {
        match &expr.kind {
            ExprKind::Integer(value) => Ok(WidthBinding::Lit(*value as u64)),
            ExprKind::Name(name) => Ok(WidthBinding::Name(intern(&mut self.width_names, name))),
            _ => Err("a binder's width is a name or an integer".into()),
        }
    }

    fn var(&self, name: &str) -> Option<usize> {
        self.vars.iter().position(|(v, _)| v == name)
    }
}

fn width_binding(width: &Width, scope: &mut Scope) -> Result<WidthBinding, String> {
    match width {
        Width::Concrete(value) => Ok(WidthBinding::Lit(u64::from(*value))),
        Width::Named(name) => Ok(WidthBinding::Name(intern(&mut scope.width_names, name))),
        Width::Any => Err("`int<_>` has no width for a proof".into()),
    }
}

pub(crate) fn axiom_from_rule(rule: &tir_pdl::Rule) -> Result<Axiom, String> {
    let mut scope = Scope::new(rule)?;
    let root_width = match (&rule.lhs.ty, rule.materializes()) {
        (Some(Type::Integer(width)), _) => width_binding(width, &mut scope)?,
        // A materialize rule matches every constant class, so the constant's own
        // width is the root's.
        (None, true) => scope.vars[scope.const_vars[0]].1.clone(),
        _ => return Err(format!("rule `{}` has no root width", rule.name)),
    };

    let lhs = node(&rule.lhs, Side::Lhs, &scope)?;
    let rhs = node(&rule.rhs, Side::Rhs, &scope)?;

    let mut guards = Vec::new();
    let mut value_guards = Vec::new();
    for guard in &rule.guards {
        push_guard(guard, &scope, &mut guards, &mut value_guards)?;
    }

    let mut uses_root = false;
    let mut used_vars = std::collections::HashSet::new();
    references(&rhs, &mut uses_root, &mut used_vars);
    if uses_root && !used_vars.is_empty() {
        return Err("rhs may reference `root` or vars, not both".into());
    }
    let mut lhs_holes = Vec::new();
    holes_of(&lhs, &mut lhs_holes);
    if !used_vars.is_empty() {
        // The proof realizes the whole LHS, so every hole needs a known width.
        for (name, var) in &lhs_holes {
            if var.is_none() && !scope.width_names.contains(name) {
                return Err(format!("lhs atom `{name}` must be declared to be provable"));
            }
        }
    }
    let obligation = if contains_kind(&lhs, SymKind::Loop) || contains_kind(&rhs, SymKind::Loop) {
        if contains_kind(&lhs, SymKind::Theta) || contains_kind(&rhs, SymKind::Theta) {
            return Err("a rule mixes `#theta` and `#loop`".into());
        }
        ProofObligation::LoopInvariant {
            rhs_loops: contains_kind(&rhs, SymKind::Loop),
        }
    } else if contains_kind(&lhs, SymKind::Theta) || contains_kind(&rhs, SymKind::Theta) {
        ProofObligation::ThetaInvariant
    } else {
        ProofObligation::Equivalence
    };

    Ok(Axiom {
        name: rule.name.clone(),
        width_names: scope.width_names,
        vars: scope.vars,
        const_vars: scope.const_vars,
        root_width,
        guards,
        value_guards,
        lhs,
        rhs,
        uses_root,
        obligation,
        post_saturation: rule.post_saturation,
        materialize: rule.materializes(),
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Lhs,
    Rhs,
}

fn node(term: &Term, side: Side, scope: &Scope) -> Result<AxNode, String> {
    match &term.kind {
        TermKind::Root => Ok(AxNode::Root),
        TermKind::Keep(inner) => Ok(AxNode::Keep(Box::new(node(inner, side, scope)?))),
        TermKind::Binder { name, .. } => match (side, scope.var(name)) {
            (_, Some(index)) => Ok(AxNode::Hole(name.clone(), Some(index))),
            (Side::Lhs, None) => Ok(AxNode::Hole(name.clone(), None)),
            // A bare name on the right that is not a var is a width, which
            // materializes as the constant carrying it.
            (Side::Rhs, None) => Ok(AxNode::Const(
                width_expr(&name_expr(name), scope)?,
                ConstWidth::Register,
            )),
        },
        TermKind::Value(expr) => {
            let value = width_expr(expr, scope)?;
            Ok(match side {
                Side::Lhs => AxNode::ConstMatch(value),
                Side::Rhs => AxNode::Const(value, ConstWidth::Register),
            })
        }
        TermKind::Constant { width, value } => {
            let ExprKind::Integer(width) = width.kind else {
                return Err("a constant's width must be an integer".into());
            };
            let width = u32::try_from(width).map_err(|_| "constant width is out of range")?;
            Ok(AxNode::Const(
                width_expr(value, scope)?,
                ConstWidth::Fixed(width),
            ))
        }
        TermKind::Operation {
            operator,
            operands,
            dependencies,
            ..
        } => {
            let Operator::Semantic(name) = operator else {
                return Err(format!(
                    "op terms have no `sem:` expansion here; rule names `{}`",
                    operator_name(operator)
                ));
            };
            let kind =
                op_kind(name).ok_or_else(|| format!("unknown semantic operator `{name}`"))?;
            let children = operands
                .iter()
                .chain(dependencies)
                .map(|operand| node(operand, side, scope))
                .collect::<Result<_, _>>()?;
            Ok(AxNode::Node(kind, children))
        }
        TermKind::String(_) => Err("a string is not a term the prover reads".into()),
    }
}

fn operator_name(operator: &Operator) -> String {
    match operator {
        Operator::Dialect { dialect, name } => format!("{dialect}.{name}"),
        Operator::Semantic(name) => format!("#{name}"),
    }
}

fn name_expr(name: &str) -> Expr {
    Expr {
        kind: ExprKind::Name(name.to_string()),
        span: (0..0).into(),
    }
}

/// An integer expression over bound widths: names, literals, subtraction and
/// `ones(e)`.
fn width_expr(expr: &Expr, scope: &Scope) -> Result<WidthExpr, String> {
    match &expr.kind {
        ExprKind::Integer(value) => Ok(WidthExpr::Lit(*value as u64)),
        ExprKind::Name(name) => scope
            .width_names
            .iter()
            .position(|n| n == name)
            .map(WidthExpr::Name)
            .ok_or_else(|| format!("unknown width `{name}`")),
        ExprKind::Binary {
            op: BinaryOp::Subtract,
            lhs,
            rhs,
        } => Ok(WidthExpr::Sub(
            Box::new(width_expr(lhs, scope)?),
            Box::new(width_expr(rhs, scope)?),
        )),
        ExprKind::Call { name, args } if name == "ones" => match args.as_slice() {
            [arg] => Ok(WidthExpr::Ones(Box::new(width_expr(arg, scope)?))),
            _ => Err("`ones` takes one argument".into()),
        },
        _ => Err("width expressions are names, integers, `a - b`, or `ones(e)`".into()),
    }
}

fn push_guard(
    guard: &Expr,
    scope: &Scope,
    guards: &mut Vec<Guard_>,
    value_guards: &mut Vec<ValueGuard>,
) -> Result<(), String> {
    let (guard, negated) = match &guard.kind {
        ExprKind::Unary {
            op: UnaryOp::Not,
            value,
        } => (value.as_ref(), true),
        _ => (guard, false),
    };
    match &guard.kind {
        ExprKind::Call { name, args } if name == "fits" || name == "ufits" => {
            let [ExprKind::Name(var), ExprKind::Integer(bits)] = [&args[0].kind, &args[1].kind]
            else {
                return Err(format!("`{name}` takes a constant binder and a bit count"));
            };
            let var = scope
                .var(var)
                .ok_or_else(|| format!("`{name}` names an undeclared binder `{var}`"))?;
            let bits = u32::try_from(*bits).map_err(|_| "bit count is out of range")?;
            if !(1..=64).contains(&bits) {
                return Err("fits bit count must be in 1..=64".into());
            }
            value_guards.push(ValueGuard {
                var,
                bits,
                unsigned: name == "ufits",
                negated,
            });
            Ok(())
        }
        _ if negated => Err("only `fits` and `ufits` may be negated".into()),
        ExprKind::Binary { op, lhs, rhs } => {
            let (lhs, rhs) = (width_expr(lhs, scope)?, width_expr(rhs, scope)?);
            guards.push(match op {
                BinaryOp::Less => Guard_::Lt(lhs, rhs),
                BinaryOp::Equal => Guard_::Eq(lhs, rhs),
                _ => return Err("width guards are `a < b` and `a == b`".into()),
            });
            Ok(())
        }
        _ => Err("guards are `a < b`, `a == b`, `[u]fits(v, n)` or their negation".into()),
    }
}
