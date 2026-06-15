//! `#[derive(TirType)]`: capture a type's dialect, name and constructor
//! parameters into the `TYPE_SCHEMAS` registry so it can be built structurally
//! (no textual form) and exposed to language bindings. Hand-written
//! `Type`/parse/print impls are left untouched.
//!
//! Parameters are read from the struct's named fields; supported field types are
//! `u32`, `u64`, `i64`, `bool` and `TypeId`. Unit structs have no parameters.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, Type, parse_macro_input};

pub fn construct_tir_type(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let struct_name = &input.ident;

    let mut dialect: Option<String> = None;
    let mut name: Option<String> = None;
    for attr in &input.attrs {
        if attr.path().is_ident("tir_type") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("dialect") {
                    dialect = Some(meta.value()?.parse::<LitStr>()?.value());
                } else if meta.path.is_ident("name") {
                    name = Some(meta.value()?.parse::<LitStr>()?.value());
                }
                Ok(())
            })
            .expect("invalid #[tir_type(...)] attribute");
        }
    }
    let dialect = dialect.expect("#[tir_type(dialect = \"...\")] is required");
    let name = name.expect("#[tir_type(name = \"...\")] is required");

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => named.named.iter().collect::<Vec<_>>(),
            Fields::Unit => vec![],
            _ => panic!("TirType supports only named-field or unit structs"),
        },
        _ => panic!("TirType can only be derived for structs"),
    };

    let nparams = fields.len();
    let mut params = Vec::new();
    let mut decoders = Vec::new();
    let mut field_idents = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        let ident = field.ident.clone().unwrap();
        let fname = ident.to_string();
        let (kind, variant) = kind_for(&field.ty, &fname, struct_name);
        params.push(quote! { tir::TypeParam { name: #fname, kind: tir::TypeParamKind::#kind } });
        decoders.push(quote! {
            let #ident = match &args[#i] {
                tir::TypeArg::#variant(v) => *v,
                _ => return Err(format!(
                    "type '{}.{}' parameter '{}' has the wrong kind", #dialect, #name, #fname
                )),
            };
        });
        field_idents.push(ident);
    }

    let ctor = if fields.is_empty() {
        quote! { #struct_name }
    } else {
        quote! { #struct_name { #(#field_idents),* } }
    };

    quote! {
        const _: () = {
            #[tir::linkme::distributed_slice(tir::TYPE_SCHEMAS)]
            #[linkme(crate = tir::linkme)]
            static __TIR_TYPE_SCHEMA: tir::TypeSchema = tir::TypeSchema {
                dialect: #dialect,
                name: #name,
                params: &[#(#params),*],
                build: |context, args| {
                    if args.len() != #nparams {
                        return Err(format!(
                            "type '{}.{}' expects {} argument(s), got {}",
                            #dialect, #name, #nparams, args.len()
                        ));
                    }
                    #(#decoders)*
                    Ok(context.get_type_id(::std::sync::Arc::new(#ctor)))
                },
            };
        };
    }
    .into()
}

/// Map a field type to its `TypeParamKind` and matching `TypeArg` variant.
fn kind_for(ty: &Type, field: &str, struct_name: &syn::Ident) -> (syn::Ident, syn::Ident) {
    let last = match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    };
    let kind = match last.as_deref() {
        Some("u32") => "U32",
        Some("u64") => "U64",
        Some("i64") => "I64",
        Some("bool") => "Bool",
        Some("TypeId") => "Type",
        other => panic!(
            "TirType field `{struct_name}::{field}` has unsupported type `{}`; \
             supported: u32, u64, i64, bool, TypeId",
            other.unwrap_or("?")
        ),
    };
    let id = |s: &str| syn::Ident::new(s, proc_macro2::Span::call_site());
    (id(kind), id(kind))
}
