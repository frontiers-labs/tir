// ---------------------------------------------------------------------------
// Table-driven isel rule emission: every rule site lowers to a static
// `RuleSpec` plus a static `EmitSpec` with a shim, interpreted by
// `tir::backend::isel::build_rules` / `emit_with`.

/// `&[Feature::A as u16, ...]` for a rule's availability set.
fn feature_id_slice(for_isas: &[String]) -> proc_macro2::TokenStream {
    let ids = for_isas.iter().map(|name| {
        let ident = format_ident!("{}", name);
        quote! { Feature::#ident as u16 }
    });
    quote! { &[#(#ids),*] }
}

fn emit_attr_result(name: &str, result: usize, class: &proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let result_lit = proc_macro2::Literal::u16_unsuffixed(result as u16);
    quote! {
        tir::backend::isel::EmitAttr::Result {
            attr: #name_lit,
            result: #result_lit,
            class: #class,
        }
    }
}

fn emit_attr_value(name: &str, symbol: u32, class: &proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
    quote! {
        tir::backend::isel::EmitAttr::Value {
            attr: #name_lit,
            symbol: #symbol_lit,
            class: #class,
        }
    }
}

fn emit_attr_fixed_use(
    name: &str,
    symbol: u32,
    class: &proc_macro2::TokenStream,
    index: u16,
) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
    let index_lit = proc_macro2::Literal::u16_unsuffixed(index);
    quote! {
        tir::backend::isel::EmitAttr::FixedUse {
            attr: #name_lit,
            symbol: #symbol_lit,
            class: #class,
            index: #index_lit,
        }
    }
}

fn emit_attr_result_fixed_def(
    name: &str,
    result: usize,
    class: &proc_macro2::TokenStream,
    index: u16,
) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let result_lit = proc_macro2::Literal::u16_unsuffixed(result as u16);
    let index_lit = proc_macro2::Literal::u16_unsuffixed(index);
    quote! {
        tir::backend::isel::EmitAttr::ResultFixedDef {
            attr: #name_lit,
            result: #result_lit,
            class: #class,
            index: #index_lit,
        }
    }
}

fn emit_attr_physical(name: &str, class: &proc_macro2::TokenStream, index: u16) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let index_lit = proc_macro2::Literal::u16_unsuffixed(index);
    quote! {
        tir::backend::isel::EmitAttr::Physical {
            attr: #name_lit,
            class: #class,
            index: #index_lit,
        }
    }
}

fn emit_attr_int(name: &str, symbol: u32) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
    quote! {
        tir::backend::isel::EmitAttr::Int {
            attr: #name_lit,
            symbol: #symbol_lit,
        }
    }
}

fn emit_attr_block(name: &str, symbol: u32) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
    quote! {
        tir::backend::isel::EmitAttr::Block {
            attr: #name_lit,
            symbol: #symbol_lit,
        }
    }
}

fn emit_attr_int_or_value(name: &str, symbol: u32) -> proc_macro2::TokenStream {
    let name_lit = proc_macro2::Literal::string(name);
    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
    quote! {
        tir::backend::isel::EmitAttr::IntOrValue {
            attr: #name_lit,
            symbol: #symbol_lit,
        }
    }
}

