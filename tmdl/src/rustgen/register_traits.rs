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

