use std::collections::HashSet;

use tir_symbolic::lang::{SymKind, op_kind};

use crate::Diagnostic;
use crate::Span;
use crate::ast::*;

pub fn analyze(file: &File) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut groups = HashSet::new();
    let mut rules = HashSet::new();

    for item in &file.items {
        match item {
            Item::Group(group) if !groups.insert(group.name.as_str()) => {
                diagnostics.push(Diagnostic::new(
                    format!("duplicate type group '{}'", group.name),
                    "this group was already defined",
                    group.span,
                ))
            }
            Item::Rule(rule) if !rules.insert(rule.name.as_str()) => {
                diagnostics.push(Diagnostic::new(
                    format!("duplicate rule '{}'", rule.name),
                    "this rule was already defined",
                    rule.span,
                ))
            }
            _ => {}
        }
    }

    for item in &file.items {
        let Item::Rule(rule) = item else { continue };
        if matches!(rule.kind, RuleKind::Refinement) && rule.proof == Some(Proof::Trusted) {
            diagnostics.push(Diagnostic::new(
                format!("refinement rule '{}' cannot use trusted proof", rule.name),
                "refinement candidates require a checked proof",
                rule.span,
            ));
        }
        if matches!(rule.kind, RuleKind::Equality(_)) && rule.proof == Some(Proof::Contract) {
            diagnostics.push(Diagnostic::new(
                format!("equality rule '{}' cannot use contract proof", rule.name),
                "contract proofs only license directional refinements",
                rule.span,
            ));
        }
        let mut binders = HashSet::new();
        let mut widths = HashSet::new();
        collect_lhs_bindings(&rule.lhs, &mut binders, &mut widths, &mut diagnostics);
        validate_operators(&rule.lhs, &mut diagnostics);
        validate_operators(&rule.rhs, &mut diagnostics);
        validate_lhs_shape(rule, &mut diagnostics);
        validate_rhs(&rule.rhs, &binders, &widths, &mut diagnostics);
        for side in [&rule.lhs, &rule.rhs] {
            if let Some(span) = port_outside_loop(side, false) {
                diagnostics.push(Diagnostic::new(
                    format!("rule '{}' reads a port outside any loop", rule.name),
                    "a `#port` sits under the `#loop` whose value it reads",
                    span,
                ));
            }
        }
        if grows_theta_under_theta(&rule.rhs) {
            diagnostics.push(Diagnostic::new(
                format!("rule '{}' unrolls a loop under itself", rule.name),
                "a theta operand may not hold another theta on the right-hand side",
                rule.rhs.span,
            ));
        }
        for guard in &rule.guards {
            validate_expr(guard, &binders, &widths, &mut diagnostics);
        }
    }

    diagnostics
}

fn collect_lhs_bindings<'a>(
    term: &'a Term,
    binders: &mut HashSet<&'a str>,
    widths: &mut HashSet<&'a str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &term.kind {
        TermKind::Operation {
            attributes,
            operands,
            dependencies,
            ..
        } => {
            for attribute in attributes {
                if let AttributeValue::Binder(name) = &attribute.value {
                    binders.insert(name);
                }
            }
            for operand in operands.iter().chain(dependencies) {
                collect_lhs_bindings(operand, binders, widths, diagnostics);
            }
        }
        TermKind::Value(expr) => collect_width_names(expr, widths),
        TermKind::Binder { name, ty } => {
            let repeated = !binders.insert(name);
            if repeated && ty.is_some() {
                diagnostics.push(Diagnostic::new(
                    format!("type on repeated binder '{name}'"),
                    "only the first occurrence may declare the binder type",
                    term.span,
                ));
            }
            if let Some(BindingType::Type(Type::Integer(Width::Named(width)))) = ty {
                widths.insert(width);
            }
            if let Some(BindingType::Constant(Some(width))) = ty {
                collect_width_names(width, widths);
            }
        }
        _ => {}
    }
    if let Some(Type::Integer(Width::Named(width))) = &term.ty {
        widths.insert(width);
    }
}

