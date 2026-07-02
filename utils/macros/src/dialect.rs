use proc_macro::TokenStream;
use quote::quote;
use syn::{ExprStruct, Ident, Member, parse::Parse, parse_macro_input};

use crate::utils::{expr_as_ident_vec, expr_as_string};

pub fn construct_dialect(item: TokenStream) -> TokenStream {
    let Dialect {
        struct_name,
        name,
        operations,
        types,
    } = parse_macro_input!(item as Dialect);

    let register_operations = make_register_operations(&name, &operations);
    let register_types = make_register_types(&name, &types);

    quote! {
        pub struct #struct_name {
            dyn_converters: std::collections::HashMap<&'static str, fn(std::sync::Arc<tir::OpInstance>) -> Box<dyn tir::Operation>>,
            parsers: std::collections::HashMap<&'static str, fn(&mut tir::parse::text::Parser<'_>, &tir::Context) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)>>,
            type_parsers: std::collections::HashMap<&'static str, tir::TypeParser>,
        }

        impl tir::Dialect for #struct_name {
            fn new() -> std::sync::Arc<dyn tir::Dialect> {
                std::sync::Arc::new(#struct_name {
                    dyn_converters: std::collections::HashMap::new(),
                    parsers: std::collections::HashMap::new(),
                    type_parsers: std::collections::HashMap::new(),
                })
            }

            fn name() -> &'static str {
                #name
            }

            #register_operations
            #register_types

            fn get_dyn_op(&self, op: std::sync::Arc<tir::OpInstance>) -> Box<dyn tir::Operation> {
               assert_eq!(op.dialect(), #name);
               let converter = self.dyn_converters.get(op.name()).unwrap();
               converter(op)
            }

            fn get_parser(&self, name: &str)
            -> Result<fn(&mut tir::parse::text::Parser<'_>, &tir::Context) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)>, tir::Error> {
                self.parsers.get(name).cloned().ok_or(tir::Error::UnknownOperation(#name.to_string(), name.to_string()))
            }

            fn get_type_parser(&self, name: &str) -> Result<tir::TypeParser, tir::Error> {
                self.type_parsers
                    .get(name)
                    .cloned()
                    .ok_or(tir::Error::UnknownType(#name.to_string(), name.to_string()))
            }
        }
    }
    .into()
}

struct Dialect {
    struct_name: Ident,
    name: String,
    operations: Vec<Ident>,
    types: Vec<Ident>,
}

impl Parse for Dialect {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let struct_: ExprStruct = input.parse()?;

        let struct_name = struct_.path.require_ident()?.clone();

        let name = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "name" {
                        Some(expr_as_string(&f.expr))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap();

        let operations = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "operations" {
                        Some(expr_as_ident_vec(&f.expr))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        let types = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "types" {
                        Some(expr_as_ident_vec(&f.expr))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        Ok(Dialect {
            struct_name,
            name,
            operations,
            types,
        })
    }
}

fn make_register_operations(dialect_name: &str, operations: &[Ident]) -> proc_macro2::TokenStream {
    let op = operations
        .iter()
        .map(|name| {
            quote! {
                assert_eq!(#name::dialect(), #dialect_name);
                self.dyn_converters.insert(#name::name(), #name::from_op_instance_dyn);
                self.parsers.insert(#name::name(), #name::parse);
                #name::register_interfaces(context);
            }
        })
        .collect::<Vec<_>>();
    quote! {
        fn register_operations(&mut self, context: &tir::Context) {
            use tir::Operation;
            #(#op)*
        }
    }
}

fn make_register_types(_dialect_name: &str, types: &[Ident]) -> proc_macro2::TokenStream {
    let ty = types
        .iter()
        .map(|name| {
            quote! {
                self.type_parsers.insert(#name::parse_key(), #name::parse);
                for key in #name::extra_parse_keys() {
                    self.type_parsers.insert(key, #name::parse);
                }
            }
        })
        .collect::<Vec<_>>();
    quote! {
        fn register_types(&mut self, _context: &tir::Context) {
            use tir::Type;
            #(#ty)*
        }
    }
}
