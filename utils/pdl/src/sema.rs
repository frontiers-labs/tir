use std::collections::HashSet;

use crate::Diagnostic;
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
        let mut binders = HashSet::new();
        let mut widths = HashSet::new();
        collect_lhs_bindings(&rule.lhs, &mut binders, &mut widths, &mut diagnostics);
        validate_rhs(&rule.rhs, &binders, &widths, &mut diagnostics);
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
            ..
        } => {
            for attribute in attributes {
                if let AttributeValue::Binder(name) = &attribute.value {
                    binders.insert(name);
                }
            }
            for operand in operands {
                collect_lhs_bindings(operand, binders, widths, diagnostics);
            }
        }
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
            ..
        } => {
            for attribute in attributes {
                if let AttributeValue::Binder(name) = &attribute.value
                    && !binders.contains(name.as_str())
                {
                    diagnostics.push(unbound(name, attribute.span));
                }
            }
            for operand in operands {
                validate_rhs(operand, binders, widths, diagnostics);
            }
        }
        TermKind::Binder { name, ty } => {
            if !binders.contains(name.as_str()) {
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
        TermKind::Integer(_) | TermKind::String(_) => {}
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
