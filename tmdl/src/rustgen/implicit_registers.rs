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
