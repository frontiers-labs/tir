//! Inline CDSL parsing and checking through the shared compiler.
extern crate proc_macro;
use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

/// Compile CDSL with the file compiler's grammar and checks, yielding a diagnostic string literal.
#[proc_macro]
pub fn cdsl(input: TokenStream) -> TokenStream {
    match tir_cdsl::compile_tokens(input).map(|program| program.render_typed()) {
        Ok(artifact) => TokenStream::from(TokenTree::Literal(Literal::string(&artifact))),
        Err(error) => TokenStream::from_iter([
            TokenTree::Ident(Ident::new("compile_error", Span::call_site())),
            TokenTree::Punct(Punct::new('!', Spacing::Alone)),
            TokenTree::Group(Group::new(
                Delimiter::Parenthesis,
                TokenStream::from(TokenTree::Literal(Literal::string(&error.to_string()))),
            )),
        ]),
    }
}
