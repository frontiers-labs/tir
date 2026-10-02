// Derive selection rules for instructions that compute through fixed physical
// registers (x86 `idiv`/`div`, whose dividend is the implicit `rdx:rax` pair
// and whose quotient/remainder land in `rax`/`rdx`). A *definer* writes one
// fixed register as a pure function of other fixed registers and constants
// (`cqo`: `rdx = sext(rax)`; `xor edx, edx`: `rdx = 0`); a *reader* reads those
// fixed registers under a guard and writes results back to them. Composing a
// definer's write into a reader's guard folds the case-split to a constant,
// leaving the honest single-width arm as a plain value pattern — the register
// analog of the flag definer/reader composition in `flag_emission.rs`.
//
// This is the target-agnostic entry point: it keys off "reads/writes fixed
// physical registers of an allocatable class", never off x86 register names.

/// A register written or read by path, as `(class, encoding index)`.
type FixedReg = (String, u16);

/// An instruction whose behavior writes exactly one fixed allocatable register
/// as a pure function of other fixed-register reads and constants, taking no
/// operands. Status flags it sets along the way (`xor edx, edx`) are side
/// effects, as they are for readers.
struct Definer<'a> {
    inst: &'a ast::Instruction,
    /// The op's registered name (`OPNAME`, falling back to `MNEMONIC`).
    op_name: String,
    mnemonic: String,
    written: FixedReg,
    /// The right-hand side of the single fixed-register write.
    write_rhs: &'a ast::Expr,
}

/// An instruction whose behavior is `if COND { <fixed writes> } else { … }` and
/// which reads fixed registers — the honest model of a fixed-register compute
/// instruction (`idiv`), case-split so a definer folds the guard.
struct Reader<'a> {
    inst: &'a ast::Instruction,
    /// The op's registered name (`OPNAME`, falling back to `MNEMONIC`).
    op_name: String,
    mnemonic: String,
    ops: Vec<(String, Type)>,
    cond: &'a ast::Expr,
    /// The then-arm's fixed-register writes, `(written register, rhs)`.
    then_writes: Vec<(FixedReg, &'a ast::Expr)>,
    /// Every fixed register the behavior reads by path.
    reads: HashSet<FixedReg>,
    isa_param_values: HashMap<String, i64>,
}

#[allow(clippy::too_many_arguments)]
fn emit_fixed_register_rules<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    register_index_map: &HashMap<(String, String), u32>,
    register_name_map: &HashMap<(String, u32), String>,
    flag_classes: &HashSet<String>,
    dialect: &str,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    rule_spec_idents: &mut Vec<proc_macro2::Ident>,
) -> Result<(), TMDLError> {
    let (float_classes, polymorphic_classes) = register_class_kinds(files);
    let mut definers: Vec<Definer<'a>> = Vec::new();
    let mut readers: Vec<Reader<'a>> = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        if behavior_uses_todo(&inst.behavior) {
            continue;
        }
        let resolved_params = resolve_params_for_instruction(inst, item_cache);
        let Some((mnemonic, op_name)) = instruction_names(&resolved_params) else {
            continue;
        };
        let isa_param_values = resolve_isa_param_values(inst, item_cache);
        let ops = resolve_operand_widths(
            resolve_operands_for_instruction(inst, item_cache),
            &isa_param_values,
        );

        if let Some(definer) = classify_definer(
            inst,
            &op_name,
            &mnemonic,
            &ops,
            register_index_map,
            flag_classes,
        ) {
            definers.push(definer);
        } else if let Some(reader) = classify_reader(
            inst,
            &op_name,
            &mnemonic,
            &ops,
            register_index_map,
            &isa_param_values,
        ) {
            readers.push(reader);
        }
    }

    for (definer, reader) in pair_definers_with_readers(&definers, &readers) {
        emit_division_rules(
            definer,
            reader,
            register_index_map,
            register_name_map,
            &float_classes,
            &polymorphic_classes,
            dialect,
            isel_rule_emitters,
            rule_spec_idents,
        );
    }
    Ok(())
}

