use super::*;

pub(super) fn analyze_instruction_register_reads(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    fp_fields: &HashMap<String, FpField>,
) -> Option<HashMap<(String, u32), u32>> {
    let mut assignments = Vec::new();
    crate::utils::visit_statements(behavior, &mut |stmt| {
        if let ast::Expr::Assign(a) = stmt
            && let Some((class, _)) = assignment_dest_register_path(&a.dest)
            && let Some(field) = fp_fields.get(&class)
        {
            assignments.push((a, *field));
        }
    });
    let flags: Vec<_> = assignments
        .iter()
        .filter(|(_, field)| field.kind == tir_symbolic::lang::StateFieldKind::FpFlags)
        .collect();
    let exact = match flags.as_slice() {
        [(assign, _)] => match assign.value.as_ref() {
            ast::Expr::Binary(binary)
                if binary.op == ast::BinOp::BitwiseOr
                    && assignment_dest_register_path(&binary.lhs)
                        == assignment_dest_register_path(&assign.dest) =>
            {
                Some(binary.rhs.as_ref())
            }
            _ => None,
        },
        _ => None,
    };
    let assigns_state = defined_register_operands.is_empty() && !assignments.is_empty();
    let mut expressions = if assigns_state {
        assignments.iter().map(|(a, _)| a.value.as_ref()).collect()
    } else {
        vec![resolve_behavior_rhs(
            behavior,
            operands,
            defined_register_operands,
        )?]
    };
    expressions.extend(exact);
    let mut pattern = tir_symbolic::sem::SemGraph::new();
    let (_, mut lowering) = ast::Expr::lower_all_to_sema_with_isa(
        &expressions,
        &mut pattern,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    lowering
        .register_symbols
        .retain(|(class, _), _| !fp_fields.contains_key(class));
    Some(lowering.register_symbols)
}

pub(super) fn register_operand_names(operands: &[(String, Type)]) -> HashSet<&str> {
    operands
        .iter()
        .filter_map(|(name, ty)| match ty {
            Type::Struct(_) => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn assignment_dest_name(dest: &ast::Expr) -> Option<String> {
    match dest {
        ast::Expr::Ident(id) => Some(id.name.clone()),
        ast::Expr::Path(path) if path.remainder.len() == 1 => Some(path.remainder[0].clone()),
        _ => None,
    }
}

pub(super) fn assignment_dest_register_path(dest: &ast::Expr) -> Option<(String, String)> {
    match dest {
        ast::Expr::Path(path) if path.remainder.len() == 1 => {
            Some((path.base.clone(), path.remainder[0].clone()))
        }
        _ => None,
    }
}

pub(super) fn referenced_operands(expr: &ast::Expr, operands: &HashSet<&str>) -> Vec<String> {
    let mut out = Vec::new();
    collect_referenced_idents(expr, operands, &mut out);
    out
}

fn collect_referenced_idents(expr: &ast::Expr, operands: &HashSet<&str>, out: &mut Vec<String>) {
    match expr {
        ast::Expr::Ident(id) => {
            if operands.contains(id.name.as_str()) && !out.iter().any(|n| n == &id.name) {
                out.push(id.name.clone());
            }
        }
        ast::Expr::Lit(_)
        | ast::Expr::Path(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Tuple(_)
        | ast::Expr::Invalid => {}
        ast::Expr::Assign(a) => {
            collect_referenced_idents(&a.dest, operands, out);
            collect_referenced_idents(&a.value, operands, out);
        }
        ast::Expr::Let(l) => collect_referenced_idents(&l.value, operands, out),
        ast::Expr::Binary(b) => {
            collect_referenced_idents(&b.lhs, operands, out);
            collect_referenced_idents(&b.rhs, operands, out);
        }
        ast::Expr::Unary(u) => collect_referenced_idents(&u.x, operands, out),
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_referenced_idents(stmt, operands, out);
            }
        }
        ast::Expr::Call(c) => {
            // `regnum(op)` reads the operand's encoding index, not the value in
            // its register: an instruction that only asks which register an
            // operand names does not read that register.
            if matches!(
                c.callee.as_ref(),
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Regnum)
            ) {
                return;
            }
            collect_referenced_idents(&c.callee, operands, out);
            for arg in &c.arguments {
                collect_referenced_idents(arg, operands, out);
            }
        }
        ast::Expr::Field(f) => collect_referenced_idents(&f.base, operands, out),
        ast::Expr::If(i) => {
            collect_referenced_idents(&i.cond, operands, out);
            collect_referenced_idents(&i.then, operands, out);
            if let Some(e) = &i.else_ {
                collect_referenced_idents(e, operands, out);
            }
        }
        ast::Expr::IndexAccess(i) => collect_referenced_idents(&i.base, operands, out),
        ast::Expr::Slice(s) => collect_referenced_idents(&s.base, operands, out),
        ast::Expr::Cast(c) => {
            collect_referenced_idents(&c.x, operands, out);
            collect_referenced_idents(&c.width, operands, out);
        }
        ast::Expr::Try(t) => {
            collect_referenced_idents(&t.body, operands, out);
            for h in &t.handlers {
                collect_referenced_idents(&h.body, operands, out);
            }
        }
        ast::Expr::Lambda(l) => collect_referenced_idents(&l.body, operands, out),
    }
}

pub(super) fn literal_register_class_width(files: &[ast::File], class_name: &str) -> Option<u32> {
    files
        .iter()
        .flat_map(|f| f.register_classes())
        .find(|rc| rc.name == class_name)?
        .parameters
        .get("WIDTH")
        .and_then(|(_ty, value)| match value {
            Some(ast::Expr::Lit(ast::Lit::Int(li))) => Some(parse_literal_value(li) as u32),
            _ => None,
        })
}

pub(super) fn behavior_references_pc(expr: &ast::Expr, pc_classes: &HashSet<String>) -> bool {
    let mut found = false;
    crate::utils::visit_exprs(expr, &mut |e| {
        if let ast::Expr::Path(path) = e {
            found |= pc_classes.contains(&path.base);
        }
    });
    found
}

pub(super) fn behavior_reads_flag_register(
    expr: &ast::Expr,
    flag_classes: &HashSet<String>,
) -> bool {
    let is_flag = |e: &ast::Expr| matches!(e, ast::Expr::Path(p) if flag_classes.contains(&p.base));
    let (mut mentions, mut writes) = (0usize, 0usize);
    crate::utils::visit_exprs(expr, &mut |e: &ast::Expr| match e {
        ast::Expr::Assign(a) if is_flag(&a.dest) => writes += 1,
        _ if is_flag(e) => mentions += 1,
        _ => {}
    });
    mentions > writes
}

pub(super) fn value_reads_flag_register(expr: &ast::Expr, flag_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Assign(a) => {
            !assignment_dest_register_path(&a.dest)
                .is_some_and(|(class, _)| flag_classes.contains(&class))
                && behavior_reads_flag_register(expr, flag_classes)
        }
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| value_reads_flag_register(stmt, flag_classes)),
        ast::Expr::If(i) => {
            value_reads_flag_register(&i.then, flag_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| value_reads_flag_register(e, flag_classes))
                || (defines_value(&i.then, flag_classes)
                    || i.else_
                        .as_ref()
                        .is_some_and(|e| defines_value(e, flag_classes)))
                    && behavior_reads_flag_register(&i.cond, flag_classes)
        }
        ast::Expr::Try(t) => value_reads_flag_register(&t.body, flag_classes),
        other => behavior_reads_flag_register(other, flag_classes),
    }
}

