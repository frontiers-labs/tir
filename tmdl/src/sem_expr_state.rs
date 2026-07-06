use crate::ast;

/// Statement-level hooks invoked by [`compile_to_state`] while folding a
/// behavior into a single state expression. `None` from a hook marks the
/// statement unsupported.
pub trait StateEmitter {
    /// Boolean condition of an `if`.
    fn cond(&self, e: &ast::Expr) -> String;
    fn assign(&self, a: &ast::Assign, st_name: &str) -> Option<String>;
    /// A bare `store(addr, size, value)` effect statement.
    fn store(&self, c: &ast::Call, st_name: &str) -> Option<String>;
    /// A bare `store_conditional(...)` effect statement: the reservation-gated
    /// memory write applies, the `bits<1>` success value is discarded.
    fn store_conditional(&self, c: &ast::Call, st_name: &str) -> Option<String>;
    /// A `fence(pred, succ)`/`fence_i()` effect statement.
    fn fence(&self, c: &ast::Call, st_name: &str) -> Option<String>;
    /// A `trap(args...)` statement: the ISA's trap-entry sequence, compiled
    /// against the current state via `compile`.
    fn trap(
        &self,
        c: &ast::Call,
        st_name: &str,
        compile: &dyn Fn(&ast::Expr, &str) -> String,
    ) -> Option<String>;
    fn ite(&self, cond: &str, then_state: &str, else_state: &str) -> String;
    /// Assemble a try/except from the already-compiled no-trap `body_state`;
    /// handler bodies are compiled against the entry state via `compile`,
    /// giving them precise-trap semantics.
    fn try_except(
        &self,
        t: &ast::TryExcept,
        st_name: &str,
        body_state: &str,
        compile: &dyn Fn(&ast::Expr, &str) -> String,
    ) -> Option<String>;
    fn unsupported(&self, e: &ast::Expr);
}

fn is_store_call(e: &ast::Expr) -> bool {
    matches!(
        e,
        ast::Expr::Call(c) if matches!(
            &*c.callee,
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store)
        )
    )
}

fn is_trap_call(e: &ast::Expr) -> bool {
    matches!(
        e,
        ast::Expr::Call(c) if matches!(
            &*c.callee,
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Trap)
        )
    )
}

fn is_store_conditional_call(e: &ast::Expr) -> bool {
    matches!(
        e,
        ast::Expr::Call(c) if matches!(
            &*c.callee,
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::StoreConditional)
        )
    )
}

fn is_fence_call(e: &ast::Expr) -> bool {
    matches!(
        e,
        ast::Expr::Call(c) if matches!(
            &*c.callee,
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Fence | ast::BuiltinFunction::FenceI)
        )
    )
}

pub fn compile_to_state(expr: &ast::Expr, st_name: &str, em: &dyn StateEmitter) -> String {
    let or_unsupported = |state: Option<String>| {
        state.unwrap_or_else(|| {
            em.unsupported(expr);
            st_name.to_string()
        })
    };
    match expr {
        ast::Expr::Assign(a) => or_unsupported(em.assign(a, st_name)),
        ast::Expr::Call(c) if is_store_call(expr) => or_unsupported(em.store(c, st_name)),
        ast::Expr::Call(c) if is_store_conditional_call(expr) => {
            or_unsupported(em.store_conditional(c, st_name))
        }
        ast::Expr::Call(c) if is_fence_call(expr) => or_unsupported(em.fence(c, st_name)),
        ast::Expr::Call(c) if is_trap_call(expr) => {
            or_unsupported(em.trap(c, st_name, &|e, st| compile_to_state(e, st, em)))
        }
        ast::Expr::Block(b) => {
            let mut current = st_name.to_string();
            for stmt in &b.stmts {
                if matches!(
                    stmt,
                    ast::Expr::Assign(_)
                        | ast::Expr::Block(_)
                        | ast::Expr::If(_)
                        | ast::Expr::Try(_)
                ) || is_store_call(stmt)
                    || is_store_conditional_call(stmt)
                    || is_fence_call(stmt)
                    || is_trap_call(stmt)
                {
                    current = compile_to_state(stmt, &current, em);
                } else {
                    em.unsupported(stmt);
                }
            }
            current
        }
        ast::Expr::If(i) => {
            let cond = em.cond(&i.cond);
            let then_state = compile_to_state(&i.then, st_name, em);
            let else_state = if let Some(e) = &i.else_ {
                compile_to_state(e, st_name, em)
            } else {
                st_name.to_string()
            };
            em.ite(&cond, &then_state, &else_state)
        }
        ast::Expr::Try(t) => {
            let body_state = compile_to_state(&t.body, st_name, em);
            or_unsupported(em.try_except(t, st_name, &body_state, &|e, st| {
                compile_to_state(e, st, em)
            }))
        }
        _ => {
            em.unsupported(expr);
            st_name.to_string()
        }
    }
}