/// Emit the rule for a (definer, reader) pair. The reader's then-arm writes
/// the quotient — a bare division — to the dividend register (`rax`) and the
/// remainder — the Euclidean identity
/// `dividend - (dividend / divisor) * divisor` — to the register the definer
/// sets up (`rdx`). Both are results of the one instruction pair, so they are
/// the results of one rule: the quotient first, the remainder second, each
/// landing in its register. A result nothing reads is a clobber.
#[allow(clippy::too_many_arguments)]
fn emit_division_rules(
    definer: &Definer,
    reader: &Reader,
    register_index_map: &HashMap<(String, String), u32>,
    register_name_map: &HashMap<(String, u32), String>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
    dialect: &str,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    rule_spec_idents: &mut Vec<proc_macro2::Ident>,
) {
    // The quotient is the then-arm write whose value is a bare division; its
    // register is the dividend/quotient register (`rax`). The sibling (`rdx`) is
    // the register the definer writes.
    let Some((dividend_reg, quotient_rhs)) = reader.then_writes.iter().find(|(_, rhs)| {
        matches!(
            **rhs,
            ast::Expr::Binary(ref b)
                if matches!(b.op, ast::BinOp::Div | ast::BinOp::UnsignedDiv)
        )
    }) else {
        return;
    };
    let sibling_reg = &definer.written;
    if dividend_reg == sibling_reg {
        return;
    }
    if !guard_folds_to_true(
        definer,
        reader,
        &sibling_reg.0,
        sibling_reg.1,
        register_index_map,
    ) {
        return;
    }
    // The remainder is the then-arm write to the sibling register (`rdx`); its
    // value is the Euclidean identity, matching the `remsi`/`remui` semantics.
    let remainder_rhs = reader
        .then_writes
        .iter()
        .find(|(reg, _)| reg == sibling_reg)
        .map(|(_, rhs)| *rhs);
    let (dividend_class, dividend_index) = dividend_reg.clone();

    // Lower every result through one symbol table, so the patterns name the
    // same operands.
    let results: Vec<&ast::Expr> = std::iter::once(*quotient_rhs).chain(remainder_rhs).collect();
    let mut graph = tir_symbolic::sem::SemGraph::<()>::new();
    let Some((roots, lowering)) = ast::Expr::lower_all_to_sema_with_isa(
        &results,
        &mut graph,
        &HashMap::new(),
        &reader.isa_param_values,
        register_index_map,
    ) else {
        return;
    };
    let Some(&lhs_symbol) = lowering
        .register_symbols
        .get(&(dividend_class.clone(), u32::from(dividend_index)))
    else {
        return;
    };
    // The single register operand is the divisor.
    let Some((divisor_name, Type::Struct(divisor_class))) = reader
        .ops
        .iter()
        .find(|(_, ty)| matches!(ty, Type::Struct(_)))
    else {
        return;
    };
    let Some(&divisor_symbol) = lowering.variable_symbols.get(divisor_name) else {
        return;
    };

    let patterns: Vec<SpecPattern> = roots
        .iter()
        .map(|&root| {
            let (canon_pattern, canon_root, forced_widths) =
                tir_symbolic::lang::canonicalize_for_selection(&graph, root, &HashSet::new());
            let pattern_widths = selection_pattern_widths(&canon_pattern, forced_widths);
            let (offset, typed) = intern_dag(&canon_pattern, canon_root, &pattern_widths);
            SpecPattern {
                offset,
                typed,
                float_width: None,
            }
        })
        .collect();

    // Both operands of a division are width-sensitive: their full width reaches
    // the result.
    let sensitive: HashSet<u32> = [lhs_symbol, divisor_symbol].into_iter().collect();
    let synthetic_ops = vec![
        ("__lhs".to_string(), Type::Struct(dividend_class.clone())),
        (divisor_name.clone(), Type::Struct(divisor_class.clone())),
    ];
    let synthetic_varsyms: HashMap<String, u32> = [
        ("__lhs".to_string(), lhs_symbol),
        (divisor_name.clone(), divisor_symbol),
    ]
    .into_iter()
    .collect();
    let operand_register_specs = operand_register_specs_for_ops(
        &synthetic_ops,
        &synthetic_varsyms,
        &sensitive,
        float_classes,
        polymorphic_classes,
    );

    let dividend_name = register_name_map
        .get(&(dividend_class.clone(), u32::from(dividend_index)))
        .cloned()
        .unwrap_or_else(|| dividend_class.clone());
    let sibling_name = register_name_map
        .get(&(sibling_reg.0.clone(), u32::from(sibling_reg.1)))
        .cloned()
        .unwrap_or_else(|| sibling_reg.0.clone());

    let class_id = reg_class_id(&dividend_class);
    let sibling_class_id = reg_class_id(&sibling_reg.0);
    let sibling_index = sibling_reg.1;
    let dividend_use_slot = fixed_read_slot_name(&dividend_name);
    let dividend_def_slot = fixed_write_slot_name(&dividend_name);
    let sibling_use_slot = fixed_read_slot_name(&sibling_name);
    let sibling_def_slot = fixed_write_slot_name(&sibling_name);

    let reader_op_ty = format_ident!("{}Op", &reader.inst.name);
    let definer_op_ty = format_ident!("{}Op", &definer.inst.name);
    let reader_lower = reader.inst.name.to_lowercase();
    let definer_lower = definer.inst.name.to_lowercase();
    let rule_key = format!("{}_via_{}", reader_lower, definer_lower);
    let prelude_key = format!("prelude_{}_via_{}", definer_lower, reader_lower);
    let rule_name = format!("{}+{}", definer.mnemonic, reader.mnemonic);

    let shared_isas: Vec<String> = reader
        .inst
        .for_isas
        .iter()
        .filter(|isa| definer.inst.for_isas.contains(isa))
        .cloned()
        .collect();

    // A definer that extends the dividend (`cdq`) reads its register and has a
    // slot for it; one that only clears the sibling (`xor edx, edx`) has none.
    let mut definer_reads = HashSet::new();
    collect_register_path_reads(&definer.inst.behavior, &mut definer_reads);
    let definer_reads_dividend = definer_reads.iter().any(|(class, regname)| {
        *class == dividend_class
            && register_index_map.get(&(class.clone(), regname.clone()))
                == Some(&u32::from(dividend_index))
    });
    let mut prelude_attrs = Vec::new();
    if definer_reads_dividend {
        prelude_attrs.push(emit_attr_fixed_use(
            &dividend_use_slot,
            lhs_symbol,
            &class_id,
            dividend_index,
        ));
    }
    prelude_attrs.push(emit_attr_physical(
        &sibling_def_slot,
        &sibling_class_id,
        sibling_index,
    ));
    let (prelude_ts, prelude_spec) = emit_emitter_spec(
        &prelude_key,
        dialect,
        &definer.op_name,
        &definer_op_ty,
        &prelude_attrs,
        &definer.inst.name,
    );

    // Each written register is defined as the result it holds; one holding no
    // result of the rule is clobbered.
    let sibling_def_attr = if remainder_rhs.is_some() {
        emit_attr_result_fixed_def(&sibling_def_slot, 1, &sibling_class_id, sibling_index)
    } else {
        emit_attr_physical(&sibling_def_slot, &sibling_class_id, sibling_index)
    };
    let emit_attrs = [
        emit_attr_value(divisor_name, divisor_symbol),
        emit_attr_fixed_use(&dividend_use_slot, lhs_symbol, &class_id, dividend_index),
        emit_attr_result_fixed_def(&dividend_def_slot, 0, &class_id, dividend_index),
        emit_attr_physical(&sibling_use_slot, &sibling_class_id, sibling_index),
        sibling_def_attr,
    ];
    let (emitter_ts, emit_spec) = emit_emitter_spec(
        &rule_key,
        dialect,
        &reader.op_name,
        &reader_op_ty,
        &emit_attrs,
        &reader.inst.name,
    );
    let constraints = [
        constraint_entry(
            lhs_symbol,
            quote! { tir::backend::isel::OperandConstraint::Register },
        ),
        constraint_entry(
            divisor_symbol,
            quote! { tir::backend::isel::OperandConstraint::Register },
        ),
    ];
    let (rule_ts, rule_ident) = emit_rule_spec(
        &rule_key,
        &rule_name,
        &shared_isas,
        &patterns[0],
        &patterns[1..],
        quote! { tir::backend::isel::RuleKind::Value },
        &[&prelude_spec, &emit_spec],
        &constraints,
        &operand_register_specs,
        None,
        &[],
        None,
        FpFlags::None,
    );
    isel_rule_emitters.push(quote! {
        #prelude_ts
        #emitter_ts
        #rule_ts
    });
    rule_spec_idents.push(rule_ident);
}

