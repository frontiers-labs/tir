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
        #[allow(dead_code)]
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

