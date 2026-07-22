use std::collections::BTreeMap;

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

use crate::ast::*;
use crate::{Diagnostic, Span};

struct RustOperation {
    path: TokenStream,
    emitter: Option<Ident>,
}

fn rust_operation(operator: &Operator) -> Option<RustOperation> {
    let Operator::Dialect { dialect, name } = operator else {
        return None;
    };
    match (dialect.as_str(), name.as_str()) {
        ("builtin", "addi") => Some(RustOperation {
            path: quote! { crate::builtin::AddIOp },
            emitter: None,
        }),
        ("builtin", "muli") => Some(RustOperation {
            path: quote! { crate::builtin::MulIOp },
            emitter: None,
        }),
        ("builtin", "subi") => Some(RustOperation {
            path: quote! { crate::builtin::SubIOp },
            emitter: None,
        }),
        ("builtin", "shli") => Some(RustOperation {
            path: quote! { crate::builtin::ShlIOp },
            emitter: Some(format_ident!("emit_shl")),
        }),
        _ => None,
    }
}

pub fn generate(file: &File) -> Result<String, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    for item in &file.items {
        if let Item::Rule(rule) = item {
            validate_codegen_rule(rule, &mut diagnostics);
        }
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let mut initializers = Vec::new();
    let mut generated_rules = Vec::new();
    for (rule_index, rule) in file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Rule(rule) => Some(rule.as_ref()),
            Item::Group(_) => None,
        })
        .enumerate()
    {
        let function = function_name(rule_index);
        let emit = root_operator(&rule.rhs)
            .and_then(rust_operation)
            .and_then(|operation| operation.emitter)
            .map_or_else(|| quote! { None }, |emitter| quote! { Some(#emitter()) });
        initializers.push(quote! {
            let index = ruleset.rewrites.len();
            ruleset.push(#function(context.clone(), index), #emit);
        });
        let Some(generated_rule) = generate_rule(rule, function) else {
            diagnostics.push(Diagnostic::new(
                format!("failed to lower rule '{}'", rule.name),
                "this rule uses a construct that cannot be lowered safely",
                rule.span,
            ));
            continue;
        };
        generated_rules.push(generated_rule);
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    format_rust(quote! {
        pub(super) fn generated_ruleset(context: &Context) -> Ruleset {
            let mut ruleset = Ruleset::new();
            #(#initializers)*
            ruleset
        }

        #(#generated_rules)*
    })
    .map_err(|error| {
        vec![Diagnostic::new(
            "failed to format generated Rust",
            error.to_string(),
            Span::from(0..0),
        )]
    })
}

fn format_rust(tokens: TokenStream) -> Result<String, syn::Error> {
    syn::parse2(tokens).map(|file| prettyplease::unparse(&file))
}

fn validate_codegen_rule(rule: &Rule, diagnostics: &mut Vec<Diagnostic>) {
    if rule.direction == Direction::Bidirectional {
        diagnostics.push(Diagnostic::new(
            "bidirectional Rust code generation is not implemented",
            "use a forward rule in the initial compiler",
            rule.span,
        ));
    }
    let mut binders = BTreeMap::new();
    collect_binder_types(&rule.lhs, &mut binders);
    validate_lhs(&rule.lhs, diagnostics);
    validate_rhs(&rule.rhs, true, &binders, diagnostics);
    for guard in &rule.guards {
        validate_codegen_expr(guard, &binders, diagnostics);
    }
    if contains_nested_rhs_operation(&rule.rhs, true) {
        diagnostics.push(Diagnostic::new(
            "nested RHS operation emission is not implemented",
            "materialize only one operation per rule",
            rule.rhs.span,
        ));
    }
}

fn collect_binder_types<'a>(
    term: &'a Term,
    binders: &mut BTreeMap<&'a str, Option<&'a BindingType>>,
) {
    match &term.kind {
        TermKind::Operation { operands, .. } => {
            for operand in operands {
                collect_binder_types(operand, binders);
            }
        }
        TermKind::Binder { name, ty } => {
            binders.entry(name).or_insert(ty.as_ref());
        }
        _ => {}
    }
}

fn validate_lhs(term: &Term, diagnostics: &mut Vec<Diagnostic>) {
    if let TermKind::Operation {
        operator,
        attributes,
        operands,
        ..
    } = &term.kind
    {
        if matches!(operator, Operator::Gate(name) if name != "gamma") {
            diagnostics.push(unsupported("gate patterns other than #gamma", term.span));
        }
        if let Operator::Dialect { dialect, name } = operator
            && rust_operation(operator).is_none()
        {
            diagnostics.push(Diagnostic::new(
                format!("unknown operation '{dialect}.{name}'"),
                "this operation has no typed Rust lowering",
                term.span,
            ));
        }
        if !attributes.is_empty() {
            diagnostics.push(Diagnostic::new(
                "attribute code generation is not implemented",
                "remove attributes from this initial ruleset",
                term.span,
            ));
        }
        for operand in operands {
            validate_lhs(operand, diagnostics);
        }
    }
    match &term.kind {
        TermKind::Binder {
            ty: Some(BindingType::Type(Type::Named(name))),
            ..
        } => diagnostics.push(Diagnostic::new(
            format!("type group '{name}' is not supported by Rust code generation"),
            "use an integer type in the initial compiler",
            term.span,
        )),
        TermKind::Binder {
            ty: Some(BindingType::Constant(Some(width))),
            ..
        } => validate_constant_width(width, diagnostics),
        TermKind::Constant { .. } | TermKind::String(_) => {
            diagnostics.push(unsupported("this left-hand-side term", term.span))
        }
        _ => {}
    }
    if term.ty.is_some() {
        diagnostics.push(unsupported("operation result type constraints", term.span));
    }
}

fn validate_rhs(
    term: &Term,
    root: bool,
    binders: &BTreeMap<&str, Option<&BindingType>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &term.kind {
        TermKind::Operation {
            operator,
            attributes,
            operands,
        } => {
            if !root {
                return;
            }
            if matches!(operator, Operator::Gate(_)) {
                diagnostics.push(unsupported("gate emission", term.span));
            }
            if let Operator::Dialect { dialect, name } = operator {
                match rust_operation(operator) {
                    None => diagnostics.push(Diagnostic::new(
                        format!("unknown operation '{dialect}.{name}'"),
                        "this operation has no typed Rust lowering",
                        term.span,
                    )),
                    Some(operation) if operation.emitter.is_none() => {
                        diagnostics.push(Diagnostic::new(
                            format!("cannot emit operation '{dialect}.{name}'"),
                            "this operation has no typed Rust emitter",
                            term.span,
                        ))
                    }
                    Some(_) => {}
                }
            }
            if !attributes.is_empty() {
                diagnostics.push(unsupported("attribute emission", term.span));
            }
            for operand in operands {
                validate_rhs(operand, false, binders, diagnostics);
            }
        }
        TermKind::Constant { width, value } => {
            validate_constant_width(width, diagnostics);
            validate_number_expr(width, binders, diagnostics);
            validate_number_expr(value, binders, diagnostics);
        }
        TermKind::Binder { .. } => {}
        TermKind::Integer(_) | TermKind::String(_) => {
            diagnostics.push(unsupported("this right-hand-side term", term.span));
        }
    }
    if term.ty.is_some() {
        diagnostics.push(unsupported("operation result types on the RHS", term.span));
    }
}

fn validate_constant_width(expr: &Expr, diagnostics: &mut Vec<Diagnostic>) {
    if let ExprKind::Integer(width) = expr.kind
        && !(1..=64).contains(&width)
    {
        diagnostics.push(Diagnostic::new(
            "constant width must be between 1 and 64",
            "use a width supported by APInt",
            expr.span,
        ));
    }
}

fn validate_codegen_expr(
    expr: &Expr,
    binders: &BTreeMap<&str, Option<&BindingType>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Name(name) => {
            if let Some(ty) = binders.get(name.as_str())
                && !matches!(ty, Some(BindingType::Constant(_)))
            {
                diagnostics.push(Diagnostic::new(
                    format!("binder '{name}' is not a constant"),
                    "only constant binders can be used in expressions",
                    expr.span,
                ));
            }
        }
        ExprKind::Call { name, args } => {
            let supported = matches!(name.as_str(), "popcount" | "ctz" | "clz");
            if !supported || args.len() != 1 {
                diagnostics.push(Diagnostic::new(
                    format!("unsupported expression function '{name}'"),
                    "the initial compiler supports one-argument popcount, ctz, and clz",
                    expr.span,
                ));
            } else if let Some(argument) = args.first()
                && !is_constant_binder(argument, binders)
            {
                diagnostics.push(Diagnostic::new(
                    "bit-count function requires a constant binder",
                    "pass a constant binder directly so its bit width is preserved",
                    argument.span,
                ));
            }
            for arg in args {
                validate_codegen_expr(arg, binders, diagnostics);
            }
        }
        ExprKind::Unary { value, .. } => validate_codegen_expr(value, binders, diagnostics),
        ExprKind::Binary { lhs, rhs, .. } => {
            validate_codegen_expr(lhs, binders, diagnostics);
            validate_codegen_expr(rhs, binders, diagnostics);
        }
        ExprKind::Integer(_) => {}
    }
}

fn validate_number_expr(
    expr: &Expr,
    binders: &BTreeMap<&str, Option<&BindingType>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Unary {
            op: UnaryOp::Not, ..
        }
        | ExprKind::Binary {
            op:
                BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::LessEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterEqual
                | BinaryOp::LogicalAnd
                | BinaryOp::LogicalOr,
            ..
        } => diagnostics.push(Diagnostic::new(
            "boolean expression cannot be used as a number",
            "use an arithmetic or bitwise expression",
            expr.span,
        )),
        ExprKind::Unary {
            op: UnaryOp::Negate,
            value,
        } => validate_number_expr(value, binders, diagnostics),
        ExprKind::Binary { lhs, rhs, .. } => {
            validate_number_expr(lhs, binders, diagnostics);
            validate_number_expr(rhs, binders, diagnostics);
        }
        ExprKind::Integer(_) | ExprKind::Name(_) | ExprKind::Call { .. } => {
            validate_codegen_expr(expr, binders, diagnostics);
        }
    }
}

fn is_constant_binder(expr: &Expr, binders: &BTreeMap<&str, Option<&BindingType>>) -> bool {
    let ExprKind::Name(name) = &expr.kind else {
        return false;
    };
    matches!(
        binders.get(name.as_str()).copied().flatten(),
        Some(BindingType::Constant(_))
    )
}

fn unsupported(feature: &str, span: Span) -> Diagnostic {
    Diagnostic::new(
        format!("{feature} is not supported by Rust code generation"),
        "this construct is outside the initial compiler",
        span,
    )
}

fn contains_nested_rhs_operation(term: &Term, root: bool) -> bool {
    let TermKind::Operation { operands, .. } = &term.kind else {
        return false;
    };
    (!root)
        || operands
            .iter()
            .any(|operand| contains_nested_rhs_operation(operand, false))
}

fn root_operator(term: &Term) -> Option<&Operator> {
    match &term.kind {
        TermKind::Operation { operator, .. } => Some(operator),
        _ => None,
    }
}

fn generate_rule(rule: &Rule, function: Ident) -> Option<TokenStream> {
    let rule_name = &rule.name;
    let mut generator = PatternGenerator::default();
    let lhs_root = generator.term(&rule.lhs)?;
    let lhs_statements = &generator.statements;

    let binder_declarations: Vec<_> = generator
        .binders
        .values()
        .map(|index| {
            let binding = binding_ident(*index);
            quote! {
                let #binding = operand(subst, #index);
                let _ = #binding;
            }
        })
        .collect();
    let literal_checks: Vec<_> = generator
        .literals
        .iter()
        .map(|literal| {
            let name = &literal.name;
            let index = literal.index;
            let value = literal.value;
            quote! {
                let #name = operand(subst, #index);
                if !class_is_literal(eg, #name, #value) {
                    return;
                }
            }
        })
        .collect();
    let width_declaration = generator
        .constraints
        .iter()
        .any(constraint_binds_width)
        .then(|| quote! { let mut widths = std::collections::HashMap::new(); });
    let constraints: Vec<_> = generator
        .constraints
        .iter()
        .map(generate_constraint)
        .collect::<Option<_>>()?;
    let guards: Vec<_> = rule
        .guards
        .iter()
        .map(|guard| {
            let guard = bool_expr(guard, &generator.binders)?;
            Some(quote! {
                if !(#guard) {
                    return;
                }
            })
        })
        .collect::<Option<_>>()?;
    let rhs = generate_rhs(&rule.rhs, &generator.binders)?;

    Some(quote! {
        fn #function(context: Context, index: usize) -> Rule {
            let mut lhs = Pattern::new();
            #(#lhs_statements)*
            let _ = #lhs_root;
            Rewrite::new(
                #rule_name,
                lhs,
                Rhs::Apply(Box::new(move |eg, subst, root| {
                    let _ = (&context, index);
                    #(#binder_declarations)*
                    #(#literal_checks)*
                    #width_declaration
                    #(#constraints)*
                    #(#guards)*
                    #rhs
                    eg.union(root, replacement);
                })),
            )
        }
    })
}

