/// Derive the flag-mediated selection rules for an ISA (x86 EFLAGS, AArch64
/// PSTATE): flag definers compose with flag-guarded branches into conditional
/// branch rules and with flag-reading instructions into `If`-rooted value rules.
#[allow(clippy::too_many_arguments)]
fn emit_flag_rules<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    register_index_map: &HashMap<(String, String), u32>,
    pc_classes: &HashSet<String>,
    flag_classes: &HashSet<String>,
    dialect: &str,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    rule_spec_idents: &mut Vec<proc_macro2::Ident>,
) -> Result<(), TMDLError> {
    if flag_classes.is_empty() {
        return Ok(());
    }

    let mut definers: Vec<(FlagInst<'a>, FlagDefinerSemantics)> = Vec::new();
    let mut branches: Vec<(FlagInst<'a>, FlagBranchSemantics)> = Vec::new();
    let mut readers: Vec<(FlagInst<'a>, FlagReaderSemantics)> = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        // Unmodeled (`todo()`) semantics produce no rules of any kind.
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
        let op_name = resolved_params
            .get("OPNAME")
            .and_then(|(_, value)| value.as_ref())
            .and_then(resolve_string)
            .unwrap_or_else(|| mnemonic.clone());
        let isa_param_values = resolve_isa_param_values(inst, item_cache);
        let ops = resolve_operand_widths(
            resolve_operands_for_instruction(inst, item_cache),
            &isa_param_values,
        );
        let numeric_params: HashMap<String, i64> = resolved_params
            .into_iter()
            .filter_map(|(name, (_ty, value))| match value {
                Some(ast::Expr::Lit(ast::Lit::Int(li))) => {
                    Some((name, parse_literal_value(&li) as i64))
                }
                _ => None,
            })
            .collect();
        let info = FlagInst {
            inst,
            ops,
            constraints: resolve_operand_constraints_for_instruction(inst, item_cache),
            op_name,
            mnemonic,
            isa_param_values,
        };
        if let Some(sem) = analyze_flag_definer_semantics(
            inst,
            &info.ops,
            &numeric_params,
            &info.isa_param_values,
            register_index_map,
            flag_classes,
            pc_classes,
        ) {
            definers.push((info, sem));
        } else if let Some(sem) = analyze_flag_branch_semantics(
            inst,
            &info.ops,
            &numeric_params,
            &info.isa_param_values,
            register_index_map,
            flag_classes,
            pc_classes,
        ) {
            branches.push((info, sem));
        } else if let Some(sem) = analyze_flag_reader_semantics(
            inst,
            &info.ops,
            &numeric_params,
            &info.isa_param_values,
            register_index_map,
            flag_classes,
            pc_classes,
        ) {
            readers.push((info, sem));
        }
    }

    let mut emitted_preludes: HashSet<String> = HashSet::new();
    emit_flag_branch_rules(
        files,
        &definers,
        &branches,
        &mut emitted_preludes,
        dialect,
        isel_rule_emitters,
        rule_spec_idents,
    );
    emit_flag_reader_rules(
        files,
        &definers,
        &readers,
        &mut emitted_preludes,
        dialect,
        isel_rule_emitters,
        rule_spec_idents,
    );
    emit_aliased_zero_branch_rules(
        files,
        &definers,
        &branches,
        dialect,
        isel_rule_emitters,
        rule_spec_idents,
    );
    Ok(())
}

/// Compose each flag definer with each flag-guarded branch: the definer's
/// per-flag semantics substitute into the branch's condition, and when the
/// composition is provably one canonical comparison over the definer's operands
/// the pair registers a [`RuleKind::CondBranch`] rule whose emission is the
/// definer followed by the branch — two real instructions from TMDL alone.
fn emit_flag_branch_rules(
    files: &[ast::File],
    definers: &[(FlagInst<'_>, FlagDefinerSemantics)],
    branches: &[(FlagInst<'_>, FlagBranchSemantics)],
    emitted_preludes: &mut HashSet<String>,
    dialect: &str,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    rule_spec_idents: &mut Vec<proc_macro2::Ident>,
) {
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
    for (b, b_sem) in branches {
        for (d, d_sem) in definers {
            if d_sem.class != b_sem.class {
                continue;
            }
            let shared_isas: Vec<String> = b
                .inst
                .for_isas
                .iter()
                .filter(|isa| d.inst.for_isas.contains(isa))
                .cloned()
                .collect();
            if shared_isas.is_empty() {
                continue;
            }
            if !b_sem
                .flag_symbols
                .values()
                .all(|index| d_sem.flag_roots.contains_key(index))
            {
                continue;
            }
            // The canonical comparisons are binary: exactly two operands.
            if d_sem.variable_symbols.len() != 2 {
                continue;
            }
            let Some(symbols) = definer_comparison_symbols(files, d, d_sem) else {
                continue;
            };

            let mut spliced = tir_symbolic::sem::SemGraph::new();
            let substitute: HashMap<u32, tir_graph::NodeId> = b_sem
                .flag_symbols
                .iter()
                .map(|(symbol, index)| (*symbol, d_sem.flag_roots[index]))
                .collect();
            let spliced_root = compose_guard_with_definer(
                &mut spliced,
                &b_sem.graph,
                b_sem.root,
                &substitute,
                &d_sem.graph,
                &mut HashMap::new(),
                &mut HashMap::new(),
            );
            let (composed, _) = fold_constant_subtrees(&spliced, spliced_root);

            let Some((candidate, candidate_root)) =
                find_equivalent_comparison(&composed, &symbols)
            else {
                continue;
            };

            let immediate_symbols = definer_immediate_symbols(d, d_sem);
            let (canon_pattern, canon_root, forced_widths) = tir_symbolic::lang::canonicalize_for_selection(
                &candidate,
                candidate_root,
                &immediate_symbols,
            );
            let mut pattern_widths = tir_symbolic::lang::infer_widths(&canon_pattern, |_| None);
            for (index, forced) in forced_widths.iter().enumerate() {
                if forced.is_some() {
                    pattern_widths[index] = *forced;
                }
            }
            let (offset, typed) = intern_dag(&canon_pattern, canon_root, &pattern_widths);
            let pattern_spec = SpecPattern {
                offset,
                typed,
                float_width: None,
            };
            let operand_register_specs = operand_register_specs_for_ops(
                &d.ops,
                &d_sem.variable_symbols,
                &width_sensitive_symbols(&canon_pattern, &pattern_widths),
                &float_classes,
                &polymorphic_classes,
            );
            let imm_range_entries = imm_range_spec_entries(&immediate_operand_ranges(
                &d_sem.graph,
                &d.ops,
                &d_sem.variable_symbols,
                &d.constraints,
            ));

            let target_symbol = d_sem
                .variable_symbols
                .values()
                .max()
                .map_or(0, |max| max + 1);
            let d_lower = d.inst.name.to_lowercase();
            let b_lower = b.inst.name.to_lowercase();
            let rule_key = format!("{}_via_{}", b_lower, d_lower);
            let rule_name = format!("{}+{}", d.mnemonic, b.mnemonic);
            let target_symbol_lit = proc_macro2::Literal::u32_unsuffixed(target_symbol);
            let b_op_ty_ident = format_ident!("{}Op", &b.inst.name);

            let (prelude_ts, prelude_shim, operand_constraint_entries) =
                emit_flag_definer_prelude(d, d_sem, emitted_preludes, dialect);
            isel_rule_emitters.push(prelude_ts);

            let emit_attrs = [emit_attr_block(&b_sem.target_operand, target_symbol)];
            let (emitter_ts, emit_shim) = emit_emitter_spec(
                &rule_key,
                dialect,
                &b.op_name,
                &b_op_ty_ident,
                &emit_attrs,
                &b.inst.name,
            );
            let (rule_ts, rule_ident) = emit_rule_spec(
                &rule_key,
                &rule_name,
                &shared_isas,
                &pattern_spec,
                &[&d.inst.name, &b.inst.name],
                quote! {
                    tir::backend::isel::RuleKind::CondBranch {
                        target_symbol: #target_symbol_lit,
                    }
                },
                Some(&prelude_shim),
                &emit_shim,
                &operand_constraint_entries,
                &operand_register_specs,
                None,
                &imm_range_entries,
                None,
            );
            isel_rule_emitters.push(quote! {
                #emitter_ts
                #rule_ts
            });
            rule_spec_idents.push(rule_ident);
        }
    }
}

/// Copy `node`'s subgraph into `dst`, rewriting every symbol id found in `map`
/// to its mapped id. Unlike `copy_subgraph_remap_symbols` the mapping is fixed
/// (not fresh-per-symbol), so two operand symbols can be aliased onto one.
fn copy_subgraph_alias(
    dst: &mut tir_symbolic::sem::SemGraph,
    src: &tir_symbolic::sem::SemGraph,
    node: tir_graph::NodeId,
    map: &HashMap<u32, u32>,
    memo: &mut HashMap<usize, tir_graph::NodeId>,
) -> tir_graph::NodeId {
    use tir_graph::{Dag, MutDag};
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let children: Vec<tir_graph::NodeId> = src.children(node).collect();
    let copied_children: Vec<tir_graph::NodeId> = children
        .into_iter()
        .map(|child| copy_subgraph_alias(dst, src, child, map, memo))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(data) = src.get_leaf_data(node) {
        let data = match data {
            tir_symbolic::lang::SymPayload::SymbolId(id) if map.contains_key(id) => {
                tir_symbolic::lang::SymPayload::SymbolId(map[id])
            }
            other => other.clone(),
        };
        dst.set_leaf_data(copied, data);
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    memo.insert(node.index(), copied);
    copied
}

/// A single-symbol comparison against a literal zero (`Ne(s0, 0)`/`Eq(s0, 0)`),
/// the SMT candidate an aliased flag definer's condition proves against.
fn zero_vs_candidate(
    kind: tir_symbolic::lang::SymKind,
    width: u32,
) -> (tir_symbolic::sem::SemGraph, tir_graph::NodeId) {
    use tir_graph::MutDag;
    let mut g = tir_symbolic::sem::SemGraph::new();
    let s = g.add_node(tir_symbolic::lang::SymKind::Symbol);
    g.set_leaf_data(s, tir_symbolic::lang::SymPayload::SymbolId(0));
    let z = g.add_node(tir_symbolic::lang::SymKind::Constant);
    g.set_leaf_data(z, tir_symbolic::sem::int_payload(width, 0, false));
    let root = g.add_node(kind);
    g.add_edge(root, s);
    g.add_edge(root, z);
    (g, root)
}

/// The `Eq`/`Ne`-vs-zero comparison the composed aliased condition is provably
/// equivalent to, proven at the operand's architectural width.
fn zero_equivalent(
    composed: &tir_symbolic::sem::SemGraph,
    symbol_widths: &[u32],
) -> Option<tir_symbolic::lang::SymKind> {
    use tir_symbolic::sem::{EquivalenceOracle, FuzzOracle, SmtOracle};
    use tir_symbolic::lang::SymKind;
    let fuzz = FuzzOracle::default();
    for kind in [SymKind::Ne, SymKind::Eq] {
        let (candidate, _) = zero_vs_candidate(kind, symbol_widths[0]);
        if fuzz.equivalent(composed, &candidate, symbol_widths)
            && SmtOracle.equivalent(composed, &candidate, symbol_widths)
        {
            return Some(kind);
        }
    }
    None
}

/// The emitted zero-branch pattern in the `zext(0b0, W)` shape the bare-i1
/// bridge injects: `Ne(s0, zext(0, Wsym))` / `Eq(s0, zext(0, Wsym))`, so the
/// derived `test c, c` + `jne`/`je` rule covers a bare boolean guard.
fn zero_branch_pattern(
    kind: tir_symbolic::lang::SymKind,
    width_symbol: u32,
) -> (tir_symbolic::sem::SemGraph, tir_graph::NodeId) {
    use tir_graph::MutDag;
    let mut g = tir_symbolic::sem::SemGraph::new();
    let s = g.add_node(tir_symbolic::lang::SymKind::Symbol);
    g.set_leaf_data(s, tir_symbolic::lang::SymPayload::SymbolId(0));
    let zero = g.add_node(tir_symbolic::lang::SymKind::Constant);
    g.set_leaf_data(zero, tir_symbolic::sem::int_payload(1, 0, false));
    let wsym = g.add_node(tir_symbolic::lang::SymKind::Symbol);
    g.set_leaf_data(wsym, tir_symbolic::lang::SymPayload::SymbolId(width_symbol));
    let zext = g.add_node(tir_symbolic::lang::SymKind::ZExt);
    g.add_edge(zext, zero);
    g.add_edge(zext, wsym);
    let root = g.add_node(kind);
    g.add_edge(root, s);
    g.add_edge(root, zext);
    (g, root)
}

/// Compose each flag-guarded branch with a two-register flag definer whose
/// operands are aliased to one symbol: `test c, c` sets the flags of `c & c`,
/// so with `jne`/`je` the condition is provably `Ne(c, 0)`/`Eq(c, 0)`. Emitted
/// in the bare-i1 bridge's `zext(0b0, W)` zero shape, the pair covers a bare
/// boolean guard with a real derived rule — retiring the hand-written
/// branch-if-nonzero fallback on targets (x86) with no direct zero-branch. The
/// definer's two operand slots both bind from the single matched value.
fn emit_aliased_zero_branch_rules(
    files: &[ast::File],
    definers: &[(FlagInst<'_>, FlagDefinerSemantics)],
    branches: &[(FlagInst<'_>, FlagBranchSemantics)],
    dialect: &str,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    rule_spec_idents: &mut Vec<proc_macro2::Ident>,
) {
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
    let mut emitted_preludes: HashSet<String> = HashSet::new();
    for (b, b_sem) in branches {
        for (d, d_sem) in definers {
            if d_sem.class != b_sem.class {
                continue;
            }
            let shared_isas: Vec<String> = b
                .inst
                .for_isas
                .iter()
                .filter(|isa| d.inst.for_isas.contains(isa))
                .cloned()
                .collect();
            if shared_isas.is_empty() {
                continue;
            }
            if !b_sem
                .flag_symbols
                .values()
                .all(|index| d_sem.flag_roots.contains_key(index))
            {
                continue;
            }
            // Exactly two register operands of one class (no immediate): the
            // aliased pair `test c, c`.
            if d_sem.variable_symbols.len() != 2 {
                continue;
            }
            let reg_ops: Vec<(&String, &String, u32)> = d
                .ops
                .iter()
                .filter_map(|(name, ty)| {
                    let Type::Struct(class) = ty else { return None };
                    let &sym = d_sem.variable_symbols.get(name)?;
                    Some((name, class, sym))
                })
                .collect();
            if reg_ops.len() != 2 {
                continue;
            }
            let (name_a, class_a, sym_a) = reg_ops[0];
            let (name_b, class_b, sym_b) = reg_ops[1];
            if class_a != class_b {
                continue;
            }
            let Some(width) = register_class_width_with_isa(files, class_a, &d.isa_param_values)
            else {
                continue;
            };

            let map = HashMap::from([(sym_a, 0u32), (sym_b, 0u32)]);
            let mut aliased_graph = tir_symbolic::sem::SemGraph::new();
            let mut alias_memo: HashMap<usize, tir_graph::NodeId> = HashMap::new();
            let aliased_roots: HashMap<u32, tir_graph::NodeId> = d_sem
                .flag_roots
                .iter()
                .map(|(&index, &root)| {
                    (
                        index,
                        copy_subgraph_alias(
                            &mut aliased_graph,
                            &d_sem.graph,
                            root,
                            &map,
                            &mut alias_memo,
                        ),
                    )
                })
                .collect();

            let mut spliced = tir_symbolic::sem::SemGraph::new();
            let substitute: HashMap<u32, tir_graph::NodeId> = b_sem
                .flag_symbols
                .iter()
                .map(|(symbol, index)| (*symbol, aliased_roots[index]))
                .collect();
            let spliced_root = compose_guard_with_definer(
                &mut spliced,
                &b_sem.graph,
                b_sem.root,
                &substitute,
                &aliased_graph,
                &mut HashMap::new(),
                &mut HashMap::new(),
            );
            let (composed, _) = fold_constant_subtrees(&spliced, spliced_root);

            let Some(kind) = zero_equivalent(&composed, &[width]) else {
                continue;
            };

            let width_symbol = 1u32;
            let (pattern, root) = zero_branch_pattern(kind, width_symbol);
            let no_immediates: HashSet<u32> = HashSet::new();
            let (canon_pattern, canon_root, forced_widths) =
                tir_symbolic::lang::canonicalize_for_selection(&pattern, root, &no_immediates);
            let mut pattern_widths = tir_symbolic::lang::infer_widths(&canon_pattern, |_| None);
            for (index, forced) in forced_widths.iter().enumerate() {
                if forced.is_some() {
                    pattern_widths[index] = *forced;
                }
            }
            let (offset, typed) = intern_dag(&canon_pattern, canon_root, &pattern_widths);
            let pattern_spec = SpecPattern {
                offset,
                typed,
                float_width: None,
            };

            let prelude_key = format!("flag_definer_{}_aliased", d.inst.name.to_lowercase());
            let d_op_ty_ident = format_ident!("{}Op", &d.inst.name);
            // Both operands read the same bound value (the aliased pair).
            let prelude_attrs = [
                emit_attr_value(name_a, 0),
                emit_attr_value(name_b, 0),
            ];
            let (prelude_ts, prelude_shim) = emit_emitter_spec(
                &prelude_key,
                dialect,
                &d.op_name,
                &d_op_ty_ident,
                &prelude_attrs,
                &d.inst.name,
            );
            if emitted_preludes.insert(d.inst.name.clone()) {
                isel_rule_emitters.push(prelude_ts);
            }

            let target_symbol = 2u32;
            let target_symbol_lit = proc_macro2::Literal::u32_unsuffixed(target_symbol);
            let b_op_ty_ident = format_ident!("{}Op", &b.inst.name);
            let b_lower = b.inst.name.to_lowercase();
            let d_lower = d.inst.name.to_lowercase();
            let rule_key = format!("{}_via_{}_selfzero", b_lower, d_lower);
            let rule_name = format!("{}+{}(self-zero)", d.mnemonic, b.mnemonic);
            // The definer compares the aliased operand against zero, so it reads
            // every bit of the register: a narrower value must not bind.
            let operand_register_specs = operand_register_specs(
                &[(0, class_a.clone())],
                &HashSet::from([0]),
                &float_classes,
                &polymorphic_classes,
            );

            let emit_attrs = [emit_attr_block(&b_sem.target_operand, target_symbol)];
            let (emitter_ts, emit_shim) = emit_emitter_spec(
                &rule_key,
                dialect,
                &b.op_name,
                &b_op_ty_ident,
                &emit_attrs,
                &b.inst.name,
            );
            let constraints =
                [constraint_entry(0, quote! { tir::graph::OperandConstraint::Register })];
            let (rule_ts, rule_ident) = emit_rule_spec(
                &rule_key,
                &rule_name,
                &shared_isas,
                &pattern_spec,
                &[&d.inst.name, &b.inst.name],
                quote! {
                    tir::backend::isel::RuleKind::CondBranch {
                        target_symbol: #target_symbol_lit,
                    }
                },
                Some(&prelude_shim),
                &emit_shim,
                &constraints,
                &operand_register_specs,
                None,
                &[],
                None,
            );
            isel_rule_emitters.push(quote! {
                #emitter_ts
                #rule_ts
            });
            rule_spec_idents.push(rule_ident);
        }
    }
}

/// Compose each flag definer with each flag-reading value instruction: the
/// definer's per-flag semantics substitute into the reader's condition, and when
/// the composition is provably one canonical comparison the pair registers an
/// `If`-rooted rule whose prelude emits the definer and whose emitter is the
/// reader (`cset`/`setcc` or `csel`/`cmov`).
/// Each ISA's transitive `requires` set. An instruction tagged with ISA `a`
/// where `requires[a]` contains `b` can co-occur with an instruction tagged
/// `b`, even when the two instructions have no shared tag.
fn isa_requires_closure(files: &[ast::File]) -> HashMap<String, HashSet<String>> {
    let mut closure: HashMap<String, HashSet<String>> = HashMap::new();
    for isa in files.iter().flat_map(|f| f.isas()) {
        let direct = match &isa.requires {
            Some(ast::IsaRequirement::Single(s)) => vec![s.clone()],
            // `All` is a conjunction: every listed ISA is guaranteed present.
            Some(ast::IsaRequirement::All(v)) => v.clone(),
            // A single-element `Any` (`requires [X86]`) is an exact
            // requirement. A multi-element `Any` is a disjunction (`[RV32I |
            // RV64I]`): no single ISA is guaranteed, so it can imply nothing
            // for the closure — assuming all would falsely pair instructions
            // that never share a machine.
            Some(ast::IsaRequirement::Any(v)) if v.len() == 1 => v.clone(),
            Some(ast::IsaRequirement::Any(_)) => vec![],
            None => vec![],
        };
        closure.entry(isa.name.clone()).or_default().extend(direct);
    }
    let names: Vec<String> = closure.keys().cloned().collect();
    let mut changed = true;
    while changed {
        changed = false;
        for name in &names {
            for req in closure[name].iter().cloned().collect::<Vec<_>>() {
                for transitively in closure.get(&req).cloned().unwrap_or_default() {
                    if closure.get_mut(name).unwrap().insert(transitively) {
                        changed = true;
                    }
                }
            }
        }
    }
    closure
}

/// The ISAs a rule composing `reader`- and `definer`-tagged instructions is valid
/// for: a shared tag, or the more-restrictive tag when one ISA requires the other
/// (so both are available). Empty when the two can never co-occur.
fn flag_rule_isas(
    reader: &[String],
    definer: &[String],
    closure: &HashMap<String, HashSet<String>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for ri in reader {
        for di in definer {
            if ri == di || closure.get(ri).is_some_and(|c| c.contains(di)) {
                out.push(ri.clone());
            } else if closure.get(di).is_some_and(|c| c.contains(ri)) {
                out.push(di.clone());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn emit_flag_reader_rules(
    files: &[ast::File],
    definers: &[(FlagInst<'_>, FlagDefinerSemantics)],
    readers: &[(FlagInst<'_>, FlagReaderSemantics)],
    emitted_preludes: &mut HashSet<String>,
    dialect: &str,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    rule_spec_idents: &mut Vec<proc_macro2::Ident>,
) {
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
    use tir_graph::MutDag;
    let isa_closure = isa_requires_closure(files);
    for (r, r_sem) in readers {
        for (d, d_sem) in definers {
            if d_sem.class != r_sem.class {
                continue;
            }
            let shared_isas = flag_rule_isas(&r.inst.for_isas, &d.inst.for_isas, &isa_closure);
            if shared_isas.is_empty() {
                continue;
            }
            if !r_sem
                .flag_symbols
                .values()
                .all(|index| d_sem.flag_roots.contains_key(index))
            {
                continue;
            }
            // The canonical comparisons are binary: exactly two operands.
            if d_sem.variable_symbols.len() != 2 {
                continue;
            }
            let Some(symbols) = definer_comparison_symbols(files, d, d_sem) else {
                continue;
            };

            let mut spliced = tir_symbolic::sem::SemGraph::new();
            let substitute: HashMap<u32, tir_graph::NodeId> = r_sem
                .flag_symbols
                .iter()
                .map(|(symbol, index)| (*symbol, d_sem.flag_roots[index]))
                .collect();
            let spliced_root = compose_guard_with_definer(
                &mut spliced,
                &r_sem.graph,
                r_sem.cond_root,
                &substitute,
                &d_sem.graph,
                &mut HashMap::new(),
                &mut HashMap::new(),
            );
            let (composed, _) = fold_constant_subtrees(&spliced, spliced_root);

            let Some((candidate, candidate_root)) =
                find_equivalent_comparison(&composed, &symbols)
            else {
                continue;
            };

            // The value pattern is `if <canonical comparison> { <then> } else
            // { <else> }`, reusing the reader's value arms. Their symbols
            // renumber above the two comparison-operand symbols.
            let mut pattern = tir_symbolic::sem::SemGraph::new();
            let cmp = copy_subgraph(
                &mut pattern,
                &candidate,
                candidate_root,
                &mut HashMap::new(),
            );
            let mut arm_remap: HashMap<u32, u32> = HashMap::new();
            let mut next_symbol = d_sem.variable_symbols.len() as u32;
            let then_ = copy_reader_arm(
                &mut pattern,
                &r_sem.graph,
                r_sem.then_root,
                &mut arm_remap,
                &mut next_symbol,
            );
            let else_ = copy_reader_arm(
                &mut pattern,
                &r_sem.graph,
                r_sem.else_root,
                &mut arm_remap,
                &mut next_symbol,
            );
            let reader_symbols: HashMap<String, u32> = r_sem
                .variable_symbols
                .iter()
                .filter_map(|(name, symbol)| {
                    arm_remap
                        .get(symbol)
                        .copied()
                        .map(|remapped| (name.clone(), remapped))
                })
                .collect();
            let if_root = pattern.add_node(tir_symbolic::lang::SymKind::If);
            pattern.add_edge(if_root, cmp);
            pattern.add_edge(if_root, then_);
            pattern.add_edge(if_root, else_);

            let mut immediate_symbols = definer_immediate_symbols(d, d_sem);
            immediate_symbols.extend(r.ops.iter().filter_map(|(name, ty)| {
                matches!(ty, Type::Bits(_) | Type::Integer)
                    .then(|| reader_symbols.get(name).copied())
                    .flatten()
            }));
            let (canon_pattern, canon_root, forced_widths) =
                tir_symbolic::lang::canonicalize_for_selection(&pattern, if_root, &immediate_symbols);
            let mut pattern_widths = tir_symbolic::lang::infer_widths(&canon_pattern, |_| None);
            for (index, forced) in forced_widths.iter().enumerate() {
                if forced.is_some() {
                    pattern_widths[index] = *forced;
                }
            }
            let (offset, typed) = intern_dag(&canon_pattern, canon_root, &pattern_widths);
            let pattern_spec = SpecPattern {
                offset,
                typed,
                float_width: None,
            };
            let sensitive_symbols = width_sensitive_symbols(&canon_pattern, &pattern_widths);
            // The rule reads registers from both composed instructions: the
            // definer's operands and the reader's arm operands.
            let register_class = |(name, ty): &(String, Type), symbols: &HashMap<String, u32>| {
                let Type::Struct(class) = ty else {
                    return None;
                };
                symbols.get(name).map(|symbol| (*symbol, class.clone()))
            };
            let mut register_operands: Vec<(u32, String)> = d
                .ops
                .iter()
                .filter_map(|op| register_class(op, &d_sem.variable_symbols))
                .chain(
                    r.ops
                        .iter()
                        .filter_map(|op| register_class(op, &reader_symbols)),
                )
                .collect();
            register_operands.sort();
            register_operands.dedup();
            let operand_register_specs = operand_register_specs(
                &register_operands,
                &sensitive_symbols,
                &float_classes,
                &polymorphic_classes,
            );
            let mut immediate_ranges =
                immediate_operand_ranges(
                    &d_sem.graph,
                    &d.ops,
                    &d_sem.variable_symbols,
                    &d.constraints,
                );
            immediate_ranges.extend(
                immediate_operand_ranges(
                    &r_sem.graph,
                    &r.ops,
                    &r_sem.variable_symbols,
                    &r.constraints,
                )
                .into_iter()
                .filter_map(|range| {
                    arm_remap
                        .get(&range.symbol)
                        .copied()
                        .map(|symbol| ImmediateRange { symbol, ..range })
                }),
            );
            let imm_range_entries = imm_range_spec_entries(&immediate_ranges);

            let Some((_, dest_class)) = r
                .ops
                .iter()
                .find(|(name, _)| name == &r_sem.dest_operand)
                .and_then(|(name, ty)| match ty {
                    Type::Struct(class) => Some((name, class.clone())),
                    _ => None,
                })
            else {
                continue;
            };
            let dest_class_id = reg_class_id(&dest_class);
            let result_spec = result_register_spec(&dest_class, &float_classes, &polymorphic_classes);
            let mut reader_constraint_entries = Vec::new();
            let mut reader_attrs: Vec<proc_macro2::TokenStream> = Vec::new();
            for (name, ty) in &r.ops {
                let Some(&symbol) = reader_symbols.get(name) else {
                    continue;
                };
                match ty {
                    Type::Struct(_) => {
                        reader_constraint_entries.push(constraint_entry(
                            symbol,
                            quote! { tir::graph::OperandConstraint::Register },
                        ));
                        // A two-address reader reads its own destination; that
                        // operand becomes the tie attribute rather than a source.
                        let attr_name = if name == &r_sem.dest_operand {
                            format!("{name}_tied")
                        } else {
                            name.clone()
                        };
                        reader_attrs.push(emit_attr_value(&attr_name, symbol));
                    }
                    Type::Bits(_) | Type::Integer => {
                        reader_constraint_entries.push(constraint_entry(
                            symbol,
                            quote! { tir::graph::OperandConstraint::Immediate },
                        ));
                        reader_attrs.push(emit_attr_int(name, symbol));
                    }
                    _ => {}
                }
            }

            let r_lower = r.inst.name.to_lowercase();
            let d_lower = d.inst.name.to_lowercase();
            let rule_key = format!("{}_via_{}", r_lower, d_lower);
            let rule_name = format!("{}+{}", d.mnemonic, r.mnemonic);
            let r_op_ty_ident = format_ident!("{}Op", &r.inst.name);

            let (prelude_ts, prelude_shim, mut operand_constraint_entries) =
                emit_flag_definer_prelude(d, d_sem, emitted_preludes, dialect);
            isel_rule_emitters.push(prelude_ts);
            operand_constraint_entries.extend(reader_constraint_entries);

            let mut emit_attrs = vec![emit_attr_result(&r_sem.dest_operand, 0, &dest_class_id)];
            emit_attrs.extend(reader_attrs);
            let (emitter_ts, emit_shim) = emit_emitter_spec(
                &rule_key,
                dialect,
                &r.op_name,
                &r_op_ty_ident,
                &emit_attrs,
                &r.inst.name,
            );
            let (rule_ts, rule_ident) = emit_rule_spec(
                &rule_key,
                &rule_name,
                &shared_isas,
                &pattern_spec,
                &[&d.inst.name, &r.inst.name],
                quote! { tir::backend::isel::RuleKind::Value },
                Some(&prelude_shim),
                &emit_shim,
                &operand_constraint_entries,
                &operand_register_specs,
                Some(result_spec),
                &imm_range_entries,
                None,
            );
            isel_rule_emitters.push(quote! {
                #emitter_ts
                #rule_ts
            });
            rule_spec_idents.push(rule_ident);
        }
    }
}
