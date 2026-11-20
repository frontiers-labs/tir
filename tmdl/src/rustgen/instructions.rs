use super::*;

pub(super) fn info_ident(inst_name: &str) -> proc_macro2::Ident {
    format_ident!("INFO_{}", inst_name.to_uppercase())
}

fn asm_symbol_step(symbol: &str) -> proc_macro2::TokenStream {
    let symbol_ident = format_ident!("{}", symbol);
    quote! { ParseStep::Symbol(AsmSymbol::#symbol_ident) }
}

pub(super) struct InstructionOptions<'a> {
    pub(super) dialect: &'a str,
    pub(super) text_only: bool,
    pub(super) custom_assembly: bool,
    pub(super) module_fragment: bool,
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
                parse_steps.push(quote! { ParseStep::Number(#number_lit) });
            }
            AsmAction::Keyword(kw) => {
                let kw_lit = proc_macro2::Literal::string(kw);
                parse_steps.push(quote! { ParseStep::Keyword(#kw_lit) });
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
                        let fn_ident = format_ident!("parse_{}", class_name.to_lowercase());
                        let class_id = reg_class_id(class_name);
                        parse_steps.push(quote! {
                            ParseStep::Register(#op_name_lit, #class_id, #fn_ident)
                        });
                    }
                    Type::Integer | Type::Bits(_) => {
                        let signed = action_index > 0 && plus_before_immediate[action_index - 1];
                        // Reject integers that do not fit the operand's
                        // `bits<N>` width so the per-mnemonic dispatch
                        // backtracks to a wider form instead of failing
                        // later in the encoder. Mirrors the encoder's
                        // union of the signed and unsigned N-bit ranges:
                        // [-(2^(N-1)), 2^N - 1].
                        let range = match ty {
                            Type::Bits(n) if *n < 64 => {
                                let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
                                let max = proc_macro2::Literal::i64_suffixed(1i64 << n);
                                quote! { Some((#min, #max)) }
                            }
                            _ => quote! { None },
                        };
                        let constraint = operand_constraints
                            .get(op_name)
                            .copied()
                            .unwrap_or_default();
                        let align = proc_macro2::Literal::u32_unsuffixed(constraint.align);
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
    let print_parts =
        print_parts
            .into_iter()
            .fold(Vec::new(), |mut merged: Vec<AsmPrintPart>, part| {
                match (merged.last_mut(), &part) {
                    (Some(AsmPrintPart::Text(prev)), AsmPrintPart::Text(text)) => {
                        prev.push_str(text)
                    }
                    _ => merged.push(part),
                }
                merged
            });

    let mut print_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for part in print_parts {
        match part {
            AsmPrintPart::Text(text) => {
                if !text.is_empty() {
                    let text_lit = proc_macro2::Literal::string(&text);
                    print_steps.push(quote! { PrintPart::Text(#text_lit) });
                }
            }
            AsmPrintPart::Operand(op_name) => {
                let Some(ty) = ops_map.get(&op_name) else {
                    continue;
                };
                let op_name_lit = proc_macro2::Literal::string(&op_name);
                match ty {
                    Type::Struct(class_name) => {
                        let fn_ident = format_ident!("print_{}", class_name.to_lowercase());
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
                reg_specificity = reg_specificity
                    .saturating_add(class_sizes.get(class).copied().unwrap_or(usize::MAX));
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

struct TargetTables {
    register_index_map: HashMap<(String, String), u32>,
    pc_classes: HashSet<String>,
    flag_classes: HashSet<String>,
    fp_fields: HashMap<String, FpField>,
    fp_flag_registers: HashSet<(String, String)>,
    effect_classes: HashSet<String>,
    register_name_map: HashMap<(String, u32), String>,
    register_files: HashMap<String, String>,
    float_classes: HashSet<String>,
    class_sizes: HashMap<String, usize>,
    reg_kinds: HashMap<String, (bool, u32)>,
}

struct InstrEmitCtx<'a> {
    inst: &'a ast::Instruction,
    op_name: &'a str,
    ops: &'a [(String, Type)],
    mnemonic_name: &'a str,
    builder_ident: &'a proc_macro2::Ident,
    ops_map: &'a HashMap<String, Type>,
    operand_constraints: &'a HashMap<String, OperandConstraint>,
    defined_register_operands: &'a [String],
    read_register_operands: &'a HashSet<String>,
    implicit_reads: &'a [(String, u32)],
}

fn attrs_schema_ts(ops: &[(String, Type)]) -> proc_macro2::TokenStream {
    let mut items = vec![];
    for (name, ty) in ops {
        let field_ident = format_ident!("{}", name);
        let ty_ts = match ty {
            Type::Struct(_) => quote! { "?Register" },
            Type::Integer | Type::Bits(_) => quote! { Integer },
            Type::String => quote! { String },
            _ => unreachable!("HM type vars should not appear as operand types"),
        };
        items.push(quote! { #field_ident: #ty_ts });
    }
    quote! { #(#items,)* }
}

fn instruction_values(tables: &TargetTables, ctx: &InstrEmitCtx<'_>) -> proc_macro2::TokenStream {
    let mut ports: Vec<(String, bool)> = vec![];
    for (name, ty) in ctx.ops {
        if !matches!(ty, Type::Struct(_)) {
            continue;
        }
        let defines = ctx.defined_register_operands.contains(name);
        ports.push((name.clone(), defines));
        if defines && ctx.read_register_operands.contains(name) {
            ports.push((name.clone(), false));
        }
    }
    for (name, _) in ctx.implicit_reads {
        ports.push((name.clone(), false));
    }
    for (slot, _, is_def) in fixed_register_role_items(
        ctx.inst,
        ctx.ops,
        &tables.register_index_map,
        &tables.register_name_map,
        &tables.effect_classes,
        &tables.pc_classes,
    ) {
        ports.push((slot, is_def));
    }

    let fields = |def| {
        let names: Vec<_> = ports
            .iter()
            .filter(|(_, d)| *d == def)
            .map(|(name, _)| format_ident!("{name}"))
            .collect();
        let types = "_?, ".repeat(names.len()) + "son.control?";
        (names, types)
    };
    let (uses, slots) = fields(false);
    let (defs, results) = fields(true);
    let scheme = format!("({slots}) -> ({results})");
    quote! { scheme: #scheme, operands: O { #(#uses,)* control }, results: R { #(#defs,)* control }, }
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
    tables: &TargetTables,
    ctx: &InstrEmitCtx<'_>,
    templates: &[String],
    text_only: bool,
    custom_assembly: bool,
    out: &mut InstrOutputs<'_>,
) -> Option<proc_macro2::Ident> {
    let mut canonical = None;
    for (index, template) in templates.iter().enumerate() {
        let descriptor = emit_assembly_template(
            tables,
            ctx,
            template,
            index,
            text_only,
            custom_assembly,
            out,
        );
        if index == 0 {
            canonical = descriptor;
        }
    }
    canonical
}

fn emit_assembly_template(
    tables: &TargetTables,
    ctx: &InstrEmitCtx<'_>,
    template: &str,
    index: usize,
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
    if text_only && index == 0 {
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

    let suffix = if index == 0 {
        String::new()
    } else {
        format!("_alias_{index}")
    };
    let parse_fn_ident = format_ident!("parse_{}{}_inst", &ctx.inst.name.to_lowercase(), suffix);
    if !custom_assembly {
        let ident = format_ident!(
            "DESC_{}{}",
            ctx.inst.name.to_uppercase(),
            suffix.to_uppercase()
        );
        out.instruction_descs.push(quote! {
            static #ident: InstrDesc = InstrDesc {
                parse: &[#(#parse_steps),*],
                print: &[#(#print_steps),*],
                register_names: REGISTER_NAMES,
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
        if index == 0 {
            desc_ident = Some(ident);
        }
    }

    let mn = ctx.mnemonic_name;
    let mn_lit = proc_macro2::Literal::string(mn);
    let inst_features = feature_slice(&ctx.inst.for_isas);
    let (arity, reg_specificity, imm_bits) = mnemonic_specificity(ctx.ops_map, &tables.class_sizes);
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

fn collect_target_tables(files: &[ast::File]) -> TargetTables {
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

    // These classes are excluded from implicit allocatable-register ports.
    let pc_classes: HashSet<String> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .filter(|rc| rc.has_program_counter())
        .map(|rc| rc.name.clone())
        .collect();

    let flag_classes: HashSet<String> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .filter(|rc| rc.has_status_flags())
        .map(|rc| rc.name.clone())
        .collect();

    let float_classes: HashSet<String> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .filter(|rc| rc.has_float_registers())
        .map(|rc| rc.name.clone())
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

    let fp_fields: HashMap<_, _> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .filter_map(|class| {
            class
                .resolve_registers()
                .find_map(|register| fp_field(&register.traits))
                .map(|field| (class.name.clone(), field))
        })
        .collect();
    let fp_flag_registers = files
        .iter()
        .flat_map(|file| file.register_classes())
        .flat_map(|class| {
            class.resolve_registers().filter_map(|register| {
                register
                    .traits
                    .contains(&ast::RegisterTrait::FpFlags)
                    .then(|| (class.name.clone(), register.name))
            })
        })
        .collect();
    let effect_classes = flag_classes
        .iter()
        .chain(fp_fields.keys())
        .cloned()
        .collect();
    TargetTables {
        fp_fields,
        fp_flag_registers,
        effect_classes,
        register_index_map,
        pc_classes,
        flag_classes,
        register_name_map,
        register_files,
        float_classes,
        class_sizes,
        reg_kinds,
    }
}
struct InstrOutputs<'a> {
    instruction_defs: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_parsers_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_parser_candidates:
        &'a mut Vec<(String, usize, usize, u32, usize, proc_macro2::TokenStream)>,
    instruction_descs: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_infos: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_info_idents: &'a mut Vec<proc_macro2::Ident>,
    machine_instruction_impls: &'a mut Vec<proc_macro2::TokenStream>,
    instruction_custom_format_impls: &'a mut Vec<proc_macro2::TokenStream>,
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
    implicit_or_update_items: &'a [proc_macro2::TokenStream],
    copy_ports: Option<(String, String)>,
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
        implicit_or_update_items,
        copy_ports,
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
    if !implicit_or_update_items.is_empty() {
        info_fields.push(quote! {
            implicit_or_updates: &[ #(#implicit_or_update_items),* ]
        });
    }
    if let Some((src, dst)) = copy_ports
        && !uncond_pc
        && !cond_pc
        && implicit_items.is_empty()
        && implicit_or_update_items.is_empty()
        && !reads_memory
        && !writes_memory
    {
        let src_lit = proc_macro2::Literal::string(src);
        let dst_lit = proc_macro2::Literal::string(dst);
        info_fields.push(quote! {
            copy: Some(tir::backend::CopyPorts { src: #src_lit, dst: #dst_lit })
        });
    }
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

fn copy_ports(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
    register_files: &HashMap<String, String>,
) -> Option<(String, String)> {
    let assignment = match behavior {
        ast::Expr::Assign(assign) => assign,
        ast::Expr::Block(block) if block.stmts.len() == 1 => {
            let ast::Expr::Assign(assign) = &block.stmts[0] else {
                return None;
            };
            assign
        }
        _ => return None,
    };
    let ast::Expr::Ident(dst) = assignment.dest.as_ref() else {
        return None;
    };
    let ast::Expr::Ident(src) = assignment.value.as_ref() else {
        return None;
    };
    if dst.name == src.name {
        return None;
    }
    let dst_ty = operands
        .iter()
        .find_map(|(name, ty)| (name == &dst.name).then_some(ty));
    let src_ty = operands
        .iter()
        .find_map(|(name, ty)| (name == &src.name).then_some(ty));
    let (Some(Type::Struct(dst_class)), Some(Type::Struct(src_class))) = (dst_ty, src_ty) else {
        return None;
    };
    if dst_class != src_class || register_files.get(dst_class) != Some(dst_class) {
        return None;
    }
    Some((src.name.clone(), dst.name.clone()))
}

fn emit_instruction(
    tables: &TargetTables,
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

    let op_name = opname.as_deref().or(mnemonic.as_deref()).ok_or_else(|| {
        TMDLError::Codegen(format!(
            "Instruction '{}' must define OPNAME or MNEMONIC",
            inst.name
        ))
    })?;
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

    // Unmodeled instructions still print, parse, and encode, but execution traps.
    let uses_todo = behavior_uses_todo(&inst.behavior);

    // Symbolic conversion substitutes bindings. Execution preserves single evaluation.
    let selection_behavior = inline_let_bindings(&inst.behavior);

    let register_reads = if !uses_todo
        && defined_register_operands.len() <= 1
        && !behavior_references_pc(&inst.behavior, &tables.pc_classes)
        && !behavior_has_atomic_ops(&inst.behavior)
        && !behavior_has_dynamic_sized_memory_access(&inst.behavior, &const_size_params)
        && !value_reads_flag_register(&selection_behavior, &tables.flag_classes)
        && !behavior_writes_fixed_register(&inst.behavior, &tables.effect_classes)
    {
        analyze_instruction_register_reads(
            &selection_behavior,
            &ops,
            &defined_register_operands,
            &numeric_params,
            &isa_param_values,
            &tables.register_index_map,
            &tables.fp_fields,
        )
    } else {
        None
    };

    // The registers the behavior reads by path, resolved to attribute names.
    // Each becomes a demand attribute on the emitted op. Reads from a value
    // register become fixed uses for register allocation; configuration reads
    // remain demands materialized by a target pass (e.g. RISC-V `vsetvli`).
    let implicit_reads: Vec<(String, u32)> = {
        let mut reads: Vec<(String, u32)> = register_reads
            .as_ref()
            .map(|s| {
                s.iter()
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

    let read_register_operands = infer_read_register_operands(&inst.behavior, &ops);
    let instr_ctx = InstrEmitCtx {
        inst,
        op_name,
        ops: &ops,
        mnemonic_name,
        builder_ident: &builder_ident,
        ops_map: &ops_map,
        operand_constraints: &operand_constraints,
        defined_register_operands: &defined_register_operands,
        read_register_operands: &read_register_operands,
        implicit_reads: &implicit_reads,
    };
    let copy_ports = copy_ports(&inst.behavior, &ops, &tables.register_files);
    let ports_schema = instruction_values(tables, &instr_ctx);

    // An instruction that writes `PC::pc` transfers control, so it is a
    // terminator: its successors are the blocks its attributes reference
    // (a branch target rewritten to a `Block` by branch selection). This
    // makes the CFG queryable post-isel — the register allocator's liveness
    // needs real successors, and dominance becomes valid on machine IR.
    let (uncond_pc, cond_pc) = pc_writes(&inst.behavior);
    let is_terminator = uncond_pc || cond_pc;
    let (interfaces_list, terminator_impl) = if is_terminator {
        (
            quote! { [tir::backend::MachineInstruction, tir::ResourceEffects, tir::Terminator] },
            quote! {
                impl tir::Terminator for #name_ident {
                    fn successors(&self) -> Vec<tir::BlockId> {
                        tir::backend::branch_successors(self)
                    }
                }
            },
        )
    } else {
        (
            quote! { [tir::backend::MachineInstruction, tir::ResourceEffects] },
            quote! {},
        )
    };

    let implicit_items =
        implicit_register_items(inst, &tables.register_index_map, &tables.pc_classes);
    let implicit_or_update_items = implicit_or_update_items(
        inst,
        &ops,
        &tables.float_classes,
        &tables.register_index_map,
        &tables.register_files,
        &tables.fp_flag_registers,
    );

    // One fact, two readers: the `InstrInfo::effects` derived from the
    // execute body says what the backend is told about the opcode's memory
    // behavior. Every instruction declares a chain to carry it in: one that
    // touches memory is put on it by selection, and a call is handed the
    // chain of the call it finalizes whatever its own opcode says.
    let (reads_memory, writes_memory) = behavior_memory_effects(&inst.behavior);
    let touches_memory = reads_memory || writes_memory;
    let touches_fp_environment = behavior_updates_fp_environment(&inst.behavior);
    let state_schema = quote! { state: "in_out", };

    out.instruction_defs.push(quote! {
        operation! {
            #name_ident {
                name: #op_name_lit,
                dialect: #dialect,
                #ports_schema
                #state_schema
                attributes: A { #attrs_schema },
                interfaces: #interfaces_list,
                format: custom,
            }
        }

        #terminator_impl
    });

    let ir_info_ident = info_ident(&inst.name);
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
                context: &tir::Context,
            ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                tir::backend::parse_machine_op::<Self>(parser, context, &#ir_info_ident)
            }
        }
    });

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
        && let Some(impl_ts) =
            emit_as_sem_expr_impl(rhs, &name_ident, &numeric_params, &isa_param_values)
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
    let desc_ident = match resolve_effective_asm_for_instruction(inst, item_cache)
        .and_then(resolve_asm_templates)
    {
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

        impl tir::ResourceEffects for #name_ident {
            fn resource_effects(&self) -> Vec<tir::ResourceEffect> {
                let effect = |resource, access| tir::ResourceEffect {
                    resource,
                    access,
                    observed: self.0.state_operands().into_iter().filter(|value| {
                        self.0.context.state_resource(self.0.context.get_value(*value).ty())
                            == Some(resource)
                    }).collect(),
                    produced: self.0.state_results().into_iter().filter(|value| {
                        self.0.context.state_resource(self.0.context.get_value(*value).ty())
                            == Some(resource)
                    }).collect(),
                };
                let mut effects = Vec::new();
                if #touches_memory {
                    effects.push(effect(
                        tir::builtin::StateResource::Memory,
                        if #writes_memory {
                            tir::ResourceAccess::Change
                        } else {
                            tir::ResourceAccess::Read
                        },
                    ));
                }
                if #touches_fp_environment {
                    effects.push(effect(
                        tir::builtin::StateResource::FpEnv,
                        tir::ResourceAccess::Change,
                    ));
                }
                effects
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
        out.instruction_decoder_dispatch
            .push((specificity, decode_spec_ident));
    }

    // One record per opcode, spelling only what departs from
    // `InstrInfo::BASE`.
    let mut info_fields = instr_info_fields(
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
            implicit_or_update_items: &implicit_or_update_items,
            copy_ports,
            reads_memory,
            writes_memory,
            desc_ident: &desc_ident,
            encode_ident: &encode_ident,
        },
        sched_tables,
        &inst.name,
    );
    if let Some(cases) = sched_tables.latency_cases.get(&inst.name) {
        let cases = emit_latency_cases(
            cases,
            &numeric_params,
            &tables.register_index_map,
            &behavior_ctx,
        )?;
        info_fields.push(quote! { latency_cases: &[#(#cases),*] });
    }
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

pub(super) fn emit_instructions<'a>(
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
    let mut machine_instruction_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_custom_format_impls: Vec<proc_macro2::TokenStream> = vec![];
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
            machine_instruction_impls: &mut machine_instruction_impls,
            instruction_custom_format_impls: &mut instruction_custom_format_impls,
            as_sem_expr_impls: &mut as_sem_expr_impls,
            instruction_encoder_impls: &mut instruction_encoder_impls,
            instruction_decoder_impls: &mut instruction_decoder_impls,
            instruction_decoder_dispatch: &mut instruction_decoder_dispatch,
            asm_syntax_entries: &mut asm_syntax_entries,
        };
        emit_instruction(
            &tables,
            item_cache,
            sched_tables,
            inst,
            &instruction_options,
            &mut out,
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

    Ok(quote! {
        #(#instruction_defs)*
        #(#instruction_custom_format_impls)*
        #(#machine_instruction_impls)*
        #(#as_sem_expr_impls)*

        #assembly_registry_section

        #infos_section

        #syntax_section

        #encoder_section

        #decode_section
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
