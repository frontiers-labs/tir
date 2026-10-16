use std::collections::BTreeMap;

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

use crate::ast::*;
use crate::{Diagnostic, Span};

/// What the generated code needs of an attribute-free op beyond its name:
/// whether it commutes and the offset laws it has are read off its core
/// declarations when the rule is built, so they live in one place.
struct RustOperation {
    path: TokenStream,
    cost: u32,
}

fn rust_operation(operator: &Operator) -> Option<RustOperation> {
    let Operator::Dialect { dialect, name } = operator else {
        return None;
    };
    match (dialect.as_str(), name.as_str()) {
        ("builtin", "addi") => Some(RustOperation {
            path: quote! { crate::builtin::AddIOp },
            cost: 1,
        }),
        ("builtin", "muli") => Some(RustOperation {
            path: quote! { crate::builtin::MulIOp },
            cost: 4,
        }),
        ("builtin", "subi") => Some(RustOperation {
            path: quote! { crate::builtin::SubIOp },
            cost: 1,
        }),
        ("builtin", "divui") => Some(RustOperation {
            path: quote! { crate::builtin::DivUIOp },
            cost: 1,
        }),
        ("builtin", "divsi") => Some(RustOperation {
            path: quote! { crate::builtin::DivSIOp },
            cost: 1,
        }),
        ("builtin", "remui") => Some(RustOperation {
            path: quote! { crate::builtin::RemUIOp },
            cost: 1,
        }),
        ("builtin", "remsi") => Some(RustOperation {
            path: quote! { crate::builtin::RemSIOp },
            cost: 1,
        }),
        ("builtin", "shli") => Some(RustOperation {
            path: quote! { crate::builtin::ShlIOp },
            cost: 1,
        }),
        ("builtin", "andi") => Some(RustOperation {
            path: quote! { crate::builtin::AndIOp },
            cost: 1,
        }),
        ("builtin", "ori") => Some(RustOperation {
            path: quote! { crate::builtin::OrIOp },
            cost: 1,
        }),
        ("builtin", "xori") => Some(RustOperation {
            path: quote! { crate::builtin::XOrIOp },
            cost: 1,
        }),
        ("builtin", "shrui") => Some(RustOperation {
            path: quote! { crate::builtin::ShrUIOp },
            cost: 1,
        }),
        ("builtin", "shrsi") => Some(RustOperation {
            path: quote! { crate::builtin::ShrSIOp },
            cost: 1,
        }),
        ("ptr", "field") => Some(RustOperation {
            path: quote! { crate::ptr::FieldOp },
            cost: 1,
        }),
        ("ptr", "ptradd") => Some(RustOperation {
            path: quote! { crate::ptr::PtrAddOp },
            cost: 1,
        }),
        ("ptr", "copy") => Some(RustOperation {
            path: quote! { crate::ptr::CopyOp },
            cost: 1,
        }),
        ("ptr", "memcpy") => Some(RustOperation {
            path: quote! { crate::ptr::MemcpyOp },
            cost: 1,
        }),
        ("builtin", "refine") => Some(RustOperation {
            path: quote! { crate::builtin::RefineOp },
            cost: 1,
        }),
        ("layout", "sizeof") => Some(RustOperation {
            path: quote! { crate::layout::SizeOfOp },
            cost: 1,
        }),
        ("layout", "offsetof") => Some(RustOperation {
            path: quote! { crate::layout::OffsetOfOp },
            cost: 1,
        }),
        ("fp", "neg") => Some(RustOperation {
            path: quote! { crate::fp::ops::NegOp },
            cost: 1,
        }),
        ("fp", "abs") => Some(RustOperation {
            path: quote! { crate::fp::ops::AbsOp },
            cost: 1,
        }),
        ("fp", "copysign") => Some(RustOperation {
            path: quote! { crate::fp::ops::CopySignOp },
            cost: 1,
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

    /// How many host functions one generated rule may name.
    const PDL_EXTERN_STRIDE: u32 = 64;

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
        // A rule stated `proof trusted` is applied only where the pipeline
        // trusts such rules.
        let push = match rule.proof {
            Some(Proof::Trusted) => quote! { push_trusted },
            _ => quote! { push_query },
        };
        initializers.push(quote! {
            ruleset.#push(#function(context));
        });
        let base = rule_index as u32 * PDL_EXTERN_STRIDE;
        let Some(generated_rule) = generate_rule(rule, function, base) else {
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

    let count = file
        .items
        .iter()
        .filter(|item| matches!(item, Item::Rule(_)))
        .count();
    let tables = (0..count).map(|index| {
        let table = format_ident!("{}_extern", function_name(index));
        let base = index as u32 * PDL_EXTERN_STRIDE;
        quote! { #base => #table(id % PDL_EXTERN_STRIDE, args, &mut *out), }
    });
    format_rust(quote! {
        pub(super) fn generated_ruleset(context: &Context) -> Ruleset {
            let mut ruleset = Ruleset::new();
            #(#initializers)*
            ruleset
        }

        /// How many host functions one rule may name.
        const PDL_EXTERN_STRIDE: u32 = 64;
        /// The host functions the generated rules' guards and heads call: PDL's
        /// own arithmetic over the words its atoms bound.
        pub(super) fn pdl_extern(id: u32, args: &[u64], out: &mut [u64]) -> bool {
            match id - id % PDL_EXTERN_STRIDE {
                #(#tables)*
                _ => false,
            }
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
    if matches!(rule.kind, RuleKind::Refinement) {
        diagnostics.push(Diagnostic::new(
            "refinement rules cannot be emitted as equality rewrites",
            "use the refinement candidate resolver",
            rule.span,
        ));
        return;
    }
    if rule.proof() == Proof::Smt && rule.is_floating_point() {
        diagnostics.push(Diagnostic::new(
            "floating-point equality requires a checked proof binding",
            "the PDL Rust generator cannot establish floating-point equality",
            rule.span,
        ));
        return;
    }
    if matches!(rule.kind, RuleKind::Equality(Direction::Bidirectional)) {
        diagnostics.push(Diagnostic::new(
            "bidirectional Rust code generation is not implemented",
            "use a forward rule in the initial compiler",
            rule.span,
        ));
    }
    let mut binders = BTreeMap::new();
    collect_binder_types(&rule.lhs, &mut binders);
    validate_typed_binders(&rule.lhs, diagnostics);
    validate_lhs(&rule.lhs, diagnostics);
    validate_rhs(&rule.rhs, true, &binders, diagnostics);
    for guard in &rule.guards {
        validate_codegen_expr(guard, &binders, diagnostics);
    }
}

fn validate_typed_binders(term: &Term, diagnostics: &mut Vec<Diagnostic>) {
    match &term.kind {
        TermKind::Operation {
            operands,
            dependencies,
            ..
        } => {
            for operand in operands.iter().chain(dependencies) {
                validate_typed_binders(operand, diagnostics);
            }
        }
        TermKind::Binder {
            ty: Some(BindingType::Type(Type::ShapedFloat { .. })),
            ..
        } => diagnostics.push(Diagnostic::new(
            "shaped floating-point matching is not available in Rust code generation",
            "use the semantic candidate resolver for these bindings",
            term.span,
        )),
        TermKind::Binder {
            ty: Some(BindingType::Type(Type::State(Resource::Named(_)))),
            ..
        } => diagnostics.push(Diagnostic::new(
            "this state resource is not supported by Rust code generation",
            "use memory or fp.env",
            term.span,
        )),
        _ => {}
    }
}

fn collect_binder_types<'a>(
    term: &'a Term,
    binders: &mut BTreeMap<&'a str, Option<&'a BindingType>>,
) {
    match &term.kind {
        TermKind::Operation {
            operands,
            dependencies,
            ..
        } => {
            for operand in operands.iter().chain(dependencies) {
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
        operands,
        dependencies,
        ..
    } = &term.kind
    {
        if let Operator::Dialect { dialect, name } = operator
            && rust_operation(operator).is_none()
        {
            diagnostics.push(Diagnostic::new(
                format!("unknown operation '{dialect}.{name}'"),
                "this operation has no typed Rust lowering",
                term.span,
            ));
        }
        for operand in operands.iter().chain(dependencies) {
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
            operands,
            dependencies,
            ..
        } => {
            if matches!(operator, Operator::Semantic(_)) {
                diagnostics.push(unsupported("semantic operator emission", term.span));
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
            // A nested operation has no class to take its type from.
            let typed = matches!(
                term.ty,
                Some(Type::Integer(Width::Concrete(_) | Width::Named(_)))
            );
            if !root && !typed {
                diagnostics.push(Diagnostic::new(
                    "a nested operation needs its result type",
                    "write `: int<W>` after it",
                    term.span,
                ));
            }
            for operand in operands.iter().chain(dependencies) {
                validate_rhs(operand, false, binders, diagnostics);
            }
            return;
        }
        TermKind::Constant { width, value } => {
            validate_constant_width(width, diagnostics);
            validate_number_expr(width, binders, diagnostics);
            validate_number_expr(value, binders, diagnostics);
        }
        TermKind::Binder { .. } => {}
        TermKind::Value(_) | TermKind::String(_) | TermKind::Root => {
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
        ExprKind::Call { name, args } if matches!(name.as_str(), "offsetof" | "sizeof") => {
            let arity = if name == "offsetof" { 2 } else { 1 };
            let attributes = args.iter().all(
                |arg| matches!(&arg.kind, ExprKind::Name(name) if !binders.contains_key(name.as_str())),
            );
            if args.len() != arity || !attributes {
                diagnostics.push(Diagnostic::new(
                    format!("'{name}' takes {arity} attribute binders"),
                    "pass the type (and field) an operation's attributes bind",
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

fn generate_rule(rule: &Rule, function: Ident, base: u32) -> Option<TokenStream> {
    let rule_name = &rule.name;
    let mut pattern = PatternGenerator::default();
    let root = pattern.term(&rule.lhs)?;

    let mut build = RuleBuilder {
        vars: pattern.vars,
        scalars: pattern.scalars,
        atoms: std::mem::take(&mut pattern.atoms),
        base,
        ..RuleBuilder::default()
    };
    for (row, name, binder) in &pattern.attribute_reads {
        let out = build.scalar();
        build.guards.push(quote! {
            Guard::Read {
                term: Source::Row(#row),
                field: crate::sem::node::attribute_field(context.intern(#name)),
                out: #out,
            }
        });
        match build.attributes.get(binder) {
            Some(&bound) => build.guards.push(quote! {
                Guard::Cmp(Cmp::Eq, Expr::Scalar(#out), Expr::Scalar(#bound))
            }),
            None => {
                build.attributes.insert(binder.clone(), out);
            }
        }
    }
    // An operand written as a number is that number, at whatever width the class
    // spells it — the reading `class_is_literal` took.
    for literal in &pattern.literals {
        let (value, width) = build.constant(literal.var);
        let expected = literal.value;
        build.guards.push(quote! {
            Guard::Cmp(
                Cmp::Eq,
                Expr::Scalar(#value),
                Expr::And(
                    Box::new(Expr::Lit(#expected)),
                    Box::new(Expr::Ones(Box::new(Expr::Scalar(#width)))),
                ),
            )
        });
    }
    for constraint in &pattern.constraints {
        generate_constraint(constraint, &mut build)?;
    }
    for guard in &rule.guards {
        let body = bool_expr(guard, &pattern.binders, &mut build)?;
        build.call(quote! { #body }, None);
    }
    let replacement = generate_rhs(&rule.rhs, root, &pattern.binders, &mut build)?;
    build
        .head
        .push(quote! { HeadOp::Union(#root, #replacement) });

    // A head's own classes are not part of a match: it binds them while it runs.
    let (vars, scalars) = (pattern.vars, build.scalars);
    let head_vars = build.vars - pattern.vars;
    let (atoms, guards, head) = (&build.atoms, &build.guards, &build.head);
    let post_saturation = rule.post_saturation;
    let externs = build.externs.iter().enumerate().map(|(id, body)| {
        let id = id as u32;
        quote! { #id => #body, }
    });
    let table = format_ident!("{function}_extern");
    let law_checks = pattern
        .constant_operands
        .iter()
        .map(|(path, operand, zero)| {
            quote! {
                debug_assert!(
                    constant_operand_kept::<#path>(context, #operand, #zero),
                    "rule '{}': a constant operand of an operator with an offset law never matches",
                    #rule_name,
                );
            }
        });
    Some(quote! {
        fn #function(context: &Context) -> tir_egraph::Rule<Node> {
            let _ = context;
            #(#law_checks)*
            tir_egraph::Rule {
                name: #rule_name.to_string(),
                plan: Plan::compile(Query {
                    vars: #vars,
                    scalars: #scalars,
                    root: #root,
                    atoms: vec![#(#atoms),*],
                    guards: vec![#(#guards),*],
                    nots: Vec::new(),
                }),
                head: vec![#(#head),*],
                head_vars: #head_vars,
                post_saturation: #post_saturation,
            }
        }

        #[allow(clippy::match_single_binding, unused_variables)]
        fn #table(id: u32, args: &[u64], out: &mut [u64]) -> bool {
            match id {
                #(#externs)*
                _ => false,
            }
        }
    })
}

/// Lowers a rule's left-hand side into query atoms: one class variable per
/// pattern node, one atom per operation, and a note of what each binder must
/// turn out to be.
#[derive(Default)]
struct PatternGenerator {
    /// Binder name -> the class variable it binds.
    binders: BTreeMap<String, u32>,
    constraints: Vec<Constraint>,
    literals: Vec<Literal>,
    atoms: Vec<TokenStream>,
    vars: u32,
    scalars: u32,
    attribute_reads: Vec<(u32, String, String)>,
    /// Each IR op holding a constant operand, which, and whether it is the
    /// literal zero. Whether the op's laws dissolve that operand is a question
    /// for its core declaration, so it is asked where the rule is built.
    constant_operands: Vec<(TokenStream, usize, bool)>,
}

struct Constraint {
    binder: u32,
    ty: BindingType,
}

struct Literal {
    var: u32,
    value: i64,
}

impl PatternGenerator {
    fn var(&mut self) -> u32 {
        self.vars += 1;
        self.vars - 1
    }

    fn term(&mut self, term: &Term) -> Option<u32> {
        match &term.kind {
            TermKind::Binder { name, ty } => {
                if let Some(&var) = self.binders.get(name) {
                    return Some(var);
                }
                let var = self.var();
                self.binders.insert(name.clone(), var);
                if let Some(ty) = ty {
                    self.constraints.push(Constraint {
                        binder: var,
                        ty: ty.clone(),
                    });
                }
                Some(var)
            }
            TermKind::Value(Expr {
                kind: ExprKind::Integer(value),
                ..
            }) => {
                let var = self.var();
                self.literals.push(Literal { var, value: *value });
                Some(var)
            }
            TermKind::Operation {
                operator,
                attributes,
                operands: term_operands,
                dependencies,
            } => {
                let operands: Vec<u32> = term_operands
                    .iter()
                    .chain(dependencies)
                    .map(|operand| self.term(operand))
                    .collect::<Option<_>>()?;
                let class = self.var();
                let children = operands.iter().map(|&var| quote! { Id::from_raw(#var) });
                let constructor = match operator {
                    Operator::Dialect { .. } => {
                        let path = rust_operation(operator)?.path;
                        for (index, operand) in term_operands.iter().enumerate() {
                            if operand.is_constant() {
                                let zero = operand.is_zero();
                                self.constant_operands.push((path.clone(), index, zero));
                            }
                        }
                        pattern_template(&path, attributes, children)
                    }
                    Operator::Semantic(name) => {
                        let kind = sym_kind_path(name)?;
                        quote! { Node::sym_pattern(#kind, vec![#(#children),*]) }
                    }
                };
                let row = self.attribute_row(attributes);
                self.atoms.push(quote! {
                    Atom::Node {
                        template: #constructor,
                        args: smallvec![#(#operands),*],
                        class: #class,
                        row: #row,
                    }
                });
                Some(class)
            }
            _ => None,
        }
    }
}

impl PatternGenerator {
    // The scalar a pattern op's row binds when it reads attributes by name.
    fn attribute_row(&mut self, attributes: &[Attribute]) -> TokenStream {
        let mut row = None;
        for attribute in attributes {
            let AttributeValue::Binder(binder) = &attribute.value else {
                continue;
            };
            let slot = *row.get_or_insert_with(|| {
                self.scalars += 1;
                self.scalars - 1
            });
            self.attribute_reads
                .push((slot, attribute.name.clone(), binder.clone()));
        }
        match row {
            Some(row) => quote! { Some(#row) },
            None => quote! { None },
        }
    }
}

// The template of a pattern op: its literal attributes fixed, the bound ones
// any value.
fn pattern_template(
    path: &TokenStream,
    attributes: &[Attribute],
    children: impl Iterator<Item = TokenStream>,
) -> TokenStream {
    if attributes.is_empty() {
        return quote! { Node::pattern::<#path>(vec![#(#children),*]) };
    }
    let mut fixed = Vec::new();
    let mut bound = Vec::new();
    for attribute in attributes {
        let name = &attribute.name;
        match attribute_literal(&attribute.value) {
            Some(value) => fixed.push(quote! { (#name, #value) }),
            None => bound.push(name.clone()),
        }
    }
    quote! {
        Node::pattern_binding::<#path>(context, vec![#(#fixed),*], &[#(#bound),*], vec![#(#children),*])
    }
}

// An attribute written as a literal, as the IR attribute it is.
fn attribute_literal(value: &AttributeValue) -> Option<TokenStream> {
    match value {
        AttributeValue::Integer(value) => {
            Some(quote! { crate::attributes::AttributeValue::Int(#value) })
        }
        AttributeValue::String(value) => {
            Some(quote! { crate::attributes::AttributeValue::from(#value) })
        }
        AttributeValue::Binder(_) => None,
    }
}

fn sym_kind_path(name: &str) -> Option<TokenStream> {
    let kind = format_ident!("{}", format!("{:?}", tir_symbolic::lang::op_kind(name)?));
    Some(quote! { SymKind::#kind })
}

/// What one rule's atoms, guards and head are built out of, plus the host
/// functions its expressions become.
#[derive(Default)]
struct RuleBuilder {
    atoms: Vec<TokenStream>,
    guards: Vec<TokenStream>,
    head: Vec<TokenStream>,
    /// The Rust bodies the extern table dispatches to, in id order.
    externs: Vec<TokenStream>,
    scalars: u32,
    vars: u32,
    /// Scalars holding a binder's constant value and its width.
    values: BTreeMap<u32, (u32, u32)>,
    /// Scalars holding each named width.
    widths: BTreeMap<String, u32>,
    /// Scalars holding the type of each binder declared with one.
    types: BTreeMap<u32, u32>,
    attributes: BTreeMap<String, u32>,
    /// The scalars an extern body reads, as `args`, in order.
    inputs: Vec<u32>,
    /// Where this rule's host-function ids start.
    base: u32,
}

impl RuleBuilder {
    fn scalar(&mut self) -> u32 {
        self.scalars += 1;
        self.scalars - 1
    }

    fn var(&mut self) -> u32 {
        self.vars += 1;
        self.vars - 1
    }

    fn constant(&mut self, binder: u32) -> (u32, u32) {
        if let Some(&known) = self.values.get(&binder) {
            return known;
        }
        let (value, width) = (self.scalar(), self.scalar());
        self.atoms.push(quote! {
            Atom::Const { key: #binder, value: #value }
        });
        self.atoms.push(quote! {
            Atom::Width { key: #binder, width: #width }
        });
        self.values.insert(binder, (value, width));
        (value, width)
    }

    fn bind_width(&mut self, name: &str, actual: u32) {
        match self.widths.get(name) {
            Some(&bound) => self.guards.push(quote! {
                Guard::Cmp(Cmp::Eq, Expr::Scalar(#actual), Expr::Scalar(#bound))
            }),
            None => {
                self.widths.insert(name.to_string(), actual);
            }
        }
    }

    fn read(&mut self, value: u32, width: u32) -> TokenStream {
        let value = self.word(value);
        let width = self.word(width);
        quote! { APInt::new(#width, #value) }
    }

    fn word(&mut self, slot: u32) -> TokenStream {
        let position = match self.inputs.iter().position(|&seen| seen == slot) {
            Some(position) => position,
            None => {
                self.inputs.push(slot);
                self.inputs.len() - 1
            }
        };
        quote! { args[#position] }
    }

    fn call(&mut self, body: TokenStream, out: Option<u32>) -> u32 {
        let id = self.base + self.externs.len() as u32;
        self.externs.push(body);
        let inputs = std::mem::take(&mut self.inputs);
        let args = inputs.iter().map(|&slot| quote! { Expr::Scalar(#slot) });
        let outs: Vec<TokenStream> = out.iter().map(|&slot| quote! { #slot }).collect();
        // The first host function is the base itself.
        let call = match id {
            0 => quote! { call::PDL },
            id => quote! { call::PDL + #id },
        };
        self.guards.push(quote! {
            Guard::Extern {
                call: #call,
                terms: SmallVec::new(),
                args: smallvec![#(#args),*],
                out: smallvec![#(#outs),*],
            }
        });
        id
    }
}

fn generate_constraint(constraint: &Constraint, build: &mut RuleBuilder) -> Option<()> {
    let binder = constraint.binder;
    match &constraint.ty {
        BindingType::Constant(width) => {
            let (_, actual) = build.constant(binder);
            match width.as_ref().map(|width| &width.kind) {
                None => {}
                Some(ExprKind::Name(name)) if name == "_" => {}
                Some(ExprKind::Integer(bits)) => {
                    let bits = u32::try_from(*bits).ok()? as i64;
                    build.guards.push(quote! {
                        Guard::Cmp(Cmp::Eq, Expr::Scalar(#actual), Expr::Lit(#bits))
                    });
                }
                Some(ExprKind::Name(name)) => build.bind_width(name, actual),
                Some(_) => return None,
            }
            Some(())
        }
        BindingType::Type(Type::Integer(Width::Named(name))) => {
            let ty = build.scalar();
            let width = build.scalar();
            build.atoms.push(quote! {
                                Atom::Fact { column: ColumnId::Type, key: #binder, value: #ty }
            });
            build.types.insert(binder, ty);
            build.guards.push(quote! {
                Guard::Extern {
                    call: call::INT_WIDTH_OF,
                    terms: SmallVec::new(),
                    args: smallvec![Expr::Scalar(#ty)],
                    out: smallvec![#width],
                }
            });
            build.bind_width(name, width);
            Some(())
        }
        BindingType::Type(Type::Pointer(Width::Named(name))) => {
            let ty = build.scalar();
            let width = build.scalar();
            build.atoms.push(quote! {
                                Atom::Fact { column: ColumnId::Type, key: #binder, value: #ty }
            });
            build.types.insert(binder, ty);
            build.guards.push(quote! {
                Guard::Extern {
                    call: call::POINTER_WIDTH_OF,
                    terms: SmallVec::new(),
                    args: smallvec![Expr::Scalar(#ty)],
                    out: smallvec![#width],
                }
            });
            build.bind_width(name, width);
            Some(())
        }
        BindingType::Type(Type::Float(format)) => {
            let ty = build.scalar();
            let width = build.scalar();
            let expected = match format {
                FloatFormat::Binary32 => 32i64,
                FloatFormat::Binary64 => 64i64,
            };
            build.atoms.push(quote! {
                                Atom::Fact { column: ColumnId::Type, key: #binder, value: #ty }
            });
            build.types.insert(binder, ty);
            build.guards.push(quote! {
                Guard::Extern {
                    call: call::FLOAT_WIDTH_OF,
                    terms: SmallVec::new(),
                    args: smallvec![Expr::Scalar(#ty)],
                    out: smallvec![Expr::Scalar(#width)],
                }
            });
            build.guards.push(quote! {
                Guard::Cmp(Cmp::Eq, Expr::Scalar(#width), Expr::Lit(#expected))
            });
            Some(())
        }
        BindingType::Type(Type::State(resource)) => {
            let ty = build.scalar();
            let value = build.scalar();
            let expected = match resource {
                Resource::Memory => 0i64,
                Resource::FpEnv => 1i64,
                Resource::Named(_) => return None,
            };
            build.atoms.push(quote! {
                                Atom::Fact { column: ColumnId::Type, key: #binder, value: #ty }
            });
            build.types.insert(binder, ty);
            build.guards.push(quote! {
                Guard::Extern {
                    call: call::STATE_RESOURCE_OF,
                    terms: SmallVec::new(),
                    args: smallvec![Expr::Scalar(#ty)],
                    out: smallvec![Expr::Scalar(#value)],
                }
            });
            build.guards.push(quote! {
                Guard::Cmp(Cmp::Eq, Expr::Scalar(#value), Expr::Lit(#expected))
            });
            Some(())
        }
        _ => None,
    }
}

fn generate_rhs(
    term: &Term,
    root: u32,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<u32> {
    match &term.kind {
        TermKind::Binder { name, .. } => binders.get(name).copied(),
        TermKind::Constant { .. } => rhs_operand(term, binders, build),
        TermKind::Operation { .. } => {
            // The replacement answers at the type the class already has.
            let ty = build.scalar();
            build.atoms.push(quote! {
                Atom::Fact { column: ColumnId::Type, key: #root, value: #ty }
            });
            rhs_operation(term, ty, binders, build)
        }
        _ => None,
    }
}

// Insert the operation `term` at the type in scalar `ty`, its operands and
// dependencies first.
fn rhs_operation(
    term: &Term,
    ty: u32,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<u32> {
    let TermKind::Operation {
        operator,
        attributes,
        operands,
        dependencies,
    } = &term.kind
    else {
        return None;
    };
    let operation = rust_operation(operator)?;
    let path = operation.path;
    let cost = operation.cost;
    let operands: Vec<u32> = operands
        .iter()
        .chain(dependencies)
        .map(|operand| rhs_operand(operand, binders, build))
        .collect::<Option<_>>()?;
    // An introduced op lives in the carrier its type is.
    let carrier = build.scalar();
    build.guards.push(quote! {
        Guard::Extern {
            call: call::CARRIER_OF,
            terms: SmallVec::new(),
            args: smallvec![Expr::Scalar(#ty)],
            out: smallvec![#carrier],
        }
    });
    let mut names = Vec::new();
    let mut fills = vec![
        quote! { (field::TY, #ty) },
        quote! { (field::CARRIER, #carrier) },
    ];
    for attribute in attributes {
        let name = &attribute.name;
        let value = match (&attribute.value, attribute_literal(&attribute.value)) {
            (AttributeValue::Binder(binder), _) => *build.attributes.get(binder)?,
            (_, Some(literal)) => {
                let word = build.scalar();
                build.guards.push(quote! {
                    Guard::Let {
                        out: #word,
                        value: Expr::Lit(
                            crate::sem::node::attribute_word(&context.names(), &#literal)
                                .expect("a literal attribute is a word") as i64,
                        ),
                    }
                });
                word
            }
            _ => return None,
        };
        names.push(name.clone());
        fills.push(quote! {
            (crate::sem::node::attribute_field(context.intern(#name)), #value)
        });
    }
    // An operand of a declared type that is the result's returns from an
    // identity element at the other, as a seeded op's does.
    let typed = operands.iter().enumerate().filter_map(|(at, var)| {
        let operand = *build.types.get(var)?;
        let mut bit = quote! {
            Expr::IsZero(Box::new(Expr::Sub(
                Box::new(Expr::Scalar(#operand)),
                Box::new(Expr::Scalar(#ty)),
            )))
        };
        for _ in 0..at {
            bit = quote! { Expr::Add(Box::new(#bit), Box::new(#bit)) };
        }
        Some(bit)
    });
    let typed: Vec<TokenStream> = typed.collect();
    if let Some(bits) = typed
        .into_iter()
        .reduce(|sum, bit| quote! { Expr::Add(Box::new(#sum), Box::new(#bit)) })
    {
        let word = build.scalar();
        build
            .guards
            .push(quote! { Guard::Let { out: #word, value: #bits } });
        fills.push(quote! { (field::RESULT_TYPED, #word) });
    }
    let into = build.var();
    let children = operands.iter().map(|&var| quote! { Id::from_raw(#var) });
    let template = match names.is_empty() {
        true => quote! {
            Node::introduced::<#path>(context, #cost, vec![#(#children),*])
        },
        false => quote! {
            Node::introduced_with::<#path>(context, #cost, &[#(#names),*], vec![#(#children),*])
        },
    };
    build.head.push(quote! {
        HeadOp::Insert {
            label: LabelFill {
                template: #template,
                fills: smallvec![#(#fills),*],
            },
            args: smallvec![#(#operands),*],
            into: #into,
        }
    });
    Some(into)
}

// The type a nested operation declares, in a scalar.
fn rhs_type(ty: &Type, build: &mut RuleBuilder) -> Option<u32> {
    let out = build.scalar();
    match ty {
        Type::Integer(Width::Concrete(bits)) => {
            let bits = u64::from(*bits);
            build.guards.push(quote! {
                Guard::Let {
                    out: #out,
                    value: Expr::Lit(context.terms().int(#bits).number() as i64),
                }
            });
        }
        Type::Integer(Width::Named(name)) => {
            let width = *build.widths.get(name)?;
            build.guards.push(quote! {
                Guard::Extern {
                    call: call::INT_TYPE,
                    terms: SmallVec::new(),
                    args: smallvec![Expr::Scalar(#width)],
                    out: smallvec![#out],
                }
            });
        }
        _ => return None,
    }
    Some(out)
}

fn rhs_operand(
    term: &Term,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<u32> {
    match &term.kind {
        TermKind::Binder { name, .. } => binders.get(name).copied(),
        TermKind::Operation { .. } => {
            let ty = rhs_type(term.ty.as_ref()?, build)?;
            rhs_operation(term, ty, binders, build)
        }
        TermKind::Constant { width, value } => {
            let width_body = number_expr(width, binders, build)?;
            let bits = build.scalar();
            build.call(
                quote! {{
                    let width = #width_body;
                    if !(1..=64).contains(&width) {
                        return false;
                    }
                    out[0] = width as u64;
                    true
                }},
                Some(bits),
            );
            let width = build.word(bits);
            let value_body = modular_number_expr(value, width, binders, build)?;
            let literal = build.scalar();
            build.call(
                quote! {{
                    out[0] = (#value_body).to_u64();
                    true
                }},
                Some(literal),
            );
            let into = build.var();
            build.head.push(quote! {
                HeadOp::Insert {
                    label: LabelFill {
                        template: konst(APInt::new(1, 0)),
                        fills: smallvec![(field::INT_VALUE, #literal), (field::INT_WIDTH, #bits)],
                    },
                    args: SmallVec::new(),
                    into: #into,
                }
            });
            Some(into)
        }
        _ => None,
    }
}

fn bool_expr(
    expr: &Expr,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<TokenStream> {
    match &expr.kind {
        ExprKind::Unary {
            op: UnaryOp::Not,
            value,
        } => {
            let value = bool_expr(value, binders, build)?;
            Some(quote! { !(#value) })
        }
        ExprKind::Binary { op, lhs, rhs } => match op {
            BinaryOp::Equal => comparison(quote! { == }, lhs, rhs, binders, build),
            BinaryOp::NotEqual => comparison(quote! { != }, lhs, rhs, binders, build),
            BinaryOp::Less => comparison(quote! { < }, lhs, rhs, binders, build),
            BinaryOp::LessEqual => comparison(quote! { <= }, lhs, rhs, binders, build),
            BinaryOp::Greater => comparison(quote! { > }, lhs, rhs, binders, build),
            BinaryOp::GreaterEqual => comparison(quote! { >= }, lhs, rhs, binders, build),
            BinaryOp::LogicalAnd => {
                let lhs = bool_expr(lhs, binders, build)?;
                let rhs = bool_expr(rhs, binders, build)?;
                Some(quote! { (#lhs) && (#rhs) })
            }
            BinaryOp::LogicalOr => {
                let lhs = bool_expr(lhs, binders, build)?;
                let rhs = bool_expr(rhs, binders, build)?;
                Some(quote! { (#lhs) || (#rhs) })
            }
            _ => {
                let value = number_expr(expr, binders, build)?;
                Some(quote! { (#value) != 0 })
            }
        },
        _ => {
            let value = number_expr(expr, binders, build)?;
            Some(quote! { (#value) != 0 })
        }
    }
}

fn comparison(
    op: TokenStream,
    lhs: &Expr,
    rhs: &Expr,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<TokenStream> {
    let lhs = number_expr(lhs, binders, build)?;
    let rhs = number_expr(rhs, binders, build)?;
    Some(quote! { (#lhs) #op (#rhs) })
}

fn number_expr(
    expr: &Expr,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<TokenStream> {
    match &expr.kind {
        ExprKind::Integer(value) => Some(quote! { #value }),
        ExprKind::Name(name) if binders.contains_key(name) => {
            let (value, width) = build.constant(*binders.get(name)?);
            let read = build.read(value, width);
            Some(quote! { #read.to_u64() as i64 })
        }
        ExprKind::Name(name) => {
            let width = *build.widths.get(name)?;
            let read = build.word(width);
            Some(quote! { #read as i64 })
        }
        // A layout query is the host's: the layout in scope answers it in bits.
        ExprKind::Call { name, args } if matches!(name.as_str(), "offsetof" | "sizeof") => {
            let attributes: Option<Vec<u32>> = (args.iter())
                .map(|arg| match &arg.kind {
                    ExprKind::Name(name) => build.attributes.get(name).copied(),
                    _ => None,
                })
                .collect();
            let terms = attributes?
                .into_iter()
                .map(|slot| quote! { Expr::Scalar(#slot) });
            let call = match name.as_str() {
                "offsetof" => quote! { call::OFFSET_OF },
                _ => quote! { call::SIZE_OF },
            };
            let out = build.scalar();
            build.guards.push(quote! {
                Guard::Extern {
                    call: #call,
                    terms: SmallVec::new(),
                    args: smallvec![#(#terms),*],
                    out: smallvec![#out],
                }
            });
            let read = build.word(out);
            Some(quote! { #read as i64 })
        }
        ExprKind::Call { name, args } => {
            let ExprKind::Name(argument) = &args.first()?.kind else {
                return None;
            };
            let (value, width) = build.constant(*binders.get(argument)?);
            let read = build.read(value, width);
            match name.as_str() {
                "popcount" => Some(quote! { #read.count_ones() as i64 }),
                "ctz" => Some(quote! { #read.count_trailing_zeros() as i64 }),
                "clz" => Some(quote! { #read.count_leading_zeros() as i64 }),
                _ => None,
            }
        }
        ExprKind::Unary {
            op: UnaryOp::Negate,
            value,
        } => {
            let value = number_expr(value, binders, build)?;
            Some(quote! {{
                let Some(value) = (#value).checked_neg() else {
                    return false;
                };
                value
            }})
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs = number_expr(lhs, binders, build)?;
            let rhs = number_expr(rhs, binders, build)?;
            match op {
                BinaryOp::Multiply => Some(quote! {{
                    let Some(value) = (#lhs).checked_mul(#rhs) else {
                        return false;
                    };
                    value
                }}),
                BinaryOp::Divide => Some(quote! {{
                    let Some(value) = (#lhs).checked_div(#rhs) else {
                        return false;
                    };
                    value
                }}),
                BinaryOp::Remainder => Some(quote! {{
                    let Some(value) = (#lhs).checked_rem(#rhs) else {
                        return false;
                    };
                    value
                }}),
                BinaryOp::Add => Some(quote! {{
                    let Some(value) = (#lhs).checked_add(#rhs) else {
                        return false;
                    };
                    value
                }}),
                BinaryOp::Subtract => Some(quote! {{
                    let Some(value) = (#lhs).checked_sub(#rhs) else {
                        return false;
                    };
                    value
                }}),
                BinaryOp::ShiftLeft => Some(quote! {{
                    let Ok(shift) = u32::try_from(#rhs) else {
                        return false;
                    };
                    let Some(value) = (#lhs).checked_shl(shift) else {
                        return false;
                    };
                    value
                }}),
                BinaryOp::ShiftRight => Some(quote! {{
                    let Ok(shift) = u32::try_from(#rhs) else {
                        return false;
                    };
                    let Some(value) = (#lhs).checked_shr(shift) else {
                        return false;
                    };
                    value
                }}),
                BinaryOp::BitAnd => Some(quote! { (#lhs) & (#rhs) }),
                BinaryOp::BitXor => Some(quote! { (#lhs) ^ (#rhs) }),
                BinaryOp::BitOr => Some(quote! { (#lhs) | (#rhs) }),
                _ => None,
            }
        }
        _ => None,
    }
}

fn modular_number_expr(
    expr: &Expr,
    width: TokenStream,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<TokenStream> {
    let ExprKind::Binary { op, lhs, rhs } = &expr.kind else {
        let value = number_expr(expr, binders, build)?;
        return Some(quote! { APInt::new(#width, (#value) as u64) });
    };
    let operation = match op {
        BinaryOp::Add => Ident::new("add", proc_macro2::Span::call_site()),
        BinaryOp::Subtract => Ident::new("sub", proc_macro2::Span::call_site()),
        BinaryOp::Multiply => Ident::new("mul", proc_macro2::Span::call_site()),
        _ => {
            let value = number_expr(expr, binders, build)?;
            return Some(quote! { APInt::new(#width, (#value) as u64) });
        }
    };
    let lhs = modular_number_expr(lhs, width.clone(), binders, build)?;
    let rhs = modular_number_expr(rhs, width, binders, build)?;
    Some(quote! { (#lhs).#operation(&(#rhs)) })
}

fn function_name(index: usize) -> Ident {
    format_ident!("pdl_rule_{index}")
}
