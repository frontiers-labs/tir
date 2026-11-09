use proc_macro::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::{FnArg, GenericArgument, ItemTrait, Pat, PathArguments, ReturnType, TraitItem, Type};

pub fn construct_interface(arguments: TokenStream, item: TokenStream) -> TokenStream {
    if !arguments.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "operation_interface does not accept arguments",
        )
        .into_compile_error()
        .into();
    }
    let interface = syn::parse_macro_input!(item as ItemTrait);
    expand(&interface)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(interface: &ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    if !interface.generics.params.is_empty()
        || interface.generics.where_clause.is_some()
        || !interface.supertraits.is_empty()
        || interface.unsafety.is_some()
        || interface.auto_token.is_some()
    {
        return Err(syn::Error::new_spanned(
            interface,
            "operation interfaces must be safe traits without generics or supertraits",
        ));
    }

    let name = &interface.ident;
    let visibility = &interface.vis;
    let callbacks = format_ident!("{name}Callbacks");
    let adapter = format_ident!("__{name}Adapter");
    let mut fields = Vec::new();
    let mut default_fields = Vec::new();
    let mut methods = Vec::new();
    let mut all_default = true;
    for item in &interface.items {
        let TraitItem::Fn(method) = item else {
            return Err(syn::Error::new_spanned(
                item,
                "operation interfaces support only methods",
            ));
        };
        let signature = &method.sig;
        if !signature.generics.params.is_empty()
            || signature.generics.where_clause.is_some()
            || signature.constness.is_some()
            || signature.asyncness.is_some()
            || signature.unsafety.is_some()
            || signature.abi.is_some()
            || signature.variadic.is_some()
            || contains_self(signature.to_token_stream())
        {
            return Err(syn::Error::new_spanned(
                signature,
                "operation interface methods must be safe, synchronous, nongeneric Rust methods without Self types",
            ));
        }
        let Some(FnArg::Receiver(receiver)) = signature.inputs.first() else {
            return Err(syn::Error::new_spanned(
                signature,
                "operation interface methods require an &self receiver",
            ));
        };
        if receiver.reference.is_none()
            || receiver.mutability.is_some()
            || receiver.colon_token.is_some()
        {
            return Err(syn::Error::new_spanned(
                receiver,
                "operation interface methods require an &self receiver",
            ));
        }

        let mut arguments = Vec::new();
        let mut argument_types = Vec::new();
        for argument in signature.inputs.iter().skip(1) {
            let FnArg::Typed(argument) = argument else {
                return Err(syn::Error::new_spanned(argument, "unexpected receiver"));
            };
            let Pat::Ident(pattern) = argument.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &argument.pat,
                    "operation interface arguments require names",
                ));
            };
            if pattern.by_ref.is_some() || pattern.subpat.is_some() {
                return Err(syn::Error::new_spanned(
                    pattern,
                    "operation interface arguments require plain names",
                ));
            }
            arguments.push(&pattern.ident);
            argument_types.push(&argument.ty);
        }

        let method_name = &signature.ident;
        let mut output = signature.output.clone();
        if let ReturnType::Type(_, ty) = &mut output {
            bind_output_lifetimes(ty)?;
        }
        let pointer = quote! {
            for<'__tir_operation> fn(&'__tir_operation tir::OpHandle, #(#argument_types),*) #output
        };
        let callback = syn::Ident::new("__tir_callback", proc_macro2::Span::mixed_site());
        let call = quote! { #callback(&self.operation, #(#arguments),*) };
        let body = if let Some(default) = &method.default {
            default_fields.push(quote! {#method_name:None});
            fields.push(quote! {
                #[doc = concat!("Override for [`", stringify!(#name), "::", stringify!(#method_name), "`].")]
                pub #method_name: Option<#pointer>
            });
            quote! {
                match self.callbacks.#method_name {
                    Some(#callback) => #call,
                    None => #default,
                }
            }
        } else {
            all_default = false;
            fields.push(quote! {
                #[doc = concat!("Implementation of [`", stringify!(#name), "::", stringify!(#method_name), "`].")]
                pub #method_name: #pointer
            });
            quote! { (self.callbacks.#method_name)(&self.operation, #(#arguments),*) }
        };
        let attributes = &method.attrs;
        methods.push(quote! { #(#attributes)* #signature { #body } });
    }
    let default = all_default.then(|| quote! { #[derive(Default)] });
    let default_constant =
        all_default.then(|| quote! {pub const DEFAULT:Self=Self{#(#default_fields),*};});
    Ok(quote! {
        #interface

        #[doc = concat!("Operation callbacks for [`", stringify!(#name), "`].")]
        #default
        #visibility struct #callbacks { #(#fields,)* }

        struct #adapter {
            operation: tir::OpHandle,
            callbacks: &'static #callbacks,
        }

        impl #name for #adapter {
            #(#methods)*
        }

        impl #callbacks {
            #default_constant
            #[doc = concat!("View an operation through [`", stringify!(#name), "`].")]
            pub fn view(&'static self, operation: tir::OpHandle) -> Box<dyn #name> {
                Box::new(#adapter { operation, callbacks: self })
            }
        }
    })
}

fn contains_self(tokens: proc_macro2::TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(ident) => ident == "Self",
        proc_macro2::TokenTree::Group(group) => contains_self(group.stream()),
        _ => false,
    })
}

fn bind_output_lifetimes(ty: &mut Type) -> syn::Result<()> {
    match ty {
        Type::Reference(reference) => {
            if reference
                .lifetime
                .as_ref()
                .is_none_or(|lifetime| lifetime.ident == "_")
            {
                reference.lifetime = Some(syn::parse_quote!('__tir_operation));
            }
            bind_output_lifetimes(&mut reference.elem)?;
        }
        Type::Path(path) => {
            for segment in &mut path.path.segments {
                if let PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
                    for argument in &mut arguments.args {
                        match argument {
                            GenericArgument::Type(ty) => bind_output_lifetimes(ty)?,
                            GenericArgument::Lifetime(lifetime) if lifetime.ident == "_" => {
                                *lifetime = syn::parse_quote!('__tir_operation);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Type::Tuple(tuple) => {
            for ty in &mut tuple.elems {
                bind_output_lifetimes(ty)?;
            }
        }
        Type::Array(array) => bind_output_lifetimes(&mut array.elem)?,
        Type::Slice(slice) => bind_output_lifetimes(&mut slice.elem)?,
        Type::Paren(paren) => bind_output_lifetimes(&mut paren.elem)?,
        Type::Group(group) => bind_output_lifetimes(&mut group.elem)?,
        Type::Ptr(pointer) => bind_output_lifetimes(&mut pointer.elem)?,
        Type::Never(_) | Type::BareFn(_) => {}
        _ => {
            return Err(syn::Error::new_spanned(
                ty,
                "unsupported operation interface return type",
            ));
        }
    }
    Ok(())
}