fn collect_width_names<'a>(expr: &'a Expr, widths: &mut HashSet<&'a str>) {
    match &expr.kind {
        ExprKind::Name(name) => {
            widths.insert(name);
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                collect_width_names(arg, widths);
            }
        }
        ExprKind::Unary { value, .. } => collect_width_names(value, widths),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_width_names(lhs, widths);
            collect_width_names(rhs, widths);
        }
        ExprKind::Integer(_) => {}
    }
}

fn validate_rhs(
    term: &Term,
    binders: &HashSet<&str>,
    widths: &HashSet<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &term.kind {
        TermKind::Operation {
            attributes,
            operands,
            dependencies,
            ..
        } => {
            for attribute in attributes {
                if let AttributeValue::Binder(name) = &attribute.value
                    && !binders.contains(name.as_str())
                {
                    diagnostics.push(unbound(name, attribute.span));
                }
            }
            for operand in operands.iter().chain(dependencies) {
                validate_rhs(operand, binders, widths, diagnostics);
            }
        }
        TermKind::Binder { name, ty } => {
            // A bare name on the right is a binder reference, or a width the
            // left-hand side bound, which materializes as a constant.
            if !binders.contains(name.as_str()) && !widths.contains(name.as_str()) {
                diagnostics.push(unbound(name, term.span));
            }
            if ty.is_some() {
                diagnostics.push(Diagnostic::new(
                    "RHS binders cannot introduce types",
                    "remove this type annotation",
                    term.span,
                ));
            }
        }
        TermKind::Constant { width, value } => {
            validate_expr(width, binders, widths, diagnostics);
            validate_expr(value, binders, widths, diagnostics);
        }
        TermKind::Value(expr) => validate_expr(expr, binders, widths, diagnostics),
        TermKind::Root | TermKind::String(_) => {}
    }
}

/// Every `#name` names a semantic operator, at an operand count that operator
/// takes.
fn validate_operators(term: &Term, diagnostics: &mut Vec<Diagnostic>) {
    if let TermKind::Operation {
        operator,
        operands,
        dependencies,
        ..
    } = &term.kind
    {
        let arity = operands.len() + dependencies.len();
        if let Operator::Semantic(name) = operator {
            if !dependencies.is_empty() {
                diagnostics.push(Diagnostic::new(
                    format!("'#{name}' takes no dependency operands"),
                    "only an operation observes a dependency",
                    term.span,
                ));
            }
            match op_kind(name) {
                None => diagnostics.push(Diagnostic::new(
                    format!("unknown semantic operator '#{name}'"),
                    "this name is not a semantic operator",
                    term.span,
                )),
                Some(kind) if !kind.accepts_arity(arity) => {
                    diagnostics.push(Diagnostic::new(
                        format!("'#{name}' takes {} operands", kind.arity()),
                        format!("this term has {arity}"),
                        term.span,
                    ));
                }
                Some(SymKind::Port)
                    if !matches!(
                        operands.as_slice(),
                        [Term {
                            kind: TermKind::Binder { .. },
                            ..
                        }]
                    ) =>
                {
                    diagnostics.push(Diagnostic::new(
                        "'#port' names its loop by a binder",
                        "this operand is not a binder",
                        term.span,
                    ));
                }
                Some(_) => {}
            }
        }
        for operand in operands.iter().chain(dependencies) {
            validate_operators(operand, diagnostics);
        }
    }
}

/// A left-hand side is an operation. `root` is a right-hand-side form.
fn validate_lhs_shape(rule: &Rule, diagnostics: &mut Vec<Diagnostic>) {
    forbid_rhs_forms(&rule.lhs, diagnostics);
    if !matches!(rule.lhs.kind, TermKind::Operation { .. }) {
        diagnostics.push(Diagnostic::new(
            "left-hand side must be an operation",
            "a bare term matches every class",
            rule.lhs.span,
        ));
    }
}

/// The left-hand-side patterns of `file` no e-graph node can match: an
/// operator with an offset law never holds a constant operand where the law
/// applies, since the e-graph keeps `x + c` as the reference `x` at offset `c`
/// rather than as a node. A zero is held where no identity dissolves it, as in
/// `0 - x`. A prover reads the same rules as statements, where they are fine,
/// so this is a check for the consumers that match them.
pub fn dissolved_constants(file: &File) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for item in &file.items {
        if let Item::Rule(rule) = item {
            forbid_dissolved_constants(&rule.lhs, &mut diagnostics);
        }
    }
    diagnostics
}