fn defines_value(expr: &ast::Expr, flag_classes: &HashSet<String>) -> bool {
    let mut defines = false;
    crate::utils::visit_statements(expr, &mut |stmt| {
        defines |= match stmt {
            ast::Expr::Assign(a) => !assignment_dest_register_path(&a.dest)
                .is_some_and(|(class, _)| flag_classes.contains(&class)),
            ast::Expr::Lit(_) | ast::Expr::Ident(_) | ast::Expr::Path(_) => false,
            _ => true,
        };
    });
    defines
}

pub(super) fn behavior_writes_fixed_register(
    expr: &ast::Expr,
    flag_classes: &HashSet<String>,
) -> bool {
    let mut writes = false;
    crate::utils::visit_statements(expr, &mut |stmt| {
        if let ast::Expr::Assign(a) = stmt {
            writes |= assignment_dest_register_path(&a.dest)
                .is_some_and(|(class, _)| !flag_classes.contains(&class));
        }
    });
    writes
}

fn collect_behavior_assignments<'a>(expr: &'a ast::Expr, out: &mut Vec<(String, &'a ast::Expr)>) {
    crate::utils::visit_statements(expr, &mut |stmt| {
        if let ast::Expr::Assign(a) = stmt
            && let Some(dst) = assignment_dest_name(&a.dest)
        {
            out.push((dst, a.value.as_ref()));
        }
    });
}