/// The `static EMIT_*` table plus the `emit_isel_*` shim fn for one emitter.
/// Returns `(tokens, shim_fn_ident)`.
fn emit_emitter_spec(
    rule_key: &str,
    dialect: &str,
    op_name: &str,
    op_ty_ident: &proc_macro2::Ident,
    attrs: &[proc_macro2::TokenStream],
    declared: &[String],
) -> (proc_macro2::TokenStream, proc_macro2::Ident) {
    let spec_ident = format_ident!("EMIT_{}", rule_key.to_uppercase());
    let shim_ident = format_ident!("emit_isel_{}", rule_key);
    let dialect_lit = proc_macro2::Literal::string(dialect);
    let op_name_lit = proc_macro2::Literal::string(op_name);
    let declared_lits: Vec<proc_macro2::Literal> = declared
        .iter()
        .map(|n| proc_macro2::Literal::string(n))
        .collect();
    let tokens = quote! {
        static #spec_ident: tir::backend::isel::EmitSpec = tir::backend::isel::EmitSpec {
            op: (#dialect_lit, #op_name_lit),
            wrap: <#op_ty_ident as tir::Operation>::from_op_instance_dyn,
            attrs: &[#(#attrs),*],
            declared: &[#(#declared_lits),*],
        };

        fn #shim_ident(
            context: &tir::Context,
            req: &tir::backend::isel::EmitRequest,
            m: &tir::backend::isel::RuleMatch,
        ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
            tir::backend::isel::emit_with(context, req, m, &#spec_ident)
        }
    };
    (tokens, shim_ident)
}

/// One `RegOperandSpec` entry.
fn reg_operand_spec(
    symbol: u32,
    class_name: &str,
    sensitive: bool,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> proc_macro2::TokenStream {
    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
    let class_id = reg_class_id(class_name);
    let capability = if polymorphic_classes.contains(class_name) {
        quote! { tir::backend::isel::CapabilityKind::Any }
    } else if float_classes.contains(class_name) {
        quote! { tir::backend::isel::CapabilityKind::Float }
    } else {
        quote! { tir::backend::isel::CapabilityKind::Integer }
    };
    quote! {
        tir::backend::isel::RegOperandSpec {
            symbol: #symbol_lit,
            class: #class_id,
            whole: #sensitive,
            capability: #capability,
        }
    }
}

/// The `RegOperandSpec` entries for `(symbol, class)` pairs.
fn operand_register_specs(
    operands: &[(u32, String)],
    sensitive_symbols: &HashSet<u32>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> Vec<proc_macro2::TokenStream> {
    operands
        .iter()
        .map(|(symbol, class_name)| {
            reg_operand_spec(
                *symbol,
                class_name,
                sensitive_symbols.contains(symbol),
                float_classes,
                polymorphic_classes,
            )
        })
        .collect()
}

/// The register operands of an instruction's own ops that bind pattern symbols.
fn operand_register_specs_for_ops(
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
    sensitive_symbols: &HashSet<u32>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> Vec<proc_macro2::TokenStream> {
    let operands: Vec<(u32, String)> = ops
        .iter()
        .filter_map(|(op_name, op_ty)| {
            let Type::Struct(class_name) = op_ty else {
                return None;
            };
            Some((*variable_symbols.get(op_name)?, class_name.clone()))
        })
        .collect();
    operand_register_specs(
        &operands,
        sensitive_symbols,
        float_classes,
        polymorphic_classes,
    )
}

fn result_register_spec(
    class_name: &str,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> proc_macro2::TokenStream {
    let class_id = reg_class_id(class_name);
    let capability = if polymorphic_classes.contains(class_name) {
        quote! { tir::backend::isel::CapabilityKind::Any }
    } else if float_classes.contains(class_name) {
        quote! { tir::backend::isel::CapabilityKind::Float }
    } else {
        quote! { tir::backend::isel::CapabilityKind::Integer }
    };
    quote! {
        tir::backend::isel::ResultRegSpec {
            class: #class_id,
            capability: #capability,
        }
    }
}

fn imm_range_spec_entries(ranges: &[(u32, u32, bool)]) -> Vec<proc_macro2::TokenStream> {
    ranges
        .iter()
        .map(|(symbol, width, signed)| {
            let symbol_lit = proc_macro2::Literal::u32_unsuffixed(*symbol);
            let width_lit = proc_macro2::Literal::u32_unsuffixed(*width);
            quote! {
                (#symbol_lit, tir::backend::isel::ImmRange { width: #width_lit, signed: #signed })
            }
        })
        .collect()
}

fn constraint_entry(symbol: u32, constraint: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
    quote! { (#symbol_lit, #constraint) }
}

/// A serialized pattern: blob offset, whether replay needs types, and the
/// float materialization width if the pattern roots a bitcast into a float
/// register.
struct SpecPattern {
    offset: u32,
    typed: bool,
    float_width: Option<u32>,
}

fn pattern_ref_tokens(pattern: &SpecPattern) -> proc_macro2::TokenStream {
    let offset_lit = proc_macro2::Literal::u32_unsuffixed(pattern.offset);
    let typed = pattern.typed;
    let float_width = match pattern.float_width {
        Some(w) => {
            let w_lit = proc_macro2::Literal::u32_unsuffixed(w);
            quote! { Some(#w_lit) }
        }
        None => quote! { None },
    };
    quote! {
        tir::backend::isel::PatternRef {
            offset: #offset_lit,
            typed: #typed,
            float_width: #float_width,
        }
    }
}

/// The `static RULE_*` table for one rule. Returns `(tokens, spec_ident)`.
#[allow(clippy::too_many_arguments)]
fn emit_rule_spec(
    rule_key: &str,
    rule_name: &str,
    for_isas: &[String],
    pattern: &SpecPattern,
    emits: &[&str],
    kind: proc_macro2::TokenStream,
    prelude_shim: Option<&proc_macro2::Ident>,
    emit_shim: &proc_macro2::Ident,
    constraints: &[proc_macro2::TokenStream],
    registers: &[proc_macro2::TokenStream],
    result: Option<proc_macro2::TokenStream>,
    imm_ranges: &[proc_macro2::TokenStream],
    guarded: Option<&SpecPattern>,
) -> (proc_macro2::TokenStream, proc_macro2::Ident) {
    let spec_ident = format_ident!("RULE_{}", rule_key.to_uppercase());
    let rule_name_lit = proc_macro2::Literal::string(rule_name);
    let features = feature_id_slice(for_isas);
    let pattern_ts = pattern_ref_tokens(pattern);
    let emit_infos: Vec<proc_macro2::Ident> = emits.iter().map(|inst| info_ident(inst)).collect();
    let prelude_ts = match prelude_shim {
        Some(ident) => quote! { Some(#ident) },
        None => quote! { None },
    };
    let result_ts = match result {
        Some(r) => quote! { Some(#r) },
        None => quote! { None },
    };
    let guarded_ts = match guarded {
        Some(g) => {
            let g = pattern_ref_tokens(g);
            quote! { Some(#g) }
        }
        None => quote! { None },
    };
    let tokens = quote! {
        static #spec_ident: tir::backend::isel::RuleSpec = tir::backend::isel::RuleSpec {
            name: #rule_name_lit,
            features: #features,
            pattern: #pattern_ts,
            emits: &[#(&#emit_infos),*],
            kind: #kind,
            prelude_emit: #prelude_ts,
            emit_fn: #emit_shim,
            constraints: &[#(#constraints),*],
            registers: &[#(#registers),*],
            result: #result_ts,
            imm_ranges: &[#(#imm_ranges),*],
            guarded: #guarded_ts,
        };
    };
    (tokens, spec_ident)
}
