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
