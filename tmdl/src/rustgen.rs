use std::collections::{HashMap, HashSet};
use std::io::Write;

use quote::{format_ident, quote};

use crate::Type;
use crate::ast;
use crate::error::TMDLError;
use crate::sem_expr_state;
use crate::utils::{
    get_encoding_arms, parse_literal_value, resolve_effective_asm_for_instruction,
    resolve_isa_param_values, resolve_operand_widths, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};

pub fn generate_rust<'a>(
    dialect: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    text_only: bool,
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    let features = emit_features(files)?;
    let register_traits = emit_register_trait_helpers(files)?;
    let registers = emit_register_parsers_and_printers(files)?;
    let register_info = emit_register_info(files)?;
    let machine_models = emit_machine_models(files, item_cache)?;
    let instruction_cost = emit_instruction_cost(files, item_cache)?;
    let instructions = emit_instructions(dialect, files, item_cache, text_only)?;

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

pub fn generate_operation_list(
    files: &[ast::File],
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    writeln!(output, "[")?;
    for inst in files.iter().flat_map(|f| f.instructions()) {
        let name = format_ident!("{}Op", &inst.name);
        writeln!(output, "    {name},")?;
    }
    writeln!(output, "]")?;

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
        // Variants take their ISA's TMDL name verbatim (e.g. `PTX_7_0`, `X86_64`),
        // which is not upper-camel-case.
        #[allow(non_camel_case_types)]
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
    text_only: bool,
) -> Result<proc_macro2::TokenStream, TMDLError> {
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
        // Each becomes a demand attribute on the emitted op: the value the read
        // bound to (an immediate or a virtual register), consumed later by a
        // target machine pass that materializes the register's definer (e.g.
        // RISC-V `vsetvli` insertion satisfying `vl` demands).
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
        for (op_name, op_ty) in &ops {
            if let Type::Struct(class_name) = op_ty {
                let attr_name_lit = proc_macro2::Literal::string(op_name);
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
            let value_reg_classes: std::collections::HashSet<&str> = ops
                .iter()
                .filter_map(|(_, ty)| match ty {
                    Type::Struct(class) => Some(class.as_str()),
                    _ => None,
                })
                .collect();
            for ((class, index), symbol) in &semantics.register_symbols {
                let is_implicit = register_name_map
                    .get(&(class.clone(), *index))
                    .map(|name| !ops.iter().any(|(op_name, _)| op_name == name))
                    .unwrap_or(false);
                if is_implicit && value_reg_classes.contains(class.as_str()) {
                    let symbol_lit = proc_macro2::Literal::u32_unsuffixed(*symbol);
                    operand_constraint_entries
                        .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Register) });
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
            let (pattern_stmts, _root_var) =
                emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);
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
                (canon_pattern.len() as u32).max(1)
            };
            let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
            let mnemonic_cost_lit = proc_macro2::Literal::string(mnemonic_name);

            // The registers the behavior reads by path (e.g. `VCSR::vl`) are real
            // dependencies not among the encoded operands. Each becomes a demand
            // attribute on the emitted op — the immediate or virtual register the
            // read bound to — satisfied later by a target machine pass that
            // materializes the register's definer (e.g. `vsetvli` insertion).
            for (name, sym) in &implicit_reads {
                let name_lit = proc_macro2::Literal::string(name);
                let sym_lit = proc_macro2::Literal::u32_unsuffixed(*sym);
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
                        ,
                    );
                }
            });
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
            instruction_printers_impls.push(quote! {
                fn #print_fn_ident(_ctx: &tir::Context, #op_param: &tir::OpInstance) -> Option<String> {
                    #attrs_binding
                    let mut out = String::new();
                    #(#print_steps)*
                    Some(out)
                }
            });

            let printer_op_name_lit = proc_macro2::Literal::string(op_name);
            instruction_printer_map_inits.push(quote! {
                let f: tir::backend::AsmInstructionPrinter = #print_fn_ident;
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
                    #parser_param: &mut tir::parse::tokens::Parser<'src, tir::backend::Token<'src>>,
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
    emit_flag_rules(
        files,
        item_cache,
        &register_index_map,
        &pc_classes,
        &flag_classes,
        &mut isel_rule_emitters,
        &mut isel_rule_inits,
    )?;

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
            pub fn asm_syntax() -> &'static [tir::backend::asm_syntax::InstrSyntax] {
                &[#(#asm_syntax_entries),*]
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
            fn get_instruction_encoders() -> std::collections::HashMap<String, tir::backend::binary::InstructionEncoder> {
                let mut map: std::collections::HashMap<String, tir::backend::binary::InstructionEncoder> = std::collections::HashMap::new();
                #(#instruction_encoder_map_inits)*

                map
            }

            fn get_instruction_patchers() -> std::collections::HashMap<String, tir::backend::binary::InstructionPatcher> {
                let mut map: std::collections::HashMap<String, tir::backend::binary::InstructionPatcher> = std::collections::HashMap::new();
                #(#instruction_patcher_map_inits)*

                map
            }
        }
    };

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
            std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>>,
            std::collections::HashSet<String>,
        ) {
            let mut map: std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>> = std::collections::HashMap::new();
            let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
            #(#instruction_parsers_impls)*
            #(#instruction_parser_map_inits)*

            // A mnemonic with any enabled form stays available.
            disabled.retain(|mnemonic| !map.contains_key(mnemonic));
            (map, disabled)
        }

        fn get_instruction_printers() -> std::collections::HashMap<String, tir::backend::AsmInstructionPrinter> {
            let mut map: std::collections::HashMap<String, tir::backend::AsmInstructionPrinter> = std::collections::HashMap::new();
            #(#instruction_printers_impls)*
            #(#instruction_printer_map_inits)*

            map
        }

        #syntax_section

        #encoder_section

        #(#instruction_decoder_impls)*

        /// Decode a 32-bit little-endian machine word into a freshly-built op in
        /// `context`, returning its id, or `None` if no instruction matches.
        /// Instructions are tried most-specific-first (by count of fixed opcode
        /// bits); each matches on its fixed opcode bits and reconstructs its
        /// operands from the word.
        pub fn decode_instruction(context: &tir::Context, word: u32) -> Option<tir::OpId> {
            let _ = (&context, word);
            #(#instruction_decoder_dispatch)*
            None
        }

        #(#isel_rule_emitters)*

        /// Instruction-selection rules for the instructions available under `features`.
        pub fn get_isel_rules(context: &tir::Context, features: &[Feature]) -> Vec<tir::backend::isel::Rule> {
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
            pub fn #fn_name<'src>(parser: &mut tir::parse::tokens::Parser<'src, tir::backend::Token<'src>>) -> Option<u16> {
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
/// [`tir::backend::regalloc::RegisterInfo`] the allocator consumes: per class, the
/// allocatable order plus the caller/callee-saved, argument, return-value, and
/// reserved index sets, all derived from each register's TMDL traits.
/// The `RegClassId` expression for a statically-known register class, referencing
/// the generated per-dialect `RegClass` enum emitted alongside `register_info()`.
fn reg_class_id(class_name: &str) -> proc_macro2::TokenStream {
    let variant = format_ident!("{}", class_name);
    quote! { RegClass::#variant.id() }
}

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
    let mut class_variants = Vec::new();
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        class_variants.push(format_ident!("{}", rc.name));
        let name_lit = proc_macro2::Literal::string(&rc.name);
        let file_lit = proc_macro2::Literal::string(rc.register_file(&classes));
        let meta = rc.allocation_metadata();
        let allocation_order = slice(&meta.allocation_order);
        let caller_saved = slice(&meta.caller_saved);
        let callee_saved = slice(&meta.callee_saved);
        let arguments = slice(&meta.arguments);
        let return_values = slice(&meta.return_values);
        let reserved = slice(&meta.reserved);
        // A `GROUP_SIZE` class param declares how many consecutive file indices
        // one register covers (RVV LMUL>1 group classes); default 1.
        let group_width = match rc.parameters.get("GROUP_SIZE") {
            Some((_ty, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
                proc_macro2::Literal::u16_unsuffixed(parse_literal_value(li).max(1) as u16)
            }
            _ => proc_macro2::Literal::u16_unsuffixed(1),
        };
        class_entries.push(quote! {
            tir::backend::regalloc::RegClassInfo {
                name: #name_lit,
                file: #file_lit,
                allocation_order: #allocation_order,
                caller_saved: #caller_saved,
                callee_saved: #callee_saved,
                arguments: #arguments,
                return_values: #return_values,
                reserved: #reserved,
                group_width: #group_width,
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
                // Cap at 128 so 128-bit SIMD/FP register files (AArch64 V
                // registers) keep their true width. Values wider than 64 bits are
                // carried as `RawBits` (byte lanes) through the register interface,
                // never as a single `APInt`, so this width is safe.
                let lit =
                    proc_macro2::Literal::u32_unsuffixed(parse_literal_value(li).min(128) as u32);
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

    // Sub-register view of each class: bit offset into its storage element and
    // whether narrow writes merge (preserve untouched bits) or zero-extend. Only
    // classes departing from the default (offset 0, zero-extend) get an entry.
    let mut view_entries = Vec::new();
    for rc in files.iter().flat_map(|f| f.register_classes()) {
        let bit_offset = match rc.parameters.get("BIT_OFFSET") {
            Some((_ty, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => parse_literal_value(li) as u32,
            _ => 0,
        };
        let merge = matches!(
            rc.parameters.get("WRITE_POLICY"),
            Some((_ty, Some(ast::Expr::Lit(ast::Lit::Str(s))))) if s.value() == "merge"
        );
        if bit_offset == 0 && !merge {
            continue;
        }
        let name_lit = proc_macro2::Literal::string(&rc.name);
        let off_lit = proc_macro2::Literal::u32_unsuffixed(bit_offset);
        view_entries.push(quote! {
            (#name_lit, tir::backend::regalloc::RegisterView { bit_offset: #off_lit, merge: #merge })
        });
    }

    let class_count = class_entries.len();

    Ok(quote! {
        /// The target's register classes, as a single `'static` table so a
        /// [`tir::backend::regalloc::RegClassId`] can point stably into it.
        static REG_CLASSES: [tir::backend::regalloc::RegClassInfo; #class_count] =
            [#(#class_entries),*];

        /// The register classes of this target. Each variant's `id()` and
        /// `register_info().classes` name the same `REG_CLASSES` entry, so a class's
        /// identity is a stable pointer.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        #[allow(dead_code)]
        pub enum RegClass {
            #(#class_variants),*
        }

        impl RegClass {
            #[allow(dead_code)]
            pub fn id(self) -> tir::backend::regalloc::RegClassId {
                tir::backend::regalloc::RegClassId::new(&REG_CLASSES[self as usize])
            }
        }

        pub fn register_info() -> tir::backend::regalloc::RegisterInfo {
            tir::backend::regalloc::RegisterInfo {
                classes: &REG_CLASSES,
            }
        }

        /// Architectural width in bits of each register class under `features`.
        pub fn register_widths(features: &[Feature]) -> Vec<(&'static str, u32)> {
            let params = isa_params(features);
            let _ = &params;
            vec![#(#width_entries),*]
        }

        /// Sub-register views (bit offset + write policy) of classes that depart
        /// from the default of offset 0 and zero-extending writes.
        pub fn register_views(features: &[Feature]) -> Vec<(&'static str, tir::backend::regalloc::RegisterView)> {
            let _ = features;
            vec![#(#view_entries),*]
        }
    })
}

/// Emit one `fn <machine>_model() -> tir::backend::sched::MachineModel` per TMDL
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
                (#mnem_lit, tir::backend::sched::InstrSchedClass {
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
                tir::backend::sched::PipelinePhase { name: #name_lit, protection: #prot_ts }
            }
        });

        let forward_lits = machine.forwards.iter().map(|f| {
            let from_lit = proc_macro2::Literal::string(&f.from);
            let to_lit = proc_macro2::Literal::string(&f.to);
            let lat_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(f.latency));
            quote! {
                tir::backend::sched::Forward { from: #from_lit, to: #to_lit, latency: #lat_lit }
            }
        });

        let resource_lits = machine.resources.iter().map(|r| {
            let name_lit = proc_macro2::Literal::string(&r.name);
            let units_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(r.units));
            quote! { tir::backend::sched::ProcUnit { name: #name_lit, units: #units_lit } }
        });

        let buffer_lits = machine.buffers.iter().map(|(name, size)| {
            let name_lit = proc_macro2::Literal::string(name);
            let size_lit = proc_macro2::Literal::u32_unsuffixed(clamp_u32(*size));
            quote! { tir::backend::sched::BufferSize { name: #name_lit, size: #size_lit } }
        });

        let reg_file_lits = machine.reg_files.iter().map(|(name, count)| {
            let name_lit = proc_macro2::Literal::string(name);
            let count_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(*count));
            quote! { tir::backend::sched::RegFile { name: #name_lit, count: #count_lit } }
        });

        let name_lit = proc_macro2::Literal::string(&machine.name);
        let issue_width_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(
            machine.issue_width.unwrap_or(1).max(1),
        ));
        let fn_ident = format_ident!("{}_model", to_snake_case(&machine.name));

        model_fns.push(quote! {
            pub fn #fn_ident() -> tir::backend::sched::MachineModel {
                tir::backend::sched::MachineModel {
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

    // A target with no `machine` models (e.g. a text-only pseudo-ISA) gets
    // trivial accessors so `features` and `names` are not spuriously unused.
    if machine_names.is_empty() {
        return Ok(quote! {
            /// No machine models are declared for this target.
            pub fn machine_model(_name: &str, _features: &[Feature]) -> Option<tir::backend::sched::MachineModel> {
                None
            }

            /// No machine models are declared for this target.
            pub fn machines(_features: &[Feature]) -> Vec<&'static str> {
                Vec::new()
            }
        });
    }

    Ok(quote! {
        #(#model_fns)*

        /// Resolve a machine by its TMDL name or alias. `None` when the name is
        /// unknown or the machine's `for [...]` clause is disjoint from `features`.
        pub fn machine_model(name: &str, features: &[Feature]) -> Option<tir::backend::sched::MachineModel> {
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

/// The `tir::backend::sched::Protection` variant for an AST protection mode.
fn protection_ts(p: ast::Protection) -> proc_macro2::TokenStream {
    match p {
        ast::Protection::Protected => quote! { tir::backend::sched::Protection::Protected },
        ast::Protection::Unprotected => quote! { tir::backend::sched::Protection::Unprotected },
        ast::Protection::Hard => quote! { tir::backend::sched::Protection::Hard },
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
    let list = hardwired_patterns.clone();
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

        /// Every `(class, index)` that reads as a hardwired zero (e.g. AArch64
        /// `xzr`). The simulator zeroes these on read so a value stored in an
        /// aliasing slot (e.g. `sp` sharing the file index with `xzr`) never
        /// leaks through the zero register.
        pub fn hardwired_zero_registers() -> &'static [(&'static str, u16)] {
            &[#(#list),*]
        }
    })
}

// ---------------------------------------------------------------------------
// Instruction analysis helpers
// ---------------------------------------------------------------------------

struct InstructionSemantics {
    pattern: tir::sem::SemGraph,
    root: tir::graph::NodeId,
    variable_symbols: HashMap<String, u32>,
    fixed_register_by_class: HashMap<String, Option<u16>>,
    /// `(register class, index) -> pattern symbol` for every register the behavior
    /// reads by path (e.g. `VCSR::vl`). These are implicit reads — registers not
    /// among the encoded operands — and become the rule's `implicit_uses`.
    register_symbols: HashMap<(String, u32), u32>,
}

/// The selectable semantics of a conditional-branch instruction: the branch
/// condition as a pattern, plus the operand carrying the taken target.
struct BranchSemantics {
    /// The condition expression (`rs1 == rs2`, …) as a pattern graph.
    pattern: tir::sem::SemGraph,
    root: tir::graph::NodeId,
    variable_symbols: HashMap<String, u32>,
    /// The immediate operand encoding the taken target (`imm`), and the fresh
    /// pattern symbol the emitter reads it from as a block binding.
    target_operand: String,
    target_symbol: u32,
}

/// Recognize the guarded-PC-write shape `if COND { PC::pc = PC::pc + …imm… }`
/// and derive a conditional-branch rule from it: the pattern is `COND` over the
/// instruction's register operands, and `imm` becomes the taken-target block
/// operand. Anything else (fallthrough writes, extra state, PC in the
/// condition) is rejected.
/// Recognize the guarded-PC-write shape `if COND { PC::pc = …imm… }` and return
/// the guard condition together with the single immediate operand the PC write
/// references (the taken target). Anything else (an `else` arm, fallthrough
/// writes, a non-immediate target) is rejected.
fn guarded_pc_write_shape<'a>(
    inst: &'a ast::Instruction,
    operands: &[(String, Type)],
    pc_classes: &HashSet<String>,
) -> Option<(&'a ast::Expr, String)> {
    // Behavior must be exactly one guarded write: `if cond { PC::pc = … }`.
    let mut body = &inst.behavior;
    while let ast::Expr::Block(block) = body {
        let [stmt] = block.stmts.as_slice() else {
            return None;
        };
        body = stmt;
    }
    let ast::Expr::If(guarded) = body else {
        return None;
    };
    if guarded.else_.is_some() {
        return None;
    }

    let mut taken = guarded.then.as_ref();
    while let ast::Expr::Block(block) = taken {
        let [stmt] = block.stmts.as_slice() else {
            return None;
        };
        taken = stmt;
    }
    let ast::Expr::Assign(assign) = taken else {
        return None;
    };
    let (dest_class, _) = assignment_dest_register_path(&assign.dest)?;
    if !pc_classes.contains(&dest_class) {
        return None;
    }

    // The taken target: the single immediate operand the PC write references.
    let operand_names: HashSet<&str> = operands.iter().map(|(name, _)| name.as_str()).collect();
    let target_refs = referenced_operands(&assign.value, &operand_names);
    let [target_operand] = target_refs.as_slice() else {
        return None;
    };
    let target_is_immediate = operands
        .iter()
        .any(|(name, ty)| name == target_operand && matches!(ty, Type::Bits(_) | Type::Integer));
    if !target_is_immediate {
        return None;
    }

    Some((&guarded.cond, target_operand.clone()))
}

fn analyze_branch_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    pc_classes: &HashSet<String>,
) -> Option<BranchSemantics> {
    let (cond, target_operand) = guarded_pc_write_shape(inst, operands, pc_classes)?;

    // The condition must be expressible over the encoded operands alone.
    if behavior_references_pc(cond, pc_classes) {
        return None;
    }
    let mut pattern = tir::sem::SemGraph::new();
    let lowering = cond.lower_to_sema_with_isa(
        &mut pattern,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    if !lowering.register_symbols.is_empty() {
        return None;
    }

    let target_symbol = lowering
        .variable_symbols
        .values()
        .max()
        .map_or(0, |max| max + 1);

    Some(BranchSemantics {
        pattern,
        root: lowering.root,
        variable_symbols: lowering.variable_symbols,
        target_operand,
        target_symbol,
    })
}

/// A flag-definer instruction (`cmp`, `test`): every behavior statement assigns
/// a status-flag register of one class. `flag_roots` maps each written flag's
/// register index to its value expression, lowered over the encoded operands
/// into `graph` through one shared symbol table.
struct FlagDefinerSemantics {
    class: String,
    graph: tir::sem::SemGraph,
    flag_roots: HashMap<u32, tir::graph::NodeId>,
    variable_symbols: HashMap<String, u32>,
}

/// A flag-guarded branch (`b.lt`, `jl`): a guarded PC write whose condition
/// reads only status-flag registers of one class.
struct FlagBranchSemantics {
    class: String,
    graph: tir::sem::SemGraph,
    root: tir::graph::NodeId,
    /// Guard symbol id -> the flag register index it reads.
    flag_symbols: HashMap<u32, u32>,
    target_operand: String,
}

/// A flag-reading value materializer (`cset`, `setcc`): defines one register as
/// `if <flags> { c1 } else { c0 }` over one class's status flags, with constant
/// arms. Composed with a flag definer it yields a boolean materializer value
/// rule (see `emit_flag_reader_rules`).
struct FlagReaderSemantics {
    class: String,
    graph: tir::sem::SemGraph,
    /// The `if`'s condition, then, and else subgraphs.
    cond_root: tir::graph::NodeId,
    then_root: tir::graph::NodeId,
    else_root: tir::graph::NodeId,
    /// Condition symbol id -> the flag register index it reads.
    flag_symbols: HashMap<u32, u32>,
    dest_operand: String,
}

/// The statement list of a behavior body (peeling wrapper blocks).
fn behavior_statements(behavior: &ast::Expr) -> Vec<&ast::Expr> {
    let mut body = behavior;
    while let ast::Expr::Block(block) = body {
        if let [stmt] = block.stmts.as_slice() {
            body = stmt;
        } else {
            return block.stmts.iter().collect();
        }
    }
    vec![body]
}

/// Recognize a flag definer: every behavior statement assigns a distinct
/// status-flag register of one class, each flag's value a pure function of the
/// encoded register operands. ISA parameters (`self.XLEN`) resolve to their
/// concrete values here — the composed condition is proved against a canonical
/// comparison, so no width expression survives into the emitted pattern.
fn analyze_flag_definer_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Option<FlagDefinerSemantics> {
    if flag_classes.is_empty() {
        return None;
    }
    let stmts = behavior_statements(&inst.behavior);
    if stmts.is_empty() {
        return None;
    }

    let mut class: Option<String> = None;
    let mut flag_exprs: Vec<(u32, &ast::Expr)> = Vec::new();
    for stmt in stmts {
        let ast::Expr::Assign(assign) = stmt else {
            return None;
        };
        let (dest_class, dest_reg) = assignment_dest_register_path(&assign.dest)?;
        if !flag_classes.contains(&dest_class) {
            return None;
        }
        match &class {
            Some(existing) if *existing != dest_class => return None,
            None => class = Some(dest_class.clone()),
            _ => {}
        }
        let index = *register_index_map.get(&(dest_class, dest_reg))?;
        if flag_exprs.iter().any(|(existing, _)| *existing == index) {
            return None;
        }
        if behavior_references_pc(&assign.value, pc_classes) {
            return None;
        }
        flag_exprs.push((index, &assign.value));
    }

    // Composition binds each register operand to a pattern symbol the emitted
    // pair reads back as a register, and at most one immediate operand to a
    // constant symbol feeding the composed comparison. Any other operand shape
    // is not derived.
    let immediate_operands = operands
        .iter()
        .filter(|(_, ty)| matches!(ty, Type::Bits(_) | Type::Integer))
        .count();
    if immediate_operands > 1
        || operands.iter().any(|(_, ty)| {
            !matches!(
                ty,
                Type::Struct(_) | Type::String | Type::Bits(_) | Type::Integer
            )
        })
    {
        return None;
    }

    let mut params = numeric_params.clone();
    params.extend(isa_param_values.iter().map(|(k, v)| (k.clone(), *v)));
    let mut graph = tir::sem::SemGraph::new();
    let exprs: Vec<&ast::Expr> = flag_exprs.iter().map(|(_, expr)| *expr).collect();
    let (roots, lowering) = ast::Expr::lower_all_to_sema_with_isa(
        &exprs,
        &mut graph,
        &params,
        isa_param_values,
        register_index_map,
    )?;
    // The flags must be functions of the encoded operands alone (no implicit
    // register reads), and every register operand must feed some flag, or the
    // emitted definer could not bind it.
    if !lowering.register_symbols.is_empty() {
        return None;
    }
    if operands.iter().any(|(name, ty)| {
        matches!(ty, Type::Struct(_)) && !lowering.variable_symbols.contains_key(name)
    }) {
        return None;
    }

    Some(FlagDefinerSemantics {
        class: class?,
        graph,
        flag_roots: flag_exprs
            .iter()
            .map(|(index, _)| *index)
            .zip(roots)
            .collect(),
        variable_symbols: lowering.variable_symbols,
    })
}

/// Recognize a flag-guarded branch: the guarded-PC-write shape whose condition
/// reads only status-flag registers of one class and whose sole encodable
/// operand is the taken target.
fn analyze_flag_branch_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Option<FlagBranchSemantics> {
    if flag_classes.is_empty() {
        return None;
    }
    let (cond, target_operand) = guarded_pc_write_shape(inst, operands, pc_classes)?;
    if operands
        .iter()
        .any(|(name, ty)| *name != target_operand && !matches!(ty, Type::String))
    {
        return None;
    }
    if behavior_references_pc(cond, pc_classes) {
        return None;
    }

    let mut params = numeric_params.clone();
    params.extend(isa_param_values.iter().map(|(k, v)| (k.clone(), *v)));
    let mut graph = tir::sem::SemGraph::new();
    let lowering =
        cond.lower_to_sema_with_isa(&mut graph, &params, isa_param_values, register_index_map)?;
    if !lowering.variable_symbols.is_empty() || lowering.register_symbols.is_empty() {
        return None;
    }

    let mut class: Option<String> = None;
    let mut flag_symbols = HashMap::new();
    for ((reg_class, index), symbol) in &lowering.register_symbols {
        if !flag_classes.contains(reg_class) {
            return None;
        }
        match &class {
            Some(existing) if existing != reg_class => return None,
            None => class = Some(reg_class.clone()),
            _ => {}
        }
        flag_symbols.insert(*symbol, *index);
    }

    Some(FlagBranchSemantics {
        class: class?,
        graph,
        root: lowering.root,
        flag_symbols,
        target_operand,
    })
}

/// Recognize a flag-reading value materializer: one register defined as `if
/// <cond> { c1 } else { c0 }` whose condition reads only status-flag registers
/// of one class and whose arms are functions of those flags alone. Selects
/// (`csel`, arms reading encoded operands) are rejected by the operand-read
/// check.
///
/// The value is lowered exactly as a plain value rule would be — `self.XLEN`
/// kept as a width symbol rather than const-folded — so the emitted arms are
/// the width-polymorphic `slt`-style form the bool-materialize bridge matches.
fn analyze_flag_reader_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Option<FlagReaderSemantics> {
    use tir::graph::Dag;
    if flag_classes.is_empty() {
        return None;
    }
    let defined_register_operands = infer_defined_register_operands(&inst.behavior, operands);
    let [dest] = defined_register_operands.as_slice() else {
        return None;
    };
    let stmts = behavior_statements(&inst.behavior);
    let [stmt] = stmts.as_slice() else {
        return None;
    };
    let ast::Expr::Assign(assign) = stmt else {
        return None;
    };
    if assignment_dest_name(&assign.dest).as_deref() != Some(dest.as_str()) {
        return None;
    }
    let ast::Expr::If(if_expr) = &*assign.value else {
        return None;
    };
    // The arms carry the materialized value: they must not themselves read flags
    // (the composition only substitutes the condition's reads).
    if if_expr.else_.is_none()
        || behavior_references_pc(&assign.value, pc_classes)
        || behavior_reads_flag_register(&if_expr.then, flag_classes)
        || if_expr
            .else_
            .as_ref()
            .is_some_and(|e| behavior_reads_flag_register(e, flag_classes))
    {
        return None;
    }

    let mut graph = tir::sem::SemGraph::new();
    let lowering = assign.value.lower_to_sema_with_isa(
        &mut graph,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    // No encoded operand feeds the value (that would be a select, not a boolean
    // materializer); `self.XLEN` is an ISA param, not an operand, so it may still
    // appear as a width symbol. It must actually read a flag.
    if operands
        .iter()
        .any(|(name, _)| lowering.variable_symbols.contains_key(name))
        || lowering.register_symbols.is_empty()
    {
        return None;
    }

    let root = lowering.root;
    if *graph.get_node(root) != tir::sem::SymKind::If {
        return None;
    }
    let children: Vec<tir::graph::NodeId> = graph.children(root).collect();
    let [cond_root, then_root, else_root] = children.as_slice() else {
        return None;
    };

    let mut class: Option<String> = None;
    let mut flag_symbols = HashMap::new();
    for ((reg_class, index), symbol) in &lowering.register_symbols {
        if !flag_classes.contains(reg_class) {
            return None;
        }
        match &class {
            Some(existing) if existing != reg_class => return None,
            None => class = Some(reg_class.clone()),
            _ => {}
        }
        flag_symbols.insert(*symbol, *index);
    }

    Some(FlagReaderSemantics {
        class: class?,
        graph,
        cond_root: *cond_root,
        then_root: *then_root,
        else_root: *else_root,
        flag_symbols,
        dest_operand: dest.clone(),
    })
}

/// Copy `node`'s subgraph from `src` into `dst`, preserving payloads. Children
/// are copied first, keeping `dst` in post order.
fn copy_subgraph(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = src.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| copy_subgraph(dst, src, child, memo))
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

/// Copy `node`'s subgraph, renumbering each distinct symbol id through `remap`
/// to a fresh id from `next`. Used to lift a reader's arm symbols (its `XLEN`
/// width var) above the two comparison-operand symbols they are spliced beside,
/// so the two symbol spaces do not collide.
fn copy_subgraph_remap_symbols(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
    remap: &mut HashMap<u32, u32>,
    next: &mut u32,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = src.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| copy_subgraph_remap_symbols(dst, src, child, memo, remap, next))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(data) = src.get_leaf_data(node) {
        let data = if let tir::sem::SymPayload::SymbolId(id) = data {
            let new_id = *remap.entry(*id).or_insert_with(|| {
                let assigned = *next;
                *next += 1;
                assigned
            });
            tir::sem::SymPayload::SymbolId(new_id)
        } else {
            data.clone()
        };
        dst.set_leaf_data(copied, data);
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    memo.insert(node.index(), copied);
    copied
}

/// Copy a boolean value materializer's arm (`zext(0/1, W)`), replacing the
/// widen-to width with a fresh capture symbol so the pattern matches the boolean
/// regardless of the destination register width. Without this an 8-bit `setcc`
/// arm (`zext(1, 8)`) fails to match the width-1 boolean the bridge produces,
/// while an `XLEN`-symbol arm (arm64 `cset`) already generalizes — the value is a
/// boolean 0/1, the register width is not part of what selects it.
fn copy_reader_arm(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    arm_root: tir::graph::NodeId,
    remap: &mut HashMap<u32, u32>,
    next: &mut u32,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    let kind = *src.get_node(arm_root);
    if matches!(kind, tir::sem::SymKind::ZExt | tir::sem::SymKind::SExt) {
        let children: Vec<tir::graph::NodeId> = src.children(arm_root).collect();
        if children.len() == 2 {
            let value = copy_subgraph_remap_symbols(
                dst,
                src,
                children[0],
                &mut HashMap::new(),
                remap,
                next,
            );
            let width = dst.add_node(tir::sem::SymKind::Symbol);
            dst.set_leaf_data(width, tir::sem::SymPayload::SymbolId(*next));
            *next += 1;
            let widened = dst.add_node(kind);
            dst.add_edge(widened, value);
            dst.add_edge(widened, width);
            return widened;
        }
    }
    copy_subgraph_remap_symbols(dst, src, arm_root, &mut HashMap::new(), remap, next)
}

/// Copy the branch guard from `guard` into `dst`, replacing each status-flag
/// read (a symbol in `substitute`) with a copy of the definer's expression for
/// that flag. The definer's operand symbols survive verbatim, so the composed
/// condition is a function of the definer's encoded operands alone.
fn compose_guard_with_definer(
    dst: &mut tir::sem::SemGraph,
    guard: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    substitute: &HashMap<u32, tir::graph::NodeId>,
    definer: &tir::sem::SemGraph,
    guard_memo: &mut HashMap<usize, tir::graph::NodeId>,
    definer_memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = guard_memo.get(&node.index()) {
        return copied;
    }
    if let Some(tir::sem::SymPayload::SymbolId(symbol)) = guard.get_leaf_data(node)
        && let Some(&flag_root) = substitute.get(symbol)
    {
        let copied = copy_subgraph(dst, definer, flag_root, definer_memo);
        guard_memo.insert(node.index(), copied);
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = guard.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| {
            compose_guard_with_definer(
                dst,
                guard,
                child,
                substitute,
                definer,
                guard_memo,
                definer_memo,
            )
        })
        .collect();
    let copied = dst.add_node(*guard.get_node(node));
    if let Some(data) = guard.get_leaf_data(node) {
        dst.set_leaf_data(copied, data.clone());
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    guard_memo.insert(node.index(), copied);
    copied
}

/// Operator kinds the constant folder may evaluate: pure scalar computations
/// with a defined interpreter semantics.
fn foldable_kind(kind: &tir::sem::SymKind) -> bool {
    use tir::sem::SymKind as K;
    matches!(
        kind,
        K::Add
            | K::Sub
            | K::Mul
            | K::Neg
            | K::And
            | K::Or
            | K::Xor
            | K::Not
            | K::ShiftLeft
            | K::ShiftRightLogic
            | K::ShiftRightArithmetic
            | K::ZExt
            | K::SExt
            | K::Extract
            | K::Log2Ceil
            | K::Concat
    )
}

/// Fold maximal constant subtrees into constant leaves. Width expressions like
/// `self.XLEN - 1` lower (with the concrete ISA parameter) to `Sub(64, 1)`;
/// the SMT oracle's bit-blaster needs them as literal extract bounds and
/// extension widths, so they are evaluated here with the reference interpreter.
fn fold_constant_subtrees(
    src: &tir::sem::SemGraph,
    root: tir::graph::NodeId,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    use tir::graph::{Dag, MutDag};

    // Whether every leaf under `node` is a constant and every operator foldable.
    fn all_constant(
        src: &tir::sem::SemGraph,
        node: tir::graph::NodeId,
        memo: &mut HashMap<usize, bool>,
    ) -> bool {
        if let Some(&known) = memo.get(&node.index()) {
            return known;
        }
        let result = match src.get_leaf_data(node) {
            Some(tir::sem::SymPayload::Int(_)) => true,
            Some(_) => false,
            None => {
                foldable_kind(src.get_node(node))
                    && src
                        .children(node)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .all(|child| all_constant(src, child, memo))
            }
        };
        memo.insert(node.index(), result);
        result
    }

    fn walk(
        dst: &mut tir::sem::SemGraph,
        src: &tir::sem::SemGraph,
        node: tir::graph::NodeId,
        const_memo: &mut HashMap<usize, bool>,
        copy_memo: &mut HashMap<usize, tir::graph::NodeId>,
    ) -> tir::graph::NodeId {
        if let Some(&copied) = copy_memo.get(&node.index()) {
            return copied;
        }
        let copied = if src.get_leaf_data(node).is_none() && all_constant(src, node, const_memo) {
            let mut sub = tir::sem::SemGraph::new();
            copy_subgraph(&mut sub, src, node, &mut HashMap::new());
            let tir::sem::Value::Int(value) = tir::sem::execute(&sub, &[]) else {
                // Not evaluable after all: copy verbatim.
                return copy_subgraph(dst, src, node, copy_memo);
            };
            let leaf = dst.add_node(tir::sem::SymKind::Constant);
            dst.set_leaf_data(leaf, tir::sem::SymPayload::Int(value));
            leaf
        } else {
            let children: Vec<tir::graph::NodeId> = src.children(node).collect();
            let copied_children: Vec<tir::graph::NodeId> = children
                .into_iter()
                .map(|child| walk(dst, src, child, const_memo, copy_memo))
                .collect();
            let copied = dst.add_node(*src.get_node(node));
            if let Some(data) = src.get_leaf_data(node) {
                dst.set_leaf_data(copied, data.clone());
            }
            for child in copied_children {
                dst.add_edge(copied, child);
            }
            copied
        };
        copy_memo.insert(node.index(), copied);
        copied
    }

    let mut dst = tir::sem::SemGraph::new();
    let folded_root = walk(
        &mut dst,
        src,
        root,
        &mut HashMap::new(),
        &mut HashMap::new(),
    );
    (dst, folded_root)
}

/// `kind(s0, s1)` (or swapped) — a candidate canonical comparison over the
/// definer's two operand symbols.
fn comparison_candidate(
    kind: tir::sem::SymKind,
    swap: bool,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    use tir::graph::MutDag;
    let mut g = tir::sem::SemGraph::new();
    let a = g.add_node(tir::sem::SymKind::Symbol);
    g.set_leaf_data(a, tir::sem::SymPayload::SymbolId(0));
    let b = g.add_node(tir::sem::SymKind::Symbol);
    g.set_leaf_data(b, tir::sem::SymPayload::SymbolId(1));
    let (lhs, rhs) = if swap { (b, a) } else { (a, b) };
    let root = g.add_node(kind);
    g.add_edge(root, lhs);
    g.add_edge(root, rhs);
    (g, root)
}

/// The comparison the composed flag condition is provably equivalent to, if
/// any: the six canonical predicates `cmpi` lowers to, in both operand orders.
/// A fuzz filter picks the candidate cheaply; the SMT oracle then proves it
/// (bit-blasted equivalence at the operands' architectural widths), so a wrong
/// flag formula derives no rule instead of a miscompiling one.
fn find_equivalent_comparison(
    composed: &tir::sem::SemGraph,
    symbol_widths: &[u32],
) -> Option<(tir::sem::SemGraph, tir::graph::NodeId)> {
    use tir::sem::{EquivalenceOracle, FuzzOracle, SmtOracle, SymKind};
    const CANDIDATES: &[(SymKind, bool)] = &[
        (SymKind::Eq, false),
        (SymKind::Ne, false),
        (SymKind::Lt, false),
        (SymKind::Lt, true),
        (SymKind::Ge, false),
        (SymKind::Ge, true),
        (SymKind::ULt, false),
        (SymKind::ULt, true),
        (SymKind::UGe, false),
        (SymKind::UGe, true),
    ];
    let fuzz = FuzzOracle::default();
    for (kind, swap) in CANDIDATES {
        let (candidate, root) = comparison_candidate(*kind, *swap);
        if fuzz.equivalent(composed, &candidate, symbol_widths)
            && SmtOracle.equivalent(composed, &candidate, symbol_widths)
        {
            return Some((candidate, root));
        }
    }
    None
}

/// A register class's architectural width: a literal `WIDTH`, or `WIDTH =
/// self.PARAM` resolved through the instruction's ISA parameter view.
fn register_class_width_with_isa(
    files: &[ast::File],
    class_name: &str,
    isa_param_values: &HashMap<String, i64>,
) -> Option<u32> {
    let rc = files
        .iter()
        .flat_map(|f| f.register_classes())
        .find(|rc| rc.name == class_name)?;
    match rc.parameters.get("WIDTH") {
        Some((_ty, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
            Some(parse_literal_value(li) as u32)
        }
        Some((_ty, Some(ast::Expr::Field(field)))) if matches!(&*field.base, ast::Expr::Ident(id) if id.name == "self") => {
            isa_param_values
                .get(field.member.as_str())
                .map(|v| *v as u32)
        }
        _ => None,
    }
}

/// The architectural bit-width of each of a definer's comparison-operand
/// symbols. A register operand's width comes from its class; the immediate
/// operand shares it — comparison operands are the same architectural width, so
/// the composed condition proves against a canonical comparison over full-width
/// symbols. `None` if a register width is unresolved or a symbol is untyped.
fn definer_symbol_widths(
    files: &[ast::File],
    d: &FlagInst<'_>,
    d_sem: &FlagDefinerSemantics,
) -> Option<Vec<u32>> {
    let mut widths = vec![0u32; d_sem.variable_symbols.len()];
    let mut imm_symbol: Option<u32> = None;
    let mut register_width: Option<u32> = None;
    for (op_name, op_ty) in &d.ops {
        let Some(&symbol) = d_sem.variable_symbols.get(op_name) else {
            continue;
        };
        match op_ty {
            Type::Struct(class_name) => {
                let width = register_class_width_with_isa(files, class_name, &d.isa_param_values)?;
                widths[symbol as usize] = width;
                register_width = Some(width);
            }
            Type::Bits(_) | Type::Integer => imm_symbol = Some(symbol),
            _ => {}
        }
    }
    if let Some(symbol) = imm_symbol {
        widths[symbol as usize] = register_width?;
    }
    if widths.contains(&0) {
        return None;
    }
    Some(widths)
}

/// The pattern symbols bound to a definer's immediate operands (there is at most
/// one), for canonicalization and immediate-range enforcement.
fn definer_immediate_symbols(d: &FlagInst<'_>, d_sem: &FlagDefinerSemantics) -> HashSet<u32> {
    d.ops
        .iter()
        .filter(|(_, ty)| matches!(ty, Type::Bits(_) | Type::Integer))
        .filter_map(|(name, _)| d_sem.variable_symbols.get(name).copied())
        .collect()
}

/// A flag-mediated instruction's resolved shape, shared by definer, branch and
/// reader analysis.
struct FlagInst<'a> {
    inst: &'a ast::Instruction,
    ops: Vec<(String, Type)>,
    mnemonic: String,
    isa_param_values: HashMap<String, i64>,
}

/// Emit an flag-definer prelude function (materializing the flag-setting
/// instruction ahead of its consumer) once per definer, and return its ident
/// plus the definer's operand register constraints. Shared by branch and reader
/// pair emission, deduping through `emitted_preludes`.
fn emit_flag_definer_prelude(
    d: &FlagInst<'_>,
    d_sem: &FlagDefinerSemantics,
    emitted_preludes: &mut HashSet<String>,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
) -> (proc_macro2::Ident, Vec<proc_macro2::TokenStream>) {
    let prelude_fn_ident = format_ident!("emit_isel_flag_definer_{}", d.inst.name.to_lowercase());
    let d_builder_ident = format_ident!("{}OpBuilder", &d.inst.name);

    let mut operand_constraint_entries: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut prelude_attr_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (op_name, op_ty) in &d.ops {
        let Some(&symbol) = d_sem.variable_symbols.get(op_name) else {
            continue;
        };
        let op_name_lit = proc_macro2::Literal::string(op_name);
        let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
        match op_ty {
            Type::Struct(class_name) => {
                let class_id = reg_class_id(class_name);
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Register) });
                prelude_attr_steps.push(quote! {
                    let src = m
                        .value_binding(#symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
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
            }
            Type::Bits(_) | Type::Integer => {
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Immediate) });
                prelude_attr_steps.push(quote! {
                    let v = m
                        .int_binding(#symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                    builder = builder.attr(
                        #op_name_lit,
                        tir::attributes::AttributeValue::Int(v),
                    );
                });
            }
            _ => continue,
        }
    }

    if emitted_preludes.insert(d.inst.name.clone()) {
        isel_rule_emitters.push(quote! {
            fn #prelude_fn_ident(
                context: &tir::Context,
                req: &tir::backend::isel::EmitRequest,
                m: &tir::backend::isel::RuleMatch,
            ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                let _ = (req, m);
                let mut builder = #d_builder_ident::new(context);
                #(#prelude_attr_steps)*
                Ok(Box::new(builder.build()))
            }
        });
    }

    (prelude_fn_ident, operand_constraint_entries)
}

/// Derive the flag-mediated selection rules for an ISA (x86 EFLAGS, AArch64
/// PSTATE): flag definers compose with flag-guarded branches into conditional
/// branch rules and with flag-reading materializers into boolean value rules.
fn emit_flag_rules<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    register_index_map: &HashMap<(String, String), u32>,
    pc_classes: &HashSet<String>,
    flag_classes: &HashSet<String>,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    isel_rule_inits: &mut Vec<proc_macro2::TokenStream>,
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
        isel_rule_emitters,
        isel_rule_inits,
    );
    emit_flag_reader_rules(
        files,
        &definers,
        &readers,
        &mut emitted_preludes,
        isel_rule_emitters,
        isel_rule_inits,
    );
    emit_aliased_zero_branch_rules(
        files,
        &definers,
        &branches,
        isel_rule_emitters,
        isel_rule_inits,
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
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    isel_rule_inits: &mut Vec<proc_macro2::TokenStream>,
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
            let Some(symbol_widths) = definer_symbol_widths(files, d, d_sem) else {
                continue;
            };

            let mut spliced = tir::sem::SemGraph::new();
            let substitute: HashMap<u32, tir::graph::NodeId> = b_sem
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
                find_equivalent_comparison(&composed, &symbol_widths)
            else {
                continue;
            };

            let immediate_symbols = definer_immediate_symbols(d, d_sem);
            let (canon_pattern, canon_root, forced_widths) = tir::sem::canonicalize_for_selection(
                &candidate,
                candidate_root,
                &immediate_symbols,
            );
            let mut pattern_widths = tir::sem::infer_widths(&canon_pattern, |_| None);
            for (index, forced) in forced_widths.iter().enumerate() {
                if forced.is_some() {
                    pattern_widths[index] = *forced;
                }
            }
            let (pattern_stmts, _root_var) =
                emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);
            let operand_register_call = emit_operand_register_call(
                &d.ops,
                &d_sem.variable_symbols,
                &width_sensitive_symbols(&canon_pattern, &pattern_widths),
                &float_classes,
                &polymorphic_classes,
            );
            let operand_imm_range_call = emit_operand_imm_range_call(&immediate_operand_ranges(
                &d_sem.graph,
                &d.ops,
                &d_sem.variable_symbols,
            ));

            let target_symbol = d_sem
                .variable_symbols
                .values()
                .max()
                .map_or(0, |max| max + 1);
            let d_lower = d.inst.name.to_lowercase();
            let b_lower = b.inst.name.to_lowercase();
            let pattern_fn_ident = format_ident!("isel_pattern_{}_via_{}", b_lower, d_lower);
            let emit_fn_ident = format_ident!("emit_isel_{}_via_{}", b_lower, d_lower);
            let rule_name_lit =
                proc_macro2::Literal::string(&format!("{}+{}", d.mnemonic, b.mnemonic));
            let target_symbol_lit = proc_macro2::Literal::u32_unsuffixed(target_symbol);
            let b_builder_ident = format_ident!("{}OpBuilder", &b.inst.name);
            let target_name_lit = proc_macro2::Literal::string(&b_sem.target_operand);

            let (prelude_fn_ident, operand_constraint_entries) =
                emit_flag_definer_prelude(d, d_sem, emitted_preludes, isel_rule_emitters);

            let base_cost = {
                use tir::graph::Dag;
                // The condition pattern plus the two emitted instructions (the
                // definer and the branch): a fused compare-and-branch is never
                // cheaper than a single-instruction direct branch (e.g. arm64
                // `cbz`) that covers the same guard.
                canon_pattern.len() as u32 + 2
            };
            let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
            let d_mnemonic_lit = proc_macro2::Literal::string(&d.mnemonic);
            let b_mnemonic_lit = proc_macro2::Literal::string(&b.mnemonic);

            isel_rule_emitters.push(quote! {
                fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
                    use tir::graph::MutDag;
                    let mut g = tir::sem::SemGraph::new();
                    #(#pattern_stmts)*
                    g
                }

                fn #emit_fn_ident(
                    context: &tir::Context,
                    req: &tir::backend::isel::EmitRequest,
                    m: &tir::backend::isel::RuleMatch,
                ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                    let mut builder = #b_builder_ident::new(context);
                    let dest = m
                        .block_binding(#target_symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                    builder = builder.attr(
                        #target_name_lit,
                        tir::attributes::AttributeValue::Block(dest),
                    );
                    Ok(Box::new(builder.build()))
                }
            });

            let pair_features = feature_slice(&shared_isas);
            isel_rule_inits.push(quote! {
                if features_enabled(features, #pair_features) {
                    rules.push(
                        tir::backend::isel::Rule::new(
                            #rule_name_lit,
                            #pattern_fn_ident(context),
                            // Structural proxy or the TMDL-modeled cost of the
                            // two emitted instructions, whichever is larger.
                            (#base_cost_lit).max(
                                instruction_cost(#d_mnemonic_lit)
                                    + instruction_cost(#b_mnemonic_lit),
                            ),
                            #emit_fn_ident,
                        )
                        .with_kind(tir::backend::isel::RuleKind::CondBranch {
                            target_symbol: #target_symbol_lit,
                        })
                        .with_prelude_emitter(#prelude_fn_ident)
                        .with_operand_constraints(vec![#(#operand_constraint_entries),*])
                        #operand_register_call
                        #operand_imm_range_call,
                    );
                }
            });
        }
    }
}

/// Copy `node`'s subgraph into `dst`, rewriting every symbol id found in `map`
/// to its mapped id. Unlike `copy_subgraph_remap_symbols` the mapping is fixed
/// (not fresh-per-symbol), so two operand symbols can be aliased onto one.
fn copy_subgraph_alias(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    map: &HashMap<u32, u32>,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = src.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| copy_subgraph_alias(dst, src, child, map, memo))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(data) = src.get_leaf_data(node) {
        let data = match data {
            tir::sem::SymPayload::SymbolId(id) if map.contains_key(id) => {
                tir::sem::SymPayload::SymbolId(map[id])
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
    kind: tir::sem::SymKind,
    width: u32,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    use tir::graph::MutDag;
    let mut g = tir::sem::SemGraph::new();
    let s = g.add_node(tir::sem::SymKind::Symbol);
    g.set_leaf_data(s, tir::sem::SymPayload::SymbolId(0));
    let z = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(z, tir::sem::int_payload(width, 0, false));
    let root = g.add_node(kind);
    g.add_edge(root, s);
    g.add_edge(root, z);
    (g, root)
}

/// The `Eq`/`Ne`-vs-zero comparison the composed aliased condition is provably
/// equivalent to, proven at the operand's architectural width.
fn zero_equivalent(
    composed: &tir::sem::SemGraph,
    symbol_widths: &[u32],
) -> Option<tir::sem::SymKind> {
    use tir::sem::{EquivalenceOracle, FuzzOracle, SmtOracle, SymKind};
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
    kind: tir::sem::SymKind,
    width_symbol: u32,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    use tir::graph::MutDag;
    let mut g = tir::sem::SemGraph::new();
    let s = g.add_node(tir::sem::SymKind::Symbol);
    g.set_leaf_data(s, tir::sem::SymPayload::SymbolId(0));
    let zero = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(zero, tir::sem::int_payload(1, 0, false));
    let wsym = g.add_node(tir::sem::SymKind::Symbol);
    g.set_leaf_data(wsym, tir::sem::SymPayload::SymbolId(width_symbol));
    let zext = g.add_node(tir::sem::SymKind::ZExt);
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
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    isel_rule_inits: &mut Vec<proc_macro2::TokenStream>,
) {
    use tir::graph::Dag;
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
            let mut aliased_graph = tir::sem::SemGraph::new();
            let mut alias_memo: HashMap<usize, tir::graph::NodeId> = HashMap::new();
            let aliased_roots: HashMap<u32, tir::graph::NodeId> = d_sem
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

            let mut spliced = tir::sem::SemGraph::new();
            let substitute: HashMap<u32, tir::graph::NodeId> = b_sem
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
                tir::sem::canonicalize_for_selection(&pattern, root, &no_immediates);
            let mut pattern_widths = tir::sem::infer_widths(&canon_pattern, |_| None);
            for (index, forced) in forced_widths.iter().enumerate() {
                if forced.is_some() {
                    pattern_widths[index] = *forced;
                }
            }
            let (pattern_stmts, _root_var) =
                emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);

            let prelude_fn_ident = format_ident!(
                "emit_isel_flag_definer_{}_aliased",
                d.inst.name.to_lowercase()
            );
            let d_builder_ident = format_ident!("{}OpBuilder", &d.inst.name);
            let class_id = reg_class_id(class_a);
            let name_a_lit = proc_macro2::Literal::string(name_a);
            let name_b_lit = proc_macro2::Literal::string(name_b);
            if emitted_preludes.insert(d.inst.name.clone()) {
                isel_rule_emitters.push(quote! {
                    fn #prelude_fn_ident(
                        context: &tir::Context,
                        req: &tir::backend::isel::EmitRequest,
                        m: &tir::backend::isel::RuleMatch,
                    ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                        let src = m
                            .value_binding(0)
                            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                        let reg = tir::attributes::AttributeValue::Register(
                            tir::attributes::RegisterAttr::Virtual {
                                id: src.number(),
                                class: Some(#class_id),
                            },
                        );
                        let builder = #d_builder_ident::new(context)
                            .attr(#name_a_lit, reg.clone())
                            .attr(#name_b_lit, reg);
                        Ok(Box::new(builder.build()))
                    }
                });
            }

            let target_symbol = 2u32;
            let target_symbol_lit = proc_macro2::Literal::u32_unsuffixed(target_symbol);
            let b_builder_ident = format_ident!("{}OpBuilder", &b.inst.name);
            let target_name_lit = proc_macro2::Literal::string(&b_sem.target_operand);
            let b_lower = b.inst.name.to_lowercase();
            let d_lower = d.inst.name.to_lowercase();
            let pattern_fn_ident =
                format_ident!("isel_pattern_{}_via_{}_selfzero", b_lower, d_lower);
            let emit_fn_ident = format_ident!("emit_isel_{}_via_{}_selfzero", b_lower, d_lower);
            let rule_name_lit =
                proc_macro2::Literal::string(&format!("{}+{}(self-zero)", d.mnemonic, b.mnemonic));
            let base_cost = canon_pattern.len() as u32 + 2;
            let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
            let d_mnemonic_lit = proc_macro2::Literal::string(&d.mnemonic);
            let b_mnemonic_lit = proc_macro2::Literal::string(&b.mnemonic);
            let pair_features = feature_slice(&shared_isas);

            isel_rule_emitters.push(quote! {
                fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
                    use tir::graph::MutDag;
                    let mut g = tir::sem::SemGraph::new();
                    #(#pattern_stmts)*
                    g
                }

                fn #emit_fn_ident(
                    context: &tir::Context,
                    req: &tir::backend::isel::EmitRequest,
                    m: &tir::backend::isel::RuleMatch,
                ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                    let mut builder = #b_builder_ident::new(context);
                    let dest = m
                        .block_binding(#target_symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                    builder = builder.attr(
                        #target_name_lit,
                        tir::attributes::AttributeValue::Block(dest),
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
                                instruction_cost(#d_mnemonic_lit)
                                    + instruction_cost(#b_mnemonic_lit),
                            ),
                            #emit_fn_ident,
                        )
                        .with_kind(tir::backend::isel::RuleKind::CondBranch {
                            target_symbol: #target_symbol_lit,
                        })
                        .with_prelude_emitter(#prelude_fn_ident)
                        .with_operand_constraints(vec![
                            (0, tir::graph::OperandConstraint::Register)
                        ]),
                    );
                }
            });
        }
    }
}

/// Compose each flag definer with each flag-reading materializer into a boolean
/// value rule: the definer's per-flag semantics substitute into the reader's
/// condition, and when the composition is provably one canonical comparison the
/// pair registers an `If`-rooted value rule whose prelude emits the definer and
/// whose emitter is the reader (`cset`/`setcc`), materializing the comparison in
/// a destination register — the value analog of the flag-branch rules.
/// Each ISA's transitive `requires` set. An instruction tagged with ISA `a`
/// where `requires[a]` contains `b` is available wherever `b` is (e.g. `X86`
/// requires `X86_64`), so two such instructions can co-occur even without a
/// shared tag — needed to pair an `[X86]` `setcc` with an `[X86_64]` `cmp`.
fn isa_requires_closure(files: &[ast::File]) -> HashMap<String, HashSet<String>> {
    let mut closure: HashMap<String, HashSet<String>> = HashMap::new();
    for isa in files.iter().flat_map(|f| f.isas()) {
        let direct = match &isa.requires {
            Some(ast::IsaRequirement::Single(s)) => vec![s.clone()],
            // `All` is a conjunction: every listed ISA is guaranteed present.
            Some(ast::IsaRequirement::All(v)) => v.clone(),
            // A single-element `Any` (`requires [X86_64]`) is an exact
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
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
    isel_rule_inits: &mut Vec<proc_macro2::TokenStream>,
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
    use tir::graph::{Dag, MutDag};
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
            let Some(symbol_widths) = definer_symbol_widths(files, d, d_sem) else {
                continue;
            };

            let mut spliced = tir::sem::SemGraph::new();
            let substitute: HashMap<u32, tir::graph::NodeId> = r_sem
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
                find_equivalent_comparison(&composed, &symbol_widths)
            else {
                continue;
            };

            // The value pattern is `if <canonical comparison> { <then> } else {
            // <else> }`, reusing the reader's arms so it is structurally the
            // `slt`-style materializer the bool-materialize bridge knows. The
            // arms' symbols (the `XLEN` width var) renumber above the two
            // comparison-operand symbols they now sit beside.
            let mut pattern = tir::sem::SemGraph::new();
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
            let if_root = pattern.add_node(tir::sem::SymKind::If);
            pattern.add_edge(if_root, cmp);
            pattern.add_edge(if_root, then_);
            pattern.add_edge(if_root, else_);

            let immediate_symbols = definer_immediate_symbols(d, d_sem);
            let (canon_pattern, canon_root, forced_widths) =
                tir::sem::canonicalize_for_selection(&pattern, if_root, &immediate_symbols);
            let mut pattern_widths = tir::sem::infer_widths(&canon_pattern, |_| None);
            for (index, forced) in forced_widths.iter().enumerate() {
                if forced.is_some() {
                    pattern_widths[index] = *forced;
                }
            }
            let (pattern_stmts, _root_var) =
                emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);
            let operand_register_call = emit_operand_register_call(
                &d.ops,
                &d_sem.variable_symbols,
                &width_sensitive_symbols(&canon_pattern, &pattern_widths),
                &float_classes,
                &polymorphic_classes,
            );
            let operand_imm_range_call = emit_operand_imm_range_call(&immediate_operand_ranges(
                &d_sem.graph,
                &d.ops,
                &d_sem.variable_symbols,
            ));

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
            let dest_name_lit = proc_macro2::Literal::string(&r_sem.dest_operand);

            let r_lower = r.inst.name.to_lowercase();
            let d_lower = d.inst.name.to_lowercase();
            let pattern_fn_ident = format_ident!("isel_pattern_{}_via_{}", r_lower, d_lower);
            let emit_fn_ident = format_ident!("emit_isel_{}_via_{}", r_lower, d_lower);
            let rule_name_lit =
                proc_macro2::Literal::string(&format!("{}+{}", d.mnemonic, r.mnemonic));
            let r_builder_ident = format_ident!("{}OpBuilder", &r.inst.name);

            let (prelude_fn_ident, operand_constraint_entries) =
                emit_flag_definer_prelude(d, d_sem, emitted_preludes, isel_rule_emitters);

            let base_cost = {
                // The comparison pattern plus the definer instruction.
                canon_pattern.len() as u32 + 1
            };
            let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
            let d_mnemonic_lit = proc_macro2::Literal::string(&d.mnemonic);
            let r_mnemonic_lit = proc_macro2::Literal::string(&r.mnemonic);

            isel_rule_emitters.push(quote! {
                fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
                    use tir::graph::MutDag;
                    let mut g = tir::sem::SemGraph::new();
                    #(#pattern_stmts)*
                    g
                }

                fn #emit_fn_ident(
                    context: &tir::Context,
                    req: &tir::backend::isel::EmitRequest,
                    m: &tir::backend::isel::RuleMatch,
                ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                    let _ = m;
                    let mut builder = #r_builder_ident::new(context);
                    let dst = req
                        .results
                        .first()
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?
                        .number();
                    builder = builder.attr(
                        #dest_name_lit,
                        tir::attributes::AttributeValue::Register(
                            tir::attributes::RegisterAttr::Virtual {
                                id: dst,
                                class: Some(#dest_class_id),
                            },
                        ),
                    );
                    Ok(Box::new(builder.build()))
                }
            });

            let pair_features = feature_slice(&shared_isas);
            isel_rule_inits.push(quote! {
                if features_enabled(features, #pair_features) {
                    rules.push(
                        tir::backend::isel::Rule::new(
                            #rule_name_lit,
                            #pattern_fn_ident(context),
                            // Structural proxy or the TMDL-modeled cost of the
                            // two emitted instructions, whichever is larger.
                            (#base_cost_lit).max(
                                instruction_cost(#d_mnemonic_lit)
                                    + instruction_cost(#r_mnemonic_lit),
                            ),
                            #emit_fn_ident,
                        )
                        .with_prelude_emitter(#prelude_fn_ident)
                        .with_operand_constraints(vec![#(#operand_constraint_entries),*])
                        #operand_register_call
                        #operand_imm_range_call,
                    );
                }
            });
        }
    }
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
    let mut pattern = tir::sem::SemGraph::new();
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

/// The boundary symbols an instruction is width-sensitive in: the operands'
/// upper register bits reach the result, so a value of a different width must
/// not bind (its bits above the value width are undefined). Comparison
/// operands always qualify — the comparison node's own type is its i1 result
/// and says nothing about operand widths. Right-shift values and
/// division/remainder operands qualify only under an *untyped* node: a typed
/// node (a word form like `sraw`) already pins its operands through width
/// inference. Low-bits-preserving operators (add/and/shl/mul low half) are
/// exempt: a narrower value's upper garbage never reaches its own low bits.
fn width_sensitive_symbols(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    node_widths: &[Option<u32>],
) -> HashSet<u32> {
    use tir::sem::SymKind as K;

    let mut out = HashSet::new();
    for index in 0..dag.len() {
        let node = tir::graph::NodeId::from_index(index);
        let untyped = node_widths.get(index).copied().flatten().is_none();
        let sensitive_children: &[usize] = match dag.get_node(node) {
            K::Eq | K::Ne | K::Lt | K::Le | K::Gt | K::Ge | K::ULt | K::ULe | K::UGt | K::UGe => {
                &[0, 1]
            }
            K::Div | K::UDiv | K::SRem | K::URem if untyped => &[0, 1],
            K::ShiftRightLogic | K::ShiftRightArithmetic if untyped => &[0],
            _ => &[],
        };
        let children: Vec<tir::graph::NodeId> = dag.children(node).collect();
        for &slot in sensitive_children {
            if let Some(child) = children.get(slot)
                && let Some(tir::sem::SymPayload::SymbolId(symbol)) = dag.get_leaf_data(*child)
            {
                out.insert(*symbol);
            }
        }
    }
    out
}

/// Emit each register operand's storage domain and whether its instruction
/// consumes the full architectural width.
fn emit_operand_register_call(
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
    sensitive_symbols: &HashSet<u32>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> proc_macro2::TokenStream {
    let register_steps: Vec<proc_macro2::TokenStream> = ops
        .iter()
        .filter_map(|(op_name, op_ty)| {
            let Type::Struct(class_name) = op_ty else {
                return None;
            };
            let &symbol = variable_symbols.get(op_name)?;
            let class_lit = proc_macro2::Literal::string(class_name);
            let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
            let capability = if polymorphic_classes.contains(class_name) {
                quote! { tir::backend::isel::RegisterCapability::any(*width) }
            } else if float_classes.contains(class_name) {
                quote! { tir::backend::isel::RegisterCapability::float(*width) }
            } else {
                quote! { tir::backend::isel::RegisterCapability::integer(*width) }
            };
            let requirement = if sensitive_symbols.contains(&symbol) {
                quote! { tir::backend::isel::RegisterRequirement::whole(#capability) }
            } else {
                quote! { tir::backend::isel::RegisterRequirement::low_bits(#capability) }
            };
            Some(quote! {
                if let Some((_, width)) =
                    __register_widths.iter().find(|(class, _)| *class == #class_lit)
                {
                    __operand_registers.push((#symbol_lit, #requirement));
                }
            })
        })
        .collect();

    if register_steps.is_empty() {
        return quote! {};
    }
    quote! {
        .with_operand_registers({
            let mut __operand_registers = Vec::new();
            #(#register_steps)*
            __operand_registers
        })
    }
}

fn emit_result_register_call(
    class_name: Option<&str>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> proc_macro2::TokenStream {
    let Some(class_name) = class_name else {
        return quote! {};
    };
    let class_lit = proc_macro2::Literal::string(class_name);
    let capability = if polymorphic_classes.contains(class_name) {
        quote! { tir::backend::isel::RegisterCapability::any(*width) }
    } else if float_classes.contains(class_name) {
        quote! { tir::backend::isel::RegisterCapability::float(*width) }
    } else {
        quote! { tir::backend::isel::RegisterCapability::integer(*width) }
    };
    quote! {
        .with_optional_result_register(
            __register_widths
                .iter()
                .find(|(class, _)| *class == #class_lit)
                .map(|(_, width)| tir::backend::isel::RegisterRequirement::low_bits(#capability))
        )
    }
}

/// The encoding range of each immediate operand: the field's bit width from the
/// operand type, signedness from how the behavior consumes the symbol —
/// `sext(imm, _)` sign-extends, everything else is unsigned — and an
/// `extract(imm, hi, 0)` wrapper (a shift-amount mask) narrows the usable bits.
/// Selection uses these to refuse constants the field cannot represent.
fn immediate_operand_ranges(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
) -> Vec<(u32, u32, bool)> {
    use tir::sem::{SymKind as K, SymPayload};

    let is_symbol_leaf = |node: tir::graph::NodeId, symbol: u32| {
        *dag.get_node(node) == K::Symbol
            && matches!(
                dag.get_leaf_data(node),
                Some(SymPayload::SymbolId(id)) if *id == symbol
            )
    };
    let const_value = |node: tir::graph::NodeId| match dag.get_leaf_data(node) {
        Some(SymPayload::Int(v)) => Some(v.to_u64()),
        _ => None,
    };

    let mut out = Vec::new();
    for (op_name, op_ty) in ops {
        let Type::Bits(bits) = op_ty else { continue };
        let Some(&symbol) = variable_symbols.get(op_name) else {
            continue;
        };
        let mut signed = false;
        let mut width = u32::from(*bits);
        for index in 0..dag.len() {
            let node = tir::graph::NodeId::from_index(index);
            let children: Vec<tir::graph::NodeId> = dag.children(node).collect();
            let uses_symbol = children
                .first()
                .is_some_and(|&child| is_symbol_leaf(child, symbol));
            if !uses_symbol {
                continue;
            }
            match dag.get_node(node) {
                K::SExt => signed = true,
                K::Extract
                    if children.len() == 3
                        && children.get(2).and_then(|&c| const_value(c)) == Some(0) =>
                {
                    if let Some(hi) = children.get(1).and_then(|&c| const_value(c)) {
                        width = width.min(hi as u32 + 1);
                    }
                }
                _ => {}
            }
        }
        out.push((symbol, width, signed));
    }
    out
}

/// Emit the `.with_operand_imm_ranges` builder call for the immediate operands'
/// encoding ranges.
fn emit_operand_imm_range_call(ranges: &[(u32, u32, bool)]) -> proc_macro2::TokenStream {
    if ranges.is_empty() {
        return quote! {};
    }
    let entries: Vec<proc_macro2::TokenStream> = ranges
        .iter()
        .map(|(symbol, width, signed)| {
            let symbol_lit = proc_macro2::Literal::u32_unsuffixed(*symbol);
            let width_lit = proc_macro2::Literal::u32_unsuffixed(*width);
            quote! {
                (#symbol_lit, tir::backend::isel::ImmRange { width: #width_lit, signed: #signed })
            }
        })
        .collect();
    quote! { .with_operand_imm_ranges(vec![#(#entries),*]) }
}

/// The literal architectural width of a register class, when its `WIDTH` param
/// is a compile-time literal (x86 `GPR32`/`GPR16`/`GPR8`). A class sized by an
/// ISA parameter (`self.XLEN`) resolves only under the enabled features and
/// yields `None`.
fn literal_register_class_width(files: &[ast::File], class_name: &str) -> Option<u32> {
    files
        .iter()
        .flat_map(|f| f.register_classes())
        .find(|rc| rc.name == class_name)?
        .parameters
        .get("WIDTH")
        .and_then(|(_ty, value)| match value {
            Some(ast::Expr::Lit(ast::Lit::Int(li))) => Some(parse_literal_value(li) as u32),
            _ => None,
        })
}

/// Operator kinds whose result is meaningfully sized by the destination register
/// width — scalar integer and float computations. Vector, memory, and control
/// kinds carry no scalar width and are never typed from a register class.
fn scalar_root_kind(kind: &tir::sem::SymKind) -> bool {
    use tir::sem::SymKind as K;
    matches!(
        kind,
        K::Add
            | K::Sub
            | K::Mul
            | K::Div
            | K::UDiv
            | K::SRem
            | K::URem
            | K::Neg
            | K::And
            | K::Or
            | K::Xor
            | K::Not
            | K::ShiftLeft
            | K::ShiftRightLogic
            | K::ShiftRightArithmetic
            | K::FAdd
            | K::FSub
            | K::FMul
            | K::FDiv
    )
}

/// Whether `expr` reads or writes a program-counter register (`PC::pc`).
fn behavior_references_pc(expr: &ast::Expr, pc_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Path(path) => pc_classes.contains(&path.base),
        ast::Expr::Ident(_) | ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) => false,
        ast::Expr::Invalid => false,
        ast::Expr::Assign(a) => {
            behavior_references_pc(&a.dest, pc_classes)
                || behavior_references_pc(&a.value, pc_classes)
        }
        ast::Expr::Binary(b) => {
            behavior_references_pc(&b.lhs, pc_classes) || behavior_references_pc(&b.rhs, pc_classes)
        }
        ast::Expr::Unary(u) => behavior_references_pc(&u.x, pc_classes),
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| behavior_references_pc(stmt, pc_classes)),
        ast::Expr::Call(c) => {
            behavior_references_pc(&c.callee, pc_classes)
                || c.arguments
                    .iter()
                    .any(|arg| behavior_references_pc(arg, pc_classes))
        }
        ast::Expr::Field(f) => behavior_references_pc(&f.base, pc_classes),
        ast::Expr::If(i) => {
            behavior_references_pc(&i.cond, pc_classes)
                || behavior_references_pc(&i.then, pc_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| behavior_references_pc(e, pc_classes))
        }
        ast::Expr::IndexAccess(i) => behavior_references_pc(&i.base, pc_classes),
        ast::Expr::Slice(s) => behavior_references_pc(&s.base, pc_classes),
        ast::Expr::Try(t) => {
            behavior_references_pc(&t.body, pc_classes)
                || t.handlers
                    .iter()
                    .any(|h| behavior_references_pc(&h.body, pc_classes))
        }
        ast::Expr::Lambda(l) => behavior_references_pc(&l.body, pc_classes),
    }
}

/// Whether a behavior *reads* a status-flag register (a `flag_classes` register
/// path in a value position). Such readers (`cset`, `csel`) compute from
/// condition-code bits a plain value rule cannot see: lifting the flag reads
/// into free symbolic operands yields a pattern structurally identical to an
/// integer comparison, so it would match `cmpi` and drop the operand bindings.
/// They instead materialize through composed definer+reader rules (see
/// `emit_flag_reader_rules`). A flag-path assignment *destination* is a write,
/// not a read, so definers (`cmp`) are not caught.
fn behavior_reads_flag_register(expr: &ast::Expr, flag_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Path(path) => flag_classes.contains(&path.base),
        ast::Expr::Ident(_) | ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) => false,
        ast::Expr::Invalid => false,
        ast::Expr::Assign(a) => {
            let dest_is_flag_write =
                matches!(&*a.dest, ast::Expr::Path(p) if flag_classes.contains(&p.base));
            behavior_reads_flag_register(&a.value, flag_classes)
                || (!dest_is_flag_write && behavior_reads_flag_register(&a.dest, flag_classes))
        }
        ast::Expr::Binary(b) => {
            behavior_reads_flag_register(&b.lhs, flag_classes)
                || behavior_reads_flag_register(&b.rhs, flag_classes)
        }
        ast::Expr::Unary(u) => behavior_reads_flag_register(&u.x, flag_classes),
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| behavior_reads_flag_register(stmt, flag_classes)),
        ast::Expr::Call(c) => {
            behavior_reads_flag_register(&c.callee, flag_classes)
                || c.arguments
                    .iter()
                    .any(|arg| behavior_reads_flag_register(arg, flag_classes))
        }
        ast::Expr::Field(f) => behavior_reads_flag_register(&f.base, flag_classes),
        ast::Expr::If(i) => {
            behavior_reads_flag_register(&i.cond, flag_classes)
                || behavior_reads_flag_register(&i.then, flag_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| behavior_reads_flag_register(e, flag_classes))
        }
        ast::Expr::IndexAccess(i) => behavior_reads_flag_register(&i.base, flag_classes),
        ast::Expr::Slice(s) => behavior_reads_flag_register(&s.base, flag_classes),
        ast::Expr::Try(t) => {
            behavior_reads_flag_register(&t.body, flag_classes)
                || t.handlers
                    .iter()
                    .any(|h| behavior_reads_flag_register(&h.body, flag_classes))
        }
        ast::Expr::Lambda(l) => behavior_reads_flag_register(&l.body, flag_classes),
    }
}

/// Whether a behavior invokes the `todo()` builtin anywhere: its semantics are
/// unmodeled, so it generates no selection rules and its `execute()` traps.
fn behavior_uses_todo(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::BuiltinFunction(ast::BuiltinFunction::Todo) => true,
        ast::Expr::Ident(_) | ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) => false,
        ast::Expr::Path(_) | ast::Expr::Invalid => false,
        ast::Expr::Assign(a) => behavior_uses_todo(&a.dest) || behavior_uses_todo(&a.value),
        ast::Expr::Binary(b) => behavior_uses_todo(&b.lhs) || behavior_uses_todo(&b.rhs),
        ast::Expr::Unary(u) => behavior_uses_todo(&u.x),
        ast::Expr::Block(b) => b.stmts.iter().any(behavior_uses_todo),
        ast::Expr::Call(c) => {
            behavior_uses_todo(&c.callee) || c.arguments.iter().any(behavior_uses_todo)
        }
        ast::Expr::Field(f) => behavior_uses_todo(&f.base),
        ast::Expr::If(i) => {
            behavior_uses_todo(&i.cond)
                || behavior_uses_todo(&i.then)
                || i.else_.as_ref().is_some_and(|e| behavior_uses_todo(e))
        }
        ast::Expr::IndexAccess(i) => behavior_uses_todo(&i.base),
        ast::Expr::Slice(s) => behavior_uses_todo(&s.base),
        ast::Expr::Try(t) => {
            behavior_uses_todo(&t.body) || t.handlers.iter().any(|h| behavior_uses_todo(&h.body))
        }
        ast::Expr::Lambda(l) => behavior_uses_todo(&l.body),
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
        _ => {}
    }
}

/// Register operands the behavior *reads*: referenced anywhere outside an
/// assignment-destination position. An operand that is also defined is a tied
/// (two-address) operand, e.g. the x86 `dst = dst + src`.
fn infer_read_register_operands(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
) -> HashSet<String> {
    fn walk(expr: &ast::Expr, operands: &HashSet<&str>, out: &mut Vec<String>) {
        if let ast::Expr::Assign(a) = expr {
            // A plain identifier/path destination is a pure write; any other
            // destination form (e.g. a slice, a partial update) reads its base.
            if assignment_dest_name(&a.dest).is_none() {
                collect_referenced_idents(&a.dest, operands, out);
            }
            walk(&a.value, operands, out);
            return;
        }
        if let ast::Expr::Block(b) = expr {
            for stmt in &b.stmts {
                walk(stmt, operands, out);
            }
            return;
        }
        if let ast::Expr::If(i) = expr {
            collect_referenced_idents(&i.cond, operands, out);
            walk(&i.then, operands, out);
            if let Some(e) = &i.else_ {
                walk(e, operands, out);
            }
            return;
        }
        if let ast::Expr::Try(t) = expr {
            walk(&t.body, operands, out);
            return;
        }
        collect_referenced_idents(expr, operands, out);
    }

    let register_operands = register_operand_names(operands);
    let mut reads = Vec::new();
    walk(behavior, &register_operands, &mut reads);
    reads.into_iter().collect()
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
    Plus,
    /// A literal identifier in the template (e.g. the condition in
    /// Literal identifier in the template (e.g. `eq` in `cset {rd}, eq` or
    /// `sp` in `c.addi4spn {rd}, sp, {imm}`); the parser requires it verbatim.
    Keyword(String),
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
            '+' => {
                actions.push(AsmAction::Plus);
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] as char == '_')
                {
                    i += 1;
                }
                actions.push(AsmAction::Keyword(template[start..i].to_string()));
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
        _ => (false, false),
    }
}

fn emit_as_sem_expr_impl(
    rhs: &ast::Expr,
    name_ident: &proc_macro2::Ident,
    numeric_params: &HashMap<String, i64>,
) -> Option<proc_macro2::TokenStream> {
    let mut dag = tir::sem::SemGraph::new();
    let lowering = rhs.lower_to_sema(&mut dag, numeric_params)?;
    // The AsSemExpr impl carries no type annotations (the program-graph builder
    // infers them), so pass no widths.
    let (stmts, root_var) = emit_dag_as_code(&dag, lowering.root, &[]);

    Some(quote! {
        impl tir::sem::AsSemExpr for #name_ident {
            fn convert(
                &self,
                g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
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

/// Whether the behavior contains any atomic/fence builtin. Such behaviors are
/// excluded from instruction selection and op-sem pattern generation.
fn behavior_has_atomic_ops(expr: &ast::Expr) -> bool {
    let is_atomic = |e: &ast::Expr| {
        matches!(e, ast::Expr::Call(ast::Call { callee, .. }) if matches!(
            callee.as_ref(),
            ast::Expr::BuiltinFunction(
                ast::BuiltinFunction::LoadReserved
                    | ast::BuiltinFunction::StoreConditional
                    | ast::BuiltinFunction::AtomicRmw
                    | ast::BuiltinFunction::Fence
                    | ast::BuiltinFunction::FenceI
            )
        ))
    };
    if is_atomic(expr) {
        return true;
    }
    match expr {
        ast::Expr::Assign(a) => {
            behavior_has_atomic_ops(&a.dest) || behavior_has_atomic_ops(&a.value)
        }
        ast::Expr::Binary(b) => behavior_has_atomic_ops(&b.lhs) || behavior_has_atomic_ops(&b.rhs),
        ast::Expr::Unary(u) => behavior_has_atomic_ops(&u.x),
        ast::Expr::Block(b) => b.stmts.iter().any(behavior_has_atomic_ops),
        ast::Expr::Call(c) => c.arguments.iter().any(behavior_has_atomic_ops),
        ast::Expr::Field(f) => behavior_has_atomic_ops(&f.base),
        ast::Expr::If(i) => {
            behavior_has_atomic_ops(&i.cond)
                || behavior_has_atomic_ops(&i.then)
                || i.else_.as_ref().is_some_and(|e| behavior_has_atomic_ops(e))
        }
        ast::Expr::IndexAccess(i) => behavior_has_atomic_ops(&i.base),
        ast::Expr::Slice(s) => behavior_has_atomic_ops(&s.base),
        ast::Expr::Try(t) => {
            behavior_has_atomic_ops(&t.body)
                || t.handlers.iter().any(|h| behavior_has_atomic_ops(&h.body))
        }
        ast::Expr::Lambda(l) => behavior_has_atomic_ops(&l.body),
        ast::Expr::Ident(_)
        | ast::Expr::Lit(_)
        | ast::Expr::Path(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Invalid => false,
    }
}

fn emit_behavior_exec(
    expr: &ast::Expr,
    trap_handler: Option<&ast::TrapHandler>,
    numeric_params: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    ctx: &RustBehaviorCtx<'_>,
) -> Option<proc_macro2::TokenStream> {
    let behavior = sem_expr_state::lower_behavior(
        expr,
        trap_handler,
        numeric_params,
        ctx.isa_param_values,
        register_index_map,
    )?;
    let (max_sym_id, sym_inits) = emit_sym_inits(
        &behavior.variable_symbols,
        &behavior.register_symbols,
        &behavior.regnum_symbols,
        ctx.ops,
        ctx.isa_param_values,
        ctx.mnemonic,
        ctx.reg_kinds,
    );
    let sym_count_lit = proc_macro2::Literal::usize_unsuffixed(max_sym_id + 1);
    let body = emit_behavior_effect(&behavior, behavior.root, ctx)?;
    Some(quote! {
        {
            let __tmdl_entry_syms: Vec<tir::sem::Value> = {
                let mut __syms: Vec<Option<tir::sem::Value>> = vec![None; #sym_count_lit];
                #(#sym_inits)*
                __syms.into_iter()
                    .map(|value| value.unwrap_or_else(|| tir::sem::int_value(64, 0)))
                    .collect()
            };
            #body
        }
    })
}

struct RustBehaviorCtx<'a> {
    ops: &'a [(String, Type)],
    isa_param_values: &'a HashMap<String, i64>,
    mnemonic: &'a proc_macro2::Literal,
    reg_kinds: &'a HashMap<String, (bool, u32)>,
}

fn emit_behavior_effect(
    behavior: &sem_expr_state::BehaviorGraph,
    effect: tir::graph::NodeId,
    ctx: &RustBehaviorCtx<'_>,
) -> Option<proc_macro2::TokenStream> {
    use tir::graph::Dag as _;

    let children: Vec<_> = behavior.graph.children(effect).collect();
    match behavior.graph.get_node(effect) {
        tir::sem::SymKind::StateAssign => {
            let sem_expr_state::EffectPayload::Assign { destination } =
                behavior.effect_payload(effect)?
            else {
                return None;
            };
            let eval = emit_behavior_value_eval(behavior, *children.first()?, ctx.mnemonic)?;
            let write = emit_graph_destination_write(destination, ctx.ops, ctx.mnemonic)?;
            Some(quote! {{ #eval #write }})
        }
        tir::sem::SymKind::StateStore
        | tir::sem::SymKind::StateStoreConditional
        | tir::sem::SymKind::StateFence => {
            let eval = emit_behavior_value_eval(behavior, *children.first()?, ctx.mnemonic)?;
            Some(quote! {{ #eval let _ = value; }})
        }
        tir::sem::SymKind::StateTrap => {
            let sem_expr_state::EffectPayload::Trap { argument_count, .. } =
                behavior.effect_payload(effect)?
            else {
                return None;
            };
            let cause = *children.get((0..*argument_count).next()?)?;
            let eval = emit_behavior_value_eval(behavior, cause, ctx.mnemonic)?;
            Some(quote! { #eval machine.raise_exception(value.to_u64())?; })
        }
        // The simulator executes the no-trap path. Handler state is modeled by
        // the SMT printer, while machine exception handling owns trap entry.
        tir::sem::SymKind::StateTry => emit_behavior_effect(behavior, *children.first()?, ctx),
        tir::sem::SymKind::StateBlock => {
            let mut steps = Vec::new();
            for effect in children {
                steps.push(emit_behavior_effect(behavior, effect, ctx)?);
            }
            Some(quote! { #(#steps)* })
        }
        tir::sem::SymKind::StateIf => {
            let cond_eval = emit_behavior_value_eval(behavior, *children.first()?, ctx.mnemonic)?;
            let then_body = emit_behavior_effect(behavior, *children.get(1)?, ctx)?;
            // Omit the `else` arm for a guard with no else clause (e.g. a
            // guarded CSR write), so codegen emits no empty `else {}`.
            let else_arm = match children.get(2) {
                Some(else_effect) => {
                    let else_body = emit_behavior_effect(behavior, *else_effect, ctx)?;
                    quote! { else { #else_body } }
                }
                None => quote! {},
            };
            Some(quote! {
                {
                    #cond_eval
                    if value.to_u64() != 0 {
                        #then_body
                    } #else_arm
                }
            })
        }
        tir::sem::SymKind::StateHandler => None,
        _ => None,
    }
}

fn emit_behavior_value_eval(
    behavior: &sem_expr_state::BehaviorGraph,
    root: tir::graph::NodeId,
    mnemonic_lit: &proc_macro2::Literal,
) -> Option<proc_macro2::TokenStream> {
    let (values, root) = behavior.value_graph(root)?;
    emit_lowered_value_eval(&values, root, mnemonic_lit)
}

fn emit_lowered_value_eval(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    root: tir::graph::NodeId,
    mnemonic_lit: &proc_macro2::Literal,
) -> Option<proc_macro2::TokenStream> {
    // Build the semantic graph inline (no type annotations, so no `_context`).
    let (dag_stmts, _root) = emit_dag_as_code(dag, root, &[]);

    Some(quote! {
        let value = {
            let mut __g = tir::sem::SemGraph::new();
            {
                use tir::graph::MutDag as _;
                let g = &mut __g;
                #(#dag_stmts)*
            }
            let __syms = __tmdl_entry_syms.clone();
            struct __TmdlMachineMemory<'a>(&'a mut dyn tir::backend::MachineContext);
            impl tir::sem::Memory for __TmdlMachineMemory<'_> {
                type Error = tir::backend::SimTrap;

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

                fn load_reserved(
                    &mut self,
                    address: u64,
                    size: usize,
                    ord: tir::sem::MemOrdering,
                ) -> Result<u64, Self::Error> {
                    self.0.load_reserved(address, size, ord)
                }

                fn store_conditional(
                    &mut self,
                    address: u64,
                    size: usize,
                    value: u64,
                    ord: tir::sem::MemOrdering,
                ) -> Result<bool, Self::Error> {
                    self.0.store_conditional(address, size, value, ord)
                }

                fn atomic_rmw(
                    &mut self,
                    op: tir::sem::AtomicRmwOp,
                    address: u64,
                    size: usize,
                    value: u64,
                    ord: tir::sem::MemOrdering,
                ) -> Result<u64, Self::Error> {
                    self.0.atomic_rmw(op, address, size, value, ord)
                }

                fn fence(&mut self, pred: u32, succ: u32, kind: u32) -> Result<(), Self::Error> {
                    self.0.fence(pred, succ, kind)
                }
            }
            let mut __memory = __TmdlMachineMemory(machine);
            match tir::sem::execute_with_memory(&__g, &__syms, &mut __memory)? {
                tir::sem::Value::Int(i) => tir::backend::RegisterValue::Int(i),
                // A float result (e.g. `fadd`) and a lane concatenation (a vector
                // destination) are written back as raw bytes; the destination
                // register's storage keeps the bit pattern.
                tir::sem::Value::Float(f) => {
                    tir::backend::RegisterValue::Bits(tir::utils::RawBits::from_apfloat(&f))
                }
                tir::sem::Value::RawBits(b) => tir::backend::RegisterValue::Bits(b),
                tir::sem::Value::Iterator(_) => {
                    return Err(tir::backend::SimTrap::InvalidInstruction {
                        op: #mnemonic_lit,
                        reason: "instruction semantic expression did not evaluate to a register value".to_string(),
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
    variable_symbols: &HashMap<String, u32>,
    register_symbols: &HashMap<(String, u32), u32>,
    regnum_symbols: &HashMap<String, u32>,
    ops: &[(String, Type)],
    isa_param_values: &HashMap<String, i64>,
    mnemonic_lit: &proc_macro2::Literal,
    reg_kinds: &HashMap<String, (bool, u32)>,
) -> (usize, Vec<proc_macro2::TokenStream>) {
    let max_sym_id = [
        variable_symbols.values().copied().max(),
        register_symbols.values().copied().max(),
        regnum_symbols.values().copied().max(),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0) as usize;

    let mut steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, &sym_id) in variable_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        if let Some((_, ty)) = ops.iter().find(|(n, _)| n == name) {
            match ty {
                Type::Struct(class_name) => {
                    let (_is_float, width) =
                        reg_kinds.get(class_name).copied().unwrap_or((false, 64));
                    // A vector operand (wider than a word) is read as raw byte
                    // lanes; the behavior splits it into lanes and interprets each
                    // as int or float. Scalar operands — integer and float alike —
                    // read as an `APInt` bit pattern: float operations reinterpret
                    // those bits via the node's float type, so a float value is
                    // never forced whole through the wrong representation, and a
                    // bit move (`fmov Xd,Dn`) reads the pattern directly.
                    let read = if width > 64 {
                        quote! {
                            tir::sem::value_from_raw_bits(machine.read_register_bits(class.name(), index)?)
                        }
                    } else {
                        quote! {
                            tir::sem::value_from_register(machine.read_register(class.name(), index)?)
                        }
                    };
                    steps.push(quote! {
                        {
                            let (class, index) = tir::backend::register_attr(self.attributes(), #name_lit)
                                .ok_or(tir::backend::SimTrap::MissingAttribute {
                                    op: #mnemonic_lit,
                                    attribute: #name_lit,
                                })?;
                            __syms[#sym_lit] = Some(#read);
                        }
                    });
                }
                Type::Integer => steps.push(quote! {
                    {
                        let value = tir::backend::int_attr(self.attributes(), #name_lit)
                            .ok_or(tir::backend::SimTrap::MissingAttribute {
                                op: #mnemonic_lit,
                                attribute: #name_lit,
                            })?;
                        __syms[#sym_lit] = Some(tir::sem::int_value_signed(64, value));
                    }
                }),
                Type::Bits(width) => {
                    let width_lit = proc_macro2::Literal::u32_unsuffixed(*width as u32);
                    steps.push(quote! {
                        {
                            let value = tir::backend::int_attr(self.attributes(), #name_lit)
                                .ok_or(tir::backend::SimTrap::MissingAttribute {
                                    op: #mnemonic_lit,
                                    attribute: #name_lit,
                                })?;
                            __syms[#sym_lit] = Some(tir::sem::int_value_signed(#width_lit, value));
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
                __syms[#sym_lit] = Some(tir::sem::int_value_signed(64, machine.isa_param(#name_lit).unwrap_or(#value_lit)));
            });
        }
    }
    for ((class, number), &sym_id) in register_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let class_lit = proc_macro2::Literal::string(class);
        let number_lit = proc_macro2::Literal::u16_unsuffixed(*number as u16);
        steps.push(quote! {
            __syms[#sym_lit] = Some(tir::sem::value_from_register(machine.read_register(#class_lit, #number_lit)?));
        });
    }

    // `regnum(op)` binds a symbol to the operand's encoding index. The index is
    // an identity, not an arithmetic value; comparisons coerce by value and
    // ignore width, so a plain 64-bit integer holds it.
    for (name, &sym_id) in regnum_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        steps.push(quote! {
            {
                let (_, index) = tir::backend::register_attr(self.attributes(), #name_lit)
                    .ok_or(tir::backend::SimTrap::MissingAttribute {
                        op: #mnemonic_lit,
                        attribute: #name_lit,
                    })?;
                __syms[#sym_lit] = Some(tir::sem::int_value(64, index as u64));
            }
        });
    }

    (max_sym_id, steps)
}

fn emit_graph_destination_write(
    dest: &sem_expr_state::Destination,
    ops: &[(String, Type)],
    mnemonic_lit: &proc_macro2::Literal,
) -> Option<proc_macro2::TokenStream> {
    use sem_expr_state::Destination;

    if matches!(dest, Destination::Path { base, members } if base == "PC" && members == &["pc"]) {
        return Some(quote! { machine.write_pc(value.to_u64()); });
    }

    if let Destination::FixedRegister { class, index, .. } = dest {
        let class_lit = proc_macro2::Literal::string(class);
        let index_lit = proc_macro2::Literal::u16_unsuffixed(*index as u16);
        return Some(quote! {
            if !register_has_trait_hardwired_zero(#class_lit, #index_lit) {
                machine.write_register_value(#class_lit, #index_lit, value)?;
            }
        });
    }

    let name = match dest {
        Destination::Ident(name) => name,
        Destination::Path { members, .. } if members.len() == 1 => &members[0],
        Destination::FixedRegister { .. }
        | Destination::Path { .. }
        | Destination::Field { .. } => return None,
    };
    if let Some((_, Type::Struct(_))) = ops.iter().find(|(n, _)| n == name) {
        let name_lit = proc_macro2::Literal::string(name);
        return Some(quote! {
            let (dst_class, dst_idx) = tir::backend::register_attr(self.attributes(), #name_lit).ok_or(
                tir::backend::SimTrap::MissingAttribute {
                    op: #mnemonic_lit,
                    attribute: #name_lit,
                },
            )?;
            if !register_has_trait_hardwired_zero(dst_class.name(), dst_idx) {
                machine.write_register_value(dst_class.name(), dst_idx, value)?;
            }
        });
    }

    None
}

/// Emit the pattern function, emit function, and rule registration for one
/// conditional-branch rule. Operands named in `zero_slots` are wired to a fixed
/// physical register (a class's hardwired-zero register) instead of bound from
/// the match — the mechanism behind the zero-form branch variants; every other
/// register/immediate operand binds from the match as usual.
#[allow(clippy::too_many_arguments)]
fn emit_cond_branch_rule(
    rule_name: &str,
    builder_ident: &proc_macro2::Ident,
    mnemonic_name: &str,
    inst_features: &proc_macro2::TokenStream,
    ops: &[(String, Type)],
    pattern: &tir::sem::SemGraph,
    root: tir::graph::NodeId,
    variable_symbols: &HashMap<String, u32>,
    target_operand: &str,
    target_symbol: u32,
    zero_slots: &HashMap<String, (String, u16)>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let emit_fn_ident = format_ident!("emit_isel_{}", rule_name);
    let pattern_fn_ident = format_ident!("isel_pattern_{}", rule_name);
    let rule_name_lit = proc_macro2::Literal::string(rule_name);
    let target_symbol_lit = proc_macro2::Literal::u32_unsuffixed(target_symbol);

    let mut operand_constraint_entries: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut emit_attr_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (op_name, op_ty) in ops {
        let op_name_lit = proc_macro2::Literal::string(op_name);
        if op_name == target_operand {
            emit_attr_steps.push(quote! {
                let dest = m
                    .block_binding(#target_symbol_lit)
                    .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                builder = builder.attr(
                    #op_name_lit,
                    tir::attributes::AttributeValue::Block(dest),
                );
            });
            continue;
        }
        if let Some((class_name, index)) = zero_slots.get(op_name) {
            let class_id = reg_class_id(class_name);
            let index_lit = proc_macro2::Literal::u16_unsuffixed(*index);
            emit_attr_steps.push(quote! {
                builder = builder.attr(
                    #op_name_lit,
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::Physical {
                            class: #class_id,
                            index: #index_lit,
                        },
                    ),
                );
            });
            continue;
        }
        let Some(&symbol) = variable_symbols.get(op_name) else {
            continue;
        };
        let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
        match op_ty {
            Type::Struct(class_name) => {
                let class_id = reg_class_id(class_name);
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Register) });
                emit_attr_steps.push(quote! {
                    let src = m
                        .value_binding(#symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
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
            }
            Type::Integer | Type::Bits(_) => {
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Immediate) });
                emit_attr_steps.push(quote! {
                    let v = m
                        .int_binding(#symbol_lit)
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

    let immediate_symbols: HashSet<u32> = ops
        .iter()
        .filter(|(_, op_ty)| matches!(op_ty, Type::Bits(_) | Type::Integer))
        .filter_map(|(op_name, _)| variable_symbols.get(op_name).copied())
        .collect();
    let (canon_pattern, canon_root, forced_widths) =
        tir::sem::canonicalize_for_selection(pattern, root, &immediate_symbols);
    let mut pattern_widths = tir::sem::infer_widths(&canon_pattern, |_| None);
    for (index, forced) in forced_widths.iter().enumerate() {
        if forced.is_some() {
            pattern_widths[index] = *forced;
        }
    }
    let (pattern_stmts, _root_var) = emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);
    let operand_register_call = emit_operand_register_call(
        ops,
        variable_symbols,
        &width_sensitive_symbols(&canon_pattern, &pattern_widths),
        float_classes,
        polymorphic_classes,
    );
    let operand_imm_range_call =
        emit_operand_imm_range_call(&immediate_operand_ranges(pattern, ops, variable_symbols));
    let base_cost = {
        use tir::graph::Dag;
        (canon_pattern.len() as u32).max(1)
    };
    let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
    let mnemonic_cost_lit = proc_macro2::Literal::string(mnemonic_name);

    let emitter = quote! {
        fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
            use tir::graph::MutDag;
            let mut g = tir::sem::SemGraph::new();
            #(#pattern_stmts)*
            g
        }

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
    };

    let init = quote! {
        if features_enabled(features, #inst_features) {
            rules.push(
                tir::backend::isel::Rule::new(
                    #rule_name_lit,
                    #pattern_fn_ident(context),
                    (#base_cost_lit).max(instruction_cost(#mnemonic_cost_lit)),
                    #emit_fn_ident,
                )
                .with_kind(tir::backend::isel::RuleKind::CondBranch {
                    target_symbol: #target_symbol_lit,
                })
                .with_operand_constraints(vec![#(#operand_constraint_entries),*])
                #operand_register_call
                #operand_imm_range_call
                ,
            );
        }
    };
    (emitter, init)
}

/// Clone `pattern` with the register-operand symbol `reg_symbol` replaced by the
/// `zext(0b0, W)` zero shape. `width_symbol` is the fresh wildcard the extension
/// width binds to — matched but never read by the emitter.
fn branch_pattern_with_zero(
    pattern: &tir::sem::SemGraph,
    root: tir::graph::NodeId,
    reg_symbol: u32,
    width_symbol: u32,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    let mut out = tir::sem::SemGraph::new();
    let mut memo: HashMap<usize, tir::graph::NodeId> = HashMap::new();
    let new_root =
        clone_pattern_with_zero(pattern, root, reg_symbol, width_symbol, &mut out, &mut memo);
    (out, new_root)
}

fn clone_pattern_with_zero(
    pattern: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    reg_symbol: u32,
    width_symbol: u32,
    out: &mut tir::sem::SemGraph,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&existing) = memo.get(&node.index()) {
        return existing;
    }
    if *pattern.get_node(node) == tir::sem::SymKind::Symbol
        && matches!(
            pattern.get_leaf_data(node),
            Some(tir::sem::SymPayload::SymbolId(s)) if *s == reg_symbol
        )
    {
        let zero = out.add_node(tir::sem::SymKind::Constant);
        out.set_leaf_data(zero, tir::sem::int_payload(1, 0, false));
        let width = out.add_node(tir::sem::SymKind::Symbol);
        out.set_leaf_data(width, tir::sem::SymPayload::SymbolId(width_symbol));
        let zext = out.add_node(tir::sem::SymKind::ZExt);
        out.add_edge(zext, zero);
        out.add_edge(zext, width);
        memo.insert(node.index(), zext);
        return zext;
    }
    // Children first: the store keeps strict post-order (a child's index must
    // precede its parent's).
    let kind = *pattern.get_node(node);
    let new_children: Vec<tir::graph::NodeId> = pattern
        .children(node)
        .collect::<Vec<_>>()
        .into_iter()
        .map(|child| clone_pattern_with_zero(pattern, child, reg_symbol, width_symbol, out, memo))
        .collect();
    let new_node = out.add_node(kind);
    if let Some(data) = pattern.get_leaf_data(node) {
        out.set_leaf_data(new_node, data.clone());
    }
    for new_child in new_children {
        out.add_edge(new_node, new_child);
    }
    memo.insert(node.index(), new_node);
    new_node
}

fn emit_dag_as_code(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    root: tir::graph::NodeId,
    widths: &[Option<u32>],
) -> (Vec<proc_macro2::TokenStream>, proc_macro2::Ident) {
    let mut stmts: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut node_vars: HashMap<usize, proc_macro2::Ident> = HashMap::new();
    let mut has_typed_node = false;
    for (counter, node_id) in dag.postorder(root).enumerate() {
        let var = format_ident!("__sem_{}", counter);

        let kind_ts = emit_expr_kind_ts(dag.get_node(node_id));
        stmts.push(quote! { let #var = g.add_node(#kind_ts); });

        if let Some(data) = dag.get_leaf_data(node_id) {
            let data_ts = emit_expr_payload_ts(data);
            stmts.push(quote! { g.set_leaf_data(#var, #data_ts); });
        }

        if !matches!(
            dag.get_node(node_id),
            tir::sem::SymKind::FAdd
                | tir::sem::SymKind::FSub
                | tir::sem::SymKind::FMul
                | tir::sem::SymKind::FDiv
                | tir::sem::SymKind::LoadMemory
                | tir::sem::SymKind::LoadReserved
        ) && dag.get_leaf_data(node_id).is_none()
            && let Some(Some(width)) = widths.get(node_id.index()).copied()
        {
            let width_lit = proc_macro2::Literal::u32_unsuffixed(width);
            stmts.push(quote! {
                g.set_actual_type(#var, tir::builtin::IntegerType::new(_context, #width_lit));
            });
            has_typed_node = true;
        }

        let children: Vec<tir::graph::NodeId> = dag.children(node_id).collect();
        for child_id in children {
            let child_var = node_vars[&child_id.index()].clone();
            stmts.push(quote! { g.add_edge(#var, #child_var); });
        }

        node_vars.insert(node_id.index(), var);
    }

    if has_typed_node {
        stmts.insert(0, quote! { use tir::graph::MetaMutDag as _; });
    }

    let root_var = node_vars[&root.index()].clone();
    (stmts, root_var)
}

fn emit_expr_kind_ts(kind: &tir::sem::SymKind) -> proc_macro2::TokenStream {
    let variant = tir::sem::scalar_op(*kind).map_or_else(
        || format_ident!("{kind:?}"),
        |op| format_ident!("{}", op.rust),
    );
    quote! { tir::sem::SymKind::#variant }
}

fn emit_expr_payload_ts(payload: &tir::sem::SymPayload<tir::ValueId>) -> proc_macro2::TokenStream {
    use tir::sem::SymPayload;
    match payload {
        SymPayload::SymbolId(id) => {
            let id_lit = proc_macro2::Literal::u32_unsuffixed(*id);
            quote! { tir::sem::SymPayload::SymbolId(#id_lit) }
        }
        SymPayload::Value(value) => {
            let value_lit = proc_macro2::Literal::u32_unsuffixed(value.number());
            quote! { tir::sem::SymPayload::Value(tir::ValueId::from_number(#value_lit)) }
        }
        SymPayload::Int(v) => {
            let width = proc_macro2::Literal::u32_unsuffixed(v.width());
            if v.is_signed() {
                let val = proc_macro2::Literal::u64_unsuffixed(v.to_i64() as u64);
                quote! { tir::sem::int_payload(#width, #val, true) }
            } else {
                let val = proc_macro2::Literal::u64_unsuffixed(v.to_u64());
                quote! { tir::sem::int_payload(#width, #val, false) }
            }
        }
        SymPayload::Float(f) => {
            let val = proc_macro2::Literal::f64_unsuffixed(f.to_f64());
            quote! { tir::sem::float_payload(#val) }
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
            // Any attribute value fits a full-width field, and the shifts
            // below would overflow at 64 bits.
            Some(Type::Bits(n)) if *n >= 64 => (quote! {}, quote! {}),
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
                        fixups.push(tir::backend::binary::InstFixup {
                            operand: #name_lit,
                            target: tir::backend::binary::FixupTarget::Symbol(s.clone()),
                        });
                    }
                    tir::attributes::AttributeValue::Block(b) => {
                        fixups.push(tir::backend::binary::InstFixup {
                            operand: #name_lit,
                            target: tir::backend::binary::FixupTarget::Block(*b),
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
        quote! { let fixups: Vec<tir::backend::binary::InstFixup> = Vec::new(); }
    } else {
        quote! { let mut fixups: Vec<tir::backend::binary::InstFixup> = Vec::new(); }
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
        ) -> Option<tir::backend::binary::EncodedInst> {
            #word_decl
            #fixups_decl
            #(#steps)*
            Some(tir::backend::binary::EncodedInst {
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
        // A full-width field admits any i64 (and the shifts would overflow).
        let range_check = if *n < 64 {
            let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
            let max = proc_macro2::Literal::i64_suffixed(1i64 << (n - 1));
            quote! {
                if !(#min..#max).contains(&value) {
                    return None;
                }
            }
        } else {
            quote! {}
        };
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
                #range_check
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

/// Compile an instruction's encoding arms into a `decode_*_inst` function — the
/// inverse of [`emit_instruction_encoder`]. Given a 32-bit little-endian
/// instruction word it matches the fixed opcode bits, reconstructs each operand
/// from its (possibly split) bit-fields, builds the corresponding op in the
/// `Context`, and returns its id.
///
/// Best-effort: returns `None` (no decoder emitted) for instructions without an
/// encoding, not exactly 32 bits wide, or using an encoding form this generator
/// cannot invert — so enabling decoding never breaks a backend's build.
fn emit_instruction_decoder(
    inst: &ast::Instruction,
    encoding_arms: &[ast::EncodingArm],
    ops_map: &HashMap<String, Type>,
    resolved_params: &HashMap<String, (Type, Option<ast::Expr>)>,
    width_bytes: u64,
) -> Option<(proc_macro2::TokenStream, proc_macro2::Ident, u128)> {
    if encoding_arms.is_empty() || width_bytes != 4 {
        return None;
    }

    let mut const_word: u128 = 0;
    let mut fixed_mask: u128 = 0;
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
        match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => {
                const_word |=
                    (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
                fixed_mask |= encoding_mask(width) << word_lo;
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
                Some(_) => return None,
                None => match resolved_params.get(&id.name) {
                    Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
                        const_word |=
                            (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
                        fixed_mask |= encoding_mask(width) << word_lo;
                    }
                    _ => return None,
                },
            },
            ast::Expr::Slice(slc) => {
                let ast::Expr::Ident(id) = &*slc.base else {
                    return None;
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return None,
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
                    return None;
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return None,
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
            _ => return None,
        }
    }

    // Reassemble one operand from its pieces: for each (word_lo, op_lo, width)
    // run, place word bits `[word_lo, word_lo+width)` at operand bits `op_lo`.
    let gather = |fields: &[IntField]| -> proc_macro2::TokenStream {
        let pieces: Vec<proc_macro2::TokenStream> = fields
            .iter()
            .map(|f| {
                let mask = proc_macro2::Literal::u64_suffixed(encoding_mask(f.width) as u64);
                let extract = if f.word_lo > 0 {
                    let word_lo = proc_macro2::Literal::u32_suffixed(f.word_lo as u32);
                    quote! { (word >> #word_lo) as u64 & #mask }
                } else {
                    quote! { word as u64 & #mask }
                };
                if f.op_lo > 0 {
                    let op_lo = proc_macro2::Literal::u32_suffixed(f.op_lo as u32);
                    quote! { value |= (#extract) << #op_lo; }
                } else {
                    quote! { value |= #extract; }
                }
            })
            .collect();
        quote! {{ let mut value: u64 = 0; #(#pieces)* value }}
    };

    let mut attr_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, fields) in &reg_fields {
        let class = match ops_map.get(name) {
            Some(Type::Struct(c)) => c,
            _ => return None,
        };
        let name_lit = proc_macro2::Literal::string(name);
        let class_id = reg_class_id(class);
        let g = gather(fields);
        attr_steps.push(quote! {
            .attr(
                #name_lit,
                tir::attributes::AttributeValue::Register(
                    tir::attributes::RegisterAttr::Physical {
                        class: #class_id,
                        index: (#g) as u16,
                    },
                ),
            )
        });
    }
    for (name, fields) in &int_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let g = gather(fields);
        attr_steps.push(quote! {
            .attr(#name_lit, tir::attributes::AttributeValue::Int((#g) as i64))
        });
    }

    let decode_fn_ident = format_ident!("decode_{}_inst", inst.name.to_lowercase());
    let builder_ident = format_ident!("{}OpBuilder", &inst.name);
    let const_word_lit = proc_macro2::Literal::u32_suffixed(const_word as u32);
    // An operand-less instruction fixes every bit, making the mask an identity.
    let guard = if fixed_mask as u32 == u32::MAX {
        quote! { if word != #const_word_lit { return None; } }
    } else {
        let fixed_mask_lit = proc_macro2::Literal::u32_suffixed(fixed_mask as u32);
        quote! { if word & #fixed_mask_lit != #const_word_lit { return None; } }
    };

    let decoder = quote! {
        fn #decode_fn_ident(context: &tir::Context, word: u32) -> Option<tir::OpId> {
            #guard
            let op = #builder_ident::new(context)
                #(#attr_steps)*
                .build();
            Some(op.id())
        }
    };

    Some((decoder, decode_fn_ident, fixed_mask))
}
