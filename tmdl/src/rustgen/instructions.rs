/// The `static INFO_*` holding one instruction's [`tir::backend::InstrInfo`],
/// named after its TMDL declaration (unique by construction, unlike a mnemonic).
fn info_ident(inst_name: &str) -> proc_macro2::Ident {
    format_ident!("INFO_{}", inst_name.to_uppercase())
}

/// A parse step matching one punctuation token of an instruction's syntax.
fn asm_symbol_step(symbol: &str) -> proc_macro2::TokenStream {
    let symbol_ident = format_ident!("{}", symbol);
    quote! { ParseStep::Symbol(AsmSymbol::#symbol_ident) }
}

struct InstructionOptions<'a> {
    dialect: &'a str,
    text_only: bool,
    custom_assembly: bool,
    include_global_rules: bool,
    module_fragment: bool,
}

fn asm_parse_steps(
    actions: &[AsmAction],
    ops_map: &HashMap<String, Type>,
    operand_constraints: &HashMap<String, OperandConstraint>,
) -> Vec<proc_macro2::TokenStream> {
    let plus_before_immediate: Vec<bool> = actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            matches!(action, AsmAction::Plus)
                && matches!(
                    actions.get(index + 1),
                    Some(AsmAction::Operand(name))
                        if matches!(ops_map.get(name), Some(Type::Integer | Type::Bits(_)))
                )
        })
        .collect();
    let mut parse_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (action_index, act) in actions.iter().enumerate() {
        match act {
            AsmAction::Comma => parse_steps.push(asm_symbol_step("Comma")),
            AsmAction::LParen => parse_steps.push(asm_symbol_step("LParen")),
            AsmAction::RParen => parse_steps.push(asm_symbol_step("RParen")),
            AsmAction::LBracket => parse_steps.push(asm_symbol_step("LBracket")),
            AsmAction::RBracket => parse_steps.push(asm_symbol_step("RBracket")),
            AsmAction::Star => parse_steps.push(asm_symbol_step("Star")),
            AsmAction::Plus => {
                if plus_before_immediate[action_index] {
                    parse_steps.push(quote! { ParseStep::Sign });
                } else {
                    parse_steps.push(asm_symbol_step("Plus"));
                }
            }
            AsmAction::Number(number) => {
                let number_lit = proc_macro2::Literal::string(number);
                parse_steps
                    .push(quote! { ParseStep::Number(#number_lit) });
            }
            AsmAction::Keyword(kw) => {
                let kw_lit = proc_macro2::Literal::string(kw);
                parse_steps
                    .push(quote! { ParseStep::Keyword(#kw_lit) });
            }
            // A `{...}` slot the template names but the instruction has no
            // operand for, and the mnemonic itself, consume no tokens.
            AsmAction::Skip | AsmAction::SkipMnemonic => {}
            AsmAction::Operand(op_name) => {
                let Some(ty) = ops_map.get(op_name) else {
                    continue;
                };
                let op_name_lit = proc_macro2::Literal::string(op_name);
                match ty {
                    Type::Struct(class_name) => {
                        let fn_ident =
                            format_ident!("parse_{}", class_name.to_lowercase());
                        let class_id = reg_class_id(class_name);
                        parse_steps.push(quote! {
                            ParseStep::Register(#op_name_lit, #class_id, #fn_ident)
                        });
                    }
                    Type::Integer | Type::Bits(_) => {
                        let signed = action_index > 0
                            && plus_before_immediate[action_index - 1];
                        // Reject integers that do not fit the operand's
                        // `bits<N>` width so the per-mnemonic dispatch
                        // backtracks to a wider form instead of failing
                        // later in the encoder. Mirrors the encoder's
                        // union of the signed and unsigned N-bit ranges:
                        // [-(2^(N-1)), 2^N - 1].
                        let range = match ty {
                            Type::Bits(n) if *n < 64 => {
                                let min = proc_macro2::Literal::i64_suffixed(
                                    -(1i64 << (n - 1)),
                                );
                                let max = proc_macro2::Literal::i64_suffixed(1i64 << n);
                                quote! { Some((#min, #max)) }
                            }
                            _ => quote! { None },
                        };
                        let constraint = operand_constraints
                            .get(op_name)
                            .copied()
                            .unwrap_or_default();
                        let align =
                            proc_macro2::Literal::u32_unsuffixed(constraint.align);
                        let nonzero = constraint.nonzero;
                        parse_steps.push(quote! {
                            ParseStep::Immediate(#op_name_lit, #signed, ImmConstraint {
                                range: #range,
                                align: #align,
                                nonzero: #nonzero,
                            })
                        });
                    }
                    // Strings in asm templates aren't currently used as
                    // operands, and consume no tokens.
                    Type::String => {}
                    _ => {}
                }
            }
        }
    }
    parse_steps
}

fn asm_syntax_parts(
    print_parts: &[AsmPrintPart],
    ops_map: &HashMap<String, Type>,
) -> Vec<proc_macro2::TokenStream> {
    print_parts
        .iter()
        .filter_map(|part| match part {
            AsmPrintPart::Text(text) if text.is_empty() => None,
            AsmPrintPart::Text(text) => {
                let lit = proc_macro2::Literal::string(text);
                Some(quote! { tir::backend::asm_syntax::AsmSyntaxPart::Text(#lit) })
            }
            AsmPrintPart::Operand(name) => {
                let name_lit = proc_macro2::Literal::string(name);
                let class = match ops_map.get(name) {
                    Some(Type::Struct(class)) => {
                        let c = proc_macro2::Literal::string(class);
                        quote! { Some(#c) }
                    }
                    _ => quote! { None },
                };
                Some(quote! {
                    tir::backend::asm_syntax::AsmSyntaxPart::Operand {
                        name: #name_lit,
                        class: #class,
                    }
                })
            }
        })
        .collect()
}

fn asm_print_steps(
    print_parts: Vec<AsmPrintPart>,
    ops_map: &HashMap<String, Type>,
) -> Vec<proc_macro2::TokenStream> {
    // Adjacent literal text (the mnemonic and the space after it, an
    // operand separator and a sigil) prints as one string.
    let print_parts = print_parts.into_iter().fold(
        Vec::new(),
        |mut merged: Vec<AsmPrintPart>, part| {
            match (merged.last_mut(), &part) {
                (Some(AsmPrintPart::Text(prev)), AsmPrintPart::Text(text)) => {
                    prev.push_str(text)
                }
                _ => merged.push(part),
            }
            merged
        },
    );

    let mut print_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for part in print_parts {
        match part {
            AsmPrintPart::Text(text) => {
                if !text.is_empty() {
                    let text_lit = proc_macro2::Literal::string(&text);
                    print_steps
                        .push(quote! { PrintPart::Text(#text_lit) });
                }
            }
            AsmPrintPart::Operand(op_name) => {
                let Some(ty) = ops_map.get(&op_name) else {
                    continue;
                };
                let op_name_lit = proc_macro2::Literal::string(&op_name);
                match ty {
                    Type::Struct(class_name) => {
                        let fn_ident =
                            format_ident!("print_{}", class_name.to_lowercase());
                        print_steps.push(quote! {
                            PrintPart::Register(#op_name_lit, #fn_ident)
                        });
                    }
                    Type::Integer | Type::Bits(_) => {
                        print_steps.push(quote! {
                            PrintPart::Immediate(#op_name_lit)
                        });
                    }
                    Type::String => {
                        print_steps.push(quote! {
                            PrintPart::Str(#op_name_lit)
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    print_steps
}

fn mnemonic_specificity(
    ops_map: &HashMap<String, Type>,
    class_sizes: &HashMap<String, usize>,
) -> (usize, usize, u32) {
    let mut arity = 0usize;
    let mut reg_specificity = 0usize;
    let mut imm_bits = 0u32;
    for ty in ops_map.values() {
        match ty {
            Type::Struct(class) => {
                arity += 1;
                reg_specificity = reg_specificity.saturating_add(
                    class_sizes.get(class).copied().unwrap_or(usize::MAX),
                );
            }
            Type::Bits(n) => {
                arity += 1;
                imm_bits += u32::from(*n);
            }
            Type::Integer => arity += 1,
            _ => {}
        }
    }
    (arity, reg_specificity, imm_bits)
}

struct TargetTables<'a> {
    files: &'a [ast::File],
    register_index_map: HashMap<(String, String), u32>,
    pc_classes: HashSet<String>,
    flag_classes: HashSet<String>,
    register_name_map: HashMap<(String, u32), String>,
    register_files: HashMap<String, String>,
    float_classes: HashSet<String>,
    polymorphic_classes: HashSet<String>,
    hardwired_zero_index: HashMap<String, u16>,
    class_sizes: HashMap<String, usize>,
    reg_kinds: HashMap<String, (bool, u32)>,
}

struct InstrEmitCtx<'a> {
    inst: &'a ast::Instruction,
    name_ident: &'a proc_macro2::Ident,
    op_name: &'a str,
    dialect: &'a str,
    ops: &'a [(String, Type)],
    mnemonic_name: &'a str,
    builder_ident: &'a proc_macro2::Ident,
    ops_map: &'a HashMap<String, Type>,
    operand_constraints: &'a HashMap<String, OperandConstraint>,
    defined_register_operands: &'a [String],
    read_register_operands: &'a HashSet<String>,
    implicit_reads: &'a [(String, u32)],
}

fn emit_value_rules(
    tables: &TargetTables<'_>,
    ctx: &InstrEmitCtx<'_>,
    semantics: &InstructionSemantics,
    out: &mut InstrOutputs<'_>,
) {
    let rule_key = ctx.inst.name.to_lowercase();

    // Per-operand constraints: registers must bind to non-constant values,
    // immediates to constants. Keyed by the operand's pattern symbol id.
    let mut operand_constraint_entries: Vec<proc_macro2::TokenStream> = Vec::new();
    for (op_name, op_ty) in ctx.ops {
        let Some(&symbol) = semantics.variable_symbols.get(op_name) else {
            continue;
        };
        let constraint = match op_ty {
            Type::Struct(_) => quote! { tir::graph::OperandConstraint::Register },
            Type::Bits(_) | Type::Integer => {
                quote! { tir::graph::OperandConstraint::Immediate }
            }
            _ => continue,
        };
        operand_constraint_entries.push(constraint_entry(symbol, constraint));
    }
    // A data register the behavior reads by path (e.g. the x86 shift count
    // in `GPR::rcx`, whose class is also a value-operand class) reads that
    // register's *value*, so it must bind a register, never a folded
    // constant — a constant count belongs to the immediate form. Without
    // this the count is stuffed into the reg as a dead attribute and the
    // encoder emits the by-`cl` form reading garbage. A config-register
    // demand (e.g. RISC-V `VCSR::vl`) is a different class and unaffected.
    let value_reg_classes: Vec<&str> = ctx.ops
        .iter()
        .filter_map(|(_, ty)| match ty {
            Type::Struct(class) => Some(class.as_str()),
            _ => None,
        })
        .collect();
    let mut fixed_value_reads = HashMap::new();
    for ((class, index), symbol) in &semantics.register_symbols {
        let is_implicit = tables.register_name_map
            .get(&(class.clone(), *index))
            .map(|name| !ctx.ops.iter().any(|(op_name, _)| op_name == name))
            .unwrap_or(false);
        let value_class = tables.register_files.get(class).and_then(|fixed_file| {
            value_reg_classes.iter().find(|value_class| {
                tables.register_files
                    .get(**value_class)
                    .is_some_and(|file| file == fixed_file)
            })
        });
        if is_implicit && let Some(value_class) = value_class {
            operand_constraint_entries.push(constraint_entry(
                *symbol,
                quote! { tir::graph::OperandConstraint::Register },
            ));
            let index = u16::try_from(*index).expect("register indices fit u16");
            fixed_value_reads.insert(*symbol, ((*value_class).to_string(), index));
        }
    }

    let mut emit_attrs: Vec<proc_macro2::TokenStream> = Vec::new();
    for (op_name, op_ty) in ctx.ops {
        match op_ty {
            Type::Struct(class_name) => {
                let class_id = reg_class_id(class_name);
                if let Some(def_pos) = ctx.defined_register_operands
                    .iter()
                    .position(|name| name == op_name)
                {
                    emit_attrs.push(emit_attr_result(op_name, def_pos, &class_id));
                    // A two-address destination also reads a pattern operand:
                    // record the bound value in a `_tied` attribute so register
                    // allocation can lower the tie to a copy.
                    if ctx.read_register_operands.contains(op_name)
                        && let Some(sym) = semantics.variable_symbols.get(op_name)
                    {
                        emit_attrs
                            .push(emit_attr_value(&format!("{op_name}_tied"), *sym));
                    }
                } else if let Some(sym) = semantics.variable_symbols.get(op_name) {
                    emit_attrs.push(emit_attr_value(op_name, *sym));
                } else if let Some(Some(reg_idx)) =
                    semantics.fixed_register_by_class.get(class_name)
                {
                    emit_attrs.push(emit_attr_physical(op_name, &class_id, *reg_idx));
                }
            }
            Type::Integer | Type::Bits(_) => {
                if let Some(sym) = semantics.variable_symbols.get(op_name) {
                    emit_attrs.push(emit_attr_int(op_name, *sym));
                }
            }
            Type::String => {}
            _ => {}
        }
    }

    // Canonicalize the behavior-derived pattern into the form selection
    // matches against (collapse word-op sext/extract wrappers to a typed op,
    // strip shift-amount masks), then type each node from its structurally
    // determined width. A plain `add` stays untyped; `addw` becomes an i32
    // `Add`; `sll` becomes a plain `ShiftLeft`.
    let immediate_symbols: std::collections::HashSet<u32> = ctx.ops
        .iter()
        .filter(|(_, op_ty)| matches!(op_ty, Type::Bits(_) | Type::Integer))
        .filter_map(|(op_name, _)| semantics.variable_symbols.get(op_name).copied())
        .collect();
    let (canon_pattern, canon_root, forced_widths) = tir_symbolic::lang::canonicalize_for_selection(
        &semantics.pattern,
        semantics.root,
        &immediate_symbols,
    );
    let mut pattern_widths = tir_symbolic::lang::infer_widths(&canon_pattern, |_| None);
    for (index, forced) in forced_widths.iter().enumerate() {
        if forced.is_some() {
            pattern_widths[index] = *forced;
        }
    }
    // A destination register class statically narrower than the
    // architectural width (x86 `add32`/`add16`/`add8`) defines exactly
    // that many bits: type the pattern root at the class width, so the
    // narrow form matches only values of its width instead of tying
    // with the full-width form on every width.
    let dst_class = ctx.defined_register_operands
        .first()
        .and_then(|name| ctx.ops_map.get(name))
        .and_then(|ty| match ty {
            Type::Struct(class) => Some(class.as_str()),
            _ => None,
        });
    if pattern_widths[canon_root.index()].is_none()
        && scalar_root_kind(tir_graph::Dag::get_node(&canon_pattern, canon_root))
        && let Some(dst_class) = dst_class
        && let Some(width) = literal_register_class_width(tables.files, dst_class)
    {
        pattern_widths[canon_root.index()] = Some(width);
    }
    let (pattern_offset, pattern_typed) =
        intern_dag(&canon_pattern, canon_root, &pattern_widths);
    let mut pattern_spec = SpecPattern {
        offset: pattern_offset,
        typed: pattern_typed,
        float_width: None,
    };
    // The destination's full guarded semantics, emitted alongside the
    // relaxed pattern so pass construction proves the guard drop sound.
    let guarded_spec =
        semantics
            .guarded_semantics
            .as_ref()
            .map(|(guarded, guarded_root)| {
                let guarded_widths = tir_symbolic::lang::infer_widths(guarded, |_| None);
                let (offset, typed) = intern_dag(guarded, *guarded_root, &guarded_widths);
                SpecPattern {
                    offset,
                    typed,
                    float_width: None,
                }
            });
    if *tir_graph::Dag::get_node(&canon_pattern, canon_root)
        == tir_symbolic::lang::SymKind::Bitcast
        && let Some(dst_class) = dst_class
        && tables.float_classes.contains(dst_class)
        && let Some(width) = literal_register_class_width(tables.files, dst_class)
    {
        if !matches!(width, 32 | 64) {
            unreachable!("unsupported scalar float register width {width}");
        }
        pattern_spec.float_width = Some(width);
    }
    let operand_register_specs = operand_register_specs_for_ops(
        ctx.ops,
        &semantics.variable_symbols,
        &width_sensitive_symbols(&canon_pattern, &pattern_widths),
        &tables.float_classes,
        &tables.polymorphic_classes,
    );
    let result_register_spec =
        dst_class.map(|c| result_register_spec(c, &tables.float_classes, &tables.polymorphic_classes));
    let imm_range_entries = imm_range_spec_entries(&immediate_operand_ranges(
        &semantics.pattern,
        ctx.ops,
        &semantics.variable_symbols,
        ctx.operand_constraints,
    ));
    // Registers read by path are dependencies outside the encoded operands.
    // Value-register reads carry a fixed-use constraint; configuration
    // reads remain demands for a target pass such as `vsetvli` insertion.
    for (name, sym) in ctx.implicit_reads {
        if let Some((class, index)) = fixed_value_reads.get(sym) {
            emit_attrs.push(emit_attr_fixed_use(name, *sym, &reg_class_id(class), *index));
            continue;
        }
        emit_attrs.push(emit_attr_int_or_value(name, *sym));
    }

    let (emitter_ts, emit_shim) = emit_emitter_spec(
        &rule_key,
        ctx.dialect,
        ctx.op_name,
        ctx.name_ident,
        &emit_attrs,
        &ctx.inst.name,
    );
    let (rule_ts, rule_spec_ident) = emit_rule_spec(
        &rule_key,
        &rule_key,
        &ctx.inst.for_isas,
        &pattern_spec,
        &[&ctx.inst.name],
        quote! { tir::backend::isel::RuleKind::Value },
        None,
        &emit_shim,
        &operand_constraint_entries,
        &operand_register_specs,
        result_register_spec.clone(),
        &imm_range_entries,
        guarded_spec.as_ref(),
    );
    out.isel_rule_emitters.push(quote! {
        #emitter_ts
        #rule_ts
    });
    out.rule_spec_idents.push(rule_spec_ident);

    // Zero-form constant materializer: when the canonical pattern is
    // `reg + imm` and the source register's class has a hardwired-zero
    // register (RISC-V `addi rs1:GPR`), derive a rule matching
    // `zext(0b0, W) + imm` — the shape the constant-materializer bridge
    // injects into fitting program-constant classes — with the register
    // slot wired to the zero register, so a bare constant selects as
    // the canonical `li` (`addi rd, x0, imm`). arm64's add-immediate
    // reads `GPRsp`, whose encoding 31 is `sp`, not a hardwired zero,
    // so no zero-form is derived there.
    let zero_form = match ctx.defined_register_operands {
        [rd_name] if !ctx.read_register_operands.contains(rd_name)
            && ctx.implicit_reads.is_empty() =>
        {
            value_zero_form_operands(
                &canon_pattern,
                canon_root,
                ctx.ops,
                &semantics.variable_symbols,
                rd_name,
                |class: &str| {
                    tables.hardwired_zero_index.contains_key(class)
                        && !tables.float_classes.contains(class)
                        && !tables.polymorphic_classes.contains(class)
                },
            )
        }
        _ => None,
    };
    if let Some((zero_reg_name, zero_reg_class, imm_sym)) = zero_form {
        let zero_rule_key = format!("{}_zero", ctx.inst.name.to_lowercase());
        let width_sym = semantics
            .variable_symbols
            .values()
            .chain(semantics.register_symbols.values())
            .copied()
            .max()
            .unwrap_or(0)
            + 1;

        // The zero pattern, built here and interned into the sem blob:
        // `zext(0b0, W) + imm`, typed at the canonical root width.
        let (zero_pattern_offset, zero_pattern_typed) = {
            use tir_graph::{Dag as _, MutDag};
            let mut g = tir_symbolic::sem::SemGraph::<()>::new();
            let zero = g.add_node(tir_symbolic::lang::SymKind::Constant);
            g.set_leaf_data(zero, tir_symbolic::sem::int_payload(1, 0, false));
            let width = g.add_node(tir_symbolic::lang::SymKind::Symbol);
            g.set_leaf_data(
                width,
                tir_symbolic::lang::SymPayload::SymbolId(width_sym),
            );
            let zext = g.add_node(tir_symbolic::lang::SymKind::ZExt);
            g.add_edge(zext, zero);
            g.add_edge(zext, width);
            let imm = g.add_node(tir_symbolic::lang::SymKind::Symbol);
            g.set_leaf_data(imm, tir_symbolic::lang::SymPayload::SymbolId(imm_sym));
            let root = g.add_node(tir_symbolic::lang::SymKind::Add);
            g.add_edge(root, zext);
            g.add_edge(root, imm);
            let zero_widths: Vec<Option<u32>> = (0..g.len())
                .map(|index| {
                    if index == root.index() {
                        pattern_widths[canon_root.index()]
                    } else {
                        None
                    }
                })
                .collect();
            intern_dag(&g, root, &zero_widths)
        };

        let rd_name = ctx.defined_register_operands
            .first()
            .expect("zero-form requires a defined register operand");
        let rd_class_id = reg_class_id(dst_class.expect("defined operand has a class"));
        let zero_class_id = reg_class_id(&zero_reg_class);
        let zero_index = tables.hardwired_zero_index[&zero_reg_class];
        let imm_name = ctx.ops
            .iter()
            .find(|(name, _)| semantics.variable_symbols.get(name) == Some(&imm_sym))
            .map(|(name, _)| name.clone())
            .expect("immediate operand has a name");
        let zero_imm_range_entries = imm_range_spec_entries(
            &immediate_operand_ranges(
                &semantics.pattern,
                ctx.ops,
                &semantics.variable_symbols,
                ctx.operand_constraints,
            )
            .into_iter()
            .filter(|range| range.symbol == imm_sym)
            .collect::<Vec<_>>(),
        );

        let zero_emit_attrs = vec![
            emit_attr_result(rd_name, 0, &rd_class_id),
            emit_attr_physical(&zero_reg_name, &zero_class_id, zero_index),
            emit_attr_int(&imm_name, imm_sym),
        ];
        let (zero_emitter_ts, zero_emit_shim) = emit_emitter_spec(
            &zero_rule_key,
            ctx.dialect,
            ctx.op_name,
            ctx.name_ident,
            &zero_emit_attrs,
            &ctx.inst.name,
        );
        let zero_pattern_spec = SpecPattern {
            offset: zero_pattern_offset,
            typed: zero_pattern_typed,
            float_width: None,
        };
        let zero_constraints =
            [constraint_entry(imm_sym, quote! { tir::graph::OperandConstraint::Immediate })];
        let (zero_rule_ts, zero_rule_ident) = emit_rule_spec(
            &zero_rule_key,
            &zero_rule_key,
            &ctx.inst.for_isas,
            &zero_pattern_spec,
            &[&ctx.inst.name],
            quote! { tir::backend::isel::RuleKind::Value },
            None,
            &zero_emit_shim,
            &zero_constraints,
            &[],
            result_register_spec.clone(),
            &zero_imm_range_entries,
            None,
        );
        out.isel_rule_emitters.push(quote! {
            #zero_emitter_ts
            #zero_rule_ts
        });
        out.rule_spec_idents.push(zero_rule_ident);
    }
}

fn emit_branch_rules(
    tables: &TargetTables<'_>,
    ctx: &InstrEmitCtx<'_>,
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    out: &mut InstrOutputs<'_>,
) {
// A guarded PC write (`if cond { PC::pc = PC::pc + imm }`) becomes a
// conditional-branch rule: the pattern is the branch condition over the
// encoded operands, and the target operand is emitted as a block
// attribute bound by branch selection.
if let Some(branch) = analyze_branch_semantics(
    ctx.inst,
    ctx.ops,
    numeric_params,
    isa_param_values,
    &tables.register_index_map,
    &tables.pc_classes,
) {
    let no_zero_slots = HashMap::new();
    let (emitter, rule_ident) = emit_cond_branch_rule(
        &ctx.inst.name.to_lowercase(),
        ctx.dialect,
        ctx.op_name,
        ctx.name_ident,
        &ctx.inst.name,
        &ctx.inst.for_isas,
        ctx.ops,
        &branch.pattern,
        branch.root,
        &branch.variable_symbols,
        &branch.target_operand,
        branch.target_symbol,
        &no_zero_slots,
        ctx.operand_constraints,
        &tables.float_classes,
        &tables.polymorphic_classes,
    );
    out.isel_rule_emitters.push(emitter);
    out.rule_spec_idents.push(rule_ident);

    // Zero-form variants: when the branch condition is a two-register
    // comparison whose operands belong to a class with a hardwired-zero
    // register (RISC-V `x0`), derive one rule per slot that wires that slot
    // to the zero register, so `cmpi x, 0`-style guards (and bare i1
    // conditions the bridge rewrites to `x != 0`) select the branch
    // directly instead of materializing the constant. The zeroed slot is
    // lowered as `zext(0b0, W)` — the shape the arm64 cbz/cbnz path and the
    // bare-i1 bridge produce, so all three unify in the program e-graph.
    let (root_kind, root_children) = {
        use tir_graph::Dag;
        (
            *branch.pattern.get_node(branch.root),
            branch.pattern.children(branch.root).collect::<Vec<_>>(),
        )
    };
    let root_is_comparison = {
        use tir_symbolic::lang::SymKind::*;
        matches!(
            root_kind,
            Eq | Ne | Lt | Le | Gt | Ge | ULt | ULe | UGt | UGe
        )
    };
    // Both comparison operands must be distinct register operands of a
    // hardwired-zero class; otherwise there is nothing to substitute (e.g.
    // a pattern already comparing against a literal zero).
    let operand_slots: Option<Vec<(String, String, u32)>> = (root_is_comparison
        && root_children.len() == 2)
        .then(|| {
            use tir_graph::Dag;
            root_children
                .iter()
                .map(|&child| {
                    let symbol = match branch.pattern.get_leaf_data(child) {
                        Some(tir_symbolic::lang::SymPayload::SymbolId(s)) => *s,
                        _ => return None,
                    };
                    let (name, class) = ctx.ops.iter().find_map(|(name, ty)| {
                        let Type::Struct(class) = ty else { return None };
                        (branch.variable_symbols.get(name) == Some(&symbol)
                            && tables.hardwired_zero_index.contains_key(class))
                        .then(|| (name.clone(), class.clone()))
                    })?;
                    Some((name, class, symbol))
                })
                .collect::<Option<Vec<_>>>()
        })
        .flatten();
    if let Some(slots) = operand_slots {
        // Equality and inequality are commutative. Prefer the form with
        // the zero register in the second operand, which is the
        // conventional spelling for RISC-V zero comparisons.
        let slots = if matches!(root_kind, tir_symbolic::lang::SymKind::Eq | tir_symbolic::lang::SymKind::Ne) {
            slots.into_iter().rev().collect::<Vec<_>>()
        } else {
            slots
        };
        for (slot_index, (slot_op_name, class_name, reg_symbol)) in slots.iter().enumerate() {
            let width_symbol = branch.target_symbol + 1;
            let (zero_pattern, zero_root) = branch_pattern_with_zero(
                &branch.pattern,
                branch.root,
                *reg_symbol,
                width_symbol,
            );
            let mut zero_variable_symbols = branch.variable_symbols.clone();
            zero_variable_symbols.remove(slot_op_name);
            let mut zero_slots = HashMap::new();
            zero_slots.insert(
                slot_op_name.clone(),
                (class_name.clone(), tables.hardwired_zero_index[class_name]),
            );
            let rule_name = format!("{}_zero{}", ctx.inst.name.to_lowercase(), slot_index);
            let (emitter, rule_ident) = emit_cond_branch_rule(
                &rule_name,
                ctx.dialect,
                ctx.op_name,
                ctx.name_ident,
                &ctx.inst.name,
                &ctx.inst.for_isas,
                ctx.ops,
                &zero_pattern,
                zero_root,
                &zero_variable_symbols,
                &branch.target_operand,
                branch.target_symbol,
                &zero_slots,
                ctx.operand_constraints,
                &tables.float_classes,
                &tables.polymorphic_classes,
            );
            out.isel_rule_emitters.push(emitter);
            out.rule_spec_idents.push(rule_ident);
        }
    }
}
}

fn attrs_schema_ts(ops: &[(String, Type)]) -> proc_macro2::TokenStream {
    let mut items = vec![];
    for (name, ty) in ops {
        let field_ident = format_ident!("{}", name);
        let ty_ts = match ty {
            Type::Struct(_) => continue,
            Type::Integer | Type::Bits(_) => quote! { Integer },
            Type::String => quote! { String },
            _ => unreachable!("HM type vars should not appear as operand types"),
        };
        items.push(quote! { #field_ident: #ty_ts });
    }
    quote! { #(#items,)* }
}

fn instruction_ports(
    tables: &TargetTables<'_>,
    ctx: &InstrEmitCtx<'_>,
) -> Vec<(String, Option<String>, bool, Option<String>)> {
let mut ports: Vec<(String, Option<String>, bool, Option<String>)> = vec![];
for (name, ty) in ctx.ops {
    if !matches!(ty, Type::Struct(_)) {
        continue;
    }
    let Type::Struct(class_name) = ty else {
        unreachable!()
    };
    let defines = ctx.defined_register_operands.contains(name);
    ports.push((name.clone(), Some(class_name.clone()), defines, None));
    if defines && ctx.read_register_operands.contains(name) {
        ports.push((
            format!("{name}_tied"),
            Some(class_name.clone()),
            false,
            Some(name.clone()),
        ));
    }
}
// A demand slot carries its class at run time (a target pass decides it),
// so the port constrains nothing beyond the value's own type.
for (name, _) in ctx.implicit_reads {
    ports.push((name.clone(), None, false, None));
}
for (slot, class_name, is_def) in fixed_register_role_items(
    ctx.inst,
    ctx.ops,
    &tables.register_index_map,
    &tables.register_name_map,
    &tables.flag_classes,
    &tables.pc_classes,
) {
    ports.push((slot, Some(class_name), is_def, None));
}

    ports
}

fn reg_port_entries(
    ports: &[(String, Option<String>, bool, Option<String>)],
) -> Vec<proc_macro2::TokenStream> {
let port_entries: Vec<proc_macro2::TokenStream> = ports
    .iter()
    .map(|(name, class_name, is_def, tied_to)| {
        let name_lit = proc_macro2::Literal::string(name);
        let class = match class_name {
            Some(class_name) => {
                let id = reg_class_id(class_name);
                quote! { Some(#id) }
            }
            None => quote! { None },
        };
        let tied = match tied_to {
            Some(destination) => {
                let lit = proc_macro2::Literal::string(destination);
                quote! { Some(#lit) }
            }
            None => quote! { None },
        };
        quote! {
            tir::backend::RegPort {
                name: #name_lit,
                class: #class,
                def: #is_def,
                tied_to: #tied,
            }
        }
    })
    .collect();
    port_entries
}

fn operands_schema_ts(
    ports: &[(String, Option<String>, bool, Option<String>)],
) -> proc_macro2::TokenStream {
    let items = ports.iter().filter(|(_, _, is_def, _)| !is_def).map(
        |(name, _, _, _)| {
            let field = format_ident!("{}", name);
            quote! { #field: "?tir::backend::RegClassType" }
        },
    );
    let items: Vec<_> = items.collect();
    if items.is_empty() {
        quote! {}
    } else {
        quote! { operands: O { #(#items,)* }, }
    }
}
fn instruction_program_ts(
    inst: &ast::Instruction,
    branch_value: Option<&ast::Expr>,
    uses_todo: bool,
    trap_handler: Option<&ast::TrapHandler>,
    numeric_params: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    behavior_ctx: &RustBehaviorCtx<'_>,
) -> proc_macro2::TokenStream {
let unsupported_lowering = quote! {
    tir::backend::exec::Program::Unsupported(
        "failed to convert behavior to executable expression",
    )
};
let program = if let Some(branch_val) = branch_value.as_ref() {
    // Conditional control transfer: `synthesize_branch_value` folds the
    // condition into one value (taken target or fall-through) written to PC
    // every cycle.
    let ast::Expr::If(branch_if) = branch_val else {
        unreachable!("synthesized branch value is an if expression")
    };
    let normalized = ast::Expr::Assign(ast::Assign {
        dest: Box::new(ast::Expr::Path(ast::Path {
            base: "PC".to_string(),
            remainder: vec!["pc".to_string()],
            span: branch_if.span,
        })),
        value: Box::new((*branch_val).clone()),
        span: branch_if.span,
    });
    emit_behavior_exec(
        &normalized,
        trap_handler,
        numeric_params,
        register_index_map,
        behavior_ctx,
    )
    .unwrap_or(unsupported_lowering)
} else if uses_todo {
    quote! {
        tir::backend::exec::Program::Unsupported(
            "instruction semantics are not modeled (todo)",
        )
    }
} else {
    emit_behavior_exec(
        &inst.behavior,
        trap_handler,
        numeric_params,
        register_index_map,
        behavior_ctx,
    )
    .unwrap_or(unsupported_lowering)
};

// Control-flow kind, derived from the behavior's `PC::pc` writes: every
// path writes PC → unconditional transfer; some paths → conditional
    program
}

fn emit_instruction_assembly(
    tables: &TargetTables<'_>,
    ctx: &InstrEmitCtx<'_>,
    template: &str,
    text_only: bool,
    custom_assembly: bool,
    out: &mut InstrOutputs<'_>,
) -> Option<proc_macro2::Ident> {
    let builder_ident = ctx.builder_ident;
    let mut desc_ident = None;
let actions = compile_asm_template(template);
let syntax_arity = actions
    .iter()
    .filter(|action| {
        matches!(
            action,
            AsmAction::Operand(_) | AsmAction::Keyword(_) | AsmAction::Number(_)
        )
    })
    .count();
let parse_steps = asm_parse_steps(&actions, ctx.ops_map, ctx.operand_constraints);

let print_parts = compile_asm_printer_template(template, ctx.mnemonic_name);

// Accumulate the data-driven syntax entry (text-only targets consume
// this). Each part is either literal text or a typed operand slot.
if text_only {
    let part_tokens = asm_syntax_parts(&print_parts, ctx.ops_map);
    let op_name_lit_s = proc_macro2::Literal::string(ctx.op_name);
    let mnemonic_lit_s = proc_macro2::Literal::string(ctx.mnemonic_name);
    out.asm_syntax_entries.push(quote! {
        tir::backend::asm_syntax::InstrSyntax {
            op_name: #op_name_lit_s,
            mnemonic: #mnemonic_lit_s,
            parts: &[#(#part_tokens),*],
        }
    });
}

let print_steps = asm_print_steps(print_parts, ctx.ops_map);

let parse_fn_ident = format_ident!("parse_{}_inst", &ctx.inst.name.to_lowercase());
if !custom_assembly {
    let ident = format_ident!("DESC_{}", ctx.inst.name.to_uppercase());
    out.instruction_descs.push(quote! {
        static #ident: InstrDesc = InstrDesc {
            parse: &[#(#parse_steps),*],
            print: &[#(#print_steps),*],
        };
    });

    out.instruction_parsers_impls.push(quote! {
        fn #parse_fn_ident<'src>(
            context: &tir::Context,
            builder: &mut tir::backend::AsmCursor,
            parser: &mut tir::parse::tokens::Parser<'src, tir::backend::Token<'src>>,
        ) -> Result<(), ()> {
            asm_desc::parse_and_insert(
                context,
                &#ident,
                parser,
                builder,
                |attributes| {
                    let mut op_builder = #builder_ident::new(context);
                    for attribute in attributes {
                        op_builder = op_builder.attr_sym(attribute.name, attribute.value);
                    }
                    op_builder.build()
                },
            )
        }
    });
    desc_ident = Some(ident);
}

let mn = ctx.mnemonic_name;
    let mn_lit = proc_macro2::Literal::string(mn);
    let inst_features = feature_slice(&ctx.inst.for_isas);
    let (arity, reg_specificity, imm_bits) =
        mnemonic_specificity(ctx.ops_map, &tables.class_sizes);
    if !custom_assembly {
        out.instruction_parser_candidates.push((
        mn.to_string(),
        syntax_arity,
        arity,
        imm_bits,
        reg_specificity,
        quote! {
            (#mn_lit, #inst_features, #parse_fn_ident as tir::backend::AsmInstructionParser)
        },
        ));
    }
    desc_ident
}

fn collect_target_tables(files: &[ast::File]) -> TargetTables<'_> {
// `(class, register-name) -> encoding index` over every register class, so the
// simulator can lower register paths that carry no numeric index in their name
// (e.g. status flags `PSTATE::z`) to a stable slot.
let register_index_map: HashMap<(String, String), u32> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .flat_map(|rc| {
        let class = rc.name.clone();
        rc.register_indices()
            .into_iter()
            .map(move |(name, idx)| ((class.clone(), name), u32::from(idx)))
    })
    .collect();

// Register count per class, used to sort same-mnemonic asm parser candidates
// by specificity: a form over a small class (e.g. 2-register `GPRsib`) is more
// constrained than one over a large class (16-register `GPR`) and is tried first.
let class_sizes: HashMap<String, usize> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .map(|rc| (rc.name.clone(), rc.resolve_registers().count()))
    .collect();
let classes: HashMap<String, &ast::RegisterClass> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .map(|rc| (rc.name.clone(), rc))
    .collect();
let register_files: HashMap<String, String> = classes
    .values()
    .map(|rc| (rc.name.clone(), rc.register_file(&classes).to_string()))
    .collect();

// The inverse mapping, used to name a demand attribute after the register a
// behavior reads implicitly (`VCSR::vl` -> attribute `vl`). Declaration names
// precede ABI aliases in `register_indices`, so first-wins keeps the
// declaration name.
let register_name_map: HashMap<(String, u32), String> = {
    let mut map = HashMap::new();
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        for (name, idx) in rc.register_indices() {
            map.entry((rc.name.clone(), u32::from(idx))).or_insert(name);
        }
    }
    map
};

// Register classes holding the program counter. An instruction whose behavior
// reads or writes the PC cannot be selected as a value rule: the pattern only
// models the assigned result, so the control-flow effect would be invisible
// (a `jal` rule would match a plain `x + 4`). Conditional PC writes instead
// produce branch rules (see `analyze_branch_semantics`).
let pc_classes: HashSet<String> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .filter(|rc| rc.has_program_counter())
    .map(|rc| rc.name.clone())
    .collect();

// Register classes holding condition-code bits (`status_flag` registers,
// e.g. AArch64 PSTATE, x86 EFLAGS). Instructions writing only such
// registers pair with the branches guarding on them into derived
// conditional-branch rules (see `emit_flag_branch_rules`).
let flag_classes: HashSet<String> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .filter(|rc| rc.has_status_flags())
    .map(|rc| rc.name.clone())
    .collect();

// Register classes holding floating-point values (`float` registers).
// Their operands and results constrain selection to float-typed values.
let float_classes: HashSet<String> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .filter(|rc| rc.has_float_registers())
    .map(|rc| rc.name.clone())
    .collect();
let polymorphic_classes: HashSet<String> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .filter(|rc| rc.has_polymorphic_registers())
    .map(|rc| rc.name.clone())
    .collect();

// Register classes with a hardwired-zero register (RISC-V `x0`, AArch64
// `xzr`), mapping the class name to that register's index. A two-register
// comparison branch over such a class gets extra zero-form rule variants that
// wire one operand to the zero register (see the zero-form derivation below).
let hardwired_zero_index: HashMap<String, u16> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .filter_map(|rc| {
        rc.hardwired_zero_register_index()
            .map(|idx| (rc.name.clone(), idx))
    })
    .collect();

// Per-class execution read routing: `(is_float, width)`. A vector operand
// (width > 64) is read as raw byte lanes, a scalar float as an `APFloat`,
// and everything else as an `APInt` — so no value crosses the register
// interface in the wrong representation.
let reg_kinds: HashMap<String, (bool, u32)> = files
    .iter()
    .flat_map(|f| f.register_classes())
    .map(|rc| {
        let width = literal_register_class_width(files, &rc.name).unwrap_or(64);
        (rc.name.clone(), (float_classes.contains(&rc.name), width))
    })
    .collect();

    TargetTables {
        files,
        register_index_map,
        pc_classes,
        flag_classes,
        register_name_map,
        register_files,
        float_classes,
        polymorphic_classes,
        hardwired_zero_index,
        class_sizes,
        reg_kinds,
    }
}
struct InstrOutputs<'a> {
    instruction_defs: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_parsers_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_parser_candidates: &'a mut Vec<(String, usize, usize, u32, usize, proc_macro2::TokenStream)>,
    instruction_descs: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_infos: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_info_idents: &'a mut Vec<proc_macro2::Ident>,
    isel_rule_emitters: &'a mut Vec<proc_macro2::TokenStream>,
    rule_spec_idents: &'a mut Vec<proc_macro2::Ident>,
    machine_instruction_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_custom_format_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_reg_ports: &'a mut Vec<proc_macro2::TokenStream>,
    as_sem_expr_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_encoder_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_decoder_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_decoder_dispatch: &'a mut Vec<(u32, proc_macro2::Ident)>,
    asm_syntax_entries: &'a mut Vec<proc_macro2::TokenStream>,
}

struct InstrInfoParts<'a> {
    op_name_lit: &'a proc_macro2::Literal,
    mnemonic_lit: &'a proc_macro2::Literal,
    program: &'a proc_macro2::TokenStream,
    width_bytes_lit: &'a proc_macro2::TokenStream,
    max_width_bytes: u8,
    uncond_pc: bool,
    cond_pc: bool,
    control_flow: &'a proc_macro2::TokenStream,
    implicit_items: &'a [proc_macro2::TokenStream],
    ports_ident: &'a proc_macro2::Ident,
    reads_memory: bool,
    writes_memory: bool,
    desc_ident: &'a Option<proc_macro2::Ident>,
    encode_ident: &'a Option<proc_macro2::Ident>,
}

fn instr_info_fields(
    parts: &InstrInfoParts<'_>,
    sched_tables: &SchedTables,
    inst_name: &str,
) -> Vec<proc_macro2::TokenStream> {
    let InstrInfoParts {
        op_name_lit,
        mnemonic_lit,
        program,
        width_bytes_lit,
        max_width_bytes,
        uncond_pc,
        cond_pc,
        control_flow,
        implicit_items,
        ports_ident,
        reads_memory,
        writes_memory,
        desc_ident,
        encode_ident,
    } = parts;
    let mut info_fields = vec![
        quote! { name: #op_name_lit },
        quote! { mnemonic: #mnemonic_lit },
        quote! { program: #program },
    ];
    if *max_width_bytes != 0 {
        info_fields.push(quote! { width_bytes: #width_bytes_lit });
    }
    if *uncond_pc || *cond_pc {
        info_fields.push(quote! { control_flow: #control_flow });
    }
    if !implicit_items.is_empty() {
        info_fields.push(quote! { implicit_regs: &[ #(#implicit_items),* ] });
    }
    info_fields.push(quote! { regs: &#ports_ident });
    if *reads_memory || *writes_memory {
        info_fields.push(quote! {
            effects: tir::backend::MemoryEffects {
                reads: #reads_memory,
                writes: #writes_memory,
            }
        });
    }
    if let Some(desc_ident) = desc_ident {
        info_fields.push(quote! { asm: Some(&#desc_ident) });
    }
    if let Some(encode_ident) = encode_ident {
        info_fields.push(quote! { encode: Some(&#encode_ident) });
    }
    // `InstrInfo::BASE` already costs one cycle.
    let cost = sched_tables.cost(inst_name);
    if cost != 1 {
        let cost_lit = proc_macro2::Literal::u32_unsuffixed(cost);
        info_fields.push(quote! { cost: #cost_lit });
    }
    if let Some(sched_ts) = sched_tables.sched(inst_name) {
        info_fields.push(quote! { sched: #sched_ts });
    }
    info_fields
}

fn emit_instruction(
    tables: &TargetTables<'_>,
    item_cache: &HashMap<&str, &ast::Item>,
    sched_tables: &SchedTables,
    inst: &ast::Instruction,
    options: &InstructionOptions<'_>,
    out: &mut InstrOutputs<'_>,
) -> Result<(), TMDLError> {
    let dialect = options.dialect;
    let name_ident = format_ident!("{}Op", &inst.name);
    let builder_ident = format_ident!("{}OpBuilder", &inst.name);
    let resolved_params = resolve_params_for_instruction(inst, item_cache);
    let mnemonic = resolved_params
        .get("MNEMONIC")
        .and_then(|(_, value)| value.as_ref())
        .and_then(resolve_string);
    let opname = resolved_params
        .get("OPNAME")
        .and_then(|(_, value)| value.as_ref())
        .and_then(resolve_string);

    let op_name = if let Some(opname) = opname.as_deref() {
        opname
    } else if let Some(mnemonic) = mnemonic.as_deref() {
        mnemonic
    } else {
        return Err(TMDLError::Codegen(format!(
            "Instruction '{}' must define OPNAME or MNEMONIC",
            inst.name
        )));
    };

    let mnemonic_name = mnemonic.as_deref().unwrap_or(op_name);
    let encoding_shapes = get_encoding_shapes(inst, item_cache);
    // The same view of the operands the shape expansion was computed
    // against, so a runtime guard reads a condition at the width the
    // expansion decided it at.
    let encoding_ctx = crate::utils::encoding_context(inst, item_cache);
    let (min_width_bytes, max_width_bytes) = width_range(&encoding_shapes);
    let op_name_lit = proc_macro2::Literal::string(op_name);
    // Width expressions resolve against the same cross-ISA parameter view
    // `execute()` uses (the per-ISA maximum, e.g. XLEN=64 for RV32+RV64).
    let ops = resolve_operand_widths(
        resolve_operands_for_instruction(inst, item_cache),
        &resolve_isa_param_values(inst, item_cache),
    );
    let ops_map = ops.clone().into_iter().collect::<HashMap<_, _>>();
    let operand_constraints = resolve_operand_constraints_for_instruction(inst, item_cache);
    let defined_register_operands = infer_defined_register_operands(&inst.behavior, &ops);

    // Build attributes schema from the operands that are not registers: a
    // register operand is an SSA port, and the slot's attribute exists only
    // when it names a physical register (see `tir::backend::RegPort`).
    let attrs_schema = attrs_schema_ts(&ops);

    let numeric_params: HashMap<String, i64> = resolve_params_for_instruction(inst, item_cache)
        .into_iter()
        .filter_map(|(name, (_ty, value))| match value {
            Some(ast::Expr::Lit(ast::Lit::Int(li))) => {
                Some((name, parse_literal_value(&li) as i64))
            }
            _ => None,
        })
        .collect();

    // `execute()` binds ISA parameters (e.g. `XLEN`) from here at runtime.
    let isa_param_values: HashMap<String, i64> = resolve_isa_param_values(inst, item_cache);
    let const_size_params = ConstSizeParams {
        numeric: &numeric_params,
        isa: &isa_param_values,
    };
    let trap_handler = inst
        .for_isas
        .iter()
        .find_map(|isa| find_trap_handler(isa, item_cache));

    // A `todo()` behavior declares the instruction's semantics unmodeled: it
    // produces no selection rule and its `execute()` traps. The op still
    // prints, parses, and encodes.
    let uses_todo = behavior_uses_todo(&inst.behavior);

    // A selection rule matches the expression a `let` stands for, so the
    // pattern is built from the behavior with its bindings substituted.
    // `execute()` keeps them: that is where single evaluation matters.
    let selection_behavior = inline_let_bindings(&inst.behavior);

    // Value-rule semantics, computed ahead of the op declaration so the
    // registers the behavior reads implicitly (e.g. `VCSR::vl`) can surface
    // as demand attributes with a `Use` role. Instructions defining several
    // register operands (e.g. CSR ops writing both `rd` and `csr`) cannot be
    // modeled by a single-value DAG pattern; emitting one for the last
    // assignment would let isel match an unrelated expression, so they get no
    // selection rule. The same goes for instructions touching the PC
    // (jal/jalr/auipc): their pattern would hide the control-flow effect and
    // match unrelated arithmetic.
    let semantics = if !uses_todo
        && defined_register_operands.len() <= 1
        && !behavior_references_pc(&inst.behavior, &tables.pc_classes)
        && !behavior_has_atomic_ops(&inst.behavior)
        && !behavior_has_dynamic_sized_memory_access(&inst.behavior, &const_size_params)
        && !value_reads_flag_register(&selection_behavior, &tables.flag_classes)
        && !behavior_writes_fixed_register(&inst.behavior, &tables.flag_classes)
    {
        analyze_instruction_semantics(
            &selection_behavior,
            &ops,
            &defined_register_operands,
            &numeric_params,
            &isa_param_values,
            &tables.register_index_map,
        )
    } else {
        None
    };

    // The registers the behavior reads by path, resolved to attribute names.
    // Each becomes a demand attribute on the emitted op. Reads from a value
    // register become fixed uses for register allocation; configuration reads
    // remain demands materialized by a target pass (e.g. RISC-V `vsetvli`).
    let implicit_reads: Vec<(String, u32)> = {
        let mut reads: Vec<(String, u32)> = semantics
            .as_ref()
            .map(|s| {
                s.register_symbols
                    .iter()
                    .filter_map(|((class, index), sym)| {
                        let name = tables.register_name_map.get(&(class.clone(), *index))?;
                        if ops.iter().any(|(op_name, _)| op_name == name) {
                            return None;
                        }
                        Some((name.clone(), *sym))
                    })
                    .collect()
            })
            .unwrap_or_default();
        reads.sort();
        reads
    };

    // The op's register ports, in the order the emitters bind them: the
    // declared register operands (a two-address destination followed by the
    // `_tied` slot its read binds through), the registers the behavior reads
    // by path, then the fixed-register slots. A register operand is an SSA
    // operand or result, so this order is also the port order
    // `tir::backend::reg_slots` walks.
    let read_register_operands = infer_read_register_operands(&inst.behavior, &ops);
    let instr_ctx = InstrEmitCtx {
        inst,
        name_ident: &name_ident,
        op_name,
        dialect: options.dialect,
        ops: &ops,
        mnemonic_name,
        builder_ident: &builder_ident,
        ops_map: &ops_map,
        operand_constraints: &operand_constraints,
        defined_register_operands: &defined_register_operands,
        read_register_operands: &read_register_operands,
        implicit_reads: &implicit_reads,
    };
    let ports = instruction_ports(tables, &instr_ctx);
    let port_entries = reg_port_entries(&ports);
    let ports_ident = format_ident!("REGS_{}", inst.name.to_uppercase());
    let port_count = port_entries.len();
    out.instruction_reg_ports.push(quote! {
        static #ports_ident: [tir::backend::RegPort; #port_count] = [#(#port_entries),*];
    });

    let operands_schema = operands_schema_ts(&ports);
    // Results are variadic: an instruction defining several registers (x86
    // `div`, writing both halves of the quotient/remainder pair) has one
    // result per def port, and one that defines none has no results.
    let results_schema = if ports.iter().any(|(_, _, is_def, _)| *is_def) {
        quote! { results: R { regs: "*tir::backend::RegClassType" }, }
    } else {
        quote! {}
    };

    // An instruction that writes `PC::pc` transfers control, so it is a
    // terminator: its successors are the blocks its attributes reference
    // (a branch target rewritten to a `Block` by branch selection). This
    // makes the CFG queryable post-isel — the register allocator's liveness
    // needs real successors, and dominance becomes valid on machine IR.
    let (uncond_pc, cond_pc) = pc_writes(&inst.behavior);
    let is_terminator = uncond_pc || cond_pc;
    let (interfaces_list, terminator_impl) = if is_terminator {
        (
            quote! { [tir::backend::MachineInstruction, tir::Terminator] },
            quote! {
                impl tir::Terminator for #name_ident {
                    fn successors(&self) -> Vec<tir::BlockId> {
                        tir::backend::branch_successors(self)
                    }
                }
            },
        )
    } else {
        (quote! { [tir::backend::MachineInstruction] }, quote! {})
    };

    let implicit_items = implicit_register_items(inst, &tables.register_index_map, &tables.pc_classes);

    // One fact, two readers: the `InstrInfo::effects` derived from the
    // execute body decides both what the backend is told about the opcode's
    // memory behavior and whether the opcode has a chain to carry it in.
    let (reads_memory, writes_memory) = behavior_memory_effects(&inst.behavior);
    let state_schema = if reads_memory || writes_memory {
        quote! { state: "in_out", }
    } else {
        quote! {}
    };

    out.instruction_defs.push(quote! {
        operation! {
            #name_ident {
                name: #op_name_lit,
                dialect: #dialect,
                #operands_schema
                #results_schema
                #state_schema
                attributes: A { #attrs_schema },
                interfaces: #interfaces_list,
                format: custom,
            }
        }

        #terminator_impl
    });

    // One shared printer: what a slot is called, what class it admits and
    // whether it is a result are fields of the opcode's `InstrInfo`.
    out.instruction_custom_format_impls.push(quote! {
        impl #name_ident {
            fn custom_print<'a, 'b: 'a>(
                &'a self,
                fmt: &'a mut tir::IRFormatter<'b>,
            ) -> Result<(), std::fmt::Error> {
                tir::backend::print_machine_op(fmt, self)
            }

            fn custom_parse<'src>(
                parser: &mut tir::parse::text::Parser<'src>,
                _context: &tir::Context,
            ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
            }
        }
    });

    if let Some(semantics) = &semantics {
        emit_value_rules(tables, &instr_ctx, semantics, out);
    }

    if !uses_todo && defined_register_operands.is_empty() {
        emit_branch_rules(
            tables,
            &instr_ctx,
            &numeric_params,
            &isa_param_values,
            out,
        );
    }

    let width_bytes_lit = {
        let min = proc_macro2::Literal::u8_unsuffixed(min_width_bytes);
        let max = proc_macro2::Literal::u8_unsuffixed(max_width_bytes);
        quote! { (#min, #max) }
    };
    let mnemonic_lit = proc_macro2::Literal::string(mnemonic_name);

    // The behavior RHS to compile. Normal instructions assign to a register
    // operand (`rd`); a conditional branch instead writes `PC::pc`, which we
    // synthesize into a single value-producing expression written to PC.
    let resolved_rhs = resolve_behavior_rhs(&selection_behavior, &ops, &defined_register_operands);
    let branch_value = if resolved_rhs.is_none() {
        synthesize_branch_value(&selection_behavior, u64::from(max_width_bytes))
    } else {
        None
    };
    let codegen_rhs: Option<&ast::Expr> = branch_value.as_ref().or(resolved_rhs);

    if let Some(rhs) = codegen_rhs
        && !uses_todo
        && !behavior_has_atomic_ops(&inst.behavior)
        && !behavior_has_dynamic_sized_memory_access(&inst.behavior, &const_size_params)
        && let Some(impl_ts) = emit_as_sem_expr_impl(rhs, &name_ident, &numeric_params, &isa_param_values)
    {
        out.as_sem_expr_impls.push(impl_ts);
    }

    let behavior_ctx = RustBehaviorCtx {
        ops: &ops,
        isa_param_values: &isa_param_values,
        reg_kinds: &tables.reg_kinds,
    };
    let program = instruction_program_ts(
        inst,
        branch_value.as_ref(),
        uses_todo,
        trap_handler,
        &numeric_params,
        &tables.register_index_map,
        &behavior_ctx,
    );
    // branch.
    let control_flow = match (uncond_pc, cond_pc) {
        (true, _) => quote! { tir::backend::ControlFlow::Unconditional },
        (false, true) => quote! { tir::backend::ControlFlow::Conditional },
        (false, false) => quote! { tir::backend::ControlFlow::None },
    };

    let info_ident = info_ident(&inst.name);
    // Filled in below when this instruction has an assembly syntax, a binary
    // encoding, or a patchable immediate; each becomes a field of its
    // `InstrInfo` rather than an entry in a string-keyed side table.
    let desc_ident = match resolve_asm_template_for_instruction(inst, item_cache) {
        Some(template) => emit_instruction_assembly(
            tables,
            &instr_ctx,
            &template,
            options.text_only,
            options.custom_assembly,
            out,
        ),
        None => None,
    };
    let mut encode_ident: Option<proc_macro2::Ident> = None;
    out.machine_instruction_impls.push(quote! {
        impl tir::backend::MachineInstruction for #name_ident {
            fn instance(&self) -> &tir::OpHandle {
                &self.0
            }

            fn info(&self) -> &'static tir::backend::InstrInfo {
                &#info_ident
            }
        }
    });


    // Text-only pseudo-ISAs have no binary encoding, so no encoder is
    // emitted at all (rather than an empty, unused table).
    if let Some(encoder) = (!options.text_only)
        .then(|| {
            emit_instruction_encoder(
                inst,
                &encoding_shapes,
                &ops_map,
                &operand_constraints,
                &resolved_params,
                &encoding_ctx,
            )
        })
        .transpose()?
        .flatten()
    {
        out.instruction_encoder_impls.push(encoder);
        encode_ident = Some(format_ident!("ENCODE_{}", inst.name.to_uppercase()));
    }

    if let Some((decoder, decode_spec_ident, specificity)) = emit_instruction_decoder(
        inst,
        &encoding_shapes,
        &ops_map,
        &resolved_params,
        options.dialect,
        op_name,
    ) {
        out.instruction_decoder_impls.push(decoder);
        out.instruction_decoder_dispatch.push((specificity, decode_spec_ident));
    }

    // One record per opcode, spelling only what departs from
    // One record per opcode, spelling only what departs from
    // `InstrInfo::BASE`.
    let info_fields = instr_info_fields(
        &InstrInfoParts {
            op_name_lit: &op_name_lit,
            mnemonic_lit: &mnemonic_lit,
            program: &program,
            width_bytes_lit: &width_bytes_lit,
            max_width_bytes,
            uncond_pc,
            cond_pc,
            control_flow: &control_flow,
            implicit_items: &implicit_items,
            ports_ident: &ports_ident,
            reads_memory,
            writes_memory,
            desc_ident: &desc_ident,
            encode_ident: &encode_ident,
        },
        sched_tables,
        &inst.name,
    );
    out.instruction_infos.push(quote! {
        static #info_ident: tir::backend::InstrInfo = tir::backend::InstrInfo {
            #(#info_fields,)*
            ..tir::backend::InstrInfo::BASE
        };
    });
    out.instruction_info_idents.push(info_ident);
    Ok(())
}

fn assembly_registry_section(
    custom_assembly: bool,
    registry_visibility: &Option<proc_macro2::TokenStream>,
    instruction_descs: &[proc_macro2::TokenStream],
    instruction_parsers_impls: &[proc_macro2::TokenStream],
    instruction_parser_rows: &[proc_macro2::TokenStream],
) -> proc_macro2::TokenStream {
    let lint_allow = generated_lint_allow();
    if custom_assembly {
    quote! {
        #lint_allow
        #registry_visibility fn get_instruction_parsers(
            _features: &[Feature],
        ) -> (
            std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>>,
            std::collections::HashSet<String>,
        ) {
            (std::collections::HashMap::new(), std::collections::HashSet::new())
        }
    }
} else {
    quote! {
        use tir::backend::asm_desc::{
            self, AsmSymbol, ImmConstraint, InstrDesc, ParseStep, PrintPart,
        };

        #(#instruction_descs)*

        /// Text to op is a genuine reverse lookup, so the parser keeps a
        /// mnemonic index; printing goes straight through `InstrInfo::asm`.
        #lint_allow
        #registry_visibility fn get_instruction_parsers(
            features: &[Feature],
        ) -> (
            std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>>,
            std::collections::HashSet<String>,
        ) {
            let mut map: std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>> = std::collections::HashMap::new();
            let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
            #(#instruction_parsers_impls)*
            static PARSERS: &[(&str, &[Feature], tir::backend::AsmInstructionParser)] =
                &[#(#instruction_parser_rows),*];
            for (mnemonic, required, parser) in PARSERS {
                if features_enabled(features, required) {
                    map.entry((*mnemonic).to_string()).or_default().push(*parser);
                } else {
                    disabled.insert((*mnemonic).to_string());
                }
            }
            disabled.retain(|mnemonic| !map.contains_key(mnemonic));
            (map, disabled)
        }
    }
}
}

fn instruction_infos_section(
    instruction_infos: &[proc_macro2::TokenStream],
    instruction_info_idents: &[proc_macro2::Ident],
    public_visibility: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
    #(#instruction_infos)*

    /// Every opcode this module describes, as the one record per opcode the
    /// backend interface reads.
    static INSTRUCTION_INFOS: &[&tir::backend::InstrInfo] =
        &[#(&#instruction_info_idents),*];

    #public_visibility fn instruction_infos() -> &'static [&'static tir::backend::InstrInfo] {
        INSTRUCTION_INFOS
    }
    }
}

fn decode_section(
    instruction_decoder_impls: &[proc_macro2::TokenStream],
    decode_spec_idents: &[proc_macro2::Ident],
    public_visibility: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let lint_allow = generated_lint_allow();
    quote! {
    #(#instruction_decoder_impls)*

    /// Decode a 32-bit little-endian machine word into a freshly-built op in
    /// `context`, returning its id, or `None` if no instruction matches.
    /// Instructions are tried most-specific-first (by count of fixed opcode
    /// bits); each matches on its fixed opcode bits and reconstructs its
    /// operands from the word.
    #lint_allow
    #public_visibility fn decode_instruction(context: &tir::Context, word: u32) -> Option<tir::OpId> {
        static DECODE_SPECS: &[&tir::backend::binary::DecodeSpec] = &[#(&#decode_spec_idents),*];
        DECODE_SPECS
            .iter()
            .find_map(|spec| tir::backend::binary::decode_with(context, word, spec))
    }
    }
}

fn isel_rules_section(
    isel_rule_emitters: &[proc_macro2::TokenStream],
    rule_spec_idents: &[proc_macro2::Ident],
    public_visibility: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
    #(#isel_rule_emitters)*

    static RULE_SPECS: &[&tir::backend::isel::RuleSpec] = &[#(&#rule_spec_idents),*];

    /// Instruction-selection rules for the instructions available under `features`.
    #public_visibility fn get_isel_rules(context: &tir::Context, features: &[Feature]) -> Vec<tir::backend::isel::Rule> {
        // Width-sensitive operands are constrained to their register class's
        // architectural width under the enabled features (e.g. XLEN).
        let register_widths = register_widths(features);
        let feature_ids: Vec<u16> = features.iter().map(|f| *f as u16).collect();
        tir::backend::isel::build_rules(
            context,
            &feature_ids,
            SEM_KINDS,
            SEM_BLOB,
            &register_widths,
            RULE_SPECS,
        )
        }
    }
}

fn emit_instructions<'a>(
    files: &'a [ast::File],
    instruction_files: &[&'a ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    sched_tables: &SchedTables,
    options: InstructionOptions<'_>,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let InstructionOptions {
        dialect,
        text_only,
        custom_assembly,
        include_global_rules,
        module_fragment,
    } = options;
    let registry_visibility = module_fragment.then(|| quote! { pub(super) });
    let public_visibility = if module_fragment {
        quote! { pub(super) }
    } else {
        quote! { pub }
    };
    let mut instruction_defs = vec![];
    let mut instruction_parsers_impls: Vec<proc_macro2::TokenStream> = vec![];
    // Each entry carries its syntax and operand specificity so same-mnemonic
    // candidates can be ordered most-constrained-first.
    let mut instruction_parser_candidates: Vec<(
        String,
        usize,
        usize,
        u32,
        usize,
        proc_macro2::TokenStream,
    )> = vec![];
    // One descriptor per assembled instruction: the shared runtime parser and
    // printer interpret these instead of a generated function body per
    // instruction. Reached through `InstrInfo::asm`.
    let mut instruction_descs: Vec<proc_macro2::TokenStream> = vec![];
    // One `InstrInfo` per instruction — the single per-opcode record — plus the
    // table of every one of them.
    let mut instruction_infos: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_info_idents: Vec<proc_macro2::Ident> = vec![];
    let mut isel_rule_emitters: Vec<proc_macro2::TokenStream> = vec![];
    let mut rule_spec_idents: Vec<proc_macro2::Ident> = vec![];
    let mut machine_instruction_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_custom_format_impls: Vec<proc_macro2::TokenStream> = vec![];
    // One `RegPort` table per instruction, referenced by its `InstrInfo`.
    let mut instruction_reg_ports: Vec<proc_macro2::TokenStream> = vec![];
    let mut as_sem_expr_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_encoder_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_decoder_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_decoder_dispatch: Vec<(u32, proc_macro2::Ident)> = vec![];
    // Data-driven assembly syntax (text-only targets): one entry per instruction,
    // consumed by a target-specific front-end to parse/print instruction bodies.
    let mut asm_syntax_entries: Vec<proc_macro2::TokenStream> = vec![];
    let instruction_options = InstructionOptions {
        dialect,
        text_only,
        custom_assembly,
        include_global_rules,
        module_fragment,
    };
    let tables = collect_target_tables(files);

    for inst in instruction_files.iter().flat_map(|f| f.instructions()) {
        let mut out = InstrOutputs {
            instruction_defs: &mut instruction_defs,
            instruction_parsers_impls: &mut instruction_parsers_impls,
            instruction_parser_candidates: &mut instruction_parser_candidates,
            instruction_descs: &mut instruction_descs,
            instruction_infos: &mut instruction_infos,
            instruction_info_idents: &mut instruction_info_idents,
            isel_rule_emitters: &mut isel_rule_emitters,
            rule_spec_idents: &mut rule_spec_idents,
            machine_instruction_impls: &mut machine_instruction_impls,
            instruction_custom_format_impls: &mut instruction_custom_format_impls,
            instruction_reg_ports: &mut instruction_reg_ports,
            as_sem_expr_impls: &mut as_sem_expr_impls,
            instruction_encoder_impls: &mut instruction_encoder_impls,
            instruction_decoder_impls: &mut instruction_decoder_impls,
            instruction_decoder_dispatch: &mut instruction_decoder_dispatch,
            asm_syntax_entries: &mut asm_syntax_entries,
        };
        emit_instruction(&tables, item_cache, sched_tables, inst, &instruction_options, &mut out)?;
    }

    // Flag-mediated rules: definer + branch pairs composed into conditional
    // branch rules, and definer + reader pairs into boolean value rules.
    if include_global_rules {
        emit_flag_rules(
            files,
            item_cache,
            &tables.register_index_map,
            &tables.pc_classes,
            &tables.flag_classes,
            dialect,
            &mut isel_rule_emitters,
            &mut rule_spec_idents,
        )?;
        emit_fixed_register_rules(
            files,
            item_cache,
            &tables.register_index_map,
            &tables.register_name_map,
            dialect,
            &mut isel_rule_emitters,
            &mut rule_spec_idents,
        )?;
    }

    // Most-specific-wins: try encodings that fix more opcode bits first, so a
    // more-general encoding declared earlier cannot shadow a specific one that
    // should claim the word. `sort_by_key` is stable, preserving declaration
    // order among equally-specific encodings.
    instruction_decoder_dispatch.sort_by_key(|d| std::cmp::Reverse(d.0));
    let decode_spec_idents: Vec<proc_macro2::Ident> = instruction_decoder_dispatch
        .into_iter()
        .map(|(_, ident)| ident)
        .collect();

    // Order same-mnemonic asm parser candidates most-constrained-first so the
    // per-mnemonic dispatch tries a tighter form before a looser one, regardless
    // of declaration order. Keys, in order:
    //   1. syntax arity, descending — a longer form is tried before a shorter
    //      form it shares a prefix with;
    //   2. total immediate bit-width, ascending — an immediate operand is the loosest
    //      match (it accepts a bare register identifier or keyword as a symbol), so a
    //      form without an immediate precedes one with, and among immediate forms imm8
    //      precedes imm32. This keeps register/keyword forms ahead of the immediate
    //      form that would swallow them (arm64 `add x,x,x`; x86 `shl dst, cl`);
    //   3. operand count, descending;
    //   4. register-class-size sum, ascending — a smaller class (2-register `GPRsib`)
    //      precedes a larger one (16-register `GPR`).
    // The stable sort keeps declaration order among equally specific candidates.
    instruction_parser_candidates.sort_by(|a, b| {
        (
            &a.0,
            std::cmp::Reverse(a.1),
            a.3,
            std::cmp::Reverse(a.2),
            a.4,
        )
            .cmp(&(
                &b.0,
                std::cmp::Reverse(b.1),
                b.3,
                std::cmp::Reverse(b.2),
                b.4,
            ))
    });
    let instruction_parser_rows: Vec<proc_macro2::TokenStream> = instruction_parser_candidates
        .into_iter()
        .map(|(.., tokens)| tokens)
        .collect();

    let lint_allow = generated_lint_allow();
    // Data-driven assembly syntax table, emitted only for text-only targets;
    // their front-end parses/prints instruction bodies from the table.
    let syntax_section = if text_only {
        quote! {
            /// The assembly syntax of every instruction, for a text-only target's
            /// front-end parser and printer.
            #lint_allow
            #public_visibility fn asm_syntax() -> &'static [tir::backend::asm_syntax::InstrSyntax] {
                &[#(#asm_syntax_entries),*]
            }
        }
    } else {
        quote! {}
    };

    // The `ENCODE_*`/`PATCH_*` specs object emission interprets, reached through
    // `InstrInfo`. Only targets with a binary encoding have any.
    let encoder_section = if text_only {
        quote! {}
    } else {
        quote! { #(#instruction_encoder_impls)* }
    };

    let assembly_registry_section = assembly_registry_section(
        custom_assembly,
        &registry_visibility,
        &instruction_descs,
        &instruction_parsers_impls,
        &instruction_parser_rows,
    );


    let infos_section = instruction_infos_section(
        &instruction_infos,
        &instruction_info_idents,
        &public_visibility,
    );
    let decode_section = decode_section(
        &instruction_decoder_impls,
        &decode_spec_idents,
        &public_visibility,
    );
    let isel_section = isel_rules_section(
        &isel_rule_emitters,
        &rule_spec_idents,
        &public_visibility,
    );

    Ok(quote! {
        #(#instruction_defs)*
        #(#instruction_reg_ports)*
        #(#instruction_custom_format_impls)*
        #(#machine_instruction_impls)*
        #(#as_sem_expr_impls)*

        #assembly_registry_section

        #infos_section

        #syntax_section

        #encoder_section

        #decode_section

        #isel_section
    })
}

fn find_trap_handler<'a>(
    isa: &str,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<&'a ast::TrapHandler> {
    let mut pending = vec![isa];
    let mut visited = HashSet::new();
    while let Some(name) = pending.pop() {
        if !visited.insert(name) {
            continue;
        }
        let Some(ast::Item::Isa(isa)) = item_cache.get(name) else {
            continue;
        };
        if let Some(handler) = &isa.trap_handler {
            return Some(handler);
        }
        match &isa.requires {
            None => {}
            Some(ast::IsaRequirement::Single(parent)) => pending.push(parent),
            Some(ast::IsaRequirement::Any(parents)) | Some(ast::IsaRequirement::All(parents)) => {
                pending.extend(parents.iter().map(String::as_str));
            }
        }
    }
    None
}
