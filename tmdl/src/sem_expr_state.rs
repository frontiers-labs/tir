use crate::ast;

pub fn compile_to_state<FEval, FAssign, FIf, FOther>(
    expr: &ast::Expr,
    st_name: &str,
    eval_expr: &FEval,
    emit_assign: &FAssign,
    emit_if: &FIf,
    on_unsupported: &FOther,
) -> String
where
    FEval: Fn(&ast::Expr) -> String,
    FAssign: Fn(&ast::Assign, &str) -> Option<String>,
    FIf: Fn(&str, &str, &str) -> String,
    FOther: Fn(&ast::Expr),
{
    match expr {
        ast::Expr::Assign(a) => emit_assign(a, st_name).unwrap_or_else(|| {
            on_unsupported(expr);
            st_name.to_string()
        }),
        ast::Expr::Block(b) => {
            let mut current = st_name.to_string();
            for stmt in &b.stmts {
                if matches!(
                    stmt,
                    ast::Expr::Assign(_) | ast::Expr::Block(_) | ast::Expr::If(_)
                ) {
                    current = compile_to_state(
                        stmt,
                        &current,
                        eval_expr,
                        emit_assign,
                        emit_if,
                        on_unsupported,
                    );
                } else {
                    on_unsupported(stmt);
                }
            }
            current
        }
        ast::Expr::If(i) => {
            let cond = eval_expr(&i.cond);
            let then_state = compile_to_state(
                &i.then,
                st_name,
                eval_expr,
                emit_assign,
                emit_if,
                on_unsupported,
            );
            let else_state = if let Some(e) = &i.else_ {
                compile_to_state(e, st_name, eval_expr, emit_assign, emit_if, on_unsupported)
            } else {
                st_name.to_string()
            };
            emit_if(&cond, &then_state, &else_state)
        }
        _ => {
            on_unsupported(expr);
            st_name.to_string()
        }
    }
}
