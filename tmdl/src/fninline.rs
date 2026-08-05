//! Inlining of `fn` items: pure expression-level helpers called from
//! behaviors. A call `f(a, b)` is replaced by the function's body with the
//! parameters substituted by the argument expressions — before sema and type
//! checking, so every later stage (semantics lowering, instruction selection,
//! verification, documentation) sees only the expanded expression.
//!
//! Functions may call functions; a call cycle is an error. Substitution is
//! capture-avoiding with respect to `map`/`reduce` lambda parameters: a lambda
//! parameter shadows a substituted name inside its body.

use std::collections::HashMap;

use crate::Span;
use crate::ast;

type Diag = chumsky::error::Rich<'static, String, Span>;

pub fn inline_functions(files: &mut [ast::File]) -> Vec<(String, Diag)> {
    let mut diags = Vec::new();
    let mut fns: HashMap<String, (ast::FnDef, String)> = HashMap::new();
    for file in files.iter() {
        for item in &file.items {
            if let ast::Item::Fn(f) = item {
                if let Some((_, first_file)) = fns.get(&f.name) {
                    diags.push((
                        file.file_name.clone(),
                        Diag::custom(
                            f.span,
                            format!(
                                "function '{}' duplicates a definition in file '{}'",
                                f.name, first_file
                            ),
                        ),
                    ));
                } else {
                    fns.insert(f.name.clone(), (f.clone(), file.file_name.clone()));
                }
            }
        }
    }

    for file in files.iter_mut() {
        let file_name = file.file_name.clone();
        for item in &mut file.items {
            match item {
                ast::Item::Instruction(inst) => {
                    let mut stack = Vec::new();
                    inst.behavior =
                        inline_expr(&inst.behavior, &fns, &mut stack, &mut diags, &file_name);
                }
                ast::Item::Isa(isa) => {
                    if let Some(trap) = &mut isa.trap_handler {
                        let mut stack = Vec::new();
                        trap.body =
                            inline_expr(&trap.body, &fns, &mut stack, &mut diags, &file_name);
                    }
                }
                _ => {}
            }
        }
    }
    diags
}

fn inline_expr(
    expr: &ast::Expr,
    fns: &HashMap<String, (ast::FnDef, String)>,
    stack: &mut Vec<String>,
    diags: &mut Vec<(String, Diag)>,
    file_name: &str,
) -> ast::Expr {
    // Inline nested calls everywhere first, then expand this call.
    let expr = map_children(expr, &mut |child| {
        inline_expr(child, fns, stack, diags, file_name)
    });
    let ast::Expr::Call(call) = &expr else {
        return expr;
    };
    let ast::Expr::Ident(callee) = &*call.callee else {
        return expr;
    };
    let Some((def, _)) = fns.get(&callee.name) else {
        return expr;
    };
    if stack.iter().any(|name| name == &callee.name) {
        diags.push((
            file_name.to_string(),
            Diag::custom(
                call.span,
                format!("recursive call to function '{}'", callee.name),
            ),
        ));
        return ast::Expr::Invalid;
    }
    if call.arguments.len() != def.params.len() {
        diags.push((
            file_name.to_string(),
            Diag::custom(
                call.span,
                format!(
                    "function '{}' takes {} arguments, but {} were supplied",
                    callee.name,
                    def.params.len(),
                    call.arguments.len()
                ),
            ),
        ));
        return ast::Expr::Invalid;
    }
    let bindings: HashMap<&str, &ast::Expr> = def
        .params
        .iter()
        .map(String::as_str)
        .zip(call.arguments.iter())
        .collect();
    let body = substitute(&def.body, &bindings);
    stack.push(callee.name.clone());
    let inlined = inline_expr(&body, fns, stack, diags, file_name);
    stack.pop();
    inlined
}

/// Clone `expr`, replacing bare identifiers bound in `bindings` with their
/// expression. Lambda parameters shadow bindings within their bodies.
fn substitute(expr: &ast::Expr, bindings: &HashMap<&str, &ast::Expr>) -> ast::Expr {
    if let ast::Expr::Ident(id) = expr
        && let Some(replacement) = bindings.get(id.name.as_str())
    {
        return (*replacement).clone();
    }
    map_children(expr, &mut |child| match child {
        ast::Expr::Lambda(lambda) => {
            let mut inner = bindings.clone();
            for param in &lambda.params {
                inner.remove(param.as_str());
            }
            ast::Expr::Lambda(ast::Lambda {
                params: lambda.params.clone(),
                body: Box::new(substitute(&lambda.body, &inner)),
                span: lambda.span,
            })
        }
        other => substitute(other, bindings),
    })
}

/// Rebuild `expr` with `f` applied to each immediate child. Leaves are
/// returned unchanged.
fn map_children(expr: &ast::Expr, f: &mut dyn FnMut(&ast::Expr) -> ast::Expr) -> ast::Expr {
    match expr {
        ast::Expr::Assign(a) => ast::Expr::Assign(ast::Assign {
            dest: a.dest.clone(),
            value: Box::new(f(&a.value)),
            span: a.span,
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
            start: s.start,
            end: s.end,
            span: s.span,
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
        ast::Expr::Ident(_)
        | ast::Expr::Path(_)
        | ast::Expr::Lit(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Invalid => expr.clone(),
    }
}
