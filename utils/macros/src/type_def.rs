use std::collections::BTreeMap;
use std::path::PathBuf;

use proc_macro::TokenStream;
use quote::quote;
use syn::{Expr, Ident, LitStr, Path, Token, braced, parse::Parse, parse_macro_input};

struct Input {
    file: LitStr,
    binding: Path,
    hosts: BTreeMap<String, Expr>,
    context: Option<Expr>,
    runtime: Path,
}
fn name(path: &Path) -> String {
    path.segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}
impl Parse for Input {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let (mut file, mut binding, mut context, mut runtime) = (None, None, None, None);
        let mut hosts = BTreeMap::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "file" => file = Some(input.parse()?),
                "binding" => binding = Some(input.parse()?),
                "context" => context = Some(input.parse()?),
                "runtime" => runtime = Some(input.parse()?),
                "hosts" => {
                    let content;
                    braced!(content in input);
                    while !content.is_empty() {
                        let path: Path = content.parse()?;
                        content.parse::<Token![=>]>()?;
                        hosts.insert(name(&path), content.parse()?);
                        if !content.is_empty() {
                            content.parse::<Token![,]>()?;
                        }
                    }
                }
                _ => return Err(syn::Error::new_spanned(key, "unknown type output option")),
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            file: file.ok_or_else(|| input.error("type output requires file"))?,
            binding: binding.ok_or_else(|| input.error("type output requires binding"))?,
            hosts,
            context,
            runtime: runtime.unwrap_or_else(|| syn::parse_quote!(::tir)),
        })
    }
}
fn expand(input: &Input, spelling: bool) -> Result<proc_macro2::TokenStream, String> {
    let root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(|error| error.to_string())?)
            .join(input.file.value());
    let (sources, files) = tir_cdsl::SourceMap::from_file(root).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let module = tir_cdsl::compile(&sources).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let binding = name(&input.binding);
    let expression = if spelling {
        let context = input
            .context
            .as_ref()
            .ok_or("type_spelling requires its existing context expression")?;
        tir_cdsl::output::type_definitions::emit_type_spelling(
            &module,
            &binding,
            &input.hosts,
            context,
            &input.runtime,
        )
    } else {
        tir_cdsl::output::type_definitions::emit_type_ctor(
            &module,
            &binding,
            &input.hosts,
            &input.runtime,
        )
    }
    .map_err(|error| error.to_string())?;
    let dependencies = files
        .iter()
        .map(|path| path.to_str().ok_or("type definition path must be UTF-8"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(quote! {{#(const _: &str=include_str!(#dependencies);)* #expression}})
}
pub fn construct_type_ctor(tokens: TokenStream) -> TokenStream {
    let input = parse_macro_input!(tokens as Input);
    expand(&input, false)
        .unwrap_or_else(|error| syn::Error::new_spanned(&input.file, error).into_compile_error())
        .into()
}
pub fn construct_type_spelling(tokens: TokenStream) -> TokenStream {
    let input = parse_macro_input!(tokens as Input);
    expand(&input, true)
        .unwrap_or_else(|error| syn::Error::new_spanned(&input.file, error).into_compile_error())
        .into()
}
