use std::collections::{HashMap, HashSet};
use std::io::Write;

use quote::{format_ident, quote};

use crate::Type;
use crate::ast;
use crate::error::TMDLError;
use crate::utils::{
    get_encoding_arms, parse_literal_value, resolve_effective_asm_for_instruction,
    resolve_isa_param_values, resolve_operand_widths, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};

pub fn generate_rust<'a>(
    dialect: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    let features = emit_features(files)?;
    let register_traits = emit_register_trait_helpers(files)?;
    let registers = emit_register_parsers_and_printers(files)?;
    let register_info = emit_register_info(files)?;
    let machine_models = emit_machine_models(files, item_cache)?;
    let instruction_cost = emit_instruction_cost(files, item_cache)?;
    let instructions = emit_instructions(dialect, files, item_cache)?;

    let final_rust = quote! {
        #features
        #register_traits

        #registers

        #register_info

        #machine_models

        #instruction_cost

        #instructions
    };

    let syntax_tree = syn::parse2(final_rust).unwrap();
    let formatted = prettyplease::unparse(&syntax_tree);

    output.write_all(formatted.as_bytes())?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level emitters
// ---------------------------------------------------------------------------

/// `(isa name, value)` for every ISA defining `param` with a literal integer
/// default, in declaration order.
fn isa_param_definers(files: &[ast::File], param: &str) -> Vec<(String, i64)> {
    let mut definers = vec![];
    for isa in files.iter().flat_map(|f| f.isas()) {
        if let Some((_ty, Some(ast::Expr::Lit(ast::Lit::Int(li))))) = isa.parameters.get(param) {
            definers.push((isa.name.clone(), parse_literal_value(li) as i64));
        }
    }
    definers
}

