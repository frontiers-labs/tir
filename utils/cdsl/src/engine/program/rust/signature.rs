use crate::{
    CheckedProgram,
    hm::{ConId, Ty, VarId},
    infer::{TUPLE, UNIT},
};
use proc_macro2::TokenStream;
use quote::quote;
use std::collections::BTreeMap;

use super::type_parts;

pub(super) struct Signature {
    kind: Option<ConId>,
    reference: Option<ConId>,
    parameters: BTreeMap<VarId, usize>,
}

impl Signature {
    pub(super) fn new(program: &CheckedProgram) -> Self {
        let family = |name| {
            program
                .type_families
                .get(&vec!["std".into(), "ir".into(), String::from(name)])
                .copied()
        };
        Self {
            kind: family("OperationKindOf"),
            reference: family("ValueRef"),
            parameters: BTreeMap::new(),
        }
    }

    pub(super) fn reset_parameters(&mut self) {
        self.parameters.clear();
    }

    pub(super) fn render(&mut self, ty: &Ty) -> TokenStream {
        match ty {
            Ty::Con(id, _) if *id == UNIT => quote!(()),
            Ty::Con(id, parts) => {
                let parts = type_parts(parts)
                    .into_iter()
                    .map(|ty| self.render(ty))
                    .collect::<Vec<_>>();
                if *id == TUPLE {
                    return quote!((#(#parts,)*));
                }
                if Some(*id) == self.kind
                    && let [inputs, outputs] = parts.as_slice()
                {
                    return quote!(OperationKindOf<#inputs, #outputs>);
                }
                if Some(*id) == self.reference
                    && let [ty] = parts.as_slice()
                {
                    return quote!(ValueRef<#ty>);
                }
                let id = id.0;
                quote!(signature::Type<#id, (#(#parts,)*)>)
            }
            Ty::Fun(inputs, output) => {
                let inputs = type_parts(inputs)
                    .into_iter()
                    .map(|ty| self.render(ty))
                    .collect::<Vec<_>>();
                let output = self.render(output);
                quote!(fn(#(#inputs),*) -> #output)
            }
            Ty::Var(id) => {
                let next = self.parameters.len();
                let index = *self.parameters.entry(*id).or_insert(next);
                quote!(signature::Parameter<#index>)
            }
            Ty::Rigid(id) => {
                let id = id.0;
                quote!(signature::Rigid<#id>)
            }
        }
    }

    pub(super) fn declarations(&self) -> TokenStream {
        quote! {
            /// Types carried by operation and value signatures.
            pub mod signature {
                #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
                pub struct Type<const FAMILY: usize, Args>(std::marker::PhantomData<fn(Args) -> Args>);

                #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
                pub struct Parameter<const INDEX: usize>;

                #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
                pub struct Rigid<const INDEX: usize>;
            }
        }
    }
}
