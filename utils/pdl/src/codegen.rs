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
        let emit = root_operator(&rule.rhs)
            .and_then(rust_operation)
            .and_then(|operation| operation.emitter)
            .map_or_else(|| quote! { None }, |emitter| quote! { Some(#emitter()) });
        initializers.push(quote! {
            let index = ruleset.rewrites.len();
            ruleset.push_query(#function(context, index), #emit);
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
            let mut ruleset = Ruleset::new(context);
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

fn generate_rule(rule: &Rule, function: Ident, base: u32) -> Option<TokenStream> {
    let rule_name = &rule.name;
    let mut pattern = PatternGenerator::default();
    let root = pattern.term(&rule.lhs)?;

    let mut build = RuleBuilder {
        vars: pattern.vars,
        atoms: std::mem::take(&mut pattern.atoms),
        base,
        ..RuleBuilder::default()
    };
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
    let externs = build.externs.iter().enumerate().map(|(id, body)| {
        let id = id as u32;
        quote! { #id => #body, }
    });
    let table = format_ident!("{function}_extern");
    Some(quote! {
        fn #function(context: &Context, index: usize) -> tir_relational::Rule<Node> {
            let _ = (context, index);
            tir_relational::Rule {
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
                post_saturation: false,
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
            TermKind::Integer(value) => {
                let var = self.var();
                self.literals.push(Literal { var, value: *value });
                Some(var)
            }
            TermKind::Operation {
                operator, operands, ..
            } => {
                let operands: Vec<u32> = operands
                    .iter()
                    .map(|operand| self.term(operand))
                    .collect::<Option<_>>()?;
                let class = self.var();
                let children = operands.iter().map(|&var| quote! { Id::from_raw(#var) });
                let constructor = match operator {
                    Operator::Dialect { .. } => {
                        let path = rust_operation(operator)?.path;
                        quote! { Node::pattern::<#path>(vec![#(#children),*]) }
                    }
                    Operator::Gate(_) => quote! { Node::gamma_pattern(vec![#(#children),*]) },
                };
                self.atoms.push(quote! {
                    Atom::Node {
                        template: #constructor,
                        args: smallvec![#(#operands),*],
                        class: #class,
                        row: None,
                    }
                });
                Some(class)
            }
            TermKind::Constant { .. } | TermKind::String(_) => None,
        }
    }
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

    /// Require `binder` to be a constant, binding its value and width.
    fn constant(&mut self, binder: u32) -> (u32, u32) {
        if let Some(&known) = self.values.get(&binder) {
            return known;
        }
        let (label, value, width) = (self.scalar(), self.scalar(), self.scalar());
        self.atoms.push(quote! {
            Atom::Fact { column: ColumnId::Const, key: #binder, value: #label }
        });
        self.guards.push(quote! {
            Guard::Read { term: Source::Label(#label), field: field::INT_VALUE, out: #value }
        });
        self.guards.push(quote! {
            Guard::Read { term: Source::Label(#label), field: field::INT_WIDTH, out: #width }
        });
        self.values.insert(binder, (value, width));
        (value, width)
    }

    /// Bind a width name to `actual`, or require the two to agree.
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

    /// Read a binder's constant inside a host function's body, adding the two
    /// scalars it takes to that function's arguments.
    fn read(&mut self, value: u32, width: u32) -> TokenStream {
        let value = self.word(value);
        let width = self.word(width);
        quote! { APInt::new(#width as u32, #value) }
    }

    /// The `args` slot a scalar arrives in.
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

    /// An expression over bound scalars, as a host function: PDL's arithmetic is
    /// its own, and the engine only moves the words in and the answer out.
    fn call(&mut self, body: TokenStream, out: Option<u32>) -> u32 {
        let id = self.base + self.externs.len() as u32;
        self.externs.push(body);
        let inputs = std::mem::take(&mut self.inputs);
        let args = inputs.iter().map(|&slot| quote! { Expr::Scalar(#slot) });
        let outs: Vec<TokenStream> = out.iter().map(|&slot| quote! { #slot }).collect();
        self.guards.push(quote! {
            Guard::Extern {
                call: call::PDL + #id,
                terms: SmallVec::new(),
                args: smallvec![#(#args),*],
                out: smallvec![#(#outs),*],
            }
        });
        id
    }
}

/// A binder's declared type: a constant at a width, or an integer whose width a
/// name binds.
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
        _ => None,
    }
}

/// The class the head unions the matched root with.
fn generate_rhs(
    term: &Term,
    root: u32,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<u32> {
    match &term.kind {
        TermKind::Binder { name, .. } => binders.get(name).copied(),
        TermKind::Constant { .. } => rhs_operand(term, binders, build),
        TermKind::Operation {
            operator, operands, ..
        } => {
            let path = rust_operation(operator)?.path;
            let operands: Vec<u32> = operands
                .iter()
                .map(|operand| rhs_operand(operand, binders, build))
                .collect::<Option<_>>()?;
            // An introduced op answers at the type the class already has.
            let ty = build.scalar();
            build.atoms.push(quote! {
                Atom::Fact { column: ColumnId::Type, key: #root, value: #ty }
            });
            let into = build.var();
            let children = operands.iter().map(|&var| quote! { Id::from_raw(#var) });
            build.head.push(quote! {
                HeadOp::Insert {
                    label: LabelFill {
                        template: Node::introduced::<#path>(
                            TypeId::from_number(0),
                            1,
                            index,
                            vec![#(#children),*],
                        ),
                        fills: smallvec![(field::TY, #ty)],
                    },
                    args: smallvec![#(#operands),*],
                    into: #into,
                }
            });
            Some(into)
        }
        TermKind::Integer(_) | TermKind::String(_) => None,
    }
}

/// An operand of the right-hand side: a bound class, or a constant the rule
/// spells out.
fn rhs_operand(
    term: &Term,
    binders: &BTreeMap<String, u32>,
    build: &mut RuleBuilder,
) -> Option<u32> {
    match &term.kind {
        TermKind::Binder { name, .. } => binders.get(name).copied(),
        TermKind::Constant { width, value } => {
            let width_body = number_expr(width, binders, build)?;
            let bits = build.scalar();
            build.call(
                quote! {{
                    let width = #width_body;
                    if !(1..=64).contains(&(width as u32)) {
                        return false;
                    }
                    out[0] = width as u64;
                    true
                }},
                Some(bits),
            );
            let value_body = number_expr(value, binders, build)?;
            let literal = build.scalar();
            build.call(
                quote! {{
                    out[0] = (#value_body) as u64;
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
        TermKind::Integer(_) | TermKind::Operation { .. } | TermKind::String(_) => None,
    }
}

/// A PDL boolean over bound scalars, as the body of a host function.
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

/// A PDL number over bound scalars. Reading a binder or a width name adds it to
/// the host function's arguments; the arithmetic stays PDL's own.
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
            Some(quote! { -(#value) })
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs = number_expr(lhs, binders, build)?;
            let rhs = number_expr(rhs, binders, build)?;
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
        _ => None,
    }
}

fn function_name(index: usize) -> Ident {
    format_ident!("pdl_rule_{index}")
}
