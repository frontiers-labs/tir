mod checked_sem;
mod dialect;
mod interface;
mod op_def;
mod sem_recipe;
mod type_def;
mod utils;

use proc_macro::TokenStream;

use dialect::construct_dialect;

#[proc_macro_attribute]
pub fn operation_interface(arguments: TokenStream, item: TokenStream) -> TokenStream {
    interface::construct_interface(arguments, item)
}

#[proc_macro]
pub fn dialect(item: TokenStream) -> TokenStream {
    construct_dialect(item)
}

#[proc_macro]
pub fn op_def(item: TokenStream) -> TokenStream {
    op_def::construct_op_def(item)
}

#[proc_macro]
pub fn type_ctor(item: TokenStream) -> TokenStream {
    type_def::construct_type_ctor(item)
}
#[proc_macro]
pub fn type_spelling(item: TokenStream) -> TokenStream {
    type_def::construct_type_spelling(item)
}