/// Whether an instruction is a fixed-register definer or reader — the shapes
/// `emit_fixed_register_rules` composes. A definer takes no register operand and
/// writes exactly one non-flag register path; a reader takes a register operand
/// and its behavior is `if COND { <register-path writes> } else { … }`.
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

/// The register-path writes a definer is judged by: all but status flags.
fn definer_writes<'a>(
    inst: &'a ast::Instruction,
    flag_classes: &HashSet<String>,
) -> Vec<((String, String), &'a ast::Expr)> {
    let mut writes = Vec::new();
    collect_register_path_writes(&inst.behavior, &mut writes);
    writes.retain(|((class, _), _)| !flag_classes.contains(class));
    writes
}

/// The register ports of a fixed-register definer/reader op: a use slot for
/// every register path it reads and a def slot for every register path it
/// writes, so register allocation and liveness see the fixed-register data flow
/// the composed rules wire up. Empty for every other instruction. Returned as
/// `(slot, class, is_def)`, reads before writes and each group sorted — the
/// order the composed emitters bind them in.
fn fixed_register_role_items(
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

/// The attribute name a fixed register's incoming value binds through (`rax`).
fn fixed_read_slot_name(reg_name: &str) -> String {
    reg_name.to_string()
}

/// The attribute name a fixed register's result binds through (`rax_def`).
fn fixed_write_slot_name(reg_name: &str) -> String {
    format!("{reg_name}_def")
}

/// Substitute the definer's write into the reader's guard, then fold: prove the
/// composed condition is `Eq(x, x)` (structurally), i.e. the definer establishes
/// exactly the region the reader's single-width arm is valid in. The definer's
/// write and the reader's guard right-hand side share the same expression by
/// construction (the honest model writes the guard as the definer's value), so
/// after aliasing the written register's read to the write the two sides of the
/// comparison are structurally identical. Returns `true` only on that proof.
fn guard_folds_to_true(
    definer: &Definer,
    reader: &Reader,
    written_symbol_class: &str,
    written_index: u16,
    register_index_map: &HashMap<(String, String), u32>,
) -> bool {
    let mut graph = tir_symbolic::sem::SemGraph::new();
    let params = HashMap::new();
    let Some((roots, lowering)) = ast::Expr::lower_all_to_sema_with_isa(
        &[reader.cond, definer.write_rhs],
        &mut graph,
        &params,
        &reader.isa_param_values,
        register_index_map,
    ) else {
        return false;
    };
    let [cond_root, def_root] = roots.as_slice() else {
        return false;
    };
    let Some(&written_symbol) = lowering
        .register_symbols
        .get(&(written_symbol_class.to_string(), u32::from(written_index)))
    else {
        return false;
    };

    // Rebuild the condition, replacing the written register's read (its symbol
    // leaf) with the definer's write subgraph.
    let mut composed = tir_symbolic::sem::SemGraph::new();
    let mut memo = HashMap::new();
    let composed_root = substitute_symbol_with_subgraph(
        &mut composed,
        &graph,
        *cond_root,
        written_symbol,
        *def_root,
        &mut memo,
    );

    if *composed.get_node(composed_root) != tir_symbolic::lang::SymKind::Eq {
        return false;
    }
    let children: Vec<_> = composed.children(composed_root).collect();
    let [lhs, rhs] = children.as_slice() else {
        return false;
    };
    // The composed guard folds to true when its two sides are the same
    // expression: the definer establishes exactly the region the reader is
    // valid in.
    composed.subgraph_eq(*lhs, &composed, *rhs)
}

/// Copy `node`'s subgraph from `src` into `dst`, replacing every `Symbol` leaf
/// carrying `symbol` with a copy of `src`'s `replacement` subgraph.
fn substitute_symbol_with_subgraph(
    dst: &mut tir_symbolic::sem::SemGraph,
    src: &tir_symbolic::sem::SemGraph,
    node: tir_adt::NodeId,
    symbol: u32,
    replacement: tir_adt::NodeId,
    memo: &mut HashMap<usize, tir_adt::NodeId>,
) -> tir_adt::NodeId {
    use tir_symbolic::lang::{SymKind, SymPayload};
    use tir_symbolic::sem::{CopyAction, copy_subgraph_with};
    copy_subgraph_with(
        dst,
        src,
        node,
        memo,
        &mut |dst, node| match src.get_leaf_data(node) {
            Some(SymPayload::SymbolId(id))
                if *id == symbol && *src.get_node(node) == SymKind::Symbol =>
            {
                CopyAction::Replace(copy_subgraph(dst, src, replacement, &mut HashMap::new()))
            }
            _ => CopyAction::Keep,
        },
    )
}

/// Pair each definer with every reader that reads the register the definer
/// writes and shares an ISA — the candidate (definer, reader) compositions.
fn pair_definers_with_readers<'a, 'b>(
    definers: &'b [Definer<'a>],
    readers: &'b [Reader<'a>],
) -> Vec<(&'b Definer<'a>, &'b Reader<'a>)> {
    let mut pairs = Vec::new();
    for definer in definers {
        for reader in readers {
            if !reader.reads.contains(&definer.written) {
                continue;
            }
            let shares_isa = reader
                .inst
                .for_isas
                .iter()
                .any(|isa| definer.inst.for_isas.contains(isa));
            if shares_isa {
                pairs.push((definer, reader));
            }
        }
    }
    pairs
}

