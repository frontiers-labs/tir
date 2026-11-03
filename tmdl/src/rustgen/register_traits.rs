use super::*;

#[derive(Clone, Copy)]
pub(super) struct FpField {
    pub(super) kind: tir_symbolic::lang::StateFieldKind,
    pub(super) hardwired_zero: bool,
}

pub(super) fn fp_field(traits: &[ast::RegisterTrait]) -> Option<FpField> {
    use tir_symbolic::lang::StateFieldKind;
    let kind = traits.iter().find_map(|role| match role {
        ast::RegisterTrait::FpFlags => Some(StateFieldKind::FpFlags),
        ast::RegisterTrait::FpRounding => Some(StateFieldKind::FpRounding),
        ast::RegisterTrait::FpTraps => Some(StateFieldKind::FpTraps),
        _ => None,
    })?;
    Some(FpField {
        kind,
        hardwired_zero: traits.contains(&ast::RegisterTrait::HardwiredZero),
    })
}

pub(super) fn emit_register_trait_helpers(
    files: &[ast::File],
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let mut hardwired_patterns = Vec::new();
    let mut hardwired_fields = Vec::new();

    for rc in files.iter().flat_map(|f| f.register_classes()) {
        let class_lit = proc_macro2::Literal::string(&rc.name);
        for register in rc.resolve_registers() {
            if let Some(field) = fp_field(&register.traits)
                && field.hardwired_zero
            {
                let kind = format_ident!("{}", format!("{:?}", field.kind));
                hardwired_fields.push(quote! {
                    (tir::sem::StateResourceKind::FpEnvironment, tir::sem::StateFieldKind::#kind)
                });
            }
        }
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
        /// State fields whose register declarations make reads zero and writes inert.
        pub fn hardwired_zero_state_fields() -> &'static [(tir::sem::StateResourceKind, tir::sem::StateFieldKind)] {
            &[#(#hardwired_fields),*]
        }

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
