// Derive the fixed registers an instruction touches without naming them in an
// operand. A behavior mentions them by path — `EFLAGS::zf = ...` writes a flag,
// `GPR::rax * dst` reads a register — and nothing else in the generated op
// records that, so the scheduler saw neither the flag dependencies of
// compare/setcc chains nor the rdx:rax protocol of the widening multiply.
//
// The result is the op's `implicit: [...]` section, which the operation macro
// turns into a `'static` table on every instance and `op_regs` resolves into
// physical defs and uses. Target-agnostic: it keys off register paths, never
// off register names.

/// The `implicit: [...]` entries for one instruction: an [`ImplicitReg`] per
/// register path its behavior reads or writes, sorted for stable output.
/// Program-counter classes are excluded — control flow is modeled by the
/// branch/terminator machinery, and threading `PC::pc` through the register
/// dependence graph would serialize every branch behind the previous one.
fn implicit_register_items(
    inst: &ast::Instruction,
    register_index_map: &HashMap<(String, String), u32>,
    pc_classes: &HashSet<String>,
) -> Vec<proc_macro2::TokenStream> {
    let mut reads = HashSet::new();
    collect_register_path_reads(&inst.behavior, &mut reads);
    let mut write_list = Vec::new();
    collect_register_path_writes(&inst.behavior, &mut write_list);
    let writes: HashSet<(String, String)> = write_list.into_iter().map(|(path, _)| path).collect();

    let mut paths: Vec<(String, String)> = reads.union(&writes).cloned().collect();
    paths.sort();

    let mut items = Vec::new();
    for path in paths {
        if pc_classes.contains(&path.0) {
            continue;
        }
        // A path over something that is not a register class (or names no such
        // register) is not a register access.
        let Some(index) = register_index_map.get(&path) else {
            continue;
        };
        let role = match (reads.contains(&path), writes.contains(&path)) {
            (true, true) => quote! { ReadWrite },
            (false, true) => quote! { Def },
            _ => quote! { Use },
        };
        let class_id = reg_class_id(&path.0);
        let index_lit = proc_macro2::Literal::u16_unsuffixed(*index as u16);
        items.push(quote! {
            tir::attributes::ImplicitReg {
                class: #class_id,
                index: #index_lit,
                role: tir::attributes::AttributeRole::#role,
            }
        });
    }
    items
}

/// The physical registers whose values are ORed into an existing `fp_flags`
/// register. This is scheduling metadata only. The architectural implicit
/// register remains a read/write register through `implicit_regs`.
fn implicit_or_update_items(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    float_classes: &HashSet<String>,
    register_index_map: &HashMap<(String, String), u32>,
    register_files: &HashMap<String, String>,
    fp_flag_registers: &HashSet<(String, String)>,
) -> Vec<proc_macro2::TokenStream> {
    if !operands
        .iter()
        .any(|(_, ty)| matches!(ty, Type::Struct(class) if float_classes.contains(class)))
    {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    let mut flag_assignments = 0;
    crate::utils::visit_exprs(&inst.behavior, &mut |expr| {
        let ast::Expr::Assign(assign) = expr else {
            return;
        };
        let Some((class, register)) = assignment_dest_register_path(&assign.dest) else {
            return;
        };
        if !fp_flag_registers.contains(&(class.clone(), register.clone())) {
            return;
        }
        flag_assignments += 1;
        let ast::Expr::Binary(binary) = assign.value.as_ref() else {
            return;
        };
        if binary.op != ast::BinOp::BitwiseOr {
            return;
        }
        let target_path = |expr: &ast::Expr| {
            assignment_dest_register_path(expr)
                == Some((class.clone(), register.clone()))
        };
        if !target_path(&binary.lhs) && !target_path(&binary.rhs) {
            return;
        }
        let Some(index) = register_index_map.get(&(class.clone(), register)) else {
            return;
        };
        candidates.push((class, *index));
    });

    if flag_assignments != 1 || candidates.len() != 1 {
        return Vec::new();
    }
    let (class, index) = candidates.pop().unwrap();
    let Some(file) = register_files.get(&class) else {
        return Vec::new();
    };
    if operands.iter().any(|(_, ty)| {
        matches!(ty, Type::Struct(class) if register_files.get(class) == Some(file))
    }) {
        return Vec::new();
    }
    // Helpers are inlined before codegen, so these two accesses must be the
    // assignment destination and its OR input. Any other access can observe
    // old flags through control flow, another output, or an aliased view.
    let mut accesses = 0;
    crate::utils::visit_exprs(&inst.behavior, &mut |expr| {
        let Some(path) = assignment_dest_register_path(expr) else {
            return;
        };
        if register_files.get(&path.0) == Some(file)
            && register_index_map.get(&path) == Some(&index)
        {
            accesses += 1;
        }
    });
    if accesses != 2 {
        return Vec::new();
    }
    let class_id = reg_class_id(&class);
    let index_lit = proc_macro2::Literal::u16_unsuffixed(index as u16);
    vec![quote! { (#class_id, #index_lit) }]
}