/// The register paths an expression assigns, as `((class, register name), rhs)`,
/// walking blocks, both arms of an `if`, and the no-trap body of a `try`.
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

/// The register paths an expression reads (register paths in value position),
/// as `(class, register name)`.
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

/// Resolve a `(class, register name)` pair to `(class, encoding index)`.
fn resolve_fixed_reg(
    (class, name): &(String, String),
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<FixedReg> {
    let index = register_index_map.get(&(class.clone(), name.clone()))?;
    u16::try_from(*index).ok().map(|idx| (class.clone(), idx))
}

/// Recognize a definer: no register operands and a behavior that writes exactly
/// one fixed register whose right-hand side references no operands (a pure
/// function of fixed-register reads and constants).
fn classify_definer<'a>(
    inst: &'a ast::Instruction,
    op_name: &str,
    mnemonic: &str,
    ops: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
    flag_classes: &HashSet<String>,
) -> Option<Definer<'a>> {
    if ops.iter().any(|(_, ty)| matches!(ty, Type::Struct(_))) {
        return None;
    }
    let writes = definer_writes(inst, flag_classes);
    let [(path, rhs)] = writes.as_slice() else {
        return None;
    };
    let written = resolve_fixed_reg(path, register_index_map)?;
    // The right-hand side must be a pure function of fixed registers and
    // constants: it may reference no operand identifiers.
    let operand_names = register_operand_names(ops);
    if !referenced_operands(rhs, &operand_names).is_empty() {
        return None;
    }
    Some(Definer {
        inst,
        op_name: op_name.to_string(),
        mnemonic: mnemonic.to_string(),
        written,
        write_rhs: rhs,
    })
}

