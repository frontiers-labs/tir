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

/// An instruction whose behavior writes exactly one fixed register as a pure
/// function of other fixed-register reads and constants, taking no operands.
struct Definer<'a> {
    inst: &'a ast::Instruction,
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
    mnemonic: String,
    ops: Vec<(String, Type)>,
    cond: &'a ast::Expr,
    /// The then-arm's fixed-register writes, `(written register, rhs)`.
    then_writes: Vec<(FixedReg, &'a ast::Expr)>,
    /// Every fixed register the behavior reads by path.
    reads: HashSet<FixedReg>,
    isa_param_values: HashMap<String, i64>,
}

fn emit_fixed_register_rules<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    register_index_map: &HashMap<(String, String), u32>,
    register_name_map: &HashMap<(String, u32), String>,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    isel_rule_inits: &mut Vec<proc_macro2::TokenStream>,
) -> Result<(), TMDLError> {
    let float_classes: HashSet<String> = files
        .iter()
        .flat_map(|file| file.register_classes())
        .filter(|class| class.has_float_registers())
        .map(|class| class.name.clone())
        .collect();
    let polymorphic_classes: HashSet<String> = files
        .iter()
        .flat_map(|file| file.register_classes())
        .filter(|class| class.has_polymorphic_registers())
        .map(|class| class.name.clone())
        .collect();
    let mut definers: Vec<Definer<'a>> = Vec::new();
    let mut readers: Vec<Reader<'a>> = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        if behavior_uses_todo(&inst.behavior) {
            continue;
        }
        let resolved_params = resolve_params_for_instruction(inst, item_cache);
        let Some(mnemonic) = resolved_params
            .get("MNEMONIC")
            .and_then(|(_, value)| value.as_ref())
            .and_then(resolve_string)
        else {
            continue;
        };
        let _ = resolved_params;
        let isa_param_values = resolve_isa_param_values(inst, item_cache);
        let ops = resolve_operand_widths(
            resolve_operands_for_instruction(inst, item_cache),
            &isa_param_values,
        );

        if let Some(definer) = classify_definer(inst, &mnemonic, &ops, register_index_map) {
            definers.push(definer);
        } else if let Some(reader) =
            classify_reader(inst, &mnemonic, &ops, register_index_map, &isa_param_values)
        {
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
            isel_rule_emitters,
            isel_rule_inits,
        );
    }
    Ok(())
}

/// Emit both the quotient and remainder rules for a (definer, reader) pair. The
/// reader's then-arm writes the quotient — a bare division — to the dividend
/// register (`rax`) and the remainder — the Euclidean identity
/// `dividend - (dividend / divisor) * divisor` — to the register the definer
/// sets up (`rdx`). Each then-arm write becomes a value rule whose result lands
/// in that register, with the sibling register clobbered.
#[allow(clippy::too_many_arguments)]
fn emit_division_rules(
    definer: &Definer,
    reader: &Reader,
    register_index_map: &HashMap<(String, String), u32>,
    register_name_map: &HashMap<(String, u32), String>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    isel_rule_inits: &mut Vec<proc_macro2::TokenStream>,
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

    emit_one_division_rule(
        definer,
        reader,
        dividend_reg,
        dividend_reg,
        quotient_rhs,
        "quotient",
        register_index_map,
        register_name_map,
        float_classes,
        polymorphic_classes,
        isel_rule_emitters,
        isel_rule_inits,
    );

    // The remainder is the then-arm write to the sibling register (`rdx`); its
    // value is the Euclidean identity, matching the `remsi`/`remui` semantics.
    if let Some((_, remainder_rhs)) = reader.then_writes.iter().find(|(reg, _)| reg == sibling_reg) {
        emit_one_division_rule(
            definer,
            reader,
            dividend_reg,
            sibling_reg,
            remainder_rhs,
            "remainder",
            register_index_map,
            register_name_map,
            float_classes,
            polymorphic_classes,
            isel_rule_emitters,
            isel_rule_inits,
        );
    }
}

/// Whether an instruction is a fixed-register definer or reader — the shapes
/// `emit_fixed_register_rules` composes. A definer takes no register operand and
/// writes exactly one register path; a reader takes a register operand and its
/// behavior is `if COND { <register-path writes> } else { … }`.
fn is_fixed_register_shape(inst: &ast::Instruction, ops: &[(String, Type)]) -> bool {
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
    let mut writes = Vec::new();
    collect_register_path_writes(&inst.behavior, &mut writes);
    let [(_, rhs)] = writes.as_slice() else {
        return false;
    };
    referenced_operands(rhs, &register_operand_names(ops)).is_empty()
}

