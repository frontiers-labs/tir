mod dialect;
mod utils;

use proc_macro::TokenStream;

use dialect::construct_dialect;

#[proc_macro]
pub fn dialect(item: TokenStream) -> TokenStream {
    construct_dialect(item)
}