fn forbid_dissolved_constants(term: &Term, diagnostics: &mut Vec<Diagnostic>) {
    let TermKind::Operation {
        operator,
        operands,
        dependencies,
        ..
    } = &term.kind
    else {
        return;
    };
    if let Operator::Semantic(name) = operator
        && let Some(kind) = op_kind(name)
    {
        for (index, operand) in operands.iter().enumerate() {
            if !operand.held_under(kind.offset_coefficient(index), kind.identity(index)) {
                diagnostics.push(Diagnostic::new(
                    "a constant operand of an operator with an offset law never matches",
                    "the constant is folded into the value the operator is; match the other operand",
                    operand.span,
                ));
            }
        }
    }
    for operand in operands.iter().chain(dependencies) {
        forbid_dissolved_constants(operand, diagnostics);
    }
}

fn forbid_rhs_forms(term: &Term, diagnostics: &mut Vec<Diagnostic>) {
    match &term.kind {
        TermKind::Root => diagnostics.push(Diagnostic::new(
            "`root` cannot appear on the left-hand side",
            "`root` names the class the rule matched",
            term.span,
        )),
        TermKind::Operation {
            operands,
            dependencies,
            ..
        } => {
            for operand in operands.iter().chain(dependencies) {
                forbid_rhs_forms(operand, diagnostics);
            }
        }
        _ => {}
    }
}

/// Whether a loop term (`#theta` or `#loop`) on the right-hand side has another
/// below it, which re-grows the shape it matched and never saturates.
fn grows_theta_under_theta(term: &Term) -> bool {
    match &term.kind {
        TermKind::Operation {
            operator, operands, ..
        } if is_theta(operator) => operands.iter().any(contains_theta),
        TermKind::Operation {
            operands,
            dependencies,
            ..
        } => operands
            .iter()
            .chain(dependencies)
            .any(grows_theta_under_theta),
        _ => false,
    }
}

fn contains_theta(term: &Term) -> bool {
    match &term.kind {
        TermKind::Operation {
            operator,
            operands,
            dependencies,
            ..
        } => is_theta(operator) || operands.iter().chain(dependencies).any(contains_theta),
        _ => false,
    }
}

fn is_theta(operator: &Operator) -> bool {
    matches!(
        operator,
        Operator::Semantic(name) if matches!(op_kind(name), Some(SymKind::Theta | SymKind::Loop))
    )
}

/// A `#port` reads the carried value of the `#loop` it sits in, so one outside
/// any `#loop` reads nothing.
fn port_outside_loop(term: &Term, inside: bool) -> Option<Span> {
    match &term.kind {
        TermKind::Operation {
            operator,
            operands,
            dependencies,
            ..
        } => {
            let is_loop = matches!(operator, Operator::Semantic(name) if op_kind(name) == Some(SymKind::Loop));
            let is_port = matches!(operator, Operator::Semantic(name) if op_kind(name) == Some(SymKind::Port));
            if is_port && !inside {
                return Some(term.span);
            }
            operands
                .iter()
                .chain(dependencies)
                .find_map(|operand| port_outside_loop(operand, inside || is_loop))
        }
        _ => None,
    }
}

fn validate_expr(
    expr: &Expr,
    binders: &HashSet<&str>,
    widths: &HashSet<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Name(name)
            if !binders.contains(name.as_str()) && !widths.contains(name.as_str()) =>
        {
            diagnostics.push(unbound(name, expr.span));
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                validate_expr(arg, binders, widths, diagnostics);
            }
        }
        ExprKind::Unary { value, .. } => validate_expr(value, binders, widths, diagnostics),
        ExprKind::Binary { lhs, rhs, .. } => {
            validate_expr(lhs, binders, widths, diagnostics);
            validate_expr(rhs, binders, widths, diagnostics);
        }
        ExprKind::Integer(_) | ExprKind::Name(_) => {}
    }
}

fn unbound(name: &str, span: crate::Span) -> Diagnostic {
    Diagnostic::new(
        format!("unbound name '{name}'"),
        "this name is not bound by the left-hand side",
        span,
    )
}