/// The `roles` schema entries for a fixed-register definer/reader op: a `Use`
/// slot for every register path it reads and a `Def` slot for every register
/// path it writes, so register allocation and liveness see the fixed-register
/// data flow the composed rules wire up. Empty for every other instruction.
fn fixed_register_role_items(
    inst: &ast::Instruction,
    ops: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
    register_name_map: &HashMap<(String, u32), String>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Vec<proc_macro2::TokenStream> {
    if !is_fixed_register_shape(inst, ops) {
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
            let ident = format_ident!("{}", slot);
            items.push(quote! { #ident: Use });
        }
    }
    for (class, regname) in &writes {
        let Some(name) = canonical_name(class, regname) else {
            continue;
        };
        let slot = fixed_write_slot_name(&name);
        if seen.insert(slot.clone()) {
            let ident = format_ident!("{}", slot);
            items.push(quote! { #ident: Def });
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

/// Two subgraphs are structurally identical: same node kinds, leaf payloads, and
/// children in order. Used to fold `a == a` (the composed guard after the
/// definer's write substitutes for its read) to a constant true.
fn subgraphs_equal(
    graph: &tir::sem::SemGraph,
    a: tir::graph::NodeId,
    b: tir::graph::NodeId,
) -> bool {
    use tir::graph::Dag;
    if a == b {
        return true;
    }
    if graph.get_node(a) != graph.get_node(b) {
        return false;
    }
    if graph.get_leaf_data(a) != graph.get_leaf_data(b) {
        return false;
    }
    let a_children: Vec<_> = graph.children(a).collect();
    let b_children: Vec<_> = graph.children(b).collect();
    if a_children.len() != b_children.len() {
        return false;
    }
    a_children
        .iter()
        .zip(&b_children)
        .all(|(&x, &y)| subgraphs_equal(graph, x, y))
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
    use tir::graph::Dag;
    let mut graph = tir::sem::SemGraph::new();
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
    let mut composed = tir::sem::SemGraph::new();
    let mut memo = HashMap::new();
    let composed_root = substitute_symbol_with_subgraph(
        &mut composed,
        &graph,
        *cond_root,
        written_symbol,
        *def_root,
        &mut memo,
    );

    if *composed.get_node(composed_root) != tir::sem::SymKind::Eq {
        return false;
    }
    let children: Vec<_> = composed.children(composed_root).collect();
    let [lhs, rhs] = children.as_slice() else {
        return false;
    };
    subgraphs_equal(&composed, *lhs, *rhs)
}

/// Copy `node`'s subgraph from `src` into `dst`, replacing every `Symbol` leaf
/// carrying `symbol` with a copy of `src`'s `replacement` subgraph.
fn substitute_symbol_with_subgraph(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    symbol: u32,
    replacement: tir::graph::NodeId,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    if *src.get_node(node) == tir::sem::SymKind::Symbol
        && let Some(tir::sem::SymPayload::SymbolId(id)) = src.get_leaf_data(node)
        && *id == symbol
    {
        let copied = copy_subgraph(dst, src, replacement, &mut HashMap::new());
        memo.insert(node.index(), copied);
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = src.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| substitute_symbol_with_subgraph(dst, src, child, symbol, replacement, memo))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(data) = src.get_leaf_data(node) {
        dst.set_leaf_data(copied, data.clone());
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    memo.insert(node.index(), copied);
    copied
}

/// Emit one division value rule (quotient or remainder) for a (definer, reader)
/// pair: lower `result_rhs` into a selection pattern, emit the definer as a
/// prelude and the reader (`idiv`/`div`) as the main instruction, routing `lhs`
/// into the dividend register (`rax`), the result out of `result_reg`, and
/// clobbering the register that is not the result.
#[allow(clippy::too_many_arguments)]
fn emit_one_division_rule(
    definer: &Definer,
    reader: &Reader,
    dividend_reg: &FixedReg,
    result_reg: &FixedReg,
    result_rhs: &ast::Expr,
    kind: &str,
    register_index_map: &HashMap<(String, String), u32>,
    register_name_map: &HashMap<(String, u32), String>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    isel_rule_inits: &mut Vec<proc_macro2::TokenStream>,
) {
    use tir::graph::Dag;

    let sibling_reg = &definer.written;
    let (dividend_class, dividend_index) = dividend_reg.clone();
    let result_is_dividend = result_reg == dividend_reg;

    // Lower the result value into the selection pattern.
    let mut pattern = tir::sem::SemGraph::new();
    let params = HashMap::new();
    let Some(lowering) = result_rhs.lower_to_sema_with_isa(
        &mut pattern,
        &params,
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
    let Some((divisor_name, Type::Struct(divisor_class))) =
        reader.ops.iter().find(|(_, ty)| matches!(ty, Type::Struct(_)))
    else {
        return;
    };
    let Some(&divisor_symbol) = lowering.variable_symbols.get(divisor_name) else {
        return;
    };

    let immediate_symbols = HashSet::new();
    let (canon_pattern, canon_root, forced_widths) =
        tir::sem::canonicalize_for_selection(&pattern, lowering.root, &immediate_symbols);
    let mut pattern_widths = tir::sem::infer_widths(&canon_pattern, |_| None);
    for (index, forced) in forced_widths.iter().enumerate() {
        if forced.is_some() {
            pattern_widths[index] = *forced;
        }
    }
    let (pattern_stmts, _root_var) = emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);

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
    let operand_register_call = emit_operand_register_call(
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
    let dividend_index_lit = proc_macro2::Literal::u16_unsuffixed(dividend_index);
    let sibling_index_lit = proc_macro2::Literal::u16_unsuffixed(sibling_reg.1);
    let lhs_symbol_lit = proc_macro2::Literal::u32_unsuffixed(lhs_symbol);
    let divisor_symbol_lit = proc_macro2::Literal::u32_unsuffixed(divisor_symbol);
    let divisor_name_lit = proc_macro2::Literal::string(divisor_name);
    let divisor_class_id = reg_class_id(divisor_class);
    let dividend_use_slot = proc_macro2::Literal::string(&fixed_read_slot_name(&dividend_name));
    let dividend_def_slot = proc_macro2::Literal::string(&fixed_write_slot_name(&dividend_name));
    let sibling_use_slot = proc_macro2::Literal::string(&fixed_read_slot_name(&sibling_name));
    let sibling_def_slot = proc_macro2::Literal::string(&fixed_write_slot_name(&sibling_name));

    // Whichever register holds this rule's result is defined as the result
    // virtual (`FixedDef`); the other written register is clobbered (`Physical`).
    let fixed_def = |class_id: &proc_macro2::TokenStream, index: &proc_macro2::Literal| {
        quote! {
            tir::attributes::RegisterAttr::FixedDef {
                id: result,
                class: #class_id,
                index: #index,
            }
        }
    };
    let clobber = |class_id: &proc_macro2::TokenStream, index: &proc_macro2::Literal| {
        quote! {
            tir::attributes::RegisterAttr::Physical {
                class: #class_id,
                index: #index,
            }
        }
    };
    let (dividend_def_attr, sibling_def_attr) = if result_is_dividend {
        (
            fixed_def(&class_id, &dividend_index_lit),
            clobber(&sibling_class_id, &sibling_index_lit),
        )
    } else {
        (
            clobber(&class_id, &dividend_index_lit),
            fixed_def(&sibling_class_id, &sibling_index_lit),
        )
    };

    let reader_builder = format_ident!("{}OpBuilder", &reader.inst.name);
    let definer_builder = format_ident!("{}OpBuilder", &definer.inst.name);
    let reader_lower = reader.inst.name.to_lowercase();
    let definer_lower = definer.inst.name.to_lowercase();
    let pattern_fn_ident =
        format_ident!("isel_pattern_{}_{}_via_{}", reader_lower, kind, definer_lower);
    let emit_fn_ident = format_ident!("emit_isel_{}_{}_via_{}", reader_lower, kind, definer_lower);
    let prelude_fn_ident =
        format_ident!("emit_isel_prelude_{}_{}_via_{}", definer_lower, kind, reader_lower);
    let rule_name_lit = proc_macro2::Literal::string(&format!(
        "{}+{} {}",
        definer.mnemonic, reader.mnemonic, kind
    ));
    let base_cost = canon_pattern.len() as u32 + 1;
    let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
    let reader_mnemonic_lit = proc_macro2::Literal::string(&reader.mnemonic);
    let definer_mnemonic_lit = proc_macro2::Literal::string(&definer.mnemonic);

    let shared_isas: Vec<String> = reader
        .inst
        .for_isas
        .iter()
        .filter(|isa| definer.inst.for_isas.contains(isa))
        .cloned()
        .collect();
    let pair_features = feature_slice(&shared_isas);

    isel_rule_emitters.push(quote! {
        fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
            use tir::graph::MutDag;
            let mut g = tir::sem::SemGraph::new();
            #(#pattern_stmts)*
            g
        }

        fn #prelude_fn_ident(
            context: &tir::Context,
            req: &tir::backend::isel::EmitRequest,
            m: &tir::backend::isel::RuleMatch,
        ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
            let _ = req;
            let lhs = m
                .value_binding(#lhs_symbol_lit)
                .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
            let builder = #definer_builder::new(context)
                .attr(
                    #dividend_use_slot,
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::FixedUse {
                            id: lhs.number(),
                            class: #class_id,
                            index: #dividend_index_lit,
                        },
                    ),
                )
                .attr(
                    #sibling_def_slot,
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::Physical {
                            class: #sibling_class_id,
                            index: #sibling_index_lit,
                        },
                    ),
                );
            Ok(Box::new(builder.build()))
        }

        fn #emit_fn_ident(
            context: &tir::Context,
            req: &tir::backend::isel::EmitRequest,
            m: &tir::backend::isel::RuleMatch,
        ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
            let lhs = m
                .value_binding(#lhs_symbol_lit)
                .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
            let divisor = m
                .value_binding(#divisor_symbol_lit)
                .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
            let result = req
                .results
                .first()
                .ok_or(tir::PassError::RewriteFailed(req.op_id()))?
                .number();
            let builder = #reader_builder::new(context)
                .attr(
                    #divisor_name_lit,
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::Virtual {
                            id: divisor.number(),
                            class: Some(#divisor_class_id),
                        },
                    ),
                )
                .attr(
                    #dividend_use_slot,
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::FixedUse {
                            id: lhs.number(),
                            class: #class_id,
                            index: #dividend_index_lit,
                        },
                    ),
                )
                .attr(
                    #dividend_def_slot,
                    tir::attributes::AttributeValue::Register(#dividend_def_attr),
                )
                .attr(
                    #sibling_use_slot,
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::Physical {
                            class: #sibling_class_id,
                            index: #sibling_index_lit,
                        },
                    ),
                )
                .attr(
                    #sibling_def_slot,
                    tir::attributes::AttributeValue::Register(#sibling_def_attr),
                );
            Ok(Box::new(builder.build()))
        }
    });

    isel_rule_inits.push(quote! {
        if features_enabled(features, #pair_features) {
            rules.push(
                tir::backend::isel::Rule::new(
                    #rule_name_lit,
                    #pattern_fn_ident(context),
                    (#base_cost_lit).max(
                        instruction_cost(#definer_mnemonic_lit)
                            + instruction_cost(#reader_mnemonic_lit),
                    ),
                    #emit_fn_ident,
                )
                .with_prelude_emitter(#prelude_fn_ident)
                .with_operand_constraints(vec![
                    (#lhs_symbol_lit, tir::graph::OperandConstraint::Register),
                    (#divisor_symbol_lit, tir::graph::OperandConstraint::Register),
                ])
                #operand_register_call,
            );
        }
    });
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
fn collect_register_path_writes<'a>(
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
fn collect_register_path_reads(expr: &ast::Expr, out: &mut HashSet<(String, String)>) {
    match expr {
        ast::Expr::Path(path) if path.remainder.len() == 1 => {
            out.insert((path.base.clone(), path.remainder[0].clone()));
        }
        ast::Expr::Path(_) | ast::Expr::Ident(_) | ast::Expr::Lit(_) => {}
        ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => {}
        ast::Expr::Assign(a) => {
            // A register-path assignment destination is a write, not a read.
            if assignment_dest_register_path(&a.dest).is_none() {
                collect_register_path_reads(&a.dest, out);
            }
            collect_register_path_reads(&a.value, out);
        }
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
    mnemonic: &str,
    ops: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<Definer<'a>> {
    if ops.iter().any(|(_, ty)| matches!(ty, Type::Struct(_))) {
        return None;
    }
    let mut writes = Vec::new();
    collect_register_path_writes(&inst.behavior, &mut writes);
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
        mnemonic: mnemonic.to_string(),
        written,
        write_rhs: rhs,
    })
}

/// Recognize a reader: the behavior is `if COND { <writes> } else { … }`, it
/// reads at least one fixed register, and it has at least one register operand.
fn classify_reader<'a>(
    inst: &'a ast::Instruction,
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
        mnemonic: mnemonic.to_string(),
        ops: ops.to_vec(),
        cond: &if_expr.cond,
        then_writes,
        reads,
        isa_param_values: isa_param_values.clone(),
    })
}
