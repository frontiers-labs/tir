use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use tir_symbolic::lang::SymKind;

/// Compiler-side construction of a flat, parser-free runtime recipe.
pub struct Recipe {
    steps: Vec<TokenStream>,
}
impl Recipe {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }
    fn push(&mut self, step: TokenStream) -> u32 {
        let index = self.steps.len() as u32;
        self.steps.push(step);
        index
    }
    pub fn operand(&mut self, index: u32) -> u32 {
        self.push(quote! { tir::sem::SemRecipeStep::Operand(#index) })
    }
    pub fn splice(&mut self, name: &str) -> u32 {
        self.push(quote! { tir::sem::SemRecipeStep::Splice(#name) })
    }
    pub fn bool_attribute(&mut self, name: &str, default: bool) -> u32 {
        self.push(quote! {tir::sem::SemRecipeStep::BoolAttribute{name:#name,default:#default}})
    }
    pub fn integer(&mut self, width: u64, value: i64, signed: bool) -> u32 {
        self.push(
            quote! { tir::sem::SemRecipeStep::Integer {width:#width,value:#value,signed:#signed} },
        )
    }
    pub fn apply(&mut self, kind: SymKind, arguments: &[u32]) -> u32 {
        let kind = format_ident!("{kind:?}");
        self.push(quote! { tir::sem::SemRecipeStep::Apply {kind:tir::sem::SymKind::#kind,arguments:&[#(#arguments),*]} })
    }
    pub fn finish(self, root: u32) -> TokenStream {
        let steps = self.steps;
        quote! { tir::sem::SemRecipe {steps:&[#(#steps),*],root:#root} }
    }
}
