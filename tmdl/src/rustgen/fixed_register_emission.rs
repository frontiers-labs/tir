use super::*;

fn unwrap_single_stmt(expr: &ast::Expr) -> &ast::Expr {
    match expr {
        ast::Expr::Block(b) if b.stmts.len() == 1 => &b.stmts[0],
        other => other,
    }
}

fn is_fixed_register_shape(
    inst: &ast::Instruction,
    ops: &[(String, Type)],
    flag_classes: &HashSet<String>,
) -> bool {
    let has_register_operand = ops.iter().any(|(_, ty)| matches!(ty, Type::Struct(_)));
    if has_register_operand {
        let ast::Expr::If(if_expr) = unwrap_single_stmt(&inst.behavior) else {
            return false;
        };
        if if_expr.else_.is_none() {
            return false;
        }
        let mut then_writes = Vec::new();
        collect_register_path_writes(&if_expr.then, &mut then_writes);
        let mut reads = HashSet::new();
        collect_register_path_reads(&inst.behavior, &mut reads);
        return !then_writes.is_empty() && !reads.is_empty();
    }
    let writes = definer_writes(inst, flag_classes);
    let [(_, rhs)] = writes.as_slice() else {
        return false;
    };
    referenced_operands(rhs, &register_operand_names(ops)).is_empty()
}

fn definer_writes<'a>(
    inst: &'a ast::Instruction,
    flag_classes: &HashSet<String>,
) -> Vec<((String, String), &'a ast::Expr)> {
    let mut writes = Vec::new();
    collect_register_path_writes(&inst.behavior, &mut writes);
    writes.retain(|((class, _), _)| !flag_classes.contains(class));
    writes
}

pub(super) fn fixed_register_role_items(
    inst: &ast::Instruction,
    ops: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
    register_name_map: &HashMap<(String, u32), String>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Vec<(String, String, bool)> {
    if !is_fixed_register_shape(inst, ops, flag_classes) {
        return Vec::new();
    }
    let allocatable = |class: &str| !flag_classes.contains(class) && !pc_classes.contains(class);
    let canonical_name = |class: &str, regname: &str| {
        let index = register_index_map.get(&(class.to_string(), regname.to_string()))?;
        register_name_map.get(&(class.to_string(), *index)).cloned()
    };

    let mut read_paths = HashSet::new();
    collect_register_path_reads(&inst.behavior, &mut read_paths);
    let mut write_list = Vec::new();
    collect_register_path_writes(&inst.behavior, &mut write_list);

    let mut reads: Vec<(String, String)> = read_paths
        .into_iter()
        .filter(|(class, _)| allocatable(class))
        .collect();
    reads.sort();
    let mut writes: Vec<(String, String)> = write_list
        .into_iter()
        .filter_map(|(path, _)| allocatable(&path.0).then_some(path))
        .collect();
    writes.sort();
    writes.dedup();

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for (class, regname) in &reads {
        let Some(name) = canonical_name(class, regname) else {
            continue;
        };
        let slot = fixed_read_slot_name(&name);
        if seen.insert(slot.clone()) {
            items.push((slot, class.clone(), false));
        }
    }
    for (class, regname) in &writes {
        let Some(name) = canonical_name(class, regname) else {
            continue;
        };
        let slot = fixed_write_slot_name(&name);
        if seen.insert(slot.clone()) {
            items.push((slot, class.clone(), true));
        }
    }
    items
}

fn fixed_read_slot_name(reg_name: &str) -> String {
    reg_name.to_string()
}

fn fixed_write_slot_name(reg_name: &str) -> String {
    format!("{reg_name}_def")
}

pub(crate) fn collect_register_path_writes<'a>(
    expr: &'a ast::Expr,
    out: &mut Vec<((String, String), &'a ast::Expr)>,
) {
    match expr {
        ast::Expr::Assign(a) => {
            if let Some(path) = assignment_dest_register_path(&a.dest) {
                out.push((path, a.value.as_ref()));
            }
        }
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_register_path_writes(stmt, out);
            }
        }
        ast::Expr::If(i) => {
            collect_register_path_writes(&i.then, out);
            if let Some(else_expr) = &i.else_ {
                collect_register_path_writes(else_expr, out);
            }
        }
        ast::Expr::Try(t) => collect_register_path_writes(&t.body, out),
        _ => {}
    }
}

pub(crate) fn collect_register_path_reads(expr: &ast::Expr, out: &mut HashSet<(String, String)>) {
    match expr {
        ast::Expr::Path(path) if path.remainder.len() == 1 => {
            out.insert((path.base.clone(), path.remainder[0].clone()));
        }
        ast::Expr::Path(_) | ast::Expr::Ident(_) | ast::Expr::Lit(_) => {}
        ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => {}
        ast::Expr::Tuple(t) => t
            .elements
            .iter()
            .for_each(|e| collect_register_path_reads(e, out)),
        ast::Expr::Assign(a) => {
            // A register-path assignment destination is a write, not a read.
            if assignment_dest_register_path(&a.dest).is_none() {
                collect_register_path_reads(&a.dest, out);
            }
            collect_register_path_reads(&a.value, out);
        }
        ast::Expr::Let(l) => collect_register_path_reads(&l.value, out),
        ast::Expr::Binary(b) => {
            collect_register_path_reads(&b.lhs, out);
            collect_register_path_reads(&b.rhs, out);
        }
        ast::Expr::Unary(u) => collect_register_path_reads(&u.x, out),
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_register_path_reads(stmt, out);
            }
        }
        ast::Expr::Call(c) => {
            collect_register_path_reads(&c.callee, out);
            for arg in &c.arguments {
                collect_register_path_reads(arg, out);
            }
        }
        ast::Expr::Field(f) => collect_register_path_reads(&f.base, out),
        ast::Expr::If(i) => {
            collect_register_path_reads(&i.cond, out);
            collect_register_path_reads(&i.then, out);
            if let Some(e) = &i.else_ {
                collect_register_path_reads(e, out);
            }
        }
        ast::Expr::IndexAccess(i) => collect_register_path_reads(&i.base, out),
        ast::Expr::Slice(s) => collect_register_path_reads(&s.base, out),
        ast::Expr::Cast(c) => {
            collect_register_path_reads(&c.x, out);
            collect_register_path_reads(&c.width, out);
        }
        ast::Expr::Try(t) => {
            collect_register_path_reads(&t.body, out);
            for h in &t.handlers {
                collect_register_path_reads(&h.body, out);
            }
        }
        ast::Expr::Lambda(l) => collect_register_path_reads(&l.body, out),
    }
}