fn emit_features(files: &[ast::File]) -> Result<proc_macro2::TokenStream, TMDLError> {
    let mut enum_variants = vec![];
    let mut all_variants = vec![];
    let mut name_arms = vec![];
    let mut from_name_arms = vec![];
    let mut requires_arms = vec![];

    for isa in files.iter().flat_map(|f| f.isas()) {
        let ident = format_ident!("{}", &isa.name);
        let name = isa.name.clone();
        let lower_name = isa.name.to_ascii_lowercase();
        enum_variants.push(quote! { #ident });
        all_variants.push(quote! { Feature::#ident });
        name_arms.push(quote! { Self::#ident => #name });
        from_name_arms.push(quote! { #lower_name => Some(Self::#ident) });

        // `requires` as conjunction of any-of groups: every inner slice must
        // intersect the enabled set for the feature to be valid.
        let groups: Vec<Vec<&str>> = match &isa.requires {
            None => vec![],
            Some(ast::IsaRequirement::Single(parent)) => vec![vec![parent.as_str()]],
            Some(ast::IsaRequirement::Any(parents)) => {
                vec![parents.iter().map(String::as_str).collect()]
            }
            Some(ast::IsaRequirement::All(parents)) => {
                parents.iter().map(|p| vec![p.as_str()]).collect()
            }
        };
        let group_ts = groups.iter().map(|group| {
            let members = group.iter().map(|name| {
                let ident = format_ident!("{}", name);
                quote! { Feature::#ident }
            });
            quote! { &[#(#members),*] }
        });
        requires_arms.push(quote! { Self::#ident => &[#(#group_ts),*] });
    }

    // One resolver block per distinct ISA parameter: the value comes from the
    // enabled ISA that defines it (widest wins if several are enabled).
    let mut param_blocks = vec![];
    let mut seen_params: HashSet<&str> = HashSet::new();
    for isa in files.iter().flat_map(|f| f.isas()) {
        for name in isa.parameters.keys() {
            if !seen_params.insert(name) {
                continue;
            }
            let definers = isa_param_definers(files, name);
            if definers.is_empty() {
                continue;
            }
            let name_lit = proc_macro2::Literal::string(name);
            let definer_arms = definers.iter().map(|(isa_name, value)| {
                let feature_ident = format_ident!("{}", isa_name);
                let value_lit = proc_macro2::Literal::i64_unsuffixed(*value);
                quote! {
                    if features.contains(&Feature::#feature_ident) {
                        value = Some(value.map_or(#value_lit, |v: i64| v.max(#value_lit)));
                    }
                }
            });
            param_blocks.push(quote! {
                {
                    let mut value: Option<i64> = None;
                    #(#definer_arms)*
                    if let Some(value) = value {
                        out.push((#name_lit, value));
                    }
                }
            });
        }
    }

    Ok(quote! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Feature {
            #(#enum_variants,)*
            Custom,
        }

        impl Feature {
            /// Every ISA/extension defined in TMDL.
            pub const ALL: &'static [Feature] = &[#(#all_variants),*];

            pub fn name(&self) -> &'static str {
                match self {
                    #(#name_arms,)*
                    Feature::Custom => "custom",
                }
            }

            /// Look a feature up by its TMDL name, case-insensitively.
            pub fn from_name(name: &str) -> Option<Self> {
                match name.to_ascii_lowercase().as_str() {
                    #(#from_name_arms,)*
                    _ => None,
                }
            }

            /// The TMDL `requires` clause: each inner slice is an any-of group
            /// that must intersect the enabled feature set.
            pub fn requires(&self) -> &'static [&'static [Feature]] {
                match self {
                    #(#requires_arms,)*
                    Feature::Custom => &[],
                }
            }
        }

        /// Check every enabled feature's `requires` clause against the set itself.
        pub fn validate_features(features: &[Feature]) -> Result<(), String> {
            for feature in features {
                for group in feature.requires() {
                    if !group.iter().any(|needed| features.contains(needed)) {
                        let names: Vec<&str> = group.iter().map(|f| f.name()).collect();
                        return Err(format!(
                            "feature '{}' requires one of: {}",
                            feature.name(),
                            names.join(", ")
                        ));
                    }
                }
            }
            Ok(())
        }

        /// An item scoped `for [A, B]` is available when any of its features is enabled.
        /// An empty requirement list means the item is unconditionally available.
        fn features_enabled(enabled: &[Feature], required: &[Feature]) -> bool {
            required.is_empty() || required.iter().any(|f| enabled.contains(f))
        }

        /// TMDL ISA parameter values (e.g. RISC-V `XLEN`) resolved from the
        /// enabled feature set. Tools install these into the simulator so
        /// instruction behaviors referencing `self.PARAM` execute with the
        /// selected ISA's value.
        pub fn isa_params(features: &[Feature]) -> Vec<(&'static str, i64)> {
            let mut out: Vec<(&'static str, i64)> = Vec::new();
            #(#param_blocks)*
            out
        }
    })
}

/// `&[Feature::A, Feature::B]` for an item's `for [A, B]` clause.
fn feature_slice(for_isas: &[String]) -> proc_macro2::TokenStream {
    let idents = for_isas.iter().map(|name| {
        let ident = format_ident!("{}", name);
        quote! { Feature::#ident }
    });
    quote! { &[#(#idents),*] }
}

fn emit_instructions<'a>(
    dialect: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let mut instruction_defs = vec![];
    let mut instruction_parsers_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_parser_map_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_printers_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_printer_map_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut isel_rule_emitters: Vec<proc_macro2::TokenStream> = vec![];
    let mut isel_rule_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut isel_definer_emitters: Vec<proc_macro2::TokenStream> = vec![];
    let mut isel_definer_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut machine_instruction_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_custom_format_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut as_sem_expr_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_encoder_impls: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_encoder_map_inits: Vec<proc_macro2::TokenStream> = vec![];
    let mut instruction_patcher_map_inits: Vec<proc_macro2::TokenStream> = vec![];

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

    for inst in files.iter().flat_map(|f| f.instructions()) {
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

        // Build roles from behavior assignments so we don't depend on naming conventions.
        let roles_schema = {
            let mut items = vec![];
            for (name, ty) in &ops {
                if let Type::Struct(_) = ty {
                    let field_ident = format_ident!("{}", name);
                    let role = if defined_register_operands.contains(name) {
                        quote! { Def }
                    } else {
                        quote! { Use }
                    };
                    items.push(quote! { #field_ident: #role });
                }
            }
            quote! { #(#items,)* }
        };

        instruction_defs.push(quote! {
            operation! {
                #name_ident {
                    name: #op_name_lit,
                    dialect: #dialect,
                    attributes: A { #attrs_schema },
                    roles: R { #roles_schema },
                    interfaces: [tir_be_common::MachineInstruction],
                    format: custom,
                }
            }
        });

        let op_display_name = format!("{}.{}", dialect, op_name);
        let op_display_name_lit = proc_macro2::Literal::string(&op_display_name);
        let mut register_attr_print_arms = Vec::new();
        for (op_name, op_ty) in &ops {
            if let Type::Struct(class_name) = op_ty {
                let attr_name_lit = proc_macro2::Literal::string(op_name);
                let print_fn_ident = format_ident!("print_{}", class_name.to_lowercase());
                // Print through the operand's declared class table, keyed by index.
                // The operand position fixes the class, so the stored attribute class
                // is not consulted: a physical register reached through an aliasing
                // class (e.g. `("GPR", 29)` materialized by hand-written prologue code
                // landing in a `GPRsp` operand) still prints the right name.
                register_attr_print_arms.push(quote! {
                    #attr_name_lit => {
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
                });
            }
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
        instruction_custom_format_impls.push(quote! {
            impl #name_ident {
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
        });

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

        // Instructions defining several register operands (e.g. CSR ops writing
        // both `rd` and `csr`) cannot be modeled by a single-value DAG pattern;
        // emitting one for the last assignment would let isel match an
        // unrelated expression, so they get no selection rule.
        if defined_register_operands.len() <= 1
            && let Some(semantics) = analyze_instruction_semantics(
                inst,
                &ops,
                &defined_register_operands,
                &numeric_params,
                &isa_param_values,
                &register_index_map,
            )
        {
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

            let mut emit_attr_steps = Vec::new();
            for (op_name, op_ty) in &ops {
                let op_name_lit = proc_macro2::Literal::string(op_name);
                match op_ty {
                    Type::Struct(class_name) => {
                        let class_lit = proc_macro2::Literal::string(class_name);
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
                                            class: Some(#class_lit.to_string()),
                                        },
                                    ),
                                );
                            });
                        } else if let Some(sym) = semantics.variable_symbols.get(op_name) {
                            let sym_lit = proc_macro2::Literal::u32_unsuffixed(*sym);
                            emit_attr_steps.push(quote! {
                                let src = m.value_binding(#sym_lit).ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                                builder = builder.attr(
                                    #op_name_lit,
                                    tir::attributes::AttributeValue::Register(
                                        tir::attributes::RegisterAttr::Virtual {
                                            id: src.number(),
                                            class: Some(#class_lit.to_string()),
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
                                            class: #class_lit.to_string(),
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
            let (canon_pattern, canon_root, forced_widths) =
                tir::sem_expr::canonicalize_for_selection(
                    &semantics.pattern,
                    semantics.root,
                    &immediate_symbols,
                );
            let mut pattern_widths = tir::sem_expr::infer_widths(&canon_pattern, |_| None);
            for (index, forced) in forced_widths.iter().enumerate() {
                if forced.is_some() {
                    pattern_widths[index] = *forced;
                }
            }
            let (pattern_stmts, _root_var) =
                emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);
            // Cost reflects the canonical pattern's size (one machine instruction).
            let base_cost = {
                use tir::graph::Dag;
                (canon_pattern.len() as u32).max(1)
            };
            let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
            let mnemonic_cost_lit = proc_macro2::Literal::string(mnemonic_name);
            isel_rule_emitters.push(quote! {
                fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem_expr::ExprPostGraph {
                    use tir::graph::MutDag;
                    let mut g = tir::sem_expr::ExprPostGraph::new();
                    #(#pattern_stmts)*
                    g
                }

                fn #emit_fn_ident(
                    context: &tir::Context,
                    req: &tir_be_common::isel::EmitRequest,
                    m: &tir_be_common::isel::RuleMatch,
                ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                    let _ = (req, m);
                    let mut builder = #builder_ident::new(context);
                    #(#emit_attr_steps)*
                    Ok(Box::new(builder.build()))
                }
            });

            // The registers the behavior reads by path (e.g. `VCSR::vl`) are implicit
            // uses: real dependencies not among the encoded operands. Selection
            // introduces each one's definer ahead of this instruction.
            let mut implicit_reads: Vec<(&(String, u32), &u32)> =
                semantics.register_symbols.iter().collect();
            implicit_reads.sort_by_key(|((class, index), _)| (class.clone(), *index));
            let implicit_use_entries: Vec<proc_macro2::TokenStream> = implicit_reads
                .iter()
                .map(|((class, index), sym)| {
                    let class_lit = proc_macro2::Literal::string(class);
                    let index_lit = proc_macro2::Literal::u32_unsuffixed(*index);
                    let sym_lit = proc_macro2::Literal::u32_unsuffixed(**sym);
                    quote! {
                        tir_be_common::isel::ImplicitUse {
                            symbol: #sym_lit,
                            register_class: #class_lit,
                            register_index: #index_lit,
                        }
                    }
                })
                .collect();

            let inst_features = feature_slice(&inst.for_isas);
            isel_rule_inits.push(quote! {
                if features_enabled(features, #inst_features) {
                    rules.push(
                        tir_be_common::isel::Rule::new(
                            #rule_name_lit,
                            #pattern_fn_ident(context),
                            // base_cost is the larger of the canonical pattern size and the
                            // TMDL-modeled instruction cost, so a genuinely expensive
                            // instruction (high `unit` latency) outweighs the structural proxy.
                            (#base_cost_lit).max(instruction_cost(#mnemonic_cost_lit)),
                            #emit_fn_ident,
                        )
                        .with_operand_constraints(vec![#(#operand_constraint_entries),*])
                        .with_implicit_uses(vec![#(#implicit_use_entries),*]),
                    );
                }
            });
        }

        // A pure register definer (e.g. `vsetvli`) gets no matching rule; instead it
        // is registered as the definer of the registers its behavior writes, to be
        // introduced ahead of instructions that read them. Its emitter hardwires the
        // discardable destination(s) to `x0` and takes the written value from the
        // def/use binding (symbol 0).
        let definer_writes =
            definer_writes(inst, &ops, &defined_register_operands, &register_index_map);
        if !definer_writes.is_empty() {
            let definer_emit_fn_ident = format_ident!("emit_definer_{}", inst.name.to_lowercase());

            let mut definer_emit_steps = Vec::new();
            for (op_name, op_ty) in &ops {
                let op_name_lit = proc_macro2::Literal::string(op_name);
                let is_value = definer_writes.iter().any(|w| &w.value_operand == op_name);
                match op_ty {
                    Type::Struct(class_name) => {
                        let class_lit = proc_macro2::Literal::string(class_name);
                        if is_value {
                            definer_emit_steps.push(quote! {
                                let src = m.value_binding(0)
                                    .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                                builder = builder.attr(
                                    #op_name_lit,
                                    tir::attributes::AttributeValue::Register(
                                        tir::attributes::RegisterAttr::Virtual {
                                            id: src.number(),
                                            class: Some(#class_lit.to_string()),
                                        },
                                    ),
                                );
                            });
                        } else {
                            // A discardable destination register, hardwired to x0.
                            definer_emit_steps.push(quote! {
                                builder = builder.attr(
                                    #op_name_lit,
                                    tir::attributes::AttributeValue::Register(
                                        tir::attributes::RegisterAttr::Physical {
                                            class: #class_lit.to_string(),
                                            index: 0,
                                        },
                                    ),
                                );
                            });
                        }
                    }
                    Type::Integer | Type::Bits(_) if is_value => {
                        definer_emit_steps.push(quote! {
                            let v = m.int_binding(0)
                                .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                            builder = builder.attr(
                                #op_name_lit,
                                tir::attributes::AttributeValue::Int(v),
                            );
                        });
                    }
                    _ => {}
                }
            }

            isel_definer_emitters.push(quote! {
                fn #definer_emit_fn_ident(
                    context: &tir::Context,
                    req: &tir_be_common::isel::EmitRequest,
                    m: &tir_be_common::isel::RuleMatch,
                ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                    let _ = (req, m);
                    let mut builder = #builder_ident::new(context);
                    #(#definer_emit_steps)*
                    Ok(Box::new(builder.build()))
                }
            });

            let definer_features = feature_slice(&inst.for_isas);
            for write in &definer_writes {
                let class_lit = proc_macro2::Literal::string(&write.register_class);
                let index_lit = proc_macro2::Literal::u32_unsuffixed(write.register_index);
                let immediate = write.value_is_immediate;
                isel_definer_inits.push(quote! {
                    if features_enabled(features, #definer_features) {
                        definers.push(tir_be_common::isel::RegisterDefiner {
                            register_class: #class_lit,
                            register_index: #index_lit,
                            value_is_immediate: #immediate,
                            value_symbol: 0,
                            emit_fn: #definer_emit_fn_ident,
                        });
                    }
                });
            }
        }

        let encoding_arms = get_encoding_arms(inst, item_cache);
        let encoding_bits = encoding_arms
            .iter()
            .map(|arm| arm.end.unwrap_or(arm.start))
            .max()
            .map(|max_end| max_end + 1)
            .unwrap_or(32);
        let width_bytes = (encoding_bits as u32).div_ceil(8) as u64;
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
            && let Some(impl_ts) = emit_as_sem_expr_impl(rhs, &name_ident, &numeric_params)
        {
            as_sem_expr_impls.push(impl_ts);
        }

        let execute_body = if let Some(branch_val) = branch_value.as_ref() {
            // Conditional control transfer: `synthesize_branch_value` folds the
            // condition into one value (taken target or fall-through) written to PC
            // every cycle.
            match emit_value_eval(
                branch_val,
                &ops,
                &numeric_params,
                &isa_param_values,
                &mnemonic_lit,
                &register_index_map,
            ) {
                Some(eval) => quote! {
                    #eval
                    machine.write_pc(value.to_u64());
                    Ok(())
                },
                None => quote! { Ok(()) },
            }
        } else {
            match emit_behavior_exec(
                &inst.behavior,
                &ops,
                &numeric_params,
                &isa_param_values,
                &mnemonic_lit,
                &register_index_map,
            ) {
                Some(body) => quote! {
                    #body
                    Ok(())
                },
                None => quote! {
                    Err(tir_be_common::SimTrap::InvalidInstruction {
                        op: #mnemonic_lit,
                        reason: "failed to convert behavior to executable expression".to_string(),
                    })
                },
            }
        };

        // Control-flow kind, derived from the behavior's `PC::pc` writes: every
        // path writes PC → unconditional transfer; some paths → conditional
        // branch. The trait default covers sequential instructions.
        let control_flow_method = match pc_writes(&inst.behavior) {
            (true, _) => quote! {
                fn control_flow(&self) -> tir_be_common::ControlFlow {
                    tir_be_common::ControlFlow::Unconditional
                }
            },
            (false, true) => quote! {
                fn control_flow(&self) -> tir_be_common::ControlFlow {
                    tir_be_common::ControlFlow::Conditional
                }
            },
            (false, false) => quote! {},
        };

        machine_instruction_impls.push(quote! {
            impl tir_be_common::MachineInstruction for #name_ident {
                fn mnemonic(&self) -> &'static str {
                    #mnemonic_lit
                }

                fn width_bytes(&self) -> u8 {
                    #width_bytes_lit
                }

                fn execute(
                    &self,
                    machine: &mut dyn tir_be_common::MachineContext,
                ) -> Result<(), tir_be_common::SimTrap> {
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
                        | AsmAction::Operand(_)
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
                                    let class_lit = proc_macro2::Literal::string(class_name);
                                    parse_steps.push(quote! {
                                        let idx = #fn_ident(parser).ok_or(())?;
                                        op_builder = op_builder.attr(
                                            #op_name_lit,
                                            tir::attributes::AttributeValue::Register(
                                                tir::attributes::RegisterAttr::Physical {
                                                    class: #class_lit.to_string(),
                                                    index: idx,
                                                },
                                            ),
                                        );
                                    });
                                }
                                Type::Integer | Type::Bits(_) => {
                                    parse_steps.push(quote! {
                                        let val = if let Some(tok) = parser.peek() {
                                            match tok {
                                                tir_be_common::Token::DecNumber(n) => {
                                                    let parsed = (*n).parse::<i64>().map_err(|_| ())?;
                                                    let _ = parser.bump();
                                                    tir::attributes::AttributeValue::Int(parsed)
                                                }
                                                tir_be_common::Token::HexNumber(h) => {
                                                    let s = *h;
                                                    let neg = s.starts_with('-');
                                                    let s = if neg { &s[1..] } else { s };
                                                    let s = if s.starts_with("0x") || s.starts_with("0X") { &s[2..] } else { s };
                                                    let v = i128::from_str_radix(s, 16).map_err(|_| ())?;
                                                    let v = if neg { -v } else { v };
                                                    let v_i64: i64 = v.try_into().map_err(|_| ())?;
                                                    let _ = parser.bump();
                                                    tir::attributes::AttributeValue::Int(v_i64)
                                                }
                                                // A bare identifier in an immediate position is a
                                                // symbol reference, resolved at object emission.
                                                tir_be_common::Token::Ident(name) => {
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
                                Some(tir_be_common::Token::LParen) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::RParen => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir_be_common::Token::RParen) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::LBracket => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir_be_common::Token::LBracket) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::RBracket => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir_be_common::Token::RBracket) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                    AsmAction::Star => {
                        parse_steps.push(quote! {
                            match parser.bump() {
                                Some(tir_be_common::Token::Star) => {}
                                _ => return Err(()),
                            }
                        });
                    }
                }
            }

            let print_parts = compile_asm_printer_template(&template, mnemonic_name);
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
            instruction_printers_impls.push(quote! {
                fn #print_fn_ident(#op_param: &tir::OpInstance) -> Option<String> {
                    #attrs_binding
                    let mut out = String::new();
                    #(#print_steps)*
                    Some(out)
                }
            });

            let printer_op_name_lit = proc_macro2::Literal::string(op_name);
            instruction_printer_map_inits.push(quote! {
                let f: tir_be_common::AsmInstructionPrinter = #print_fn_ident;
                map.insert(#printer_op_name_lit.to_string(), f);
            });

            let parse_fn_ident = format_ident!("parse_{}_inst", &inst.name.to_lowercase());
            let (parser_param, builder_binding) = if parses_operands {
                (quote! { parser }, quote! { let mut op_builder })
            } else {
                (quote! { _parser }, quote! { let op_builder })
            };
            instruction_parsers_impls.push(quote! {
                fn #parse_fn_ident<'src>(
                    context: &tir::Context,
                    builder: &mut tir::IRBuilder,
                    #parser_param: &mut tir::parse::tokens::Parser<'src, tir_be_common::Token<'src>>,
                ) -> Result<(), ()> {
                    #builder_binding = #builder_ident::new(context);
                    #(#parse_steps)*
                    let op = op_builder.build();
                    builder.insert(op);
                    Ok(())
                }
            });

            if let Some(mn) = mnemonic.as_deref().or(Some(op_name)) {
                let mn_lit = proc_macro2::Literal::string(mn);
                let inst_features = feature_slice(&inst.for_isas);
                instruction_parser_map_inits.push(quote! {
                    if features_enabled(features, #inst_features) {
                        let f: tir_be_common::AsmInstructionParser = #parse_fn_ident;
                        map.entry(#mn_lit.to_string()).or_default().push(f);
                    } else {
                        disabled.insert(#mn_lit.to_string());
                    }
                });
            }
        }

        if let Some((encoder, patcher)) = emit_instruction_encoder(
            inst,
            &encoding_arms,
            &ops_map,
            &resolved_params,
            width_bytes,
        )? {
            let encode_fn_ident = format_ident!("encode_{}_inst", inst.name.to_lowercase());
            instruction_encoder_impls.push(encoder);
            instruction_encoder_map_inits.push(quote! {
                let f: tir_be_common::binary::InstructionEncoder = #encode_fn_ident;
                map.insert(#op_name_lit.to_string(), f);
            });
            if let Some(patcher) = patcher {
                let patch_fn_ident = format_ident!("patch_{}_inst", inst.name.to_lowercase());
                instruction_encoder_impls.push(patcher);
                instruction_patcher_map_inits.push(quote! {
                    let f: tir_be_common::binary::InstructionPatcher = #patch_fn_ident;
                    map.insert(#op_name_lit.to_string(), f);
                });
            }
        }
    }

    Ok(quote! {
        #(#instruction_defs)*
        #(#instruction_custom_format_impls)*
        #(#machine_instruction_impls)*
        #(#as_sem_expr_impls)*

        /// Mnemonic-keyed parsers for the instructions available under `features`,
        /// plus the mnemonics that exist in TMDL but are disabled by the feature
        /// set (so the assembler can reject them instead of skipping them).
        fn get_instruction_parsers(
            features: &[Feature],
        ) -> (
            std::collections::HashMap<String, Vec<tir_be_common::AsmInstructionParser>>,
            std::collections::HashSet<String>,
        ) {
            let mut map: std::collections::HashMap<String, Vec<tir_be_common::AsmInstructionParser>> = std::collections::HashMap::new();
            let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
            #(#instruction_parsers_impls)*
            #(#instruction_parser_map_inits)*

            // A mnemonic with any enabled form stays available.
            disabled.retain(|mnemonic| !map.contains_key(mnemonic));
            (map, disabled)
        }

        fn get_instruction_printers() -> std::collections::HashMap<String, tir_be_common::AsmInstructionPrinter> {
            let mut map: std::collections::HashMap<String, tir_be_common::AsmInstructionPrinter> = std::collections::HashMap::new();
            #(#instruction_printers_impls)*
            #(#instruction_printer_map_inits)*

            map
        }

        #(#instruction_encoder_impls)*

        // Consumed by object-file emission.
        fn get_instruction_encoders() -> std::collections::HashMap<String, tir_be_common::binary::InstructionEncoder> {
            let mut map: std::collections::HashMap<String, tir_be_common::binary::InstructionEncoder> = std::collections::HashMap::new();
            #(#instruction_encoder_map_inits)*

            map
        }

        fn get_instruction_patchers() -> std::collections::HashMap<String, tir_be_common::binary::InstructionPatcher> {
            let mut map: std::collections::HashMap<String, tir_be_common::binary::InstructionPatcher> = std::collections::HashMap::new();
            #(#instruction_patcher_map_inits)*

            map
        }

        #(#isel_rule_emitters)*

        /// Instruction-selection rules for the instructions available under `features`.
        pub fn get_isel_rules(context: &tir::Context, features: &[Feature]) -> Vec<tir_be_common::isel::Rule> {
            let _ = (&context, &features);
            let mut rules = Vec::new();
            #(#isel_rule_inits)*
            rules
        }

        #(#isel_definer_emitters)*

        /// Instructions that define a register implicitly (e.g. `vsetvli` defining
        /// `VCSR::vl`), introduced ahead of ops that read those registers.
        pub fn get_register_definers(context: &tir::Context, features: &[Feature]) -> Vec<tir_be_common::isel::RegisterDefiner> {
            let _ = (&context, &features);
            let mut definers = Vec::new();
            #(#isel_definer_inits)*
            definers
        }
    })
}

fn emit_register_parsers_and_printers(
    files: &[ast::File],
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let mut fns = Vec::new();
    let mut dispatch_arms = Vec::new();

    for rc in files.iter().flat_map(|f| f.register_classes()) {
        let rc_name = &rc.name;
        let fn_name = format_ident!("parse_{}", rc_name.to_lowercase());
        let print_fn_name = format_ident!("print_{}", rc_name.to_lowercase());
        let name_lit = proc_macro2::Literal::string(rc_name);
        dispatch_arms.push(quote! { #name_lit => #print_fn_name(idx, prefer_abi), });
        let tables = rc.register_name_tables();

        let match_arms = tables
            .parse_names
            .iter()
            .map(|(name, idx)| {
                let idx_lit = proc_macro2::Literal::u16_unsuffixed(*idx);
                quote! { #name => Some(#idx_lit), }
            })
            .collect::<Vec<_>>();
        let abi_match_arms = tables
            .abi_names
            .iter()
            .map(|(idx, name)| {
                let idx_lit = proc_macro2::Literal::u16_unsuffixed(*idx);
                quote! { #idx_lit => Some(#name.to_string()), }
            })
            .collect::<Vec<_>>();
        let isa_match_arms = tables
            .isa_names
            .iter()
            .map(|(idx, name)| {
                let idx_lit = proc_macro2::Literal::u16_unsuffixed(*idx);
                quote! { #idx_lit => Some(#name.to_string()), }
            })
            .collect::<Vec<_>>();
        let parse_body = if match_arms.is_empty() {
            quote! {
                let _ = parser;
                None
            }
        } else {
            quote! {
                if let Some(name) = parser.parse_ident() {
                    match name {
                        #(#match_arms)*
                        _ => None,
                    }
                } else {
                    None
                }
            }
        };
        let abi_lookup = if abi_match_arms.is_empty() {
            quote! { None }
        } else {
            quote! {
                match idx {
                    #(#abi_match_arms)*
                    _ => None,
                }
            }
        };
        let isa_lookup = if isa_match_arms.is_empty() {
            quote! { None }
        } else {
            quote! {
                match idx {
                    #(#isa_match_arms)*
                    _ => None,
                }
            }
        };

        let print_body = match (abi_match_arms.is_empty(), isa_match_arms.is_empty()) {
            (true, true) => quote! {
                let _ = (idx, prefer_abi);
                None
            },
            (true, false) => quote! {
                let _ = prefer_abi;
                #isa_lookup
            },
            (false, true) => quote! {
                if prefer_abi {
                    #abi_lookup
                } else {
                    None
                }
            },
            (false, false) => quote! {
                let abi_name = if prefer_abi {
                    #abi_lookup
                } else {
                    None
                };
                abi_name.or(#isa_lookup)
            },
        };

        fns.push(quote! {
            pub fn #fn_name<'src>(parser: &mut tir::parse::tokens::Parser<'src, tir_be_common::Token<'src>>) -> Option<u16> {
                #parse_body
            }
            pub fn #print_fn_name(idx: u16, prefer_abi: bool) -> Option<String> {
                #print_body
            }
        });
    }

    // A class-name dispatcher so callers that only have the runtime `(class, index)`
    // of a register attribute can recover its ISA/ABI name (e.g. printing `x1`/`ra`
    // instead of the raw `GPR[1]`).
    fns.push(quote! {
        pub fn register_name(class: &str, idx: u16, prefer_abi: bool) -> Option<String> {
            match class {
                #(#dispatch_arms)*
                _ => None,
            }
        }
    });

    Ok(quote! { #(#fns)* })
}

/// Emit a `register_info()` constructor returning the target-independent
/// [`tir_be_common::regalloc::RegisterInfo`] the allocator consumes: per class, the
/// allocatable order plus the caller/callee-saved, argument, return-value, and
/// reserved index sets, all derived from each register's TMDL traits.
fn emit_register_info(files: &[ast::File]) -> Result<proc_macro2::TokenStream, TMDLError> {
    let slice = |indices: &[u16]| {
        let lits = indices
            .iter()
            .map(|i| proc_macro2::Literal::u16_unsuffixed(*i));
        quote! { &[#(#lits),*] }
    };

    let classes: HashMap<String, &ast::RegisterClass> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .map(|rc| (rc.name.clone(), rc))
        .collect();

    let mut class_entries = Vec::new();
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        let name_lit = proc_macro2::Literal::string(&rc.name);
        let file_lit = proc_macro2::Literal::string(rc.register_file(&classes));
        let meta = rc.allocation_metadata();
        let allocation_order = slice(&meta.allocation_order);
        let caller_saved = slice(&meta.caller_saved);
        let callee_saved = slice(&meta.callee_saved);
        let arguments = slice(&meta.arguments);
        let return_values = slice(&meta.return_values);
        let reserved = slice(&meta.reserved);
        class_entries.push(quote! {
            tir_be_common::regalloc::RegClassInfo {
                name: #name_lit,
                file: #file_lit,
                allocation_order: #allocation_order,
                caller_saved: #caller_saved,
                callee_saved: #callee_saved,
                arguments: #arguments,
                return_values: #return_values,
                reserved: #reserved,
            }
        });
    }

    // Architectural register widths: a class's `WIDTH` param is either a literal
    // or an ISA parameter reference (`self.XLEN`), resolved at runtime from the
    // enabled feature set so e.g. rv32 registers are 32 bits wide.
    let mut width_entries = Vec::new();
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        let name_lit = proc_macro2::Literal::string(&rc.name);
        let width_ts = match rc.parameters.get("WIDTH") {
            Some((_ty, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
                let lit =
                    proc_macro2::Literal::u32_unsuffixed(parse_literal_value(li).min(64) as u32);
                quote! { #lit }
            }
            Some((_ty, Some(ast::Expr::Field(field)))) if matches!(&*field.base, ast::Expr::Ident(id) if id.name == "self") =>
            {
                let param = field.member.as_str();
                let fallback = isa_param_definers(files, param)
                    .iter()
                    .map(|(_, v)| *v)
                    .max()
                    .unwrap_or(64);
                let param_lit = proc_macro2::Literal::string(param);
                let fallback_lit = proc_macro2::Literal::i64_unsuffixed(fallback);
                quote! {
                    params
                        .iter()
                        .find(|(name, _)| *name == #param_lit)
                        .map(|(_, value)| *value)
                        .unwrap_or(#fallback_lit) as u32
                }
            }
            _ => continue,
        };
        width_entries.push(quote! { (#name_lit, #width_ts) });
    }

    Ok(quote! {
        pub fn register_info() -> tir_be_common::regalloc::RegisterInfo {
            tir_be_common::regalloc::RegisterInfo {
                classes: &[#(#class_entries),*],
            }
        }

        /// Architectural width in bits of each register class under `features`.
        pub fn register_widths(features: &[Feature]) -> Vec<(&'static str, u32)> {
            let params = isa_params(features);
            let _ = &params;
            vec![#(#width_entries),*]
        }
    })
}

/// Emit one `fn <machine>_model() -> tir_be_common::sched::MachineModel` per TMDL
/// `machine` block. Each instruction's `unit` membership is resolved against the
/// machine's `bind`s at compile time into a concrete per-mnemonic scheduling class,
/// so the runtime lookup is a binary search. This is the static half of the
/// performance model: the same table feeds the compiler cost model and the
/// cycle-approximate simulator, so they cannot disagree.
fn emit_machine_models<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let unit_defaults = collect_unit_defaults(files);
    let scheduled = collect_scheduled(files, item_cache);

    let mut model_fns = Vec::new();
    let mut lookup_arms = Vec::new();
    let mut machine_names = Vec::new();
    for machine in files.iter().flat_map(|f| f.machines()) {
        let binds: HashMap<&str, &ast::UnitBind> =
            machine.binds.iter().map(|b| (b.unit.as_str(), b)).collect();
        let overrides: HashMap<&str, &ast::MachineOverride> = machine
            .overrides
            .iter()
            .map(|o| (o.instruction.as_str(), o))
            .collect();

        // Resolve each scheduled instruction to a concrete class on this machine. A
        // per-instruction `override` supersedes the `unit`-based resolution.
        let mut entries: Vec<(String, ResolvedClass)> = scheduled
            .iter()
            .map(|(name, mnemonic, units)| {
                let resolved = match overrides.get(name.as_str()) {
                    Some(ov) => resolve_spec(
                        ov.reads.as_deref(),
                        ov.writes.as_deref(),
                        ov.latency,
                        ov.throughput,
                        &ov.uses,
                        &machine.pipeline,
                    ),
                    None => resolve_sched_class(units, &binds, &unit_defaults, &machine.pipeline),
                };
                (mnemonic.clone(), resolved)
            })
            .collect();
        // Sorted + deduplicated by mnemonic for the runtime binary search.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);

        let sched_lits = entries.iter().map(|(mnem, c)| {
            let mnem_lit = proc_macro2::Literal::string(mnem);
            let lat_lit = proc_macro2::Literal::u16_unsuffixed(c.latency);
            let read_lit = proc_macro2::Literal::u16_unsuffixed(c.read_cycle);
            let rthr_lit = proc_macro2::Literal::u16_unsuffixed(c.rthroughput);
            let res_lits = c.resources.iter().map(|r| proc_macro2::Literal::string(r));
            quote! {
                (#mnem_lit, tir_be_common::sched::InstrSchedClass {
                    latency: #lat_lit,
                    read_cycle: #read_lit,
                    rthroughput: #rthr_lit,
                    resources: &[#(#res_lits),*],
                })
            }
        });

        let pipeline_lits = machine.pipeline.iter().map(|p| {
            let name_lit = proc_macro2::Literal::string(&p.name);
            let prot_ts = protection_ts(p.protection);
            quote! {
                tir_be_common::sched::PipelinePhase { name: #name_lit, protection: #prot_ts }
            }
        });

        let forward_lits = machine.forwards.iter().map(|f| {
            let from_lit = proc_macro2::Literal::string(&f.from);
            let to_lit = proc_macro2::Literal::string(&f.to);
            let lat_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(f.latency));
            quote! {
                tir_be_common::sched::Forward { from: #from_lit, to: #to_lit, latency: #lat_lit }
            }
        });

        let resource_lits = machine.resources.iter().map(|r| {
            let name_lit = proc_macro2::Literal::string(&r.name);
            let units_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(r.units));
            quote! { tir_be_common::sched::ProcUnit { name: #name_lit, units: #units_lit } }
        });

        let buffer_lits = machine.buffers.iter().map(|(name, size)| {
            let name_lit = proc_macro2::Literal::string(name);
            let size_lit = proc_macro2::Literal::u32_unsuffixed(clamp_u32(*size));
            quote! { tir_be_common::sched::BufferSize { name: #name_lit, size: #size_lit } }
        });

        let reg_file_lits = machine.reg_files.iter().map(|(name, count)| {
            let name_lit = proc_macro2::Literal::string(name);
            let count_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(*count));
            quote! { tir_be_common::sched::RegFile { name: #name_lit, count: #count_lit } }
        });

        let name_lit = proc_macro2::Literal::string(&machine.name);
        let issue_width_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(
            machine.issue_width.unwrap_or(1).max(1),
        ));
        let fn_ident = format_ident!("{}_model", to_snake_case(&machine.name));

        model_fns.push(quote! {
            pub fn #fn_ident() -> tir_be_common::sched::MachineModel {
                tir_be_common::sched::MachineModel {
                    name: #name_lit,
                    issue_width: #issue_width_lit,
                    resources: &[#(#resource_lits),*],
                    buffers: &[#(#buffer_lits),*],
                    pipeline: &[#(#pipeline_lits),*],
                    forwards: &[#(#forward_lits),*],
                    reg_files: &[#(#reg_file_lits),*],
                    sched: &[#(#sched_lits),*],
                }
            }
        });

        // Select by the machine name, and by its alias when one is declared, so
        // the tool-facing name lives in TMDL next to the machine.
        let mut keys = vec![machine.name.clone()];
        if let Some(alias) = &machine.alias {
            keys.push(alias.clone());
        }
        let machine_features = feature_slice(&machine.for_isas);
        let key_lits = keys.iter().map(|k| proc_macro2::Literal::string(k));
        let tool_name = proc_macro2::Literal::string(keys.last().unwrap());
        machine_names.push(quote! {
            if features_enabled(features, #machine_features) {
                names.push(#tool_name);
            }
        });
        lookup_arms.push(quote! {
            #(#key_lits)|* => features_enabled(features, #machine_features).then(#fn_ident)
        });
    }

    Ok(quote! {
        #(#model_fns)*

        /// Resolve a machine by its TMDL name or alias. `None` when the name is
        /// unknown or the machine's `for [...]` clause is disjoint from `features`.
        pub fn machine_model(name: &str, features: &[Feature]) -> Option<tir_be_common::sched::MachineModel> {
            match name {
                #(#lookup_arms,)*
                _ => None,
            }
        }

        /// Tool-facing names (alias preferred) of the machines compatible with `features`.
        pub fn machines(features: &[Feature]) -> Vec<&'static str> {
            let mut names = Vec::new();
            #(#machine_names)*
            names
        }
    })
}

/// Resource-agnostic `unit` defaults, keyed by name. Used both when a machine
/// does not bind a unit and to drive the machine-independent [`instruction_cost`].
fn collect_unit_defaults(files: &[ast::File]) -> HashMap<&str, &ast::SchedClassDecl> {
    files
        .iter()
        .flat_map(|f| f.count())
        .map(|u| (u.name.as_str(), u))
        .collect()
}

/// `(instruction name, mnemonic, units)` for every instruction carrying a
/// `schedule` block. The name keys per-instruction machine `override`s; the
/// mnemonic keys the runtime scheduling table.
fn collect_scheduled<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, String, Vec<String>)> {
    let mut scheduled = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        let Some(schedule) =
            crate::utils::resolve_effective_schedule_for_instruction(inst, item_cache)
        else {
            continue;
        };
        let resolved_params = resolve_params_for_instruction(inst, item_cache);
        let mnemonic = resolved_params
            .get("MNEMONIC")
            .and_then(|(_, v)| v.as_ref())
            .and_then(resolve_string)
            .or_else(|| {
                resolved_params
                    .get("OPNAME")
                    .and_then(|(_, v)| v.as_ref())
                    .and_then(resolve_string)
            });
        let Some(mnemonic) = mnemonic else {
            continue;
        };
        scheduled.push((inst.name.clone(), mnemonic, schedule.classes.clone()));
    }
    scheduled
}

/// Emit a machine-independent `instruction_cost(mnemonic) -> u32` derived from
/// each instruction's `unit` defaults (latency, falling back to 1). This is the
/// single source of truth the compiler cost model consults — most importantly the
/// instruction-selection `base_cost` (see `emit_instructions`) — so selection and
/// the simulator agree on relative instruction cost.
fn emit_instruction_cost<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let unit_defaults = collect_unit_defaults(files);
    let scheduled = collect_scheduled(files, item_cache);
    let empty_binds: HashMap<&str, &ast::UnitBind> = HashMap::new();

    let mut arms = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (_name, mnemonic, units) in &scheduled {
        if !seen.insert(mnemonic.clone()) {
            continue;
        }
        // Machine-independent: no machine binds and no pipeline, so this resolves
        // through the unit defaults to a scalar latency.
        let resolved = resolve_sched_class(units, &empty_binds, &unit_defaults, &[]);
        let m_lit = proc_macro2::Literal::string(mnemonic);
        let c_lit = proc_macro2::Literal::u32_unsuffixed(u32::from(resolved.latency));
        arms.push(quote! { #m_lit => #c_lit, });
    }

    let cost_body = if arms.is_empty() {
        quote! { 1 }
    } else {
        quote! {
            match mnemonic {
                #(#arms)*
                _ => 1,
            }
        }
    };

    Ok(quote! {
        /// Machine-independent instruction cost (a latency proxy) derived from TMDL
        /// `unit` defaults. The instruction-selection cost model consults this so it
        /// shares one source of truth with the simulator's per-machine model.
        pub fn instruction_cost(mnemonic: &str) -> u32 {
            let _ = mnemonic;
            #cost_body
        }
    })
}

/// The cycle offset (index) of a named pipeline phase within a machine's pipeline.
fn phase_cycle(pipeline: &[ast::PipelinePhase], name: &str) -> Option<u16> {
    pipeline
        .iter()
        .position(|p| p.name == name)
        .map(|i| i as u16)
}

/// The resolved scheduling cost of an instruction on one machine.
struct ResolvedClass {
    latency: u16,
    read_cycle: u16,
    rthroughput: u16,
    resources: Vec<String>,
}

/// Resolve one explicit timing spec (a `bind` or an `override`) to a class. Timing
/// is phase-based when it names `reads`/`writes` phases (cycles from the machine's
/// pipeline), else scalar (`latency = N` ≡ read at cycle 0, write at cycle N).
fn resolve_spec(
    reads: Option<&str>,
    writes: Option<&str>,
    latency: Option<i64>,
    throughput: Option<i64>,
    uses: &[String],
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let (rc, wc) = if reads.is_some() || writes.is_some() {
        let rc = reads.and_then(|p| phase_cycle(pipeline, p)).unwrap_or(0);
        let wc = writes
            .and_then(|p| phase_cycle(pipeline, p))
            .unwrap_or_else(|| rc.saturating_add(clamp_u16(latency.unwrap_or(1))));
        (rc, wc.max(rc))
    } else {
        (0, clamp_u16(latency.unwrap_or(1)))
    };
    ResolvedClass {
        latency: wc.saturating_sub(rc).max(1),
        read_cycle: rc,
        rthroughput: clamp_u16(throughput.unwrap_or(1)).max(1),
        resources: uses.to_vec(),
    }
}

/// Resolve an instruction's `unit` membership to a concrete class on one machine.
/// Precedence per unit: the machine's `bind` → the unit's resource-agnostic default
/// → the built-in `(latency 1, read 0)`. Across multiple units the result aggregates
/// conservatively: the highest-latency unit sets the latency/read-cycle, throughput
/// is the max, resources are unioned.
fn resolve_sched_class(
    units: &[String],
    binds: &HashMap<&str, &ast::UnitBind>,
    unit_defaults: &HashMap<&str, &ast::SchedClassDecl>,
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let mut latency: u16 = 0;
    let mut read_cycle: u16 = 0;
    let mut rthroughput: u16 = 0;
    let mut resources: Vec<String> = Vec::new();
    let mut chosen = false;

    for unit in units {
        let class = if let Some(b) = binds.get(unit.as_str()) {
            resolve_spec(
                b.reads.as_deref(),
                b.writes.as_deref(),
                b.latency,
                b.throughput,
                &b.uses,
                pipeline,
            )
        } else if let Some(d) = unit_defaults.get(unit.as_str()) {
            ResolvedClass {
                latency: clamp_u16(d.default_latency.unwrap_or(1)).max(1),
                read_cycle: 0,
                rthroughput: clamp_u16(d.default_throughput.unwrap_or(1)).max(1),
                resources: Vec::new(),
            }
        } else {
            ResolvedClass {
                latency: 1,
                read_cycle: 0,
                rthroughput: 1,
                resources: Vec::new(),
            }
        };

        for r in &class.resources {
            if !resources.iter().any(|e| e == r) {
                resources.push(r.clone());
            }
        }
        if !chosen || class.latency > latency {
            latency = class.latency;
            read_cycle = class.read_cycle;
            chosen = true;
        }
        rthroughput = rthroughput.max(class.rthroughput);
    }

    ResolvedClass {
        latency: latency.max(1),
        read_cycle,
        rthroughput: rthroughput.max(1),
        resources,
    }
}

/// The `tir_be_common::sched::Protection` variant for an AST protection mode.
fn protection_ts(p: ast::Protection) -> proc_macro2::TokenStream {
    match p {
        ast::Protection::Protected => quote! { tir_be_common::sched::Protection::Protected },
        ast::Protection::Unprotected => quote! { tir_be_common::sched::Protection::Unprotected },
        ast::Protection::Hard => quote! { tir_be_common::sched::Protection::Hard },
    }
}

fn clamp_u16(v: i64) -> u16 {
    v.clamp(0, u16::MAX as i64) as u16
}

fn clamp_u32(v: i64) -> u32 {
    v.clamp(0, u32::MAX as i64) as u32
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i != 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn emit_register_trait_helpers(files: &[ast::File]) -> Result<proc_macro2::TokenStream, TMDLError> {
    let mut hardwired_patterns = Vec::new();

    for rc in files.iter().flat_map(|f| f.register_classes()) {
        let class_lit = proc_macro2::Literal::string(&rc.name);
        if let Some(idx) = rc.hardwired_zero_register_index() {
            let idx_lit = proc_macro2::Literal::u16_unsuffixed(idx);
            hardwired_patterns.push(quote! { (#class_lit, #idx_lit) });
        }
    }
    let hardwired_body = if hardwired_patterns.is_empty() {
        quote! {
            let _ = (class, index);
            false
        }
    } else {
        quote! { matches!((class, index), #(#hardwired_patterns)|*) }
    };

    Ok(quote! {
        pub fn register_has_trait_hardwired_zero(class: &str, index: u16) -> bool {
            #hardwired_body
        }
    })
}

// ---------------------------------------------------------------------------
// Instruction analysis helpers
// ---------------------------------------------------------------------------

struct InstructionSemantics {
    pattern: tir::sem_expr::ExprPostGraph,
    root: tir::graph::NodeId,
    variable_symbols: HashMap<String, u32>,
    fixed_register_by_class: HashMap<String, Option<u16>>,
    /// `(register class, index) -> pattern symbol` for every register the behavior
    /// reads by path (e.g. `VCSR::vl`). These are implicit reads — registers not
    /// among the encoded operands — and become the rule's `implicit_uses`.
    register_symbols: HashMap<(String, u32), u32>,
}

fn analyze_instruction_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<InstructionSemantics> {
    let rhs = resolve_behavior_rhs(inst, operands, defined_register_operands)?;
    let mut pattern = tir::sem_expr::ExprPostGraph::new();
    let lowering = rhs.lower_to_sema_with_isa(
        &mut pattern,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    let fixed_register_by_class = split_fixed_registers(&lowering.register_symbols);

    Some(InstructionSemantics {
        pattern,
        root: lowering.root,
        variable_symbols: lowering.variable_symbols,
        fixed_register_by_class,
        register_symbols: lowering.register_symbols,
    })
}

fn split_fixed_registers(symbols: &HashMap<(String, u32), u32>) -> HashMap<String, Option<u16>> {
    let mut fixed_register_by_class: HashMap<String, Option<u16>> = HashMap::new();

    for (class, number) in symbols.keys() {
        let entry = fixed_register_by_class.entry(class.clone()).or_insert(None);
        if let Ok(number_u16) = u16::try_from(*number) {
            match entry {
                None => *entry = Some(number_u16),
                Some(existing) if *existing == number_u16 => {}
                Some(_) => *entry = None,
            }
        } else {
            *entry = None;
        }
    }

    fixed_register_by_class
}

fn register_operand_names(operands: &[(String, Type)]) -> HashSet<&str> {
    operands
        .iter()
        .filter_map(|(name, ty)| match ty {
            Type::Struct(_) => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

fn assignment_dest_name(dest: &ast::Expr) -> Option<String> {
    match dest {
        ast::Expr::Ident(id) => Some(id.name.clone()),
        ast::Expr::Path(path) if path.remainder.len() == 1 => Some(path.remainder[0].clone()),
        _ => None,
    }
}

/// `(class, register-name)` when an assignment destination is a register path
/// (e.g. `VCSR::vl`), or `None` for a plain identifier (an encoded operand).
fn assignment_dest_register_path(dest: &ast::Expr) -> Option<(String, String)> {
    match dest {
        ast::Expr::Path(path) if path.remainder.len() == 1 => {
            Some((path.base.clone(), path.remainder[0].clone()))
        }
        _ => None,
    }
}

/// The operand names referenced anywhere in `expr`, in first-seen order. Used to
/// find which operand feeds a register a definer instruction writes.
fn referenced_operands(expr: &ast::Expr, operands: &HashSet<&str>) -> Vec<String> {
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
        | ast::Expr::Invalid => {}
        ast::Expr::Assign(a) => {
            collect_referenced_idents(&a.dest, operands, out);
            collect_referenced_idents(&a.value, operands, out);
        }
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
        ast::Expr::For(f) => {
            collect_referenced_idents(&f.start, operands, out);
            collect_referenced_idents(&f.end, operands, out);
            collect_referenced_idents(&f.body, operands, out);
        }
        ast::Expr::IndexAccess(i) => collect_referenced_idents(&i.base, operands, out),
        ast::Expr::Slice(s) => collect_referenced_idents(&s.base, operands, out),
        ast::Expr::Try(t) => {
            collect_referenced_idents(&t.body, operands, out);
            for h in &t.handlers {
                collect_referenced_idents(&h.body, operands, out);
            }
        }
        ast::Expr::Lambda(l) => collect_referenced_idents(&l.body, operands, out),
    }
}

fn collect_behavior_assignments<'a>(expr: &'a ast::Expr, out: &mut Vec<(String, &'a ast::Expr)>) {
    match expr {
        ast::Expr::Assign(a) => {
            if let Some(dst) = assignment_dest_name(&a.dest) {
                out.push((dst, a.value.as_ref()));
            }
        }
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_behavior_assignments(stmt, out);
            }
        }
        ast::Expr::If(i) => {
            collect_behavior_assignments(i.then.as_ref(), out);
            if let Some(else_expr) = &i.else_ {
                collect_behavior_assignments(else_expr.as_ref(), out);
            }
        }
        // Only the no-trap path defines values; handler writes are trap state.
        ast::Expr::Try(t) => collect_behavior_assignments(&t.body, out),
        ast::Expr::For(f) => collect_behavior_assignments(&f.body, out),
        _ => {}
    }
}

/// Like [`collect_behavior_assignments`] but keeps the destination expression, so
/// a register-path write (`VCSR::vl = …`) can be resolved to its `(class, name)`.
fn collect_behavior_assignment_exprs<'a>(
    expr: &'a ast::Expr,
    out: &mut Vec<(&'a ast::Expr, &'a ast::Expr)>,
) {
    match expr {
        ast::Expr::Assign(a) => out.push((a.dest.as_ref(), a.value.as_ref())),
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_behavior_assignment_exprs(stmt, out);
            }
        }
        ast::Expr::If(i) => {
            collect_behavior_assignment_exprs(i.then.as_ref(), out);
            if let Some(else_expr) = &i.else_ {
                collect_behavior_assignment_exprs(else_expr.as_ref(), out);
            }
        }
        ast::Expr::Try(t) => collect_behavior_assignment_exprs(&t.body, out),
        ast::Expr::For(f) => collect_behavior_assignment_exprs(&f.body, out),
        _ => {}
    }
}

/// A register a definer instruction writes implicitly: `(class, index)` and the
/// operand feeding it, with whether that operand is an immediate. Derived from an
/// instruction whose behavior assigns fixed registers and no encoded result.
struct DefinerWrite {
    register_class: String,
    register_index: u32,
    value_operand: String,
    value_is_immediate: bool,
}

/// The fixed-register writes that make `inst` a register definer, or empty if it
/// is not one (it assigns an encoded result, or writes no fixed register).
fn definer_writes(
    inst: &ast::Instruction,
    ops: &[(String, Type)],
    defined_register_operands: &[String],
    register_index_map: &HashMap<(String, String), u32>,
) -> Vec<DefinerWrite> {
    // A pure definer assigns no encoded result operand; an instruction that also
    // produces a normal result (e.g. a CSR op writing `rd`) is matched by value.
    if !defined_register_operands.is_empty() {
        return Vec::new();
    }
    let operand_names: HashSet<&str> = ops.iter().map(|(name, _)| name.as_str()).collect();
    let ops_by_name: HashMap<&str, &Type> =
        ops.iter().map(|(name, ty)| (name.as_str(), ty)).collect();

    let mut assignments = Vec::new();
    collect_behavior_assignment_exprs(&inst.behavior, &mut assignments);

    let mut writes = Vec::new();
    for (dest, rhs) in assignments {
        let Some((class, name)) = assignment_dest_register_path(dest) else {
            continue;
        };
        if operand_names.contains(name.as_str()) {
            continue;
        }
        let Some(&index) = register_index_map.get(&(class.clone(), name.clone())) else {
            continue;
        };
        let referenced = referenced_operands(rhs, &operand_names);
        let Some(value_operand) = referenced.into_iter().next() else {
            continue;
        };
        let value_is_immediate = matches!(
            ops_by_name.get(value_operand.as_str()),
            Some(Type::Bits(_)) | Some(Type::Integer)
        );
        writes.push(DefinerWrite {
            register_class: class,
            register_index: index,
            value_operand,
            value_is_immediate,
        });
    }
    writes
}

fn infer_defined_register_operands(
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

fn resolve_behavior_rhs<'a>(
    inst: &'a ast::Instruction,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
) -> Option<&'a ast::Expr> {
    let register_operands = register_operand_names(operands);

    let mut assignments = Vec::new();
    collect_behavior_assignments(&inst.behavior, &mut assignments);
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
    if let Some(store) = find_store_effect_expr(&inst.behavior) {
        return Some(store);
    }
    match &inst.behavior {
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
        ast::Expr::For(f) => find_store_effect_expr(&f.body),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Template / asm helpers
// ---------------------------------------------------------------------------

fn resolve_string(expr: &ast::Expr) -> Option<String> {
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

fn resolve_asm_template_for_instruction<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<String> {
    resolve_effective_asm_for_instruction(inst, item_cache).and_then(resolve_string)
}

// Actions derived from a simple asm template string.
enum AsmAction {
    SkipMnemonic,
    Comma,
    Operand(String),
    Skip,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Star,
}

enum AsmPrintPart {
    Text(String),
    Operand(String),
}

fn compile_asm_template(template: &str) -> Vec<AsmAction> {
    let mut actions = Vec::new();
    let mut i = 0;
    let bytes = template.as_bytes();
    while i < bytes.len() {
        match bytes[i] as char {
            '{' => {
                if let Some(end) = template[i + 1..].find('}') {
                    let content = &template[i + 1..i + 1 + end];
                    i = i + 1 + end + 1;
                    if content.starts_with("self.") {
                        if content.ends_with("MNEMONIC") {
                            actions.push(AsmAction::SkipMnemonic);
                        } else {
                            actions.push(AsmAction::Skip);
                        }
                    } else {
                        actions.push(AsmAction::Operand(content.to_string()));
                    }
                    continue;
                } else {
                    i += 1;
                    continue;
                }
            }
            ',' => {
                actions.push(AsmAction::Comma);
                i += 1;
            }
            '(' => {
                actions.push(AsmAction::LParen);
                i += 1;
            }
            ')' => {
                actions.push(AsmAction::RParen);
                i += 1;
            }
            '[' => {
                actions.push(AsmAction::LBracket);
                i += 1;
            }
            ']' => {
                actions.push(AsmAction::RBracket);
                i += 1;
            }
            '*' => {
                actions.push(AsmAction::Star);
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    actions
}

fn compile_asm_printer_template(template: &str, mnemonic: &str) -> Vec<AsmPrintPart> {
    let mut parts = Vec::new();
    let mut cursor = 0;

    while let Some(open_rel) = template[cursor..].find('{') {
        let open = cursor + open_rel;
        if open > cursor {
            parts.push(AsmPrintPart::Text(template[cursor..open].to_string()));
        }

        let Some(close_rel) = template[open + 1..].find('}') else {
            parts.push(AsmPrintPart::Text(template[open..].to_string()));
            return parts;
        };
        let close = open + 1 + close_rel;
        let content = &template[open + 1..close];
        if content == "self.MNEMONIC" {
            parts.push(AsmPrintPart::Text(mnemonic.to_string()));
        } else if !content.starts_with("self.") {
            parts.push(AsmPrintPart::Operand(content.to_string()));
        }
        cursor = close + 1;
    }

    if cursor < template.len() {
        parts.push(AsmPrintPart::Text(template[cursor..].to_string()));
    }

    parts
}

// ---------------------------------------------------------------------------
// AsSemExpr code generation
// ---------------------------------------------------------------------------

/// If the behavior is a conditional control transfer `if COND { PC::pc = TARGET }`
/// (no else), synthesize the value written to PC every cycle: `if COND { TARGET }
/// else { PC::pc + width }`. The fall-through arm keeps PC advancing when the branch
/// is not taken, so the result can be written unconditionally. Returns `None` for
/// behaviors that are not a bare conditional PC write.
fn synthesize_branch_value(inst: &ast::Instruction, width_bytes: u64) -> Option<ast::Expr> {
    let ast::Expr::If(if_) = unwrap_single_stmt_block(&inst.behavior) else {
        return None;
    };
    if if_.else_.is_some() {
        return None;
    }
    let target = extract_pc_assignment_target(&if_.then)?;
    let span = if_.span;
    let pc_read = ast::Expr::Path(ast::Path {
        base: "PC".to_string(),
        remainder: vec!["pc".to_string()],
        span,
    });
    // `zext(width, 64)` so the fall-through addend matches `PC::pc`'s 64-bit width
    // (a bare literal would lower to a narrow constant and mismatch the add).
    let width_lit = ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new(
        width_bytes.to_string(),
        span,
    )));
    let xlen_lit = ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new("64".to_string(), span)));
    let width_ext = ast::Expr::Call(ast::Call {
        callee: Box::new(ast::Expr::BuiltinFunction(ast::BuiltinFunction::ZExt)),
        arguments: vec![width_lit, xlen_lit],
        span,
    });
    let fallthrough = ast::Expr::Binary(ast::Binary {
        lhs: Box::new(pc_read),
        rhs: Box::new(width_ext),
        op: ast::BinOp::Add,
        span,
    });
    Some(ast::Expr::If(ast::If {
        cond: if_.cond.clone(),
        then: Box::new(target.clone()),
        else_: Some(Box::new(fallthrough)),
        span,
    }))
}

/// Peel `{ stmt }` blocks down to their single inner statement.
fn unwrap_single_stmt_block(e: &ast::Expr) -> &ast::Expr {
    match e {
        ast::Expr::Block(b) if b.stmts.len() == 1 => unwrap_single_stmt_block(&b.stmts[0]),
        other => other,
    }
}

/// The RHS expression of a single `PC::pc = TARGET` assignment inside a branch's
/// `then` arm.
fn extract_pc_assignment_target(then: &ast::Expr) -> Option<&ast::Expr> {
    let assign = match unwrap_single_stmt_block(then) {
        ast::Expr::Block(b) if b.stmts.len() == 1 => match &b.stmts[0] {
            ast::Expr::Assign(a) => a,
            _ => return None,
        },
        ast::Expr::Assign(a) => a,
        _ => return None,
    };
    if is_pc_dest(&assign.dest) {
        Some(assign.value.as_ref())
    } else {
        None
    }
}

fn is_pc_dest(dest: &ast::Expr) -> bool {
    matches!(dest, ast::Expr::Path(p) if p.base == "PC" && p.remainder == ["pc"])
}

/// Whether `(every, any)` path through `e` assigns `PC::pc`. Reads of PC (e.g.
/// `auipc`'s `rd = PC::pc + …`) do not count — only assignment destinations.
fn pc_writes(e: &ast::Expr) -> (bool, bool) {
    match e {
        ast::Expr::Assign(a) => {
            let w = is_pc_dest(&a.dest);
            (w, w)
        }
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .map(pc_writes)
            .fold((false, false), |acc, w| (acc.0 || w.0, acc.1 || w.1)),
        ast::Expr::If(i) => {
            let (then_every, then_any) = pc_writes(&i.then);
            let (else_every, else_any) = i
                .else_
                .as_ref()
                .map(|e| pc_writes(e))
                .unwrap_or((false, false));
            (then_every && else_every, then_any || else_any)
        }
        // Control-flow kind reflects the no-trap path; handler PC writes are
        // trap entries, not branches.
        ast::Expr::Try(t) => pc_writes(&t.body),
        // A loop may run zero times, so it never *always* writes PC.
        ast::Expr::For(f) => (false, pc_writes(&f.body).1),
        _ => (false, false),
    }
}

fn emit_as_sem_expr_impl(
    rhs: &ast::Expr,
    name_ident: &proc_macro2::Ident,
    numeric_params: &HashMap<String, i64>,
) -> Option<proc_macro2::TokenStream> {
    let mut dag = tir::sem_expr::ExprPostGraph::new();
    let lowering = rhs.lower_to_sema(&mut dag, numeric_params)?;
    // The AsSemExpr impl carries no type annotations (the program-graph builder
    // infers them), so pass no widths.
    let (stmts, root_var) = emit_dag_as_code(&dag, lowering.root, &[]);

    Some(quote! {
        impl tir::sem_expr::AsSemExpr for #name_ident {
            fn convert(
                &self,
                g: &mut impl tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
            ) -> tir::graph::NodeId {
                #(#stmts)*
                #root_var
            }
        }
    })
}

fn is_store_call(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::Call(ast::Call {
            callee,
            ..
        }) if matches!(callee.as_ref(), ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store))
    )
}

fn is_trap_call(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::Call(ast::Call {
            callee,
            ..
        }) if matches!(callee.as_ref(), ast::Expr::BuiltinFunction(ast::BuiltinFunction::Trap))
    )
}

/// The constant cause code of a `trap(cause, ...)` call. `None` when the
/// first argument is not an integer literal. Further arguments (the trap
/// value payload) only matter to the SMT model; the machine's exception
/// handling owns that state here.
fn trap_call_cause(expr: &ast::Expr) -> Option<u64> {
    let ast::Expr::Call(call) = expr else {
        return None;
    };
    match call.arguments.first() {
        Some(ast::Expr::Lit(ast::Lit::Int(li))) => Some(parse_literal_value(li)),
        _ => None,
    }
}

fn emit_behavior_exec(
    expr: &ast::Expr,
    ops: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    mnemonic_lit: &proc_macro2::Literal,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<proc_macro2::TokenStream> {
    match expr {
        ast::Expr::Assign(a) => emit_assignment_exec(
            a.dest.as_ref(),
            a.value.as_ref(),
            ops,
            numeric_params,
            isa_param_values,
            mnemonic_lit,
            register_index_map,
        ),
        ast::Expr::Call(_) if is_store_call(expr) => emit_effect_exec(
            expr,
            ops,
            numeric_params,
            isa_param_values,
            mnemonic_lit,
            register_index_map,
        ),
        ast::Expr::Call(_) if is_trap_call(expr) => {
            let cause = trap_call_cause(expr)?;
            let cause_lit = proc_macro2::Literal::u64_unsuffixed(cause);
            Some(quote! {
                machine.raise_exception(#cause_lit)?;
            })
        }
        // The simulator executes the no-trap path; alignment exceptions are
        // not modeled here (the SMT backend gives the handlers meaning).
        ast::Expr::Try(t) => emit_behavior_exec(
            &t.body,
            ops,
            numeric_params,
            isa_param_values,
            mnemonic_lit,
            register_index_map,
        ),
        ast::Expr::Block(b) => {
            let mut steps = Vec::new();
            for stmt in &b.stmts {
                if let Some(step) = emit_behavior_exec(
                    stmt,
                    ops,
                    numeric_params,
                    isa_param_values,
                    mnemonic_lit,
                    register_index_map,
                ) {
                    steps.push(step);
                } else if matches!(
                    stmt,
                    ast::Expr::Assign(_)
                        | ast::Expr::Block(_)
                        | ast::Expr::If(_)
                        | ast::Expr::Try(_)
                        | ast::Expr::For(_)
                ) || is_store_call(stmt)
                    || is_trap_call(stmt)
                {
                    return None;
                }
            }
            Some(quote! { #(#steps)* })
        }
        ast::Expr::For(f) => {
            // An accumulator loop writes its folded value to `dest`; the value
            // lowers to a `Loop` node the runtime interpreter executes (so even
            // symbolic bounds work). Other loop shapes unroll for constant bounds.
            if let Some((dest, _)) = f.accumulator() {
                emit_assignment_exec(
                    dest,
                    expr,
                    ops,
                    numeric_params,
                    isa_param_values,
                    mnemonic_lit,
                    register_index_map,
                )
            } else {
                let mut consts = numeric_params.clone();
                for (k, v) in isa_param_values {
                    consts.entry(k.clone()).or_insert(*v);
                }
                let block = ast::unroll_for(f, &consts)?;
                emit_behavior_exec(
                    &block,
                    ops,
                    numeric_params,
                    isa_param_values,
                    mnemonic_lit,
                    register_index_map,
                )
            }
        }
        ast::Expr::If(i) => {
            let cond_eval = emit_value_eval(
                i.cond.as_ref(),
                ops,
                numeric_params,
                isa_param_values,
                mnemonic_lit,
                register_index_map,
            )?;
            let then_body = emit_behavior_exec(
                i.then.as_ref(),
                ops,
                numeric_params,
                isa_param_values,
                mnemonic_lit,
                register_index_map,
            )?;
            let else_body = if let Some(else_expr) = &i.else_ {
                emit_behavior_exec(
                    else_expr.as_ref(),
                    ops,
                    numeric_params,
                    isa_param_values,
                    mnemonic_lit,
                    register_index_map,
                )?
            } else {
                quote! {}
            };
            Some(quote! {
                {
                    #cond_eval
                    if value.to_u64() != 0 {
                        #then_body
                    } else {
                        #else_body
                    }
                }
            })
        }
        _ => Some(quote! {}),
    }
}

/// Emit one assignment from a behavior: evaluate its value, then write it to the
/// destination (a register operand, a fixed/status register named by class, or PC).
/// Returns `None` if the value cannot be lowered or the destination is unrecognized.
fn emit_assignment_exec(
    dest: &ast::Expr,
    rhs: &ast::Expr,
    ops: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    mnemonic_lit: &proc_macro2::Literal,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<proc_macro2::TokenStream> {
    let eval = emit_value_eval(
        rhs,
        ops,
        numeric_params,
        isa_param_values,
        mnemonic_lit,
        register_index_map,
    )?;
    let write = emit_destination_write(dest, ops, register_index_map, mnemonic_lit)?;
    Some(quote! {
        {
            #eval
            #write
        }
    })
}

fn emit_effect_exec(
    expr: &ast::Expr,
    ops: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    mnemonic_lit: &proc_macro2::Literal,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<proc_macro2::TokenStream> {
    let eval = emit_value_eval(
        expr,
        ops,
        numeric_params,
        isa_param_values,
        mnemonic_lit,
        register_index_map,
    )?;
    Some(quote! {
        {
            #eval
            let _ = value;
        }
    })
}

/// Emit the statements that bind `value` to the result of evaluating `rhs` against
/// the machine state (reading operands, fixed/status registers, and ISA params).
/// Returns `None` if the expression cannot be lowered (e.g. it loads memory).
fn emit_value_eval(
    rhs: &ast::Expr,
    ops: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    mnemonic_lit: &proc_macro2::Literal,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<proc_macro2::TokenStream> {
    let mut dag = tir::sem_expr::ExprPostGraph::new();
    let lowering =
        rhs.lower_to_sema_with_registers(&mut dag, numeric_params, register_index_map)?;
    // Build the semantic graph inline (no type annotations, so no `_context`).
    let (dag_stmts, _root) = emit_dag_as_code(&dag, lowering.root, &[]);
    let (max_sym_id, sym_inits) = emit_sym_inits(&lowering, ops, isa_param_values, mnemonic_lit);
    let sym_count_lit = proc_macro2::Literal::usize_unsuffixed(max_sym_id + 1);

    Some(quote! {
        let value = {
            let mut __g = tir::sem_expr::ExprPostGraph::new();
            {
                use tir::graph::MutDag as _;
                let g = &mut __g;
                #(#dag_stmts)*
            }
            let mut __syms: Vec<Option<tir::sem_expr::Value>> = vec![None; #sym_count_lit];
            #(#sym_inits)*
            let __syms: Vec<tir::sem_expr::Value> = __syms.into_iter()
                .map(|v| v.unwrap_or_else(|| tir::sem_expr::Value::Int(tir::utils::APInt::new(64, 0))))
                .collect();
            struct __TmdlMachineMemory<'a>(&'a mut dyn tir_be_common::MachineContext);
            impl tir::sem_expr::Memory for __TmdlMachineMemory<'_> {
                type Error = tir_be_common::SimTrap;

                fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, Self::Error> {
                    self.0.read_memory(address, size)
                }

                fn write_memory(
                    &mut self,
                    address: u64,
                    size: usize,
                    value: u64,
                ) -> Result<(), Self::Error> {
                    self.0.write_memory(address, size, value)
                }
            }
            let mut __memory = __TmdlMachineMemory(machine);
            match tir::sem_expr::execute_with_memory(&__g, &__syms, &mut __memory)? {
                tir::sem_expr::Value::Int(i) => i,
                tir::sem_expr::Value::Float(_) | tir::sem_expr::Value::Iterator(_) | tir::sem_expr::Value::RawBits(_) => {
                    return Err(tir_be_common::SimTrap::InvalidInstruction {
                        op: #mnemonic_lit,
                        reason: "instruction semantic expression did not evaluate to integer".to_string(),
                    });
                }
            }
        };
    })
}

/// Emit the steps that fill `__syms` for a lowered behavior: register operands and
/// fixed/status registers are read from the machine; integer operands and ISA
/// parameters are bound to constants. Returns the highest symbol id (to size the
/// table) and the steps.
fn emit_sym_inits(
    lowering: &ast::SemaLowering,
    ops: &[(String, Type)],
    isa_param_values: &HashMap<String, i64>,
    mnemonic_lit: &proc_macro2::Literal,
) -> (usize, Vec<proc_macro2::TokenStream>) {
    let max_sym_id = [
        lowering.variable_symbols.values().copied().max(),
        lowering.register_symbols.values().copied().max(),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0) as usize;

    let mut steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, &sym_id) in &lowering.variable_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        if let Some((_, ty)) = ops.iter().find(|(n, _)| n == name) {
            match ty {
                Type::Struct(_) => steps.push(quote! {
                    {
                        let (class, index) = tir_be_common::register_attr(self.attributes(), #name_lit)
                            .ok_or(tir_be_common::SimTrap::MissingAttribute {
                                op: #mnemonic_lit,
                                attribute: #name_lit,
                            })?;
                        __syms[#sym_lit] = Some(tir::sem_expr::Value::Int(machine.read_register(&class, index)?));
                    }
                }),
                Type::Integer => steps.push(quote! {
                    {
                        let value = tir_be_common::int_attr(self.attributes(), #name_lit)
                            .ok_or(tir_be_common::SimTrap::MissingAttribute {
                                op: #mnemonic_lit,
                                attribute: #name_lit,
                            })?;
                        __syms[#sym_lit] = Some(tir::sem_expr::Value::Int(tir::utils::APInt::new_signed(64, value)));
                    }
                }),
                Type::Bits(width) => {
                    let width_lit = proc_macro2::Literal::u32_unsuffixed(*width as u32);
                    steps.push(quote! {
                        {
                            let value = tir_be_common::int_attr(self.attributes(), #name_lit)
                                .ok_or(tir_be_common::SimTrap::MissingAttribute {
                                    op: #mnemonic_lit,
                                    attribute: #name_lit,
                                })?;
                            __syms[#sym_lit] = Some(tir::sem_expr::Value::Int(tir::utils::APInt::new_signed(#width_lit, value)));
                        }
                    });
                }
                _ => {}
            }
        } else if let Some(&value) = isa_param_values.get(name) {
            // An ISA parameter (e.g. `XLEN`): resolve it from the machine's
            // selected feature set, falling back to the widest TMDL value for
            // contexts that don't configure ISA params.
            let value_lit = proc_macro2::Literal::i64_unsuffixed(value);
            steps.push(quote! {
                __syms[#sym_lit] = Some(tir::sem_expr::Value::Int(
                    tir::utils::APInt::new_signed(64, machine.isa_param(#name_lit).unwrap_or(#value_lit)),
                ));
            });
        }
    }
    for ((class, number), &sym_id) in &lowering.register_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let class_lit = proc_macro2::Literal::string(class);
        let number_lit = proc_macro2::Literal::u16_unsuffixed(*number as u16);
        steps.push(quote! {
            __syms[#sym_lit] = Some(tir::sem_expr::Value::Int(machine.read_register(#class_lit, #number_lit)?));
        });
    }

    (max_sym_id, steps)
}

/// Emit the write that stores `value` to an assignment's destination: PC, a register
/// operand resolved from the instruction's attributes (honoring hard-wired-zero
/// registers), or a register named directly by class (e.g. a status flag
/// `PSTATE::z`). Returns `None` for an unrecognized destination.
fn emit_destination_write(
    dest: &ast::Expr,
    ops: &[(String, Type)],
    register_index_map: &HashMap<(String, String), u32>,
    mnemonic_lit: &proc_macro2::Literal,
) -> Option<proc_macro2::TokenStream> {
    if is_pc_dest(dest) {
        return Some(quote! { machine.write_pc(value.to_u64()); });
    }

    let name = assignment_dest_name(dest)?;

    // A register operand: its concrete `(class, index)` comes from the attributes.
    if let Some((_, Type::Struct(_))) = ops.iter().find(|(n, _)| *n == name) {
        let name_lit = proc_macro2::Literal::string(&name);
        return Some(quote! {
            let (dst_class, dst_idx) = tir_be_common::register_attr(self.attributes(), #name_lit).ok_or(
                tir_be_common::SimTrap::MissingAttribute {
                    op: #mnemonic_lit,
                    attribute: #name_lit,
                },
            )?;
            if !register_has_trait_hardwired_zero(&dst_class, dst_idx) {
                machine.write_register(&dst_class, dst_idx, value)?;
            }
        });
    }

    // A register named directly by class, e.g. a status flag `PSTATE::z` or a fixed
    // register like `GPR::x30`; its index is fixed at compile time.
    if let ast::Expr::Path(path) = dest
        && path.remainder.len() == 1
    {
        let key = (path.base.clone(), path.remainder[0].clone());
        if let Some(&index) = register_index_map.get(&key) {
            let class_lit = proc_macro2::Literal::string(&path.base);
            let index_lit = proc_macro2::Literal::u16_unsuffixed(index as u16);
            return Some(quote! {
                if !register_has_trait_hardwired_zero(#class_lit, #index_lit) {
                    machine.write_register(#class_lit, #index_lit, value)?;
                }
            });
        }
    }

    None
}

fn emit_dag_as_code(
    dag: &tir::sem_expr::ExprPostGraph,
    root: tir::graph::NodeId,
    widths: &[Option<u32>],
) -> (Vec<proc_macro2::TokenStream>, proc_macro2::Ident) {
    use tir::graph::Dag;

    let mut stmts: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut node_vars: HashMap<usize, proc_macro2::Ident> = HashMap::new();
    for (counter, node_id) in dag.postorder(root).enumerate() {
        let var = format_ident!("__sem_{}", counter);

        let kind_ts = emit_expr_kind_ts(dag.get_node(node_id));
        stmts.push(quote! { let #var = g.add_node(#kind_ts); });

        if let Some(data) = dag.get_leaf_data(node_id) {
            let data_ts = emit_expr_payload_ts(data);
            stmts.push(quote! { g.set_leaf_data(#var, #data_ts); });
        }

        // Type constraint for this node, where the width is structurally
        // determined (extract result, extension target, comparison). Only
        // *operation* nodes are typed: leaf operands stay wildcards (so e.g. a
        // plain `add` matches any width), and constant leaves are matched by value
        // rather than by a fragile value-derived width.
        if dag.get_leaf_data(node_id).is_none()
            && let Some(Some(width)) = widths.get(node_id.index()).copied()
        {
            let width_lit = proc_macro2::Literal::u32_unsuffixed(width);
            stmts.push(quote! {
                g.set_actual_type(#var, tir::builtin::IntegerType::new(_context, #width_lit));
            });
        }

        let children: Vec<tir::graph::NodeId> = dag.children(node_id).collect();
        for child_id in children {
            let child_var = node_vars[&child_id.index()].clone();
            stmts.push(quote! { g.add_edge(#var, #child_var); });
        }

        node_vars.insert(node_id.index(), var);
    }

    let root_var = node_vars[&root.index()].clone();
    (stmts, root_var)
}

fn emit_expr_kind_ts(kind: &tir::sem_expr::ExprKind) -> proc_macro2::TokenStream {
    use tir::sem_expr::ExprKind;
    match kind {
        ExprKind::Symbol => quote! { tir::sem_expr::ExprKind::Symbol },
        ExprKind::Constant => quote! { tir::sem_expr::ExprKind::Constant },
        ExprKind::Add => quote! { tir::sem_expr::ExprKind::Add },
        ExprKind::Sub => quote! { tir::sem_expr::ExprKind::Sub },
        ExprKind::Mul => quote! { tir::sem_expr::ExprKind::Mul },
        ExprKind::Div => quote! { tir::sem_expr::ExprKind::Div },
        ExprKind::UDiv => quote! { tir::sem_expr::ExprKind::UDiv },
        ExprKind::Eq => quote! { tir::sem_expr::ExprKind::Eq },
        ExprKind::Ne => quote! { tir::sem_expr::ExprKind::Ne },
        ExprKind::Lt => quote! { tir::sem_expr::ExprKind::Lt },
        ExprKind::Gt => quote! { tir::sem_expr::ExprKind::Gt },
        ExprKind::Ge => quote! { tir::sem_expr::ExprKind::Ge },
        ExprKind::ULt => quote! { tir::sem_expr::ExprKind::ULt },
        ExprKind::ULe => quote! { tir::sem_expr::ExprKind::ULe },
        ExprKind::UGt => quote! { tir::sem_expr::ExprKind::UGt },
        ExprKind::UGe => quote! { tir::sem_expr::ExprKind::UGe },
        ExprKind::ShiftLeft => quote! { tir::sem_expr::ExprKind::ShiftLeft },
        ExprKind::ShiftRightArithmetic => quote! { tir::sem_expr::ExprKind::ShiftRightArithmetic },
        ExprKind::ShiftRightLogic => quote! { tir::sem_expr::ExprKind::ShiftRightLogic },
        ExprKind::Or => quote! { tir::sem_expr::ExprKind::Or },
        ExprKind::And => quote! { tir::sem_expr::ExprKind::And },
        ExprKind::Xor => quote! { tir::sem_expr::ExprKind::Xor },
        ExprKind::Not => quote! { tir::sem_expr::ExprKind::Not },
        ExprKind::If => quote! { tir::sem_expr::ExprKind::If },
        ExprKind::Clamp => quote! { tir::sem_expr::ExprKind::Clamp },
        ExprKind::LoadMemory => quote! { tir::sem_expr::ExprKind::LoadMemory },
        ExprKind::StoreMemory => quote! { tir::sem_expr::ExprKind::StoreMemory },
        ExprKind::ZExt => quote! { tir::sem_expr::ExprKind::ZExt },
        ExprKind::SExt => quote! { tir::sem_expr::ExprKind::SExt },
        ExprKind::Extract => quote! { tir::sem_expr::ExprKind::Extract },
        ExprKind::Log2Ceil => quote! { tir::sem_expr::ExprKind::Log2Ceil },
        ExprKind::Sqrt => quote! { tir::sem_expr::ExprKind::Sqrt },
        ExprKind::Fma => quote! { tir::sem_expr::ExprKind::Fma },
        ExprKind::Loop => quote! { tir::sem_expr::ExprKind::Loop },
        ExprKind::IndVar => quote! { tir::sem_expr::ExprKind::IndVar },
        ExprKind::Acc => quote! { tir::sem_expr::ExprKind::Acc },
        ExprKind::VectorMap => quote! { tir::sem_expr::ExprKind::VectorMap },
        ExprKind::Lane => quote! { tir::sem_expr::ExprKind::Lane },
        ExprKind::Map => quote! { tir::sem_expr::ExprKind::Map },
        ExprKind::Zip => quote! { tir::sem_expr::ExprKind::Zip },
        ExprKind::IterConcat => quote! { tir::sem_expr::ExprKind::IterConcat },
        ExprKind::Split => quote! { tir::sem_expr::ExprKind::Split },
        ExprKind::Reduce => quote! { tir::sem_expr::ExprKind::Reduce },
        ExprKind::Arg => quote! { tir::sem_expr::ExprKind::Arg },
    }
}

fn emit_expr_payload_ts(payload: &tir::sem_expr::ExprPayload) -> proc_macro2::TokenStream {
    use tir::sem_expr::ExprPayload;
    match payload {
        ExprPayload::SymbolId(id) => {
            let id_lit = proc_macro2::Literal::u32_unsuffixed(*id);
            quote! { tir::sem_expr::ExprPayload::SymbolId(#id_lit) }
        }
        ExprPayload::Value(value) => {
            let value_lit = proc_macro2::Literal::u32_unsuffixed(value.number());
            quote! { tir::sem_expr::ExprPayload::Value(tir::ValueId::from_number(#value_lit)) }
        }
        ExprPayload::Int(v) => {
            let width = proc_macro2::Literal::u32_unsuffixed(v.width());
            if v.is_signed() {
                let val = proc_macro2::Literal::i64_unsuffixed(v.to_i64());
                quote! { tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new_signed(#width, #val)) }
            } else {
                let val = proc_macro2::Literal::u64_unsuffixed(v.to_u64());
                quote! { tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new(#width, #val)) }
            }
        }
        ExprPayload::Float(f) => {
            let val = proc_macro2::Literal::f64_unsuffixed(f.to_f64());
            quote! { tir::sem_expr::ExprPayload::Float(tir::utils::APFloat::from_f64(#val)) }
        }
    }
}

// ---------------------------------------------------------------------------
// Instruction encoders
// ---------------------------------------------------------------------------

/// One contiguous run of an integer operand's bits placed into the encoded
/// word: operand bits `[op_lo, op_lo + width)` land at word bits
/// `[word_lo, word_lo + width)`.
struct IntField {
    op_lo: u16,
    word_lo: u16,
    width: u16,
}

fn encoding_mask(width: u16) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

/// Compile an instruction's encoding arms into an `encode_*_inst` function
/// (and, when the encoding has exactly one immediate operand of known width,
/// a `patch_*_inst` function that re-scatters a resolved fixup value).
/// Returns `None` when the instruction has no encoding.
fn emit_instruction_encoder(
    inst: &ast::Instruction,
    encoding_arms: &[ast::EncodingArm],
    ops_map: &HashMap<String, Type>,
    resolved_params: &HashMap<String, (Type, Option<ast::Expr>)>,
    width_bytes: u64,
) -> Result<Option<(proc_macro2::TokenStream, Option<proc_macro2::TokenStream>)>, TMDLError> {
    if encoding_arms.is_empty() {
        return Ok(None);
    }
    if width_bytes > 16 {
        return Err(TMDLError::Codegen(format!(
            "instruction '{}': encodings wider than 128 bits are not supported",
            inst.name
        )));
    }

    let mut const_word: u128 = 0;
    // Insertion-ordered so generated code is stable across runs.
    let mut reg_fields: Vec<(String, Vec<IntField>)> = Vec::new();
    let mut int_fields: Vec<(String, Vec<IntField>)> = Vec::new();

    let push_field = |dst: &mut Vec<(String, Vec<IntField>)>, name: &str, field: IntField| match dst
        .iter_mut()
        .find(|(n, _)| n == name)
    {
        Some((_, fields)) => fields.push(field),
        None => dst.push((name.to_string(), vec![field])),
    };

    for arm in encoding_arms {
        let word_lo = arm.start;
        let width = arm.end.unwrap_or(arm.start) - arm.start + 1;
        let bad_value = || {
            TMDLError::Codegen(format!(
                "instruction '{}': unsupported encoding value at bits {}..{}",
                inst.name,
                arm.start,
                arm.end.unwrap_or(arm.start)
            ))
        };

        match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => {
                const_word |=
                    (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
            }
            ast::Expr::Ident(id) => match ops_map.get(&id.name) {
                Some(Type::Struct(_)) => push_field(
                    &mut reg_fields,
                    &id.name,
                    IntField {
                        op_lo: 0,
                        word_lo,
                        width,
                    },
                ),
                Some(Type::Integer | Type::Bits(_)) => push_field(
                    &mut int_fields,
                    &id.name,
                    IntField {
                        op_lo: 0,
                        word_lo,
                        width,
                    },
                ),
                Some(_) => return Err(bad_value()),
                None => match resolved_params.get(&id.name) {
                    Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
                        const_word |=
                            (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
                    }
                    _ => {
                        return Err(TMDLError::Codegen(format!(
                            "instruction '{}': encoding parameter '{}' has no literal value",
                            inst.name, id.name
                        )));
                    }
                },
            },
            ast::Expr::Slice(slc) => {
                let ast::Expr::Ident(id) = &*slc.base else {
                    return Err(bad_value());
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return Err(bad_value()),
                };
                push_field(
                    dst,
                    &id.name,
                    IntField {
                        op_lo: slc.start,
                        word_lo,
                        width,
                    },
                );
            }
            ast::Expr::IndexAccess(idx) => {
                let ast::Expr::Ident(id) = &*idx.base else {
                    return Err(bad_value());
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return Err(bad_value()),
                };
                push_field(
                    dst,
                    &id.name,
                    IntField {
                        op_lo: idx.index,
                        word_lo,
                        width: 1,
                    },
                );
            }
            _ => return Err(bad_value()),
        }
    }

    let scatter = |fields: &[IntField]| -> Vec<proc_macro2::TokenStream> {
        fields
            .iter()
            .map(|f| {
                let mask = proc_macro2::Literal::u128_suffixed(encoding_mask(f.width));
                let bits = if f.op_lo > 0 {
                    let op_lo = proc_macro2::Literal::u32_suffixed(f.op_lo as u32);
                    quote! { (value >> #op_lo) & #mask }
                } else {
                    quote! { value & #mask }
                };
                if f.word_lo > 0 {
                    let word_lo = proc_macro2::Literal::u32_suffixed(f.word_lo as u32);
                    quote! { word |= (#bits) << #word_lo; }
                } else {
                    quote! { word |= #bits; }
                }
            })
            .collect()
    };

    let mut steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, fields) in &reg_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let ors = scatter(fields);
        steps.push(quote! {
            {
                let attr = op.attributes.iter().find(|a| a.name == #name_lit)?;
                let value = match &attr.value {
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::Physical { index, .. },
                    ) => *index as u128,
                    _ => return None,
                };
                #(#ors)*
            }
        });
    }

    for (name, fields) in &int_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let ors = scatter(fields);
        // Immediates written in assembly may be spelled signed or unsigned
        // (`-1` vs `0xFFF`), so accept either fit within the declared width.
        let (int_check, uint_check) = match ops_map.get(name.as_str()) {
            Some(Type::Bits(n)) => {
                let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
                let max = proc_macro2::Literal::i64_suffixed(1i64 << n);
                let umax = proc_macro2::Literal::u64_suffixed(1u64 << n);
                (
                    quote! { if !(#min..#max).contains(&v) { return None; } },
                    quote! { if v >= #umax { return None; } },
                )
            }
            _ => (quote! {}, quote! {}),
        };
        steps.push(quote! {
            {
                let attr = op.attributes.iter().find(|a| a.name == #name_lit)?;
                match &attr.value {
                    tir::attributes::AttributeValue::Int(v) => {
                        let v = *v;
                        #int_check
                        let value = v as u128;
                        #(#ors)*
                    }
                    tir::attributes::AttributeValue::UInt(v) => {
                        let v = *v;
                        #uint_check
                        let value = v as u128;
                        #(#ors)*
                    }
                    tir::attributes::AttributeValue::Str(s) => {
                        fixups.push(tir_be_common::binary::InstFixup {
                            operand: #name_lit,
                            target: tir_be_common::binary::FixupTarget::Symbol(s.clone()),
                        });
                    }
                    tir::attributes::AttributeValue::Block(b) => {
                        fixups.push(tir_be_common::binary::InstFixup {
                            operand: #name_lit,
                            target: tir_be_common::binary::FixupTarget::Block(*b),
                        });
                    }
                    _ => return None,
                }
            }
        });
    }

    let encode_fn_ident = format_ident!("encode_{}_inst", inst.name.to_lowercase());
    let const_word_lit = proc_macro2::Literal::u128_suffixed(const_word);
    let wb_lit = proc_macro2::Literal::usize_unsuffixed(width_bytes as usize);
    let word_decl = if reg_fields.is_empty() && int_fields.is_empty() {
        quote! { let word: u128 = #const_word_lit; }
    } else {
        quote! { let mut word: u128 = #const_word_lit; }
    };
    let fixups_decl = if int_fields.is_empty() {
        quote! { let fixups: Vec<tir_be_common::binary::InstFixup> = Vec::new(); }
    } else {
        quote! { let mut fixups: Vec<tir_be_common::binary::InstFixup> = Vec::new(); }
    };
    // Operand-less instructions (e.g. ecall) encode to a constant word and never
    // consult the op's attributes.
    let op_param = if reg_fields.is_empty() && int_fields.is_empty() {
        quote! { _op }
    } else {
        quote! { op }
    };
    let encoder = quote! {
        fn #encode_fn_ident(
            #op_param: &tir::OpInstance,
        ) -> Option<tir_be_common::binary::EncodedInst> {
            #word_decl
            #fixups_decl
            #(#steps)*
            Some(tir_be_common::binary::EncodedInst {
                bytes: word.to_le_bytes()[..#wb_lit].to_vec(),
                fixups,
            })
        }
    };

    // A patcher is only meaningful when the encoding has exactly one immediate
    // operand of known width: the value scattered into it is a resolved fixup
    // (e.g. a pc-relative branch delta), which must fit as a signed quantity.
    let patcher = if let [(name, fields)] = &int_fields[..]
        && let Some(Type::Bits(n)) = ops_map.get(name.as_str())
    {
        let patch_fn_ident = format_ident!("patch_{}_inst", inst.name.to_lowercase());
        let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
        let max = proc_macro2::Literal::i64_suffixed(1i64 << (n - 1));
        let lowest_bit = fields.iter().map(|f| f.op_lo).min().unwrap_or(0);
        // Operand bits below the lowest encoded bit are silently dropped by the
        // scatter (e.g. bit 0 of RISC-V branch offsets); a value with any of
        // them set cannot be represented.
        let dropped_check = if lowest_bit > 0 {
            let dropped_mask = proc_macro2::Literal::u128_suffixed(encoding_mask(lowest_bit));
            quote! { if (value as u128) & #dropped_mask != 0 { return None; } }
        } else {
            quote! {}
        };
        let ors = scatter(fields);
        Some(quote! {
            fn #patch_fn_ident(bytes: &mut [u8], value: i64) -> Option<()> {
                if !(#min..#max).contains(&value) {
                    return None;
                }
                #dropped_check
                if bytes.len() < #wb_lit {
                    return None;
                }
                let mut word: u128 = 0;
                for (i, b) in bytes.iter().enumerate().take(#wb_lit) {
                    word |= (*b as u128) << (8 * i);
                }
                let value = value as u128;
                #(#ors)*
                let out = word.to_le_bytes();
                bytes[..#wb_lit].copy_from_slice(&out[..#wb_lit]);
                Some(())
            }
        })
    } else {
        None
    };

    Ok(Some((encoder, patcher)))
}