pub(crate) fn infer_read_register_operands(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
) -> HashSet<String> {
    fn walk(expr: &ast::Expr, operands: &HashSet<&str>, out: &mut Vec<String>) {
        if let ast::Expr::Assign(a) = expr {
            // A plain identifier/path destination is a pure write; any other
            // destination form (e.g. a slice, a partial update) reads its base.
            if assignment_dest_name(&a.dest).is_none() {
                collect_referenced_idents(&a.dest, operands, out);
            }
            walk(&a.value, operands, out);
            return;
        }
        if let ast::Expr::Block(b) = expr {
            for stmt in &b.stmts {
                walk(stmt, operands, out);
            }
            return;
        }
        if let ast::Expr::If(i) = expr {
            collect_referenced_idents(&i.cond, operands, out);
            walk(&i.then, operands, out);
            if let Some(e) = &i.else_ {
                walk(e, operands, out);
            }
            return;
        }
        if let ast::Expr::Try(t) = expr {
            walk(&t.body, operands, out);
            return;
        }
        collect_referenced_idents(expr, operands, out);
    }

    let register_operands = register_operand_names(operands);
    let mut reads = Vec::new();
    walk(behavior, &register_operands, &mut reads);
    reads.into_iter().collect()
}

pub(crate) fn infer_defined_register_operands(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
) -> Vec<String> {
    let register_operands = register_operand_names(operands);

    let mut defs = Vec::new();
    let mut assignments = Vec::new();
    collect_behavior_assignments(behavior, &mut assignments);
    for (dst, _) in assignments {
        if register_operands.contains(dst.as_str()) && !defs.iter().any(|existing| existing == &dst)
        {
            defs.push(dst);
        }
    }
    defs
}

pub(super) fn resolve_behavior_rhs<'a>(
    behavior: &'a ast::Expr,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
) -> Option<&'a ast::Expr> {
    let register_operands = register_operand_names(operands);

    let mut assignments = Vec::new();
    collect_behavior_assignments(behavior, &mut assignments);
    for (dst, rhs) in assignments.iter().rev() {
        if defined_register_operands.iter().any(|d| d == dst) {
            return Some(*rhs);
        }
    }
    for (dst, rhs) in assignments.iter().rev() {
        if register_operands.contains(dst.as_str()) {
            return Some(*rhs);
        }
    }
    if let Some(store) = find_store_effect_expr(behavior) {
        return Some(store);
    }
    match behavior {
        ast::Expr::Assign(a) => Some(a.value.as_ref()),
        ast::Expr::Block(_) | ast::Expr::If(_) => None,
        other => Some(other),
    }
}

fn find_store_effect_expr(expr: &ast::Expr) -> Option<&ast::Expr> {
    match expr {
        ast::Expr::Call(_) if is_store_call(expr) => Some(expr),
        ast::Expr::Block(b) => b.stmts.iter().find_map(find_store_effect_expr),
        ast::Expr::Try(t) => find_store_effect_expr(&t.body),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Template / asm helpers
// ---------------------------------------------------------------------------

pub(crate) fn resolve_string(expr: &ast::Expr) -> Option<String> {
    match &expr {
        ast::Expr::Lit(ast::Lit::Str(lstr)) => Some(lstr.value().to_owned()),
        ast::Expr::Lit(_) => None,
        ast::Expr::Block(b) => {
            if b.last_expr_return
                && let Some(ast::Expr::Lit(ast::Lit::Str(s))) = b.stmts.last()
            {
                return Some(s.value().to_owned());
            }
            None
        }
        _ => None,
    }
}

pub(crate) fn resolve_asm_templates(expr: &ast::Expr) -> Option<Vec<String>> {
    match expr {
        ast::Expr::Block(block) if block.last_expr_return && block.stmts.len() == 1 => {
            resolve_asm_templates(&block.stmts[0])
        }
        ast::Expr::Tuple(tuple) if !tuple.elements.is_empty() => tuple
            .elements
            .iter()
            .map(|element| match element {
                ast::Expr::Lit(ast::Lit::Str(value)) => Some(value.value().to_owned()),
                _ => None,
            })
            .collect(),
        ast::Expr::Lit(ast::Lit::Str(value)) => Some(vec![value.value().to_owned()]),
        _ => None,
    }
}

pub(super) fn behavior_updates_fp_environment(expr: &ast::Expr) -> bool {
    let mut updates = false;
    crate::utils::visit_exprs(expr, &mut |expr| {
        updates |= matches!(
            expr,
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::FPFlags)
        );
    });
    updates
}
