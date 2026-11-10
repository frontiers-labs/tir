//! A checked file written back as the Rust that builds it, so a consumer
//! whose rules are fixed when it is built parses nothing when it runs.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::ast::*;

/// Rust source defining `fn file() -> tir_pdl::File` that returns `file`,
/// spans aside: they point into a source the consumer no longer has. Each item
/// is a function of its own, which keeps every body small for the compiler.
pub fn file_to_rust(file: &File) -> String {
    let names: Vec<_> = (0..file.items.len())
        .map(|index| format_ident!("item_{index}"))
        .collect();
    let bodies = file.items.iter().map(item);
    let tokens = quote! {
        pub(crate) fn file() -> tir_pdl::File {
            tir_pdl::File { items: vec![#(#names()),*] }
        }
        #(fn #names() -> tir_pdl::Item { #bodies })*
    };
    let parsed = syn::parse2(tokens).expect("generated items parse");
    prettyplease::unparse(&parsed)
}

fn span() -> TokenStream {
    quote!(tir_pdl::Span::from(0..0))
}

fn variant(ty: &str, value: &impl std::fmt::Debug) -> TokenStream {
    let (ty, value) = (format_ident!("{ty}"), format_ident!("{value:?}"));
    quote!(tir_pdl::#ty::#value)
}

fn string(value: &str) -> TokenStream {
    quote!(String::from(#value))
}

fn option<T>(value: &Option<T>, some: impl Fn(&T) -> TokenStream) -> TokenStream {
    match value {
        Some(value) => {
            let value = some(value);
            quote!(Some(#value))
        }
        None => quote!(None),
    }
}

fn list<T>(values: &[T], each: impl Fn(&T) -> TokenStream) -> TokenStream {
    let values = values.iter().map(each);
    quote!(vec![#(#values),*])
}

fn item(item: &Item) -> TokenStream {
    match item {
        Item::Group(group) => {
            let name = string(&group.name);
            let (alternatives, span) = (list(&group.alternatives, ty), span());
            quote!(tir_pdl::Item::Group(tir_pdl::Group {
                name: #name,
                alternatives: #alternatives,
                span: #span,
            }))
        }
        Item::Rule(rule) => {
            let name = string(&rule.name);
            let kind = match rule.kind {
                RuleKind::Equality(direction) => {
                    let direction = variant("Direction", &direction);
                    quote!(tir_pdl::RuleKind::Equality(#direction))
                }
                RuleKind::Refinement => quote!(tir_pdl::RuleKind::Refinement),
            };
            let (lhs, rhs) = (term(&rule.lhs), term(&rule.rhs));
            let guards = list(&rule.guards, expr);
            let requirements = list(&rule.requirements, |requirement| match requirement {
                RefinementRequirement::Permits(permission) => {
                    let permission = variant("ContractPermission", permission);
                    quote!(tir_pdl::RefinementRequirement::Permits(#permission))
                }
                other => variant("RefinementRequirement", other),
            });
            let proof = option(&rule.proof, |proof| variant("Proof", proof));
            let (post_saturation, span) = (rule.post_saturation, span());
            quote!(tir_pdl::Item::Rule(Box::new(tir_pdl::Rule {
                name: #name,
                kind: #kind,
                lhs: #lhs,
                rhs: #rhs,
                guards: #guards,
                requirements: #requirements,
                proof: #proof,
                post_saturation: #post_saturation,
                span: #span,
            })))
        }
    }
}

fn term(term: &Term) -> TokenStream {
    let kind = match &term.kind {
        TermKind::Operation {
            operator,
            attributes,
            operands,
            dependencies,
        } => {
            let operator = match operator {
                Operator::Dialect { dialect, name } => {
                    let (dialect, name) = (string(dialect), string(name));
                    quote!(tir_pdl::Operator::Dialect { dialect: #dialect, name: #name })
                }
                Operator::Semantic(name) => {
                    let name = string(name);
                    quote!(tir_pdl::Operator::Semantic(#name))
                }
            };
            let attributes = list(attributes, attribute);
            let (operands, dependencies) =
                (list(operands, self::term), list(dependencies, self::term));
            quote!(tir_pdl::TermKind::Operation {
                operator: #operator,
                attributes: #attributes,
                operands: #operands,
                dependencies: #dependencies,
            })
        }
        TermKind::Binder { name, ty } => {
            let name = string(name);
            let ty = option(ty, |ty| match ty {
                BindingType::Type(ty) => {
                    let ty = self::ty(ty);
                    quote!(tir_pdl::BindingType::Type(#ty))
                }
                BindingType::Constant(width) => {
                    let width = option(width, expr);
                    quote!(tir_pdl::BindingType::Constant(#width))
                }
            });
            quote!(tir_pdl::TermKind::Binder { name: #name, ty: #ty })
        }
        TermKind::Value(value) => {
            let value = expr(value);
            quote!(tir_pdl::TermKind::Value(#value))
        }
        TermKind::String(value) => {
            let value = string(value);
            quote!(tir_pdl::TermKind::String(#value))
        }
        TermKind::Constant { width, value } => {
            let (width, value) = (expr(width), expr(value));
            quote!(tir_pdl::TermKind::Constant { width: #width, value: #value })
        }
        TermKind::Root => quote!(tir_pdl::TermKind::Root),
    };
    let (ty, span) = (option(&term.ty, ty), span());
    quote!(tir_pdl::Term { kind: #kind, ty: #ty, span: #span })
}

fn attribute(attribute: &Attribute) -> TokenStream {
    let name = string(&attribute.name);
    let value = match &attribute.value {
        AttributeValue::Integer(value) => quote!(tir_pdl::AttributeValue::Integer(#value)),
        AttributeValue::String(value) => {
            let value = string(value);
            quote!(tir_pdl::AttributeValue::String(#value))
        }
        AttributeValue::Binder(name) => {
            let name = string(name);
            quote!(tir_pdl::AttributeValue::Binder(#name))
        }
    };
    let span = span();
    quote!(tir_pdl::Attribute { name: #name, value: #value, span: #span })
}

fn ty(ty: &Type) -> TokenStream {
    match ty {
        Type::Integer(width) => {
            let width = match width {
                Width::Concrete(bits) => quote!(tir_pdl::Width::Concrete(#bits)),
                Width::Named(name) => {
                    let name = string(name);
                    quote!(tir_pdl::Width::Named(#name))
                }
                Width::Any => quote!(tir_pdl::Width::Any),
            };
            quote!(tir_pdl::Type::Integer(#width))
        }
        Type::Float(format) => {
            let format = variant("FloatFormat", format);
            quote!(tir_pdl::Type::Float(#format))
        }
        Type::ShapedFloat { format, shape } => {
            let (format, shape) = (variant("FloatFormat", format), list(shape, expr));
            quote!(tir_pdl::Type::ShapedFloat { format: #format, shape: #shape })
        }
        Type::State(resource) => {
            let resource = match resource {
                Resource::Named(name) => {
                    let name = string(name);
                    quote!(tir_pdl::Resource::Named(#name))
                }
                other => variant("Resource", other),
            };
            quote!(tir_pdl::Type::State(#resource))
        }
        Type::Pointer(Width::Named(name)) => {
            let name = string(name);
            quote!(tir_pdl::Type::Pointer(tir_pdl::Width::Named(#name)))
        }
        Type::Pointer(_) => unreachable!("a pointer type binds its width by name"),
        Type::Named(name) => {
            let name = string(name);
            quote!(tir_pdl::Type::Named(#name))
        }
    }
}

fn expr(expr: &Expr) -> TokenStream {
    let kind = match &expr.kind {
        ExprKind::Integer(value) => quote!(tir_pdl::ExprKind::Integer(#value)),
        ExprKind::Name(name) => {
            let name = string(name);
            quote!(tir_pdl::ExprKind::Name(#name))
        }
        ExprKind::Call { name, args } => {
            let (name, args) = (string(name), list(args, self::expr));
            quote!(tir_pdl::ExprKind::Call { name: #name, args: #args })
        }
        ExprKind::Unary { op, value } => {
            let (op, value) = (variant("UnaryOp", op), self::expr(value));
            quote!(tir_pdl::ExprKind::Unary { op: #op, value: Box::new(#value) })
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let op = variant("BinaryOp", op);
            let (lhs, rhs) = (self::expr(lhs), self::expr(rhs));
            quote!(tir_pdl::ExprKind::Binary {
                op: #op,
                lhs: Box::new(#lhs),
                rhs: Box::new(#rhs),
            })
        }
    };
    let span = span();
    quote!(tir_pdl::Expr { kind: #kind, span: #span })
}