#[derive(Default)]
struct PatternGenerator {
    binders: BTreeMap<String, u32>,
    constraints: Vec<Constraint>,
    literals: Vec<Literal>,
    statements: Vec<TokenStream>,
    next_symbol: u32,
    next_node: usize,
}

struct Constraint {
    binder: u32,
    ty: BindingType,
}

struct Literal {
    name: Ident,
    index: u32,
    value: i64,
}

impl PatternGenerator {
    fn term(&mut self, term: &Term) -> Option<Ident> {
        match &term.kind {
            TermKind::Binder { name, ty } => {
                let index = if let Some(index) = self.binders.get(name) {
                    *index
                } else {
                    let index = self.symbol();
                    self.binders.insert(name.clone(), index);
                    if let Some(ty) = ty {
                        self.constraints.push(Constraint {
                            binder: index,
                            ty: ty.clone(),
                        });
                    }
                    index
                };
                Some(self.pattern_var(index))
            }
            TermKind::Integer(value) => {
                let index = self.symbol();
                let name = format_ident!("literal_{index}");
                self.literals.push(Literal {
                    name,
                    index,
                    value: *value,
                });
                Some(self.pattern_var(index))
            }
            TermKind::Operation {
                operator, operands, ..
            } => {
                let operands: Vec<_> = operands
                    .iter()
                    .map(|operand| self.term(operand))
                    .collect::<Option<_>>()?;
                let name = self.node();
                let constructor = match operator {
                    Operator::Dialect { .. } => {
                        let operation = rust_operation(operator)?;
                        let path = operation.path;
                        quote! { Node::pattern::<#path>(vec![#(#operands),*]) }
                    }
                    Operator::Gate(_) => quote! { Node::gamma_pattern(vec![#(#operands),*]) },
                };
                self.statements.push(quote! {
                    let #name = lhs.add(#constructor);
                });
                Some(name)
            }
            TermKind::Constant { .. } | TermKind::String(_) => None,
        }
    }

    fn pattern_var(&mut self, index: u32) -> Ident {
        let name = self.node();
        self.statements.push(quote! {
            let #name = lhs.var(Var::Symbol(#index));
        });
        name
    }

    fn symbol(&mut self) -> u32 {
        let symbol = self.next_symbol;
        self.next_symbol += 1;
        symbol
    }

    fn node(&mut self) -> Ident {
        let node = format_ident!("pattern_{}", self.next_node);
        self.next_node += 1;
        node
    }
}

fn generate_constraint(constraint: &Constraint) -> Option<TokenStream> {
    let binding = binding_ident(constraint.binder);
    match &constraint.ty {
        BindingType::Constant(width) => {
            let value = binding_value_ident(constraint.binder);
            let width_check = match width.as_ref().map(|width| &width.kind) {
                None => quote! {},
                Some(ExprKind::Name(name)) if name == "_" => quote! {},
                Some(ExprKind::Integer(width)) => {
                    let width = u32::try_from(*width).ok()?;
                    quote! {
                        if #value.width() != #width {
                            return;
                        }
                    }
                }
                Some(ExprKind::Name(width)) => quote! {
                    if !bind_width(&mut widths, #width, #value.width()) {
                        return;
                    }
                },
                Some(_) => return None,
            };
            Some(quote! {
                let Some(#value) = const_value(eg, #binding) else { return; };
                #width_check
            })
        }
        BindingType::Type(Type::Integer(Width::Named(width))) => {
            let value = format_ident!("binding_{}_width", constraint.binder);
            Some(quote! {
                let Some(#value) = class_int_width(&context, eg, #binding) else { return; };
                if !bind_width(&mut widths, #width, #value) {
                    return;
                }
            })
        }
        BindingType::Type(Type::Integer(Width::Concrete(width))) => Some(quote! {
            if class_int_width(&context, eg, #binding) != Some(#width) {
                return;
            }
        }),
        BindingType::Type(Type::Integer(Width::Any)) => Some(quote! {
            if class_int_width(&context, eg, #binding).is_none() {
                return;
            }
        }),
        BindingType::Type(Type::Named(_)) => None,
    }
}

fn constraint_binds_width(constraint: &Constraint) -> bool {
    match &constraint.ty {
        BindingType::Type(Type::Integer(Width::Named(_))) => true,
        BindingType::Constant(Some(width)) => {
            matches!(&width.kind, ExprKind::Name(name) if name != "_")
        }
        _ => false,
    }
}

fn generate_rhs(term: &Term, binders: &BTreeMap<String, u32>) -> Option<TokenStream> {
    match &term.kind {
        TermKind::Binder { name, .. } => {
            let binding = binding_ident(*binders.get(name)?);
            Some(quote! { let replacement = #binding; })
        }
        TermKind::Constant { width, value } => {
            let width = number_expr(width, binders)?;
            let value = number_expr(value, binders)?;
            Some(quote! {
                let replacement_width = (#width) as u32;
                if !(1..=64).contains(&replacement_width) {
                    return;
                }
                let replacement = eg.add(konst(APInt::new(replacement_width, (#value) as u64)));
            })
        }
        TermKind::Operation {
            operator, operands, ..
        } => {
            let operation = rust_operation(operator)?;
            let path = operation.path;
            let mut prelude = Vec::new();
            let operands: Vec<_> = operands
                .iter()
                .map(|operand| rhs_operand(operand, binders, &mut prelude))
                .collect::<Option<_>>()?;
            Some(quote! {
                #(#prelude)*
                let Some(result_type) = class_type(eg, root) else { return; };
                let replacement = eg.add(Node::introduced::<#path>(
                    result_type,
                    1,
                    index,
                    vec![#(#operands),*],
                ));
            })
        }
        TermKind::Integer(_) | TermKind::String(_) => None,
    }
}

fn rhs_operand(
    term: &Term,
    binders: &BTreeMap<String, u32>,
    prelude: &mut Vec<TokenStream>,
) -> Option<TokenStream> {
    match &term.kind {
        TermKind::Binder { name, .. } => {
            let binding = binding_ident(*binders.get(name)?);
            Some(quote! { #binding })
        }
        TermKind::Constant { width, value } => {
            let name = format_ident!("rhs_constant_{}", term.span.start);
            let width_name = format_ident!("rhs_constant_{}_width", term.span.start);
            let width = number_expr(width, binders)?;
            let value = number_expr(value, binders)?;
            prelude.push(quote! {
                let #width_name = (#width) as u32;
                if !(1..=64).contains(&#width_name) {
                    return;
                }
                let #name = eg.add(konst(APInt::new(#width_name, (#value) as u64)));
            });
            Some(quote! { #name })
        }
        TermKind::Operation { .. } | TermKind::Integer(_) | TermKind::String(_) => None,
    }
}

fn bool_expr(expr: &Expr, binders: &BTreeMap<String, u32>) -> Option<TokenStream> {
    match &expr.kind {
        ExprKind::Unary {
            op: UnaryOp::Not,
            value,
        } => {
            let value = bool_expr(value, binders)?;
            Some(quote! { !(#value) })
        }
        ExprKind::Binary { op, lhs, rhs } => match op {
            BinaryOp::Equal => comparison(quote! { == }, lhs, rhs, binders),
            BinaryOp::NotEqual => comparison(quote! { != }, lhs, rhs, binders),
            BinaryOp::Less => comparison(quote! { < }, lhs, rhs, binders),
            BinaryOp::LessEqual => comparison(quote! { <= }, lhs, rhs, binders),
            BinaryOp::Greater => comparison(quote! { > }, lhs, rhs, binders),
            BinaryOp::GreaterEqual => comparison(quote! { >= }, lhs, rhs, binders),
            BinaryOp::LogicalAnd => {
                let lhs = bool_expr(lhs, binders)?;
                let rhs = bool_expr(rhs, binders)?;
                Some(quote! { (#lhs) && (#rhs) })
            }
            BinaryOp::LogicalOr => {
                let lhs = bool_expr(lhs, binders)?;
                let rhs = bool_expr(rhs, binders)?;
                Some(quote! { (#lhs) || (#rhs) })
            }
            _ => {
                let value = number_expr(expr, binders)?;
                Some(quote! { (#value) != 0 })
            }
        },
        _ => {
            let value = number_expr(expr, binders)?;
            Some(quote! { (#value) != 0 })
        }
    }
}

fn comparison(
    operator: TokenStream,
    lhs: &Expr,
    rhs: &Expr,
    binders: &BTreeMap<String, u32>,
) -> Option<TokenStream> {
    let lhs = number_expr(lhs, binders)?;
    let rhs = number_expr(rhs, binders)?;
    Some(quote! { (#lhs) #operator (#rhs) })
}

fn number_expr(expr: &Expr, binders: &BTreeMap<String, u32>) -> Option<TokenStream> {
    match &expr.kind {
        ExprKind::Integer(value) => Some(quote! { #value }),
        ExprKind::Name(name) if binders.contains_key(name) => {
            let value = binding_value_ident(*binders.get(name)?);
            Some(quote! { #value.to_u64() as i64 })
        }
        ExprKind::Name(name) => Some(quote! { widths[#name] as i64 }),
        ExprKind::Call { name, args } => {
            let ExprKind::Name(argument) = &args.first()?.kind else {
                return None;
            };
            let value = binding_value_ident(*binders.get(argument)?);
            match name.as_str() {
                "popcount" => Some(quote! { #value.count_ones() as i64 }),
                "ctz" => Some(quote! { #value.count_trailing_zeros() as i64 }),
                "clz" => Some(quote! { #value.count_leading_zeros() as i64 }),
                _ => None,
            }
        }
        ExprKind::Unary {
            op: UnaryOp::Negate,
            value,
        } => {
            let value = number_expr(value, binders)?;
            Some(quote! { -(#value) })
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs = number_expr(lhs, binders)?;
            let rhs = number_expr(rhs, binders)?;
            match op {
                BinaryOp::Multiply => Some(quote! { (#lhs) * (#rhs) }),
                BinaryOp::Divide => Some(quote! { (#lhs) / (#rhs) }),
                BinaryOp::Remainder => Some(quote! { (#lhs) % (#rhs) }),
                BinaryOp::Add => Some(quote! { (#lhs) + (#rhs) }),
                BinaryOp::Subtract => Some(quote! { (#lhs) - (#rhs) }),
                BinaryOp::ShiftLeft => Some(quote! { (#lhs) << (#rhs) }),
                BinaryOp::ShiftRight => Some(quote! { (#lhs) >> (#rhs) }),
                BinaryOp::BitAnd => Some(quote! { (#lhs) & (#rhs) }),
                BinaryOp::BitXor => Some(quote! { (#lhs) ^ (#rhs) }),
                BinaryOp::BitOr => Some(quote! { (#lhs) | (#rhs) }),
                _ => None,
            }
        }
        ExprKind::Unary {
            op: UnaryOp::Not, ..
        } => None,
    }
}

fn function_name(index: usize) -> Ident {
    format_ident!("pdl_rule_{index}")
}

fn binding_ident(index: u32) -> Ident {
    format_ident!("binding_{index}")
}

fn binding_value_ident(index: u32) -> Ident {
    format_ident!("binding_{index}_value")
}