/// Recognize a reader: the behavior is `if COND { <writes> } else { … }`, it
/// reads at least one fixed register, and it has at least one register operand.
fn classify_reader<'a>(
    inst: &'a ast::Instruction,
    op_name: &str,
    mnemonic: &str,
    ops: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
    isa_param_values: &HashMap<String, i64>,
) -> Option<Reader<'a>> {
    if !ops.iter().any(|(_, ty)| matches!(ty, Type::Struct(_))) {
        return None;
    }
    let ast::Expr::If(if_expr) = unwrap_single_stmt(&inst.behavior) else {
        return None;
    };
    if_expr.else_.as_deref()?;

    let mut then_paths = Vec::new();
    collect_register_path_writes(&if_expr.then, &mut then_paths);
    let then_writes: Vec<(FixedReg, &ast::Expr)> = then_paths
        .iter()
        .filter_map(|(path, rhs)| Some((resolve_fixed_reg(path, register_index_map)?, *rhs)))
        .collect();
    if then_writes.is_empty() {
        return None;
    }

    let mut read_paths = HashSet::new();
    collect_register_path_reads(&inst.behavior, &mut read_paths);
    let reads: HashSet<FixedReg> = read_paths
        .iter()
        .filter_map(|path| resolve_fixed_reg(path, register_index_map))
        .collect();
    if reads.is_empty() {
        return None;
    }

    Some(Reader {
        inst,
        op_name: op_name.to_string(),
        mnemonic: mnemonic.to_string(),
        ops: ops.to_vec(),
        cond: &if_expr.cond,
        then_writes,
        reads,
        isa_param_values: isa_param_values.clone(),
    })
}
