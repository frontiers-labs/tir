mod binds;
mod dialect;
mod operation;
mod utils;

use proc_macro::TokenStream;

use dialect::construct_dialect;
use operation::construct_operation;

#[proc_macro]
pub fn dialect(item: TokenStream) -> TokenStream {
    construct_dialect(item)
}

#[proc_macro]
pub fn operation(item: TokenStream) -> TokenStream {
    construct_operation(item)
}
