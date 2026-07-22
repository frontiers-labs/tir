struct InstructionOptions<'a> {
    dialect: &'a str,
    text_only: bool,
    custom_assembly: bool,
    include_global_rules: bool,
    module_fragment: bool,
}

fn emit_instructions<'a>(
    files: &'a [ast::File],
    instruction_files: &[&'a ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
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
    // Each entry carries its specificity key (operand count, total immediate
    // bit-width, sum of register-class sizes) so same-mnemonic candidates can be
    // ordered most-constrained-first, independent of declaration order.
    let mut instruction_parser_candidates: Vec<(
        String,
        usize,
        u32,
        usize,
        proc_macro2::TokenStream,
    )> = vec![];
    let mut instruction_printers_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_printer_map_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut isel_rule_emitters: Vec<proc_macro2::TokenStream> = vec![];
    let mut isel_rule_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut machine_instruction_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_custom_format_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut as_sem_expr_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_encoder_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_encoder_map_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_patcher_map_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_decoder_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_decoder_dispatch: Vec<(u128, proc_macro2::Ident)> = vec![];
    // Data-driven assembly syntax (text-only targets): one entry per instruction,
    // consumed by a target-specific front-end to parse/print instruction bodies.
    let mut asm_syntax_entries: Vec<proc_macro2::TokenStream> = vec![];
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

    for inst in instruction_files.iter().flat_map(|f| f.instructions()) {
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
        let op_name_lit = proc_macro2::Literal::string(op_name);
        // Width expressions resolve against the same cross-ISA parameter view
        // `execute()` uses (the per-ISA maximum, e.g. XLEN=64 for RV32+RV64).
        let ops = resolve_operand_widths(
            resolve_operands_for_instruction(inst, item_cache),
            &resolve_isa_param_values(inst, item_cache),
        );
        let ops_map = ops.clone().into_iter().collect::<HashMap<_, _>>();
        let defined_register_operands = infer_defined_register_operands(&inst.behavior, &ops);

        // Build attributes schema from operands
        let attrs_schema = {
            let mut items = vec![];
            for (name, ty) in &ops {
                let field_ident = format_ident!("{}", name);
                let ty_ts = match ty {
                    Type::Struct(_) => quote! { Register },
                    Type::Integer | Type::Bits(_) => quote! { Integer },
                    Type::String => quote! { String },
                    _ => unreachable!("HM type vars should not appear as operand types"),
                };
                items.push(quote! { #field_ident: #ty_ts });
            }
            quote! { #(#items,)* }
        };

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
        let trap_handler = inst
            .for_isas
            .iter()
            .find_map(|isa| find_trap_handler(isa, item_cache));

        // A `todo()` behavior declares the instruction's semantics unmodeled: it
        // produces no selection rule and its `execute()` traps. The op still
        // prints, parses, and encodes.
        let uses_todo = behavior_uses_todo(&inst.behavior);

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
            && !behavior_references_pc(&inst.behavior, &pc_classes)
            && !behavior_has_atomic_ops(&inst.behavior)
            && !behavior_reads_flag_register(&inst.behavior, &flag_classes)
        {
            analyze_instruction_semantics(
                inst,
                &ops,
                &defined_register_operands,
                &numeric_params,
                &isa_param_values,
                &register_index_map,
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
                            let name = register_name_map.get(&(class.clone(), *index))?;
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

        // Build roles from behavior assignments so we don't depend on naming
        // conventions. An operand both written and read (e.g. the two-address x86
        // `dst = dst + src`) is ReadWrite; its isel-emitted op additionally carries
        // a `<name>_tied` register attribute naming the value the read binds to,
        // which register allocation lowers to a copy (see `lower_tied_operands`).
        let read_register_operands = infer_read_register_operands(&inst.behavior, &ops);
        let roles_schema = {
            let mut items = vec![];
            for (name, ty) in &ops {
                if let Type::Struct(_) = ty {
                    let field_ident = format_ident!("{}", name);
                    let role = if defined_register_operands.contains(name) {
                        if read_register_operands.contains(name) {
                            quote! { ReadWrite }
                        } else {
                            quote! { Def }
                        }
                    } else {
                        quote! { Use }
                    };
                    items.push(quote! { #field_ident: #role });
                    if defined_register_operands.contains(name)
                        && read_register_operands.contains(name)
                    {
                        let tied_ident = format_ident!("{}_tied", name);
                        items.push(quote! { #tied_ident: Use });
                    }
                }
            }
            for (name, _) in &implicit_reads {
                let field_ident = format_ident!("{}", name);
                items.push(quote! { #field_ident: Use });
            }
            items.extend(fixed_register_role_items(
                inst,
                &ops,
                &register_index_map,
                &register_name_map,
                &flag_classes,
                &pc_classes,
            ));
            quote! { #(#items,)* }
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

        instruction_defs.push(quote! {
            operation! {
                #name_ident {
                    name: #op_name_lit,
                    dialect: #dialect,
                    attributes: A { #attrs_schema },
                    roles: R { #roles_schema },
                    interfaces: #interfaces_list,
                    format: custom,
                }
            }

            #terminator_impl
        });

        let op_display_name = format!("{}.{}", dialect, op_name);
        let op_display_name_lit = proc_macro2::Literal::string(&op_display_name);
        let mut register_attr_print_arms = Vec::new();
        let mut text_only_register_attrs = Vec::new();
        for (op_name, op_ty) in &ops {
            if let Type::Struct(class_name) = op_ty {
                let attr_name_lit = proc_macro2::Literal::string(op_name);
                text_only_register_attrs.push(attr_name_lit.clone());
                let print_fn_ident = format_ident!("print_{}", class_name.to_lowercase());
                // Text-only targets use one nominal operand class and derive the real
                // class per register (PTX banks), so print through the attribute's
                // stored class. Encoded targets print through the operand's declared
                // class table: the operand position fixes the class, so an aliasing
                // physical register (e.g. `("GPR", 29)` landing in a `GPRsp` operand)
                // still prints the right name.
                let print_body = if text_only {
                    quote! {
                        if let tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Physical { class, index }) = &attr.value {
                            if let Some(name) = register_name(class.name(), *index, false) {
                                fmt.write(name)?;
                            } else {
                                attr.value.print(fmt, &context)?;
                            }
                        } else {
                            attr.value.print(fmt, &context)?;
                        }
                    }
                } else {
                    quote! {
                        if let tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Physical { index, .. }) = &attr.value {
                            if let Some(name) = #print_fn_ident(*index, false) {
                                fmt.write(name)?;
                            } else {
                                attr.value.print(fmt, &context)?;
                            }
                        } else {
                            attr.value.print(fmt, &context)?;
                        }
                    }
                };
                register_attr_print_arms.push(quote! {
                    #attr_name_lit => { #print_body }
                });
            }
        }
        // A demand attribute holds a value register whose class is only known at
        // run time (the attribute value carries it), so it prints through the
        // class-dispatching `register_name`.
        for (name, _) in &implicit_reads {
            let attr_name_lit = proc_macro2::Literal::string(name);
            text_only_register_attrs.push(attr_name_lit.clone());
            register_attr_print_arms.push(quote! {
                #attr_name_lit => {
                    if let tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Physical { class, index }) = &attr.value {
                        if let Some(name) = register_name(class.name(), *index, false) {
                            fmt.write(name)?;
                        } else {
                            attr.value.print(fmt, &context)?;
                        }
                    } else {
                        attr.value.print(fmt, &context)?;
                    }
                }
            });
        }
        let custom_print_attr_body = if register_attr_print_arms.is_empty() {
            quote! {
                attr.value.print(fmt, &context)?;
            }
        } else {
            quote! {
                match attr.name.as_str() {
                    #(#register_attr_print_arms,)*
                    _ => attr.value.print(fmt, &context)?,
                }
            }
        };
        let custom_format_impl = if text_only {
            quote! {
                fn custom_print<'a, 'b: 'a>(
                    &'a self,
                    fmt: &'a mut tir::IRFormatter<'b>,
                ) -> Result<(), std::fmt::Error> {
                    custom_print_text_operation(
                        &self.0,
                        #op_display_name_lit,
                        &[#(#text_only_register_attrs),*],
                        fmt,
                    )
                }

                fn custom_parse<'src>(
                    parser: &mut tir::parse::text::Parser<'src>,
                    _context: &tir::Context,
                ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                    custom_parse_text_operation(parser)
                }
            }
        } else {
            quote! {
                fn custom_print<'a, 'b: 'a>(
                    &'a self,
                    fmt: &'a mut tir::IRFormatter<'b>,
                ) -> Result<(), std::fmt::Error> {
                    use tir::Operation;

                    fmt.write(#op_display_name_lit)?;
                    if !self.attributes().is_empty() {
                        fmt.write(" ")?;
                        fmt.write("{")?;
                        let mut first = true;
                        let context = self.0.context.upgrade();
                        for attr in self.attributes() {
                            if !first {
                                fmt.write(", ")?;
                            }
                            first = false;
                            fmt.write(&attr.name)?;
                            fmt.write(" = ")?;
                            #custom_print_attr_body
                        }
                        fmt.write("}")?;
                    }
                    fmt.write("\n")?;
                    Ok(())
                }

                fn custom_parse<'src>(
                    parser: &mut tir::parse::text::Parser<'src>,
                    _context: &tir::Context,
                ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                    Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
                }
            }
        };
        instruction_custom_format_impls.push(quote! {
            impl #name_ident {
                #custom_format_impl
            }
        });

        if let Some(semantics) = &semantics {
            let emit_fn_ident = format_ident!("emit_isel_{}", inst.name.to_lowercase());
            let pattern_fn_ident = format_ident!("isel_pattern_{}", inst.name.to_lowercase());
            let rule_name_lit = proc_macro2::Literal::string(&inst.name.to_lowercase());

            // Per-operand constraints: registers must bind to non-constant values,
            // immediates to constants. Keyed by the operand's pattern symbol id.
            let mut operand_constraint_entries: Vec<proc_macro2::TokenStream> = Vec::new();
            for (op_name, op_ty) in &ops {
                let Some(&symbol) = semantics.variable_symbols.get(op_name) else {
                    continue;
                };
                let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
                let constraint = match op_ty {
                    Type::Struct(_) => quote! { tir::graph::OperandConstraint::Register },
                    Type::Bits(_) | Type::Integer => {
                        quote! { tir::graph::OperandConstraint::Immediate }
                    }
                    _ => continue,
                };
                operand_constraint_entries.push(quote! { (#symbol_lit, #constraint) });
            }
            // A data register the behavior reads by path (e.g. the x86 shift count
            // in `GPR::rcx`, whose class is also a value-operand class) reads that
            // register's *value*, so it must bind a register, never a folded
            // constant — a constant count belongs to the immediate form. Without
            // this the count is stuffed into the reg as a dead attribute and the
            // encoder emits the by-`cl` form reading garbage. A config-register
            // demand (e.g. RISC-V `VCSR::vl`) is a different class and unaffected.
            let value_reg_classes: Vec<&str> = ops
                .iter()
                .filter_map(|(_, ty)| match ty {
                    Type::Struct(class) => Some(class.as_str()),
                    _ => None,
                })
                .collect();
            let mut fixed_value_reads = HashMap::new();
            for ((class, index), symbol) in &semantics.register_symbols {
                let is_implicit = register_name_map
                    .get(&(class.clone(), *index))
                    .map(|name| !ops.iter().any(|(op_name, _)| op_name == name))
                    .unwrap_or(false);
                let value_class = register_files.get(class).and_then(|fixed_file| {
                    value_reg_classes.iter().find(|value_class| {
                        register_files
                            .get(**value_class)
                            .is_some_and(|file| file == fixed_file)
                    })
                });
                if is_implicit && let Some(value_class) = value_class {
                    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(*symbol);
                    operand_constraint_entries
                        .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Register) });
                    let index = u16::try_from(*index).expect("register indices fit u16");
                    fixed_value_reads.insert(*symbol, ((*value_class).to_string(), index));
                }
            }

            let mut emit_attr_steps = Vec::new();
            for (op_name, op_ty) in &ops {
                let op_name_lit = proc_macro2::Literal::string(op_name);
                match op_ty {
                    Type::Struct(class_name) => {
                        let class_id = reg_class_id(class_name);
                        if let Some(def_pos) = defined_register_operands
                            .iter()
                            .position(|name| name == op_name)
                        {
                            let def_pos_lit = proc_macro2::Literal::usize_unsuffixed(def_pos);
                            let result_accessor = if def_pos == 0 {
                                quote! { .first() }
                            } else {
                                quote! { .get(#def_pos_lit) }
                            };
                            emit_attr_steps.push(quote! {
                                let dst = req
                                    .results
                                    #result_accessor
                                    .ok_or(tir::PassError::RewriteFailed(req.op_id()))?
                                    .number();
                                builder = builder.attr(
                                    #op_name_lit,
                                    tir::attributes::AttributeValue::Register(
                                        tir::attributes::RegisterAttr::Virtual {
                                            id: dst,
                                            class: Some(#class_id),
                                        },
                                    ),
                                );
                            });
                            // A two-address destination also reads a pattern operand:
                            // record the bound value in a `_tied` attribute so register
                            // allocation can lower the tie to a copy.
                            if read_register_operands.contains(op_name)
                                && let Some(sym) = semantics.variable_symbols.get(op_name)
                            {
                                let tied_name_lit =
                                    proc_macro2::Literal::string(&format!("{op_name}_tied"));
                                let sym_lit = proc_macro2::Literal::u32_unsuffixed(*sym);
                                emit_attr_steps.push(quote! {
                                    let tied = m.value_binding(#sym_lit).ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                                    builder = builder.attr(
                                        #tied_name_lit,
                                        tir::attributes::AttributeValue::Register(
                                            tir::attributes::RegisterAttr::Virtual {
                                                id: tied.number(),
                                                class: Some(#class_id),
                                            },
                                        ),
                                    );
                                });
                            }
                        } else if let Some(sym) = semantics.variable_symbols.get(op_name) {
                            let sym_lit = proc_macro2::Literal::u32_unsuffixed(*sym);
                            emit_attr_steps.push(quote! {
                                let src = m.value_binding(#sym_lit).ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                                builder = builder.attr(
                                    #op_name_lit,
                                    tir::attributes::AttributeValue::Register(
                                        tir::attributes::RegisterAttr::Virtual {
                                            id: src.number(),
                                            class: Some(#class_id),
                                        },
                                    ),
                                );
                            });
                        } else if let Some(Some(reg_idx)) =
                            semantics.fixed_register_by_class.get(class_name)
                        {
                            let idx_lit = proc_macro2::Literal::u16_unsuffixed(*reg_idx);
                            emit_attr_steps.push(quote! {
                                builder = builder.attr(
                                    #op_name_lit,
                                    tir::attributes::AttributeValue::Register(
                                        tir::attributes::RegisterAttr::Physical {
                                            class: #class_id,
                                            index: #idx_lit,
                                        },
                                    ),
                                );
                            });
                        }
                    }
                    Type::Integer | Type::Bits(_) => {
                        if let Some(sym) = semantics.variable_symbols.get(op_name) {
                            let sym_lit = proc_macro2::Literal::u32_unsuffixed(*sym);
                            emit_attr_steps.push(quote! {
                                let v = m.int_binding(#sym_lit).ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                                builder = builder.attr(
                                    #op_name_lit,
                                    tir::attributes::AttributeValue::Int(v),
                                );
                            });
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
            let immediate_symbols: std::collections::HashSet<u32> = ops
                .iter()
                .filter(|(_, op_ty)| matches!(op_ty, Type::Bits(_) | Type::Integer))
                .filter_map(|(op_name, _)| semantics.variable_symbols.get(op_name).copied())
                .collect();
            let (canon_pattern, canon_root, forced_widths) = tir::sem::canonicalize_for_selection(
                &semantics.pattern,
                semantics.root,
                &immediate_symbols,
            );
            let mut pattern_widths = tir::sem::infer_widths(&canon_pattern, |_| None);
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
            let dst_class = defined_register_operands
                .first()
                .and_then(|name| ops_map.get(name))
                .and_then(|ty| match ty {
                    Type::Struct(class) => Some(class.as_str()),
                    _ => None,
                });
            if pattern_widths[canon_root.index()].is_none()
                && scalar_root_kind(tir::graph::Dag::get_node(&canon_pattern, canon_root))
                && let Some(dst_class) = dst_class
                && let Some(width) = literal_register_class_width(files, dst_class)
            {
                pattern_widths[canon_root.index()] = Some(width);
            }
            let (mut pattern_stmts, root_var) =
                emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);
            // The destination's full guarded semantics, emitted alongside the
            // relaxed pattern so pass construction proves the guard drop sound.
            let guarded_fn_ident = format_ident!("isel_guarded_{}", inst.name.to_lowercase());
            let (guarded_emitter, guarded_semantics_call) = match &semantics.guarded_semantics {
                Some((guarded, guarded_root)) => {
                    let guarded_widths = tir::sem::infer_widths(guarded, |_| None);
                    let (guarded_stmts, _) =
                        emit_dag_as_code(guarded, *guarded_root, &guarded_widths);
                    (
                        quote! {
                            fn #guarded_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
                                use tir::graph::MutDag;
                                let mut g = tir::sem::SemGraph::new();
                                #(#guarded_stmts)*
                                g
                            }
                        },
                        quote! { .with_guarded_semantics(#guarded_fn_ident(context)) },
                    )
                }
                None => (quote! {}, quote! {}),
            };
            if *tir::graph::Dag::get_node(&canon_pattern, canon_root)
                == tir::sem::SymKind::Bitcast
                && let Some(dst_class) = dst_class
                && float_classes.contains(dst_class)
                && let Some(width) = literal_register_class_width(files, dst_class)
            {
                let result_ty = match width {
                    32 => quote! { tir::builtin::FloatType::f32(_context) },
                    64 => quote! { tir::builtin::FloatType::f64(_context) },
                    _ => unreachable!("unsupported scalar float register width {width}"),
                };
                pattern_stmts.insert(0, quote! { use tir::graph::MetaMutDag as _; });
                pattern_stmts.push(quote! { g.set_actual_type(#root_var, #result_ty); });
            }
            let operand_register_call = emit_operand_register_call(
                &ops,
                &semantics.variable_symbols,
                &width_sensitive_symbols(&canon_pattern, &pattern_widths),
                &float_classes,
                &polymorphic_classes,
            );
            let result_register_call =
                emit_result_register_call(dst_class, &float_classes, &polymorphic_classes);
            let operand_imm_range_call = emit_operand_imm_range_call(&immediate_operand_ranges(
                &semantics.pattern,
                &ops,
                &semantics.variable_symbols,
            ));
            // Cost reflects the canonical pattern's size (one machine instruction).
            let base_cost = {
                use tir::graph::Dag;
                (canon_pattern.len() as u32 + implicit_reads.len() as u32).max(1)
            };
            let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
            let mnemonic_cost_lit = proc_macro2::Literal::string(mnemonic_name);

            // Registers read by path are dependencies outside the encoded operands.
            // Value-register reads carry a fixed-use constraint; configuration
            // reads remain demands for a target pass such as `vsetvli` insertion.
            for (name, sym) in &implicit_reads {
                let name_lit = proc_macro2::Literal::string(name);
                let sym_lit = proc_macro2::Literal::u32_unsuffixed(*sym);
                if let Some((class, index)) = fixed_value_reads.get(sym) {
                    let class_id = reg_class_id(class);
                    let index_lit = proc_macro2::Literal::u16_unsuffixed(*index);
                    emit_attr_steps.push(quote! {
                        let src = m.value_binding(#sym_lit)
                            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                        builder = builder.attr(
                            #name_lit,
                            tir::attributes::AttributeValue::Register(
                                tir::attributes::RegisterAttr::FixedUse {
                                    id: src.number(),
                                    class: #class_id,
                                    index: #index_lit,
                                },
                            ),
                        );
                    });
                    continue;
                }
                emit_attr_steps.push(quote! {
                    if let Some(v) = m.int_binding(#sym_lit) {
                        builder = builder.attr(#name_lit, tir::attributes::AttributeValue::Int(v));
                    } else {
                        let src = m.value_binding(#sym_lit)
                            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                        builder = builder.attr(
                            #name_lit,
                            tir::attributes::AttributeValue::Register(
                                tir::attributes::RegisterAttr::Virtual {
                                    id: src.number(),
                                    class: None,
                                },
                            ),
                        );
                    }
                });
            }

            isel_rule_emitters.push(quote! {
                fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
                    use tir::graph::MutDag;
                    let mut g = tir::sem::SemGraph::new();
                    #(#pattern_stmts)*
                    g
                }

                #guarded_emitter

                fn #emit_fn_ident(
                    context: &tir::Context,
                    req: &tir::backend::isel::EmitRequest,
                    m: &tir::backend::isel::RuleMatch,
                ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                    let _ = (req, m);
                    let mut builder = #builder_ident::new(context);
                    #(#emit_attr_steps)*
                    Ok(Box::new(builder.build()))
                }
            });

            let inst_features = feature_slice(&inst.for_isas);
            isel_rule_inits.push(quote! {
                if features_enabled(features, #inst_features) {
                    rules.push(
                        tir::backend::isel::Rule::new(
                            #rule_name_lit,
                            #pattern_fn_ident(context),
                            // base_cost is the larger of the canonical pattern size and the
                            // TMDL-modeled instruction cost, so a genuinely expensive
                            // instruction (high `unit` latency) outweighs the structural proxy.
                            (#base_cost_lit).max(instruction_cost(#mnemonic_cost_lit)),
                            #emit_fn_ident,
                        )
                        .with_operand_constraints(vec![#(#operand_constraint_entries),*])
                        #operand_register_call
                        #result_register_call
                        #operand_imm_range_call
                        #guarded_semantics_call
                        ,
                    );
                }
            });

            // Zero-form constant materializer: when the canonical pattern is
            // `reg + imm` and the source register's class has a hardwired-zero
            // register (RISC-V `addi rs1:GPR`), derive a rule matching
            // `zext(0b0, W) + imm` — the shape the constant-materializer bridge
            // injects into fitting program-constant classes — with the register
            // slot wired to the zero register, so a bare constant selects as
            // the canonical `li` (`addi rd, x0, imm`). arm64's add-immediate
            // reads `GPRsp`, whose encoding 31 is `sp`, not a hardwired zero,
            // so no zero-form is derived there.
            let zero_form = match defined_register_operands.as_slice() {
                [rd_name] if !read_register_operands.contains(rd_name)
                    && implicit_reads.is_empty() =>
                {
                    value_zero_form_operands(
                        &canon_pattern,
                        canon_root,
                        &ops,
                        &semantics.variable_symbols,
                        rd_name,
                        |class: &str| {
                            hardwired_zero_index.contains_key(class)
                                && !float_classes.contains(class)
                                && !polymorphic_classes.contains(class)
                        },
                    )
                }
                _ => None,
            };
            if let Some((zero_reg_name, zero_reg_class, imm_sym)) = zero_form {
                let zero_pattern_fn_ident =
                    format_ident!("isel_pattern_{}_zero", inst.name.to_lowercase());
                let zero_emit_fn_ident =
                    format_ident!("emit_isel_{}_zero", inst.name.to_lowercase());
                let zero_rule_name_lit =
                    proc_macro2::Literal::string(&format!("{}_zero", inst.name.to_lowercase()));
                let width_sym = semantics
                    .variable_symbols
                    .values()
                    .chain(semantics.register_symbols.values())
                    .copied()
                    .max()
                    .unwrap_or(0)
                    + 1;
                let width_sym_lit = proc_macro2::Literal::u32_unsuffixed(width_sym);
                let imm_sym_lit = proc_macro2::Literal::u32_unsuffixed(imm_sym);
                let (zero_meta_use, root_ty_stmt) = pattern_widths[canon_root.index()]
                    .map(|width| {
                        let width_lit = proc_macro2::Literal::u32_unsuffixed(width);
                        (
                            quote! { use tir::graph::MetaMutDag as _; },
                            quote! {
                                g.set_actual_type(
                                    __root,
                                    tir::builtin::IntegerType::new(_context, #width_lit),
                                );
                            },
                        )
                    })
                    .unwrap_or_default();

                let rd_name = defined_register_operands
                    .first()
                    .expect("zero-form requires a defined register operand");
                let rd_name_lit = proc_macro2::Literal::string(rd_name);
                let rd_class_id = reg_class_id(dst_class.expect("defined operand has a class"));
                let zero_reg_name_lit = proc_macro2::Literal::string(&zero_reg_name);
                let zero_class_id = reg_class_id(&zero_reg_class);
                let zero_index_lit =
                    proc_macro2::Literal::u16_unsuffixed(hardwired_zero_index[&zero_reg_class]);
                let imm_name = ops
                    .iter()
                    .find(|(name, _)| semantics.variable_symbols.get(name) == Some(&imm_sym))
                    .map(|(name, _)| name.clone())
                    .expect("immediate operand has a name");
                let imm_name_lit = proc_macro2::Literal::string(&imm_name);
                let zero_imm_range_call = emit_operand_imm_range_call(
                    &immediate_operand_ranges(&semantics.pattern, &ops, &semantics.variable_symbols)
                        .into_iter()
                        .filter(|(symbol, _, _)| *symbol == imm_sym)
                        .collect::<Vec<_>>(),
                );

                isel_rule_emitters.push(quote! {
                    fn #zero_pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
                        #zero_meta_use
                        use tir::graph::MutDag;
                        let mut g = tir::sem::SemGraph::new();
                        let __zero = g.add_node(tir::sem::SymKind::Constant);
                        g.set_leaf_data(__zero, tir::sem::int_payload(1, 0, false));
                        let __width = g.add_node(tir::sem::SymKind::Symbol);
                        g.set_leaf_data(__width, tir::sem::SymPayload::SymbolId(#width_sym_lit));
                        let __zext = g.add_node(tir::sem::SymKind::ZExt);
                        g.add_edge(__zext, __zero);
                        g.add_edge(__zext, __width);
                        let __imm = g.add_node(tir::sem::SymKind::Symbol);
                        g.set_leaf_data(__imm, tir::sem::SymPayload::SymbolId(#imm_sym_lit));
                        let __root = g.add_node(tir::sem::SymKind::Add);
                        g.add_edge(__root, __zext);
                        g.add_edge(__root, __imm);
                        #root_ty_stmt
                        g
                    }

                    fn #zero_emit_fn_ident(
                        context: &tir::Context,
                        req: &tir::backend::isel::EmitRequest,
                        m: &tir::backend::isel::RuleMatch,
                    ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                        let mut builder = #builder_ident::new(context);
                        let dst = req
                            .results
                            .first()
                            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?
                            .number();
                        builder = builder.attr(
                            #rd_name_lit,
                            tir::attributes::AttributeValue::Register(
                                tir::attributes::RegisterAttr::Virtual {
                                    id: dst,
                                    class: Some(#rd_class_id),
                                },
                            ),
                        );
                        builder = builder.attr(
                            #zero_reg_name_lit,
                            tir::attributes::AttributeValue::Register(
                                tir::attributes::RegisterAttr::Physical {
                                    class: #zero_class_id,
                                    index: #zero_index_lit,
                                },
                            ),
                        );
                        let v = m
                            .int_binding(#imm_sym_lit)
                            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                        builder = builder.attr(
                            #imm_name_lit,
                            tir::attributes::AttributeValue::Int(v),
                        );
                        Ok(Box::new(builder.build()))
                    }
                });

                isel_rule_inits.push(quote! {
                    if features_enabled(features, #inst_features) {
                        rules.push(
                            tir::backend::isel::Rule::new(
                                #zero_rule_name_lit,
                                #zero_pattern_fn_ident(context),
                                (5).max(instruction_cost(#mnemonic_cost_lit)),
                                #zero_emit_fn_ident,
                            )
                            .with_operand_constraints(vec![(
                                #imm_sym_lit,
                                tir::graph::OperandConstraint::Immediate,
                            )])
                            #result_register_call
                            #zero_imm_range_call
                            ,
                        );
                    }
                });
            }
        }

        // A guarded PC write (`if cond { PC::pc = PC::pc + imm }`) becomes a
        // conditional-branch rule: the pattern is the branch condition over the
        // encoded operands, and the target operand is emitted as a block
        // attribute bound by branch selection.
        if !uses_todo
            && defined_register_operands.is_empty()
            && let Some(branch) = analyze_branch_semantics(
                inst,
                &ops,
                &numeric_params,
                &isa_param_values,
                &register_index_map,
                &pc_classes,
            )
        {
            let inst_features = feature_slice(&inst.for_isas);
            let no_zero_slots = HashMap::new();
            let (emitter, init) = emit_cond_branch_rule(
                &inst.name.to_lowercase(),
                &builder_ident,
                mnemonic_name,
                &inst_features,
                &ops,
                &branch.pattern,
                branch.root,
                &branch.variable_symbols,
                &branch.target_operand,
                branch.target_symbol,
                &no_zero_slots,
                &float_classes,
                &polymorphic_classes,
            );
            isel_rule_emitters.push(emitter);
            isel_rule_inits.push(init);

            // Zero-form variants: when the branch condition is a two-register
            // comparison whose operands belong to a class with a hardwired-zero
            // register (RISC-V `x0`), derive one rule per slot that wires that slot
            // to the zero register, so `cmpi x, 0`-style guards (and bare i1
            // conditions the bridge rewrites to `x != 0`) select the branch
            // directly instead of materializing the constant. The zeroed slot is
            // lowered as `zext(0b0, W)` — the shape the arm64 cbz/cbnz path and the
            // bare-i1 bridge produce, so all three unify in the program e-graph.
            let (root_kind, root_children) = {
                use tir::graph::Dag;
                (
                    *branch.pattern.get_node(branch.root),
                    branch.pattern.children(branch.root).collect::<Vec<_>>(),
                )
            };
            let root_is_comparison = {
                use tir::sem::SymKind::*;
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
                    use tir::graph::Dag;
                    root_children
                        .iter()
                        .map(|&child| {
                            let symbol = match branch.pattern.get_leaf_data(child) {
                                Some(tir::sem::SymPayload::SymbolId(s)) => *s,
                                _ => return None,
                            };
                            let (name, class) = ops.iter().find_map(|(name, ty)| {
                                let Type::Struct(class) = ty else { return None };
                                (branch.variable_symbols.get(name) == Some(&symbol)
                                    && hardwired_zero_index.contains_key(class))
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
                let slots = if matches!(root_kind, tir::sem::SymKind::Eq | tir::sem::SymKind::Ne) {
                    slots.into_iter().rev().collect::<Vec<_>>()
                } else {
                    slots
                };
                for (slot_index, (op_name, class_name, reg_symbol)) in slots.iter().enumerate() {
                    let width_symbol = branch.target_symbol + 1;
                    let (zero_pattern, zero_root) = branch_pattern_with_zero(
                        &branch.pattern,
                        branch.root,
                        *reg_symbol,
                        width_symbol,
                    );
                    let mut zero_variable_symbols = branch.variable_symbols.clone();
                    zero_variable_symbols.remove(op_name);
                    let mut zero_slots = HashMap::new();
                    zero_slots.insert(
                        op_name.clone(),
                        (class_name.clone(), hardwired_zero_index[class_name]),
                    );
                    let rule_name = format!("{}_zero{}", inst.name.to_lowercase(), slot_index);
                    let (emitter, init) = emit_cond_branch_rule(
                        &rule_name,
                        &builder_ident,
                        mnemonic_name,
                        &inst_features,
                        &ops,
                        &zero_pattern,
                        zero_root,
                        &zero_variable_symbols,
                        &branch.target_operand,
                        branch.target_symbol,
                        &zero_slots,
                        &float_classes,
                        &polymorphic_classes,
                    );
                    isel_rule_emitters.push(emitter);
                    isel_rule_inits.push(init);
                }
            }
        }

        let encoding_arms = get_encoding_arms(inst, item_cache);
        // With no encoding (a text-only pseudo-ISA) there is no binary width; report
        // 0 bytes rather than the 32-bit default assumed for real ISAs.
        let width_bytes = encoding_arms
            .iter()
            .map(|arm| arm.end.unwrap_or(arm.start))
            .max()
            .map(|max_end| ((max_end + 1) as u32).div_ceil(8) as u64)
            .unwrap_or(0);
        let width_bytes_lit = proc_macro2::Literal::u8_unsuffixed(width_bytes as u8);
        let mnemonic_lit = proc_macro2::Literal::string(mnemonic_name);

        // The behavior RHS to compile. Normal instructions assign to a register
        // operand (`rd`); a conditional branch instead writes `PC::pc`, which we
        // synthesize into a single value-producing expression written to PC.
        let resolved_rhs = resolve_behavior_rhs(inst, &ops, &defined_register_operands);
        let branch_value = if resolved_rhs.is_none() {
            synthesize_branch_value(inst, width_bytes)
        } else {
            None
        };
        let codegen_rhs: Option<&ast::Expr> = branch_value.as_ref().or(resolved_rhs);

        if let Some(rhs) = codegen_rhs
            && !behavior_has_atomic_ops(&inst.behavior)
            && let Some(impl_ts) = emit_as_sem_expr_impl(rhs, &name_ident, &numeric_params)
        {
            as_sem_expr_impls.push(impl_ts);
        }

        let behavior_ctx = RustBehaviorCtx {
            ops: &ops,
            isa_param_values: &isa_param_values,
            mnemonic: &mnemonic_lit,
            reg_kinds: &reg_kinds,
        };
        let execute_body = if let Some(branch_val) = branch_value.as_ref() {
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
                value: Box::new(branch_val.clone()),
                span: branch_if.span,
            });
            match emit_behavior_exec(
                &normalized,
                trap_handler,
                &numeric_params,
                &register_index_map,
                &behavior_ctx,
            ) {
                Some(body) => quote! {
                    #body
                    Ok(())
                },
                None => quote! {
                    Err(tir::backend::SimTrap::InvalidInstruction {
                        op: #mnemonic_lit,
                        reason: "failed to convert behavior to executable expression".to_string(),
                    })
                },
            }
        } else if uses_todo {
            quote! {
                Err(tir::backend::SimTrap::InvalidInstruction {
                    op: #mnemonic_lit,
                    reason: "instruction semantics are not modeled (todo)".to_string(),
                })
            }
        } else {
            match emit_behavior_exec(
                &inst.behavior,
                trap_handler,
                &numeric_params,
                &register_index_map,
                &behavior_ctx,
            ) {
                Some(body) => quote! {
                    #body
                    Ok(())
                },
                None => quote! {
                    Err(tir::backend::SimTrap::InvalidInstruction {
                        op: #mnemonic_lit,
                        reason: "failed to convert behavior to executable expression".to_string(),
                    })
                },
            }
        };

        // Control-flow kind, derived from the behavior's `PC::pc` writes: every
        // path writes PC → unconditional transfer; some paths → conditional
        // branch. The trait default covers sequential instructions.
        let control_flow_method = match (uncond_pc, cond_pc) {
            (true, _) => quote! {
                fn control_flow(&self) -> tir::backend::ControlFlow {
                    tir::backend::ControlFlow::Unconditional
                }
            },
            (false, true) => quote! {
                fn control_flow(&self) -> tir::backend::ControlFlow {
                    tir::backend::ControlFlow::Conditional
                }
            },
            (false, false) => quote! {},
        };

        // A no-op behavior (e.g. c.nop) or an unmodeled (`todo()`) one whose
        // `execute()` only traps never touches the machine context.
        let behavior_is_empty = matches!(&inst.behavior, ast::Expr::Block(b) if b.stmts.is_empty());
        let machine_param = if (behavior_is_empty || uses_todo) && branch_value.is_none() {
            quote! { _machine }
        } else {
            quote! { machine }
        };
        machine_instruction_impls.push(quote! {
            impl tir::backend::MachineInstruction for #name_ident {
                fn mnemonic(&self) -> &'static str {
                    #mnemonic_lit
                }

                fn width_bytes(&self) -> u8 {
                    #width_bytes_lit
                }

                fn execute(
                    &self,
                    #machine_param: &mut dyn tir::backend::MachineContext,
                ) -> Result<(), tir::backend::SimTrap> {
                    #execute_body
                }

                #control_flow_method
            }
        });

        // Emit parser implementations based on asm template (simple template support)
        if let Some(template) = resolve_asm_template_for_instruction(inst, item_cache) {
            let actions = compile_asm_template(&template);
            // Operand-less instructions (e.g. ecall) consume no tokens beyond
            // the mnemonic and set no attributes.
            let parses_operands = actions.iter().any(|a| {
                matches!(
                    a,
                    AsmAction::Comma
                        | AsmAction::LParen
                        | AsmAction::RParen
                        | AsmAction::LBracket
                        | AsmAction::RBracket
                        | AsmAction::Star
                        | AsmAction::Plus
                        | AsmAction::Operand(_)
                        | AsmAction::Keyword(_)
                )
            });

            let mut parse_steps: Vec<proc_macro2::TokenStream> = Vec::new();
            for act in actions {
                match act {
                    AsmAction::Comma => {
                        parse_steps.push(quote! {
                            parser
                                .expect_symbol(tir::parse::tokens::Symbol::Comma)
                                .map_err(|_| ())?;
                        });
                    }
                    AsmAction::Operand(op_name) => {
                        if let Some(ty) = ops_map.get(&op_name) {
                            let op_name_lit = proc_macro2::Literal::string(&op_name);
                            match ty {
                                Type::Struct(class_name) => {
                                    let fn_ident =
                                        format_ident!("parse_{}", class_name.to_lowercase());
                                    let class_id = reg_class_id(class_name);
                                    parse_steps.push(quote! {
                                        let idx = #fn_ident(parser).ok_or(())?;
                                        op_builder = op_builder.attr(
                                            #op_name_lit,
                                            tir::attributes::AttributeValue::Register(
                                                tir::attributes::RegisterAttr::Physical {
                                                    class: #class_id,
                                                    index: idx,
                                                },
                                            ),
                                        );
                                    });
                                }
                                Type::Integer | Type::Bits(_) => {
                                    // Reject integers that do not fit the operand's
                                    // `bits<N>` width so the per-mnemonic dispatch
                                    // backtracks to a wider form instead of failing
                                    // later in the encoder. Mirrors the encoder's
                                    // union of the signed and unsigned N-bit ranges:
                                    // [-(2^(N-1)), 2^N - 1].
                                    let imm_guard = match ty {
                                        Type::Bits(n) if *n < 64 => {
                                            let min = proc_macro2::Literal::i64_suffixed(
                                                -(1i64 << (n - 1)),
                                            );
                                            let max = proc_macro2::Literal::i64_suffixed(1i64 << n);
                                            Some(
                                                quote! { if !(#min..#max).contains(&value) { return Err(()); } },
                                            )
                                        }
                                        _ => None,
                                    };
                                    parse_steps.push(quote! {
                                        let val = if let Some(tok) = parser.peek() {
                                            match tok {
                                                tir::backend::Token::DecNumber(n) => {
                                                    let value = (*n).parse::<i64>().map_err(|_| ())?;
                                                    #imm_guard
                                                    let _ = parser.bump();
                                                    tir::attributes::AttributeValue::Int(value)
                                                }
                                                tir::backend::Token::HexNumber(h) => {
                                                    let s = *h;
                                                    let neg = s.starts_with('-');
                                                    let s = if neg { &s[1..] } else { s };
                                                    let s = if s.starts_with("0x") || s.starts_with("0X") { &s[2..] } else { s };
                                                    let v = i128::from_str_radix(s, 16).map_err(|_| ())?;
                                                    let v = if neg { -v } else { v };
                                                    let value: i64 = v.try_into().map_err(|_| ())?;
                                                    #imm_guard
                                                    let _ = parser.bump();
                                                    tir::attributes::AttributeValue::Int(value)
                                                }
                                                // A bare identifier in an immediate position is a
                                                // symbol reference, resolved at object emission.
                                                tir::backend::Token::Ident(name) => {
                                                    let symbol = (*name).to_string();
                                                    let _ = parser.bump();
                                                    tir::attributes::AttributeValue::Str(symbol)
                                                }
                                                _ => { return Err(()); }
                                            }
                                        } else { return Err(()); };
                                        op_builder = op_builder.attr(#op_name_lit, val);
                                    });
                                }
                                Type::String => {
                                    // Strings in asm templates aren't currently used as operands; skip for now.
                                    parse_steps.push(quote! { let _ = parser.peek(); });
                                }
                                _ => {}
                            }
                        }
                    }
                    AsmAction::Skip => {
                        parse_steps.push(quote! {});
                    }
                    AsmAction::SkipMnemonic => {
                        parse_steps.push(quote! {});
                    }
                    AsmAction::LParen => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir::backend::Token::LParen) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::RParen => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir::backend::Token::RParen) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::LBracket => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir::backend::Token::LBracket) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::RBracket => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir::backend::Token::RBracket) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::Star => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir::backend::Token::Star) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::Plus => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir::backend::Token::Plus) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::Keyword(kw) => {
                        let kw_lit = proc_macro2::Literal::string(&kw);
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir::backend::Token::Ident(s)) if *s == #kw_lit => {}
                                _ => return Err(()),
                            }
                        });
                    }
                }
            }

            let print_parts = compile_asm_printer_template(&template, mnemonic_name);

            // Accumulate the data-driven syntax entry (text-only targets consume
            // this). Each part is either literal text or a typed operand slot.
            if text_only {
                let part_tokens: Vec<proc_macro2::TokenStream> = print_parts
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
                    .collect();
                let op_name_lit_s = proc_macro2::Literal::string(op_name);
                let mnemonic_lit_s = proc_macro2::Literal::string(mnemonic_name);
                asm_syntax_entries.push(quote! {
                    tir::backend::asm_syntax::InstrSyntax {
                        op_name: #op_name_lit_s,
                        mnemonic: #mnemonic_lit_s,
                        parts: &[#(#part_tokens),*],
                    }
                });
            }

            if custom_assembly {
                continue;
            }

            let prints_operands = print_parts
                .iter()
                .any(|p| matches!(p, AsmPrintPart::Operand(_)));
            let mut print_steps: Vec<proc_macro2::TokenStream> = Vec::new();
            for part in print_parts {
                match part {
                    AsmPrintPart::Text(text) => {
                        if !text.is_empty() {
                            let mut chars = text.chars();
                            let first = chars.next().expect("text is not empty");
                            if chars.next().is_none() {
                                let char_lit = proc_macro2::Literal::character(first);
                                print_steps.push(quote! {
                                    out.push(#char_lit);
                                });
                            } else {
                                let text_lit = proc_macro2::Literal::string(&text);
                                print_steps.push(quote! {
                                    out.push_str(#text_lit);
                                });
                            }
                        }
                    }
                    AsmPrintPart::Operand(op_name) => {
                        if let Some(ty) = ops_map.get(&op_name) {
                            let op_name_lit = proc_macro2::Literal::string(&op_name);
                            match ty {
                                Type::Struct(class_name) => {
                                    let fn_ident =
                                        format_ident!("print_{}", class_name.to_lowercase());
                                    print_steps.push(quote! {
                                        let attr = attrs.iter().find(|attr| attr.name == #op_name_lit)?;
                                        let operand = match &attr.value {
                                            tir::attributes::AttributeValue::Register(
                                                tir::attributes::RegisterAttr::Physical { index, .. },
                                            ) => #fn_ident(*index, false)?,
                                            tir::attributes::AttributeValue::Register(
                                                tir::attributes::RegisterAttr::Virtual { id, .. },
                                            ) => format!("%virt{id}"),
                                            _ => return None,
                                        };
                                        out.push_str(&operand);
                                    });
                                }
                                Type::Integer | Type::Bits(_) => {
                                    print_steps.push(quote! {
                                        let attr = attrs.iter().find(|attr| attr.name == #op_name_lit)?;
                                        match &attr.value {
                                            tir::attributes::AttributeValue::Int(value) => {
                                                out.push_str(&value.to_string());
                                            }
                                            tir::attributes::AttributeValue::UInt(value) => {
                                                out.push_str(&value.to_string());
                                            }
                                            tir::attributes::AttributeValue::Str(symbol) => {
                                                out.push_str(symbol);
                                            }
                                            // A local branch target: print the block's label,
                                            // falling back to `.L<n>` for unnamed blocks.
                                            tir::attributes::AttributeValue::Block(block) => {
                                                match _ctx.get_block(*block).attr("name") {
                                                    Some(tir::attributes::AttributeValue::Str(label)) => {
                                                        out.push_str(&label);
                                                    }
                                                    _ => {
                                                        out.push_str(".L");
                                                        out.push_str(&block.number().to_string());
                                                    }
                                                }
                                            }
                                            _ => return None,
                                        }
                                    });
                                }
                                Type::String => {
                                    print_steps.push(quote! {
                                        let attr = attrs.iter().find(|attr| attr.name == #op_name_lit)?;
                                        match &attr.value {
                                            tir::attributes::AttributeValue::Str(value) => {
                                                out.push_str(value);
                                            }
                                            _ => return None,
                                        }
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            let print_fn_ident = format_ident!("print_{}_inst", &inst.name.to_lowercase());
            // Operand-less instructions (e.g. ecall) never consult the attributes.
            let (op_param, attrs_binding) = if prints_operands {
                (quote! { op }, quote! { let attrs = &op.attributes; })
            } else {
                (quote! { _op }, quote! {})
            };
            if !custom_assembly {
                instruction_printers_impls.push(quote! {
                    fn #print_fn_ident(_ctx: &tir::Context, #op_param: &tir::OpInstance) -> Option<String> {
                        #attrs_binding
                        let mut out = String::new();
                        #(#print_steps)*
                        Some(out)
                    }
                });
            }

            let printer_op_name_lit = proc_macro2::Literal::string(op_name);
            if !custom_assembly {
                instruction_printer_map_inits.push(quote! {
                    let f: tir::backend::AsmInstructionPrinter = #print_fn_ident;
                    map.insert(#printer_op_name_lit.to_string(), f);
                });
            }

            let parse_fn_ident = format_ident!("parse_{}_inst", &inst.name.to_lowercase());
            let (parser_param, builder_binding) = if parses_operands {
                (quote! { parser }, quote! { let mut op_builder })
            } else {
                (quote! { _parser }, quote! { let op_builder })
            };
            if !custom_assembly {
                instruction_parsers_impls.push(quote! {
                    fn #parse_fn_ident<'src>(
                        context: &tir::Context,
                        builder: &mut tir::IRBuilder,
                        #parser_param: &mut tir::parse::tokens::Parser<'src, tir::backend::Token<'src>>,
                    ) -> Result<(), ()> {
                        #builder_binding = #builder_ident::new(context);
                        #(#parse_steps)*
                        let op = op_builder.build();
                        builder.insert(op);
                        Ok(())
                    }
                });
            }

            if let Some(mn) = mnemonic.as_deref().or(Some(op_name)) {
                let mn_lit = proc_macro2::Literal::string(mn);
                let inst_features = feature_slice(&inst.for_isas);
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
                if !custom_assembly {
                    instruction_parser_candidates.push((
                    mn.to_string(),
                    arity,
                    imm_bits,
                    reg_specificity,
                    quote! {
                        if features_enabled(features, #inst_features) {
                            let f: tir::backend::AsmInstructionParser = #parse_fn_ident;
                            map.entry(#mn_lit.to_string()).or_default().push(f);
                        } else {
                            disabled.insert(#mn_lit.to_string());
                        }
                    },
                    ));
                }
            }
        }

        // Text-only pseudo-ISAs have no binary encoding, so no encoders/patchers
        // are emitted at all (rather than empty, unused functions).
        if let Some((encoder, patcher)) = (!text_only)
            .then(|| {
                emit_instruction_encoder(
                    inst,
                    &encoding_arms,
                    &ops_map,
                    &resolved_params,
                    width_bytes,
                )
            })
            .transpose()?
            .flatten()
        {
            let encode_fn_ident = format_ident!("encode_{}_inst", inst.name.to_lowercase());
            instruction_encoder_impls.push(encoder);
            instruction_encoder_map_inits.push(quote! {
                let f: tir::backend::binary::InstructionEncoder = #encode_fn_ident;
                map.insert(#op_name_lit.to_string(), f);
            });
            if let Some(patcher) = patcher {
                let patch_fn_ident = format_ident!("patch_{}_inst", inst.name.to_lowercase());
                instruction_encoder_impls.push(patcher);
                instruction_patcher_map_inits.push(quote! {
                    let f: tir::backend::binary::InstructionPatcher = #patch_fn_ident;
                    map.insert(#op_name_lit.to_string(), f);
                });
            }
        }

        if let Some((decoder, decode_fn_ident, fixed_mask)) = emit_instruction_decoder(
            inst,
            &encoding_arms,
            &ops_map,
            &resolved_params,
            width_bytes,
        ) {
            instruction_decoder_impls.push(decoder);
            instruction_decoder_dispatch.push((fixed_mask, decode_fn_ident));
        }
    }

    // Flag-mediated rules: definer + branch pairs composed into conditional
    // branch rules, and definer + reader pairs into boolean value rules.
    if include_global_rules {
        emit_flag_rules(
            files,
            item_cache,
            &register_index_map,
            &pc_classes,
            &flag_classes,
            &mut isel_rule_emitters,
            &mut isel_rule_inits,
        )?;
        emit_fixed_register_rules(
            files,
            item_cache,
            &register_index_map,
            &register_name_map,
            &mut isel_rule_emitters,
            &mut isel_rule_inits,
        )?;
    }

    // Most-specific-wins: try encodings that fix more opcode bits first, so a
    // more-general encoding declared earlier cannot shadow a specific one that
    // should claim the word. `sort_by_key` is stable, preserving declaration
    // order among equally-specific encodings.
    instruction_decoder_dispatch.sort_by_key(|d| std::cmp::Reverse(d.0.count_ones()));
    let instruction_decoder_dispatch: Vec<proc_macro2::TokenStream> = instruction_decoder_dispatch
        .into_iter()
        .map(|(_, ident)| {
            quote! {
                if let Some(id) = #ident(context, word) {
                    return Some(id);
                }
            }
        })
        .collect();

    // Order same-mnemonic asm parser candidates most-constrained-first so the
    // per-mnemonic dispatch tries a tighter form before a looser one, regardless
    // of declaration order. Keys, in order:
    //   1. total immediate bit-width, ascending — an immediate operand is the loosest
    //      match (it accepts a bare register identifier or keyword as a symbol), so a
    //      form without an immediate precedes one with, and among immediate forms imm8
    //      precedes imm32. This keeps register/keyword forms ahead of the immediate
    //      form that would swallow them (arm64 `add x,x,x`; x86 `shl dst, cl`);
    //   2. operand count, descending — with equal immediate width, a longer form is
    //      tried before a shorter one it shares a prefix with, so `imul rax, rbx` is
    //      not stolen by the 1-operand `imul rax`;
    //   3. register-class-size sum, ascending — a smaller class (2-register `GPRsib`)
    //      precedes a larger one (16-register `GPR`).
    // The stable sort keeps declaration order among equally specific candidates.
    instruction_parser_candidates.sort_by(|a, b| {
        (&a.0, a.2, std::cmp::Reverse(a.1), a.3).cmp(&(&b.0, b.2, std::cmp::Reverse(b.1), b.3))
    });
    let instruction_parser_map_inits: Vec<proc_macro2::TokenStream> = instruction_parser_candidates
        .into_iter()
        .map(|(.., tokens)| tokens)
        .collect();

    // Data-driven assembly syntax table, emitted only for text-only targets;
    // their front-end parses/prints instruction bodies from the table.
    let syntax_section = if text_only {
        quote! {
            /// The assembly syntax of every instruction, for a text-only target's
            /// front-end parser and printer.
            #public_visibility fn asm_syntax() -> &'static [tir::backend::asm_syntax::InstrSyntax] {
                &[#(#asm_syntax_entries),*]
            }
        }
    } else {
        quote! {}
    };

    let text_only_format_helpers = if text_only && !instruction_defs.is_empty() {
        quote! {
            fn custom_print_text_operation(
                op: &tir::OpInstance,
                op_name: &str,
                register_attributes: &[&str],
                fmt: &mut tir::IRFormatter<'_>,
            ) -> Result<(), std::fmt::Error> {
                fmt.write(op_name)?;
                if !op.attributes.is_empty() {
                    fmt.write(" ")?;
                    fmt.write("{")?;
                    let mut first = true;
                    let context = op.context.upgrade();
                    for attr in &op.attributes {
                        if !first {
                            fmt.write(", ")?;
                        }
                        first = false;
                        fmt.write(&attr.name)?;
                        fmt.write(" = ")?;
                        if register_attributes.contains(&attr.name.as_str()) {
                            if let tir::attributes::AttributeValue::Register(
                                tir::attributes::RegisterAttr::Physical { class, index },
                            ) = &attr.value
                            {
                                if let Some(name) = register_name(class.name(), *index, false) {
                                    fmt.write(name)?;
                                } else {
                                    attr.value.print(fmt, &context)?;
                                }
                            } else {
                                attr.value.print(fmt, &context)?;
                            }
                        } else {
                            attr.value.print(fmt, &context)?;
                        }
                    }
                    fmt.write("}")?;
                }
                fmt.write("\n")?;
                Ok(())
            }

            fn custom_parse_text_operation<'src>(
                parser: &mut tir::parse::text::Parser<'src>,
            ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
            }
        }
    } else {
        quote! {}
    };

    // The object-file emission interface (per-instruction encoders/patchers and
    // their lookup maps) is emitted only for targets with a binary encoding.
    let encoder_section = if text_only {
        quote! {}
    } else {
        quote! {
            #(#instruction_encoder_impls)*

            // Consumed by object-file emission.
            #registry_visibility fn get_instruction_encoders() -> std::collections::HashMap<String, tir::backend::binary::InstructionEncoder> {
                let mut map: std::collections::HashMap<String, tir::backend::binary::InstructionEncoder> = std::collections::HashMap::new();
                #(#instruction_encoder_map_inits)*

                map
            }

            #registry_visibility fn get_instruction_patchers() -> std::collections::HashMap<String, tir::backend::binary::InstructionPatcher> {
                let mut map: std::collections::HashMap<String, tir::backend::binary::InstructionPatcher> = std::collections::HashMap::new();
                #(#instruction_patcher_map_inits)*

                map
            }
        }
    };

    let assembly_registry_section = if custom_assembly {
        quote! {
            #registry_visibility fn get_instruction_parsers(
                _features: &[Feature],
            ) -> (
                std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>>,
                std::collections::HashSet<String>,
            ) {
                (std::collections::HashMap::new(), std::collections::HashSet::new())
            }

            #registry_visibility fn get_instruction_printers() -> std::collections::HashMap<String, tir::backend::AsmInstructionPrinter> {
                std::collections::HashMap::new()
            }
        }
    } else {
        quote! {
            #registry_visibility fn get_instruction_parsers(
                features: &[Feature],
            ) -> (
                std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>>,
                std::collections::HashSet<String>,
            ) {
                let mut map: std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>> = std::collections::HashMap::new();
                let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
                #(#instruction_parsers_impls)*
                #(#instruction_parser_map_inits)*
                disabled.retain(|mnemonic| !map.contains_key(mnemonic));
                (map, disabled)
            }

            #registry_visibility fn get_instruction_printers() -> std::collections::HashMap<String, tir::backend::AsmInstructionPrinter> {
                let mut map: std::collections::HashMap<String, tir::backend::AsmInstructionPrinter> = std::collections::HashMap::new();
                #(#instruction_printers_impls)*
                #(#instruction_printer_map_inits)*
                map
            }
        }
    };

    Ok(quote! {
        #(#instruction_defs)*
        #text_only_format_helpers
        #(#instruction_custom_format_impls)*
        #(#machine_instruction_impls)*
        #(#as_sem_expr_impls)*

        #assembly_registry_section

        #syntax_section

        #encoder_section

        #(#instruction_decoder_impls)*

        /// Decode a 32-bit little-endian machine word into a freshly-built op in
        /// `context`, returning its id, or `None` if no instruction matches.
        /// Instructions are tried most-specific-first (by count of fixed opcode
        /// bits); each matches on its fixed opcode bits and reconstructs its
        /// operands from the word.
        #public_visibility fn decode_instruction(context: &tir::Context, word: u32) -> Option<tir::OpId> {
            let _ = (&context, word);
            #(#instruction_decoder_dispatch)*
            None
        }

        #(#isel_rule_emitters)*

        /// Instruction-selection rules for the instructions available under `features`.
        #public_visibility fn get_isel_rules(context: &tir::Context, features: &[Feature]) -> Vec<tir::backend::isel::Rule> {
            let _ = (&context, &features);
            // Width-sensitive operands are constrained to their register class's
            // architectural width under the enabled features (e.g. XLEN).
            let __register_widths = register_widths(features);
            let _ = &__register_widths;
            #[allow(unused_mut)]
            let mut rules = Vec::new();
            #(#isel_rule_inits)*
            rules
        }
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
