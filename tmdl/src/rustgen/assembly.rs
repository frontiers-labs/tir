// Actions derived from a simple asm template string.

enum AsmAction {
    SkipMnemonic,
    Comma,
    Operand(String),
    Skip,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Star,
    Plus,
    /// A literal identifier in the template (e.g. the condition in
    /// Literal identifier in the template (e.g. `eq` in `cset {rd}, eq` or
    /// `sp` in `c.addi4spn {rd}, sp, {imm}`); the parser requires it verbatim.
    Keyword(String),
}

enum AsmPrintPart {
    Text(String),
    Operand(String),
}

fn compile_asm_template(template: &str) -> Vec<AsmAction> {
    let mut actions = Vec::new();
    let mut i = 0;
    let bytes = template.as_bytes();
    while i < bytes.len() {
        match bytes[i] as char {
            '{' => {
                if let Some(end) = template[i + 1..].find('}') {
                    let content = &template[i + 1..i + 1 + end];
                    i = i + 1 + end + 1;
                    if content.starts_with("self.") {
                        if content.ends_with("MNEMONIC") {
                            actions.push(AsmAction::SkipMnemonic);
                        } else {
                            actions.push(AsmAction::Skip);
                        }
                    } else {
                        actions.push(AsmAction::Operand(content.to_string()));
                    }
                    continue;
                } else {
                    i += 1;
                    continue;
                }
            }
            ',' => {
                actions.push(AsmAction::Comma);
                i += 1;
            }
            '(' => {
                actions.push(AsmAction::LParen);
                i += 1;
            }
            ')' => {
                actions.push(AsmAction::RParen);
                i += 1;
            }
            '[' => {
                actions.push(AsmAction::LBracket);
                i += 1;
            }
            ']' => {
                actions.push(AsmAction::RBracket);
                i += 1;
            }
            '*' => {
                actions.push(AsmAction::Star);
                i += 1;
            }
            '+' => {
                actions.push(AsmAction::Plus);
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] as char == '_')
                {
                    i += 1;
                }
                actions.push(AsmAction::Keyword(template[start..i].to_string()));
            }
            _ => {
                i += 1;
            }
        }
    }
    actions
}

fn compile_asm_printer_template(template: &str, mnemonic: &str) -> Vec<AsmPrintPart> {
    let mut parts = Vec::new();
    let mut cursor = 0;

    while let Some(open_rel) = template[cursor..].find('{') {
        let open = cursor + open_rel;
        if open > cursor {
            parts.push(AsmPrintPart::Text(template[cursor..open].to_string()));
        }

        let Some(close_rel) = template[open + 1..].find('}') else {
            parts.push(AsmPrintPart::Text(template[open..].to_string()));
            return parts;
        };
        let close = open + 1 + close_rel;
        let content = &template[open + 1..close];
        if content == "self.MNEMONIC" {
            parts.push(AsmPrintPart::Text(mnemonic.to_string()));
        } else if !content.starts_with("self.") {
            parts.push(AsmPrintPart::Operand(content.to_string()));
        }
        cursor = close + 1;
    }

    if cursor < template.len() {
        parts.push(AsmPrintPart::Text(template[cursor..].to_string()));
    }

    parts
}

// ---------------------------------------------------------------------------
// AsSemExpr code generation
// ---------------------------------------------------------------------------

/// If the behavior is a conditional control transfer `if COND { PC::pc = TARGET }`
/// (no else), synthesize the value written to PC every cycle: `if COND { TARGET }
/// else { PC::pc + width }`. The fall-through arm keeps PC advancing when the branch
/// is not taken, so the result can be written unconditionally. Returns `None` for
/// behaviors that are not a bare conditional PC write.
fn synthesize_branch_value(inst: &ast::Instruction, width_bytes: u64) -> Option<ast::Expr> {
    let ast::Expr::If(if_) = unwrap_single_stmt_block(&inst.behavior) else {
        return None;
    };
    if if_.else_.is_some() {
        return None;
    }
    let target = extract_pc_assignment_target(&if_.then)?;
    let span = if_.span;
    let pc_read = ast::Expr::Path(ast::Path {
        base: "PC".to_string(),
        remainder: vec!["pc".to_string()],
        span,
    });
    // `zext(width, 64)` so the fall-through addend matches `PC::pc`'s 64-bit width
    // (a bare literal would lower to a narrow constant and mismatch the add).
    let width_lit = ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new(
        width_bytes.to_string(),
        span,
    )));
    let xlen_lit = ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new("64".to_string(), span)));
    let width_ext = ast::Expr::Call(ast::Call {
        callee: Box::new(ast::Expr::BuiltinFunction(ast::BuiltinFunction::ZExt)),
        arguments: vec![width_lit, xlen_lit],
        span,
    });
    let fallthrough = ast::Expr::Binary(ast::Binary {
        lhs: Box::new(pc_read),
        rhs: Box::new(width_ext),
        op: ast::BinOp::Add,
        span,
    });
    Some(ast::Expr::If(ast::If {
        cond: if_.cond.clone(),
        then: Box::new(target.clone()),
        else_: Some(Box::new(fallthrough)),
        span,
    }))
}

/// Peel `{ stmt }` blocks down to their single inner statement.
fn unwrap_single_stmt_block(e: &ast::Expr) -> &ast::Expr {
    match e {
        ast::Expr::Block(b) if b.stmts.len() == 1 => unwrap_single_stmt_block(&b.stmts[0]),
        other => other,
    }
}

/// The RHS expression of a single `PC::pc = TARGET` assignment inside a branch's
/// `then` arm.
fn extract_pc_assignment_target(then: &ast::Expr) -> Option<&ast::Expr> {
    let assign = match unwrap_single_stmt_block(then) {
        ast::Expr::Block(b) if b.stmts.len() == 1 => match &b.stmts[0] {
            ast::Expr::Assign(a) => a,
            _ => return None,
        },
        ast::Expr::Assign(a) => a,
        _ => return None,
    };
    if is_pc_dest(&assign.dest) {
        Some(assign.value.as_ref())
    } else {
        None
    }
}

fn is_pc_dest(dest: &ast::Expr) -> bool {
    matches!(dest, ast::Expr::Path(p) if p.base == "PC" && p.remainder == ["pc"])
}

/// Whether `(every, any)` path through `e` assigns `PC::pc`. Reads of PC (e.g.
/// `auipc`'s `rd = PC::pc + …`) do not count — only assignment destinations.
fn pc_writes(e: &ast::Expr) -> (bool, bool) {
    match e {
        ast::Expr::Assign(a) => {
            let w = is_pc_dest(&a.dest);
            (w, w)
        }
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .map(pc_writes)
            .fold((false, false), |acc, w| (acc.0 || w.0, acc.1 || w.1)),
        ast::Expr::If(i) => {
            let (then_every, then_any) = pc_writes(&i.then);
            let (else_every, else_any) = i
                .else_
                .as_ref()
                .map(|e| pc_writes(e))
                .unwrap_or((false, false));
            (then_every && else_every, then_any || else_any)
        }
        // Control-flow kind reflects the no-trap path; handler PC writes are
        // trap entries, not branches.
        ast::Expr::Try(t) => pc_writes(&t.body),
        _ => (false, false),
    }
}

