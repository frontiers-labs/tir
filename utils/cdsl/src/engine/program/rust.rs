use std::{
    borrow::Cow,
    hash::{DefaultHasher, Hash, Hasher},
};

use proc_macro2::{Ident, Literal as RustLiteral, TokenStream};
use quote::{format_ident, quote};
use tir_egglog::Schedule;

use super::*;
use crate::{
    compiler::CARGO_PACKAGE_ROOT_PREFIX,
    hm::{Ty, TyList},
};

mod signature;
use signature::Signature;

type Host<'a> = (&'a [String], &'a [Expr], &'a Expr);

fn snake(name: &str) -> String {
    let mut lowered = String::new();
    for (index, letter) in name.chars().enumerate() {
        if index != 0 && letter.is_ascii_uppercase() {
            lowered.push('_');
        }
        lowered.push(letter.to_ascii_lowercase());
    }
    lowered
}

fn identifier_part(name: &str) -> Cow<'_, str> {
    if name.starts_with(CARGO_PACKAGE_ROOT_PREFIX) {
        let mut hash = DefaultHasher::new();
        name.hash(&mut hash);
        Cow::Owned(format!("dep_{:016x}", hash.finish()))
    } else {
        Cow::Borrowed(name)
    }
}

fn method(path: &[String]) -> Ident {
    format_ident!(
        "{}",
        path.iter()
            .map(|name| snake(&identifier_part(name)))
            .collect::<Vec<_>>()
            .join("_")
    )
}

fn enum_name(path: &[String], selected: &[String]) -> Ident {
    let path = path.strip_prefix(selected).unwrap_or(path);
    let name = path
        .iter()
        .map(|part| identifier_part(part))
        .collect::<Vec<_>>()
        .join("_");
    format_ident!("{name}")
}

enum Codec {
    Int,
    Bool,
    String,
    BigInt,
    OpId,
    ValueId,
    ValueRef(TokenStream),
    Kind(TokenStream),
    Enum(Ident),
}

impl Codec {
    fn read(
        ty: &Expr,
        checked: &Ty,
        signature: &mut Signature,
        schema: &Schema,
        selected: &[String],
    ) -> Result<Self, Error> {
        let (family, _) = if let Expression::Generic { value, arguments } = &ty.value {
            (path(value)?, arguments.as_slice())
        } else {
            (path(ty)?, &[][..])
        };
        Ok(
            match family
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["std", "ir", "OperationId"] => Self::OpId,
                ["std", "ir", "ValueId"] => Self::ValueId,
                ["std", "ir", "IntegerRef" | "ValueRef"] => {
                    Self::ValueRef(signature.render(checked))
                }
                ["std", "ir", "OperationKind"] => Self::Kind(quote!(OperationKind)),
                ["std", "ir", "OperationKindOf" | "BinaryIntegerOperation"] => {
                    let ty = signature.render(checked);
                    signature.kind_uses.push((ty.clone(), checked.clone()));
                    Self::Kind(ty)
                }
                _ => match schema.database().schema().sort(schema.sort(ty)?).name() {
                    "i64" => Self::Int,
                    "bool" => Self::Bool,
                    "String" => Self::String,
                    "BigInt" => Self::BigInt,
                    _ if schema
                        .constructors
                        .iter()
                        .any(|variant| variant.starts_with(family)) =>
                    {
                        Self::Enum(enum_name(family, selected))
                    }
                    name => return Err(Error(format!("cannot emit a Rust value for sort {name}"))),
                },
            },
        )
    }

    fn ty(&self) -> TokenStream {
        match self {
            Self::Int => quote!(i64),
            Self::Bool => quote!(bool),
            Self::String => quote!(String),
            Self::BigInt => quote!(::tir_egglog::sort::BigInt),
            Self::OpId => quote!(OperationId),
            Self::ValueId => quote!(ValueId),
            Self::Kind(ty) | Self::ValueRef(ty) => ty.clone(),
            Self::Enum(name) => quote!(#name),
        }
    }

    fn field(&self) -> TokenStream {
        let ty = self.ty();
        match self {
            Self::Enum(_) => quote!(Box<#ty>),
            _ => ty,
        }
    }

    fn base(&self, db: TokenStream, value: TokenStream) -> TokenStream {
        match self {
            Self::OpId | Self::ValueId | Self::ValueRef(_) => {
                let ty = self.ty();
                quote!(<#ty>::from_raw(u32::try_from(#db.base_value::<i64>(#value))?))
            }
            Self::Kind(ty) => quote!(<#ty>::decode(
                #db, #db.native_identity(#value).ok_or("invalid native operation cell")?
            )?),
            Self::Enum(name) => quote!({
                let (terms, term) = extractor
                    .get_or_init(|| ::tir_egglog::extract::Extractor::new_in(db, color))
                    .best(#value)?;
                #name::decode(db, &terms, term)?
            }),
            _ => {
                let ty = self.ty();
                quote!(#db.base_value::<#ty>(#value))
            }
        }
    }

    fn encode(&self, db: TokenStream, value: TokenStream) -> TokenStream {
        match self {
            Self::OpId | Self::ValueId | Self::ValueRef(_) => {
                quote!(#db.base(i64::from(#value.raw())))
            }
            Self::Kind(_) => quote!(#db.base(#value.0)),
            _ => quote!(#db.base(#value)),
        }
    }

    fn decode(&self, value: &Ident) -> TokenStream {
        let literal = |variant: &str, success: TokenStream| {
            let variant = format_ident!("{variant}");
            quote!(match terms.get(*#value) {
                ::tir_egglog::Term::Lit(::tir_egglog::ast::Literal::#variant(value)) => #success,
                _ => return Err(invalid()),
            })
        };
        match self {
            Self::Int => literal("Int", quote!(*value)),
            Self::Bool => literal("Bool", quote!(*value)),
            Self::String => literal("String", quote!(value.clone())),
            Self::Kind(ty) => literal("Native", quote!(<#ty>::decode(_db, *value)?)),
            Self::OpId | Self::ValueId | Self::ValueRef(_) => {
                let ty = self.ty();
                literal("Int", quote!(<#ty>::from_raw(u32::try_from(*value)?)))
            }
            Self::BigInt => quote!(match terms.get(*#value) {
                ::tir_egglog::Term::App(name, args) if name == "from-string" => match args.as_slice() {
                    [text] => match terms.get(*text) {
                        ::tir_egglog::Term::Lit(::tir_egglog::ast::Literal::String(text)) => text.parse()?,
                        _ => return Err(invalid()),
                    },
                    _ => return Err(invalid()),
                },
                _ => return Err(invalid()),
            }),
            Self::Enum(name) => quote!(Box::new(#name::decode(_db, terms, *#value)?)),
        }
    }
}

fn codecs<'a>(
    types: impl IntoIterator<Item = &'a Expr>,
    checked: &[&Ty],
    signature: &mut Signature,
    schema: &Schema,
    selected: &[String],
) -> Result<Vec<Codec>, Error> {
    let types = types.into_iter().collect::<Vec<_>>();
    if types.len() != checked.len() {
        return Err(Error(format!(
            "checked signature has {} columns, expected {}",
            checked.len(),
            types.len()
        )));
    }
    signature.reset_parameters();
    types
        .into_iter()
        .zip(checked)
        .map(|(ty, checked)| Codec::read(ty, checked, signature, schema, selected))
        .collect()
}

fn type_parts(mut types: &TyList) -> Vec<&Ty> {
    let mut parts = Vec::new();
    while let TyList::Cons(head, tail) = types {
        parts.push(&**head);
        types = tail;
    }
    parts
}

fn function_inputs(ty: &Ty) -> Vec<&Ty> {
    match ty {
        Ty::Fun(inputs, _) => type_parts(inputs),
        _ => Vec::new(),
    }
}

pub(in crate::engine) fn relation_columns<'a>(source: &Expr, checked: &'a Ty) -> Vec<&'a Ty> {
    match (arrow(source), checked) {
        (Some((input, _)), Ty::Fun(args, _)) => {
            let args = type_parts(args);
            if let Expression::Record(fields) = &input.value
                && let [Ty::Con(_, types)] = args.as_slice()
            {
                let mut names = fields.iter().map(|(name, _)| name).collect::<Vec<_>>();
                names.sort_unstable();
                let types = names
                    .into_iter()
                    .zip(type_parts(types))
                    .collect::<BTreeMap<_, _>>();
                return fields.iter().map(|(name, _)| types[name]).collect();
            }
            args
        }
        (None, Ty::Fun(args, _)) if table_type(source).is_some() => type_parts(args),
        _ => Vec::new(),
    }
}

fn schedule_tokens(schedule: &Schedule) -> TokenStream {
    match schedule {
        Schedule::Run(_, config) => {
            let name = &config.ruleset;
            quote!(::tir_egglog::Schedule::run(#name))
        }
        Schedule::Repeat(_, count, child) => {
            let child = schedule_tokens(child);
            quote!(::tir_egglog::Schedule::repeat(#count, #child))
        }
        Schedule::Saturate(_, child) => {
            let child = schedule_tokens(child);
            quote!(::tir_egglog::Schedule::saturate(#child))
        }
        Schedule::Sequence(_, children) => {
            let children = children.iter().map(schedule_tokens);
            quote!(::tir_egglog::Schedule::seq([#(#children),*]))
        }
    }
}

fn enum_tokens(
    name: &Ident,
    path: &[String],
    variants: &[crate::ast::Variant],
    program: &CheckedProgram,
    signature: &mut Signature,
    schema: &Schema,
    selected: &[String],
) -> Result<TokenStream, Error> {
    let mut declarations = Vec::new();
    let mut cases = Vec::new();
    for variant in variants {
        let case = format_ident!("{}", variant.name);
        let variant_path = [path, std::slice::from_ref(&variant.name)].concat();
        let constructor = schema.name(&variant_path);
        let checked = function_inputs(&program.types()[&variant_path].ty);
        let fields = codecs(
            variant.arguments.as_deref().unwrap_or_default(),
            &checked,
            signature,
            schema,
            selected,
        )?;
        let args = (0..fields.len())
            .map(|i| format_ident!("arg_{i}"))
            .collect::<Vec<_>>();
        let types = fields.iter().map(Codec::field);
        let decoded = fields.iter().zip(&args).map(|(ty, arg)| ty.decode(arg));
        if args.is_empty() {
            declarations.push(quote!(#case));
            cases.push(quote!((#constructor, []) => Ok(Self::#case)));
        } else {
            declarations.push(quote!(#case(#(#types),*)));
            cases.push(quote!((#constructor, [#(#args),*]) => Ok(Self::#case(#(#decoded),*))));
        }
    }
    let owner_qualified = name
        .to_string()
        .contains('_')
        .then(|| quote!(#[allow(non_camel_case_types)]));
    Ok(quote! {
        #[allow(clippy::type_complexity)]
        #[derive(Clone, Debug, PartialEq, Eq)]
        #owner_qualified
        pub enum #name { #(#declarations),* }

        impl #name {
            // The checked enum determines how many constructor arms are needed.
            #[allow(clippy::too_many_lines)]
            fn decode(_db: &::tir_egglog::Database, terms: &::tir_egglog::TermDag, term: ::tir_egglog::TermId) -> Result<Self, Box<dyn std::error::Error>> {
                let invalid = || -> Box<dyn std::error::Error> {
                    concat!("invalid engine term for ", stringify!(#name)).into()
                };
                match terms.get(term) {
                    ::tir_egglog::Term::App(tag, args) => match (tag.as_str(), args.as_slice()) {
                        #(#cases,)*
                        _ => Err(invalid()),
                    },
                    _ => Err(invalid()),
                }
            }
        }
    })
}

fn constructor_tokens(
    program: &CheckedProgram,
    path: &[String],
    name: &Ident,
    variants: &[crate::ast::Variant],
    signature: &mut Signature,
    schema: &Schema,
    selected: &[String],
) -> Result<TokenStream, Error> {
    let mut methods = Vec::new();
    for variant in variants {
        let variant_path = [path, std::slice::from_ref(&variant.name)].concat();
        let checked = function_inputs(&program.types()[&variant_path].ty);
        let fields = codecs(
            variant.arguments.as_deref().unwrap_or_default(),
            &checked,
            signature,
            schema,
            selected,
        )?;
        let constructor = schema.name(&variant_path);
        let short = method(
            &[
                path.strip_prefix(selected).unwrap_or(path),
                std::slice::from_ref(&variant.name),
            ]
            .concat(),
        );
        let lookup = format_ident!("lookup_{short}");
        let rows = format_ident!("rows_{short}");
        let by_result = format_ident!("index_{short}_by_result");
        let args = (0..fields.len())
            .map(|i| format_ident!("arg_{i}"))
            .collect::<Vec<_>>();
        let columns = (0..fields.len()).collect::<Vec<_>>();
        let result = fields.len();
        let types = fields
            .iter()
            .map(|ty| match ty {
                Codec::Enum(name) => quote!(Class<#name>),
                _ => ty.ty(),
            })
            .collect::<Vec<_>>();
        let encoded = fields
            .iter()
            .zip(&args)
            .map(|(ty, arg)| match ty {
                Codec::Enum(_) => quote!(#arg.value_for(db.id())?),
                _ => ty.encode(quote!(db), quote!(#arg)),
            })
            .collect::<Vec<_>>();
        let decoded = fields
            .iter()
            .zip(&args)
            .map(|(ty, arg)| match ty {
                Codec::Enum(_) => quote!(Class::new(db, #arg)),
                _ => ty.base(quote!(db), quote!(#arg)),
            })
            .collect::<Vec<_>>();
        methods.push(quote! {
            /// Index this variant by the resulting class.
            pub fn #by_result(&mut self) -> Result<Index<Class<#name>, (#(#types,)* Class<#name>,)>, ::tir_egglog::Error> {
                let table = self.db.table(#constructor)?;
                Ok(Index {
                    index: self.db.add_kept_index(table, &[#result], &[]),
                    database: self.db.id(),
                    key: |db, key| key.value_for(db.id()),
                    decode: |db, row| {
                        #(let #args = row[#columns];)*
                        Ok((#(#decoded,)* Class::new(db, row[#result]),))
                    },
                })
            }

            /// Look up the class produced by these constructor arguments.
            pub fn #lookup(&self, #(#args: #types),*) -> Result<Option<Class<#name>>, ::tir_egglog::Error> {
                let db = &*self.db;
                let table = db.table(#constructor)?;
                Ok(db.lookup(table, &[#(#encoded),*]).map(|value| Class::new(db, value)))
            }

            /// Read constructor arguments and the resulting class.
            pub fn #rows(&self) -> Result<impl Iterator<Item=DecodeResult<(#(#types,)* Class<#name>,)>> + '_, ::tir_egglog::Error> {
                let db = &*self.db;
                let table = db.table(#constructor)?;
                Ok(db.rows(table).map(move |row| {
                    #(let #args = row[#columns];)*
                    Ok((#(#decoded,)* Class::new(db, row[#result]),))
                }))
            }
        });
    }
    Ok(quote!(#(#methods)*))
}

fn relation_types(ty: &Expr) -> Option<Vec<&Expr>> {
    if let Some((inputs, output)) = arrow(ty) {
        return proposition(output).then(|| match &inputs.value {
            Expression::Record(fields) => fields.iter().map(|(_, field)| field).collect(),
            _ => parts(inputs).iter().collect(),
        });
    }
    if let Expression::Generic { value, arguments } = &ty.value
        && path(value).is_ok_and(|path| path == &["std", "table", "Table"])
        && let [keys, output, _] = arguments.as_slice()
    {
        return Some(parts(keys).iter().chain([output]).collect());
    }
    None
}

fn decode_rows(codecs: &[Codec], rows: TokenStream) -> TokenStream {
    let columns = 0..codecs.len();
    let args = (0..codecs.len())
        .map(|i| format_ident!("column_{i}"))
        .collect::<Vec<_>>();
    let values = codecs
        .iter()
        .zip(&args)
        .map(|(ty, arg)| ty.base(quote!(db), quote!(#arg)));
    let extractor = codecs
        .iter()
        .any(|ty| matches!(ty, Codec::Enum(_)))
        .then(|| quote!(let extractor = std::cell::OnceCell::new();));
    quote! {
        #extractor
        #rows.map(move |row| {
            #(let #args = row[#columns];)*
            Ok((#(#values,)*))
        })
    }
}

fn relation_tokens(
    name: &Ident,
    table: &Ident,
    codecs: &[Codec],
    fields: &[String],
) -> (TokenStream, TokenStream) {
    let in_color = format_ident!("{name}_in");
    let reader = if name == "yield" {
        format_ident!("r#yield")
    } else {
        name.clone()
    };
    let insert = format_ident!("insert_{name}");
    let insert_in = format_ident!("insert_{name}_in");
    let types = codecs.iter().map(Codec::ty).collect::<Vec<_>>();
    let args = (0..codecs.len())
        .map(|i| format_ident!("column_{i}"))
        .collect::<Vec<_>>();
    let decoded = decode_rows(codecs, quote!(db.rows_in(color, self.#table)));
    let columns = (0..codecs.len()).collect::<Vec<_>>();
    let has_enum = codecs.iter().any(|ty| matches!(ty, Codec::Enum(_)));
    let type_name = name
        .to_string()
        .split('_')
        .filter_map(|part| {
            let mut chars = part.chars();
            Some(
                chars
                    .next()?
                    .to_uppercase()
                    .chain(chars)
                    .collect::<String>(),
            )
        })
        .collect::<String>();
    let row = format_ident!("{type_name}Row");
    let mut aliases = quote! {
        /// Columns of the declared CDSL relation.
        #[allow(clippy::type_complexity)]
        pub type #row = (#(#types,)*);
    };
    let class_row = if has_enum {
        format_ident!("{type_name}ClassRow")
    } else {
        row.clone()
    };
    let handles = has_enum.then(|| {
        let class_types = codecs.iter().map(|ty| match ty {
            Codec::Enum(name) => quote!(Class<#name>),
            _ => ty.ty(),
        });
        aliases.extend(quote! {
            /// Columns with e-class identities in place of extracted terms.
            #[allow(clippy::type_complexity)]
            pub type #class_row = (#(#class_types,)*);
        });
        let handles = format_ident!("{name}_classes");
        let decode = codecs.iter().zip(&args).map(|(ty, arg)| match ty {
            Codec::Enum(_) => quote!(Class {
                value: #arg,
                database: db.id(),
                sort: std::marker::PhantomData,
            }),
            _ => ty.base(quote!(db), quote!(#arg)),
        });
        quote! {
            /// Read rows with typed e-class identities in place of extracted terms.
            pub fn #handles(&self, color: ::tir_egglog::Color) -> impl Iterator<Item=DecodeResult<#class_row>> + '_ {
                let db = &*self.db;
                db.rows_in(color, self.#table).map(move |row| {
                    #(let #args = row[#columns];)*
                    Ok((#(#decode,)*))
                })
            }
        }
    });
    let checked = codecs.iter().zip(&args).filter_map(|(ty, arg)| match ty {
        Codec::Enum(_) => Some(quote!(let #arg = #arg.value_for(db.id())?;)),
        _ => None,
    });
    let encoded = codecs.iter().zip(&args).map(|(ty, arg)| match ty {
        Codec::Enum(_) => quote!(#arg),
        _ => ty.encode(quote!(db), quote!(#arg)),
    });
    let indices = fields
        .iter()
        .zip(codecs)
        .enumerate()
        .map(|(column, (field, codec))| {
            let method = format_ident!("index_{name}_by_{field}");
            let key_type = match codec {
                Codec::Enum(name) => quote!(Class<#name>),
                _ => codec.ty(),
            };
            let encoded_key = match codec {
                Codec::Enum(_) => quote!(key.value_for(db.id())),
                _ => {
                    let value = codec.encode(quote!(db), quote!(key));
                    quote!(Ok(#value))
                }
            };
            let decode = codecs.iter().zip(&args).map(|(codec, value)| match codec {
                Codec::Enum(_) => quote!(Class::new(db, #value)),
                _ => codec.base(quote!(db), quote!(#value)),
            });
            quote! {
                /// Build a kept index keyed by this declared column.
                pub fn #method(&mut self) -> Index<#key_type, #class_row> {
                    Index {
                        index: self.db.add_kept_index(self.#table, &[#column], &[]),
                        database: self.db.id(),
                        key: |db, key| #encoded_key,
                        decode: |db, row| {
                            #(let #args = row[#columns];)*
                            Ok((#(#decode,)*))
                        },
                    }
                }
            }
        });
    let methods = quote! {
        #(#indices)*

        /// Read the rows in the base color.
        pub fn #reader(&self) -> impl Iterator<Item=DecodeResult<#row>> + '_ {
            self.#in_color(::tir_egglog::Color::BASE)
        }

        /// Read the rows visible in the given color.
        pub fn #in_color(&self, color: ::tir_egglog::Color) -> impl Iterator<Item=DecodeResult<#row>> + '_ {
            let db = &*self.db;
            #decoded
        }

        #handles

        /// Insert one row in the base color.
        pub fn #insert(&mut self, row: #class_row) -> Result<(), ::tir_egglog::Error> {
            self.#insert_in(::tir_egglog::Color::BASE, row)
        }

        /// Insert one row in the given color.
        pub fn #insert_in(&mut self, color: ::tir_egglog::Color, (#(#args,)*): #class_row) -> Result<(), ::tir_egglog::Error> {
            let db = &mut *self.db;
            #(#checked)*
            let row = [#(#encoded),*];
            db.insert_in(color, self.#table, &row)
        }
    };
    (aliases, methods)
}

fn host_tokens(
    (path, inputs, output): Host<'_>,
    program: &CheckedProgram,
    signature: &mut Signature,
    selected: &[String],
    schema: &Schema,
) -> Result<TokenStream, Error> {
    let name = format_ident!(
        "bind_{}",
        method(path.strip_prefix(selected).unwrap_or(path))
    );
    let engine = schema.name(path);
    let sort = |expr| {
        schema
            .sort(expr)
            .map(|id| schema.database().schema().sort(id).name())
    };
    let input_sorts = inputs.iter().map(sort).collect::<Result<Vec<_>, _>>()?;
    let output_sort = sort(output)?;
    let Ty::Fun(checked_inputs, checked_output) = &program.types()[path].ty else {
        return Err(Error(format!("host primitive {engine} must be a function")));
    };
    let inputs = codecs(
        inputs,
        &type_parts(checked_inputs),
        signature,
        schema,
        selected,
    )?;
    let output = Codec::read(output, checked_output, signature, schema, selected)?;
    if inputs
        .iter()
        .chain([&output])
        .any(|ty| matches!(ty, Codec::Enum(_)))
    {
        return Err(Error(format!("host primitive {engine} uses an enum")));
    }
    let args = (0..inputs.len())
        .map(|i| format_ident!("arg_{i}"))
        .collect::<Vec<_>>();
    let input_types = inputs.iter().map(Codec::ty);
    let output_type = output.ty();
    let decode = inputs.iter().zip(&args).map(|(ty, arg)| match ty {
        Codec::OpId | Codec::ValueId | Codec::ValueRef(_) => {
            let ty = ty.ty();
            quote!(<#ty>::from_raw(u32::try_from(p.base_value::<i64>(*#arg)).ok()?))
        }
        _ => ty.base(quote!(p), quote!(*#arg)),
    });
    let encoded = output.encode(quote!(p), quote!(output));
    Ok(quote! {
        /// Bind the implementation of the declared host function.
        #[allow(clippy::type_complexity)]
        pub fn #name(
            db: &mut ::tir_egglog::Database,
            implementation: impl Fn(&::tir_egglog::PrimCtx<'_>, #(#input_types),*) -> Option<#output_type> + Send + Sync + 'static,
        ) -> Result<(), ::tir_egglog::Error> {
            let signature = ::tir_egglog::Signature {
                inputs: vec![#(db.sort(#input_sorts)?),*],
                output: db.sort(#output_sort)?,
            };
            db.add_primitive(#engine, signature, move |p, values| {
                let [#(#args),*] = values else { return None };
                let output = implementation(p, #(#decode),*)?;
                Some(#encoded)
            });
            Ok(())
        }
    })
}

fn ir_type_tokens() -> TokenStream {
    let ids = [format_ident!("OperationId"), format_ident!("ValueId")].map(|id| {
        quote! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
            pub struct #id(u32);

            impl #id {
                /// Convert a host IR entity number to its declared ID type.
                pub const fn from_raw(raw: u32) -> Self { Self(raw) }
                /// Retrieve the host IR entity number.
                pub const fn raw(self) -> u32 { self.0 }
            }
        }
    });
    quote! {
        #(#ids)*

        /// A value reference whose type is declared in CDSL.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct ValueRef<T>(u32, std::marker::PhantomData<fn(T) -> T>);

        impl<T> ValueRef<T> {
            fn from_raw(raw: u32) -> Self {
                Self(raw, std::marker::PhantomData)
            }

            /// Retrieve the host IR entity number.
            pub fn raw(self) -> u32 { self.0 }
        }

        /// An operation declaration without a port signature refinement.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct OperationKind(::tir_egglog::NativeIdentity);

        impl From<::tir_egglog::NativeIdentity> for OperationKind {
            fn from(declaration: ::tir_egglog::NativeIdentity) -> Self { Self(declaration) }
        }

        impl OperationKind {
            fn decode(_: &::tir_egglog::Database, identity: ::tir_egglog::NativeIdentity) -> DecodeResult<Self> {
                Ok(Self(identity))
            }

            pub fn declaration(&self) -> ::tir_egglog::NativeIdentity { self.0 }

            /// Retrieve the operation's diagnostic spelling.
            pub fn as_str(&self) -> &str { self.0.as_str() }
        }

        type Invariant<T> = std::marker::PhantomData<fn(T) -> T>;
        struct KindWitness<T>(Invariant<T>);

        /// A declaration admitted by the checked operand and result signature.
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct OperationKindOf<I, O>(::tir_egglog::NativeIdentity, Invariant<(I, O)>);

        impl<I: 'static, O: 'static> OperationKindOf<I, O> {
            fn decode(db: &::tir_egglog::Database, identity: ::tir_egglog::NativeIdentity) -> DecodeResult<Self> {
                if db.admits_declaration::<KindWitness<Self>>(identity) {
                    Ok(Self(identity, std::marker::PhantomData))
                } else {
                    Err("operation declaration does not have the checked port signature".into())
                }
            }

            fn admit(db: &mut ::tir_egglog::Database, identity: ::tir_egglog::NativeIdentity) {
                db.admit_declaration::<KindWitness<Self>>(identity);
            }

            pub fn declaration(&self) -> ::tir_egglog::NativeIdentity { self.0 }

            /// Retrieve the operation's diagnostic spelling.
            pub fn as_str(&self) -> &str { self.0.as_str() }
        }
    }
}

pub(super) fn exported_members(
    program: &CheckedProgram,
    selected: &[String],
) -> BTreeMap<Path, Path> {
    let mut members = BTreeMap::new();
    let exports = &program.artifact().module_exports;
    let mut pending = vec![(
        Vec::new(),
        selected.to_vec(),
        BTreeSet::from([selected.to_vec()]),
    )];
    while let Some((parent, module, active)) = pending.pop() {
        let Some(exports) = exports.get(&module) else {
            continue;
        };
        for (name, target) in exports {
            let visible = [parent.as_slice(), std::slice::from_ref(name)].concat();
            if program.artifact().module_exports.contains_key(target) {
                if !active.contains(target) && target.first().is_none_or(|part| part != "std") {
                    let mut active = active.clone();
                    active.insert(target.clone());
                    pending.push((visible, target.clone(), active));
                }
            } else {
                members.insert(visible, target.clone());
            }
        }
    }
    members
}

fn native_import_tokens(
    program: &CheckedProgram,
    imports: &BTreeMap<Path, OperationImport>,
    uses: &[(TokenStream, Ty)],
) -> (TokenStream, TokenStream) {
    if imports.is_empty() {
        return Default::default();
    }
    let tags = (0..imports.len())
        .map(|index| format_ident!("Declaration{index}"))
        .collect::<Vec<_>>();
    let imported = imports.values().zip(&tags).map(|(import, tag)| {
        let name = &import.name;
        let symbol = &import.symbol;
        let ports = import.ports.map_or_else(
            || quote!(None),
            |(inputs, outputs)| quote!(Some((#inputs, #outputs))),
        );
        quote! {
            struct #tag;
            impl ::tir_egglog::NativeDeclaration for #tag {
                const NAME: &'static str = #name;
                const SYMBOL: &'static str = #symbol;
                const PORTS: Option<(usize, usize)> = #ports;
            }
        }
    });
    let mut witnesses = BTreeMap::new();
    for (rust_type, checked) in uses {
        let matching = imports
            .keys()
            .zip(&tags)
            .filter_map(|(path, tag)| program.admits_kind(path, checked).then_some(tag));
        witnesses
            .entry(rust_type.to_string())
            .or_insert_with(|| (rust_type, BTreeSet::new()))
            .1
            .extend(matching);
    }
    let admissions = witnesses.into_values().filter(|(_, tags)| !tags.is_empty()).map(|(rust_type, declarations)| {
        let tags = declarations.into_iter();
        quote!((<#rust_type>::admit, &[#(<#tags as ::tir_egglog::NativeDeclaration>::SYMBOL),*]))
    });
    let declarations = quote! {
        #(#imported)*
        fn link_declaration<T: ::tir_egglog::NativeDeclaration>(
            db: &mut ::tir_egglog::Database,
        ) -> Result<(), ::tir_egglog::LoadError> {
            if db.declaration(T::SYMBOL).is_none() {
                db.bind_declaration::<T>().map_err(|_| ::tir_egglog::LoadError::SortMismatch {
                    name: "NativeIdentity".to_owned(),
                })?;
            }
            Ok(())
        }
        type DeclarationLink = fn(&mut ::tir_egglog::Database) -> Result<(), ::tir_egglog::LoadError>;
        const DECLARATION_LINKS: &[DeclarationLink] = &[#(link_declaration::<#tags>),*];
        type KindAdmission = fn(&mut ::tir_egglog::Database, ::tir_egglog::NativeIdentity);
        const KIND_ADMISSIONS: &[(KindAdmission, &[&str])] = &[#(#admissions),*];
    };
    let install = quote! {
        for link in DECLARATION_LINKS {
            link(db)?;
        }
        for (admit, symbols) in KIND_ADMISSIONS {
            for symbol in *symbols {
                if let Some(identity) = db.declaration(symbol) {
                    admit(db, identity);
                }
            }
        }
    };
    (declarations, install)
}

fn schedule_api(
    name: &Ident,
    schedule: &Schedule,
    (declaration, ty): (Option<&Declaration>, &Ty),
    signature: &mut Signature,
    schema: &Schema,
    selected: &[String],
) -> Result<(Option<TokenStream>, TokenStream), Error> {
    let schedule = schedule_tokens(schedule);
    let parameters = match declaration {
        Some(Declaration::Let { binding, .. }) => match &binding.value.value {
            Expression::Function(function) => function.parameters.as_slice(),
            _ => &[],
        },
        _ => &[],
    };
    let checked = function_inputs(ty);
    let codecs = codecs(
        parameters.iter().map(|binder| &binder.ty),
        &checked,
        signature,
        schema,
        selected,
    )?;
    let arguments = parameters
        .iter()
        .map(|binder| format_ident!("r#{}", binder.name))
        .collect::<Vec<_>>();
    let types = codecs.iter().map(Codec::ty);
    let values = codecs
        .iter()
        .zip(&arguments)
        .map(|(codec, arg)| codec.encode(quote!(self.db), quote!(#arg)));
    let (attached, invocation) = if parameters.is_empty() {
        let accessor = format_ident!("schedule_{name}");
        let run_on = format_ident!("run_{name}");
        (
            Some(quote! {
                /// Construct the declared CDSL schedule.
                pub fn #accessor() -> ::tir_egglog::Schedule {
                    #schedule
                }

                /// Run this program's attached schedule.
                pub fn #run_on(engine: &mut ::tir_egglog::EGraph) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error> {
                    engine.run_schedule(&#accessor())
                }
            }),
            quote!(#accessor()),
        )
    } else {
        (None, schedule)
    };
    let methods = quote! {
        /// Run the declared CDSL schedule.
        pub fn #name(&mut self, #(#arguments: #types),*) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error> {
            let args = [#(#values),*];
            self.code.run(self.db, &#invocation, &args)
        }
    };
    Ok((attached, methods))
}

fn executable_variants<'a>(
    schema: &Schema,
    path: &[String],
    declaration: &'a Declaration,
) -> Option<&'a [crate::ast::Variant]> {
    let Declaration::Enum { variants, .. } = declaration else {
        return None;
    };
    schema.database().schema().sort_named(&schema.name(path))?;
    Some(variants)
}

fn enum_api(
    program: &CheckedProgram,
    schema: &Schema,
    selected: &[String],
    declarations: &BTreeMap<Path, &Declaration>,
    signature: &mut Signature,
) -> Result<(Vec<TokenStream>, Vec<TokenStream>), Error> {
    let mut enums = Vec::new();
    let mut methods = Vec::new();
    for (path, declaration) in declarations {
        if let Some(variants) = executable_variants(schema, path, declaration) {
            enums.push(enum_tokens(
                &enum_name(path, selected),
                path,
                variants,
                program,
                signature,
                schema,
                selected,
            )?);
            methods.push(constructor_tokens(
                program,
                path,
                &enum_name(path, selected),
                variants,
                signature,
                schema,
                selected,
            )?);
        }
    }
    Ok((enums, methods))
}

pub(super) fn emit(
    program: &CheckedProgram,
    selected: &[String],
    schema: &Schema,
    lowered: Option<&LoweredProgram>,
    binary: Option<&[u8]>,
    hosts: &[Host<'_>],
) -> Result<String, Error> {
    let mut signature = Signature::new(program);
    let mut accessors = Vec::new();
    let mut rows = Vec::new();
    let mut tables = Vec::new();
    let declarations = program
        .artifact()
        .all_modules()
        .flat_map(|module| {
            module.ast.declarations.iter().filter_map(|declaration| {
                let name = match &declaration.value {
                    Declaration::Axiom { name, .. } | Declaration::Enum { name, .. } => {
                        std::slice::from_ref(name)
                    }
                    Declaration::Let { binding, .. } => binding.path.as_slice(),
                    _ => return None,
                };
                Some(([module.path.as_slice(), name].concat(), &declaration.value))
            })
        })
        .collect::<BTreeMap<_, _>>();
    let (mut enums, mut methods) =
        enum_api(program, schema, selected, &declarations, &mut signature)?;
    for (visible, path) in exported_members(program, selected) {
        let name = method(&visible);
        if declarations
            .get(&path)
            .is_some_and(|declaration| executable_variants(schema, &path, declaration).is_some())
        {
            let enum_name = enum_name(&path, selected);
            let alias = format_ident!("{}", visible.join("_"));
            enums.extend((alias != enum_name).then(|| quote!(pub use #enum_name as #alias;)));
        } else if let Some(schedule) = lowered.and_then(|lowered| lowered.schedules.get(&path)) {
            let (attached, callable) = schedule_api(
                &name,
                schedule,
                (declarations.get(&path).copied(), &program.types()[&path].ty),
                &mut signature,
                schema,
                selected,
            )?;
            accessors.extend(attached);
            methods.push(callable);
        } else if let Some(Declaration::Axiom { ty, .. }) = declarations.get(&path)
            && let Some(types) = relation_types(ty)
            && schema
                .database()
                .schema()
                .table_named(&schema.name(&path))
                .is_some()
        {
            let table = format_ident!("table_{}", tables.len());
            let checked = relation_columns(ty, &program.types()[&path].ty);
            let codecs = codecs(types, &checked, &mut signature, schema, selected)
                .map_err(|err| Error(format!("{}: {err}", path_name(&path))))?;
            let fields = schema
                .named_relations
                .get(&schema.name(&path))
                .map(Vec::as_slice)
                .unwrap_or_default();
            let (row, readers) = relation_tokens(&name, &table, &codecs, fields);
            rows.push(row);
            methods.push(readers);
            let engine_name = schema.name(&path);
            let access = format_ident!("table_{name}");
            accessors.push(quote! {
                /// Resolve this relation in the caller's database.
                pub fn #access(db: &::tir_egglog::Database) -> Result<::tir_egglog::TableId, ::tir_egglog::Error> {
                    db.table(#engine_name)
                }
            });
            tables.push((table, engine_name));
        }
    }
    let (fields, names): (Vec<_>, Vec<_>) = tables.into_iter().unzip();
    let catalog = RustLiteral::byte_string(&schema.to_bytes()?);
    let code = binary.map(RustLiteral::byte_string);
    let code_declaration = code.as_ref().map(|bytes| quote! {
        static CODE: &[u8] = #bytes;

        /// Attach the rules to an engine that shares this schema.
        pub fn attach(engine: &mut ::tir_egglog::EGraph) -> Result<(), Box<dyn std::error::Error>> {
            install_schema(engine.database_mut())?;
            engine.attach_program(CODE)?;
            Ok(())
        }
    });
    let code_field = code.as_ref().map(|_| quote!(code: ::tir_egglog::Program,));
    let load_code = code
        .as_ref()
        .map(|_| quote!(let code = ::tir_egglog::Program::from_static(CODE, db)?;));
    let code_init = code.as_ref().map(|_| quote!(code,));
    let host = hosts
        .iter()
        .map(|host| host_tokens(*host, program, &mut signature, selected, schema))
        .collect::<Result<Vec<_>, _>>()?;
    let (native_declarations, native_imports) =
        native_import_tokens(program, &schema.operation_imports, &signature.kind_uses);
    let ir_types = ir_type_tokens();
    let signatures = signature.declarations();
    let source = quote! {
        #ir_types
        #signatures
        #native_declarations
        #(#enums)*
        #(#rows)*
        #(#host)*

        type DecodeResult<T> = Result<T, Box<dyn std::error::Error>>;

        /// A typed identity in the program's database.
        pub struct Class<T> {
            value: ::tir_egglog::Value,
            database: ::tir_egglog::DatabaseId,
            sort: std::marker::PhantomData<fn() -> T>,
        }

        impl<T> Copy for Class<T> {}
        impl<T> Clone for Class<T> {
            fn clone(&self) -> Self { *self }
        }

        impl<T> PartialEq for Class<T> {
            fn eq(&self, other: &Self) -> bool {
                self.database == other.database && self.value == other.value
            }
        }
        impl<T> Eq for Class<T> {}
        impl<T> PartialOrd for Class<T> {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl<T> Ord for Class<T> {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                (self.database, self.value).cmp(&(other.database, other.value))
            }
        }

        impl<T> Class<T> {
            fn new(db: &::tir_egglog::Database, value: ::tir_egglog::Value) -> Self {
                Self { value, database: db.id(), sort: std::marker::PhantomData }
            }

            fn value_for(self, database: ::tir_egglog::DatabaseId) -> Result<::tir_egglog::Value, ::tir_egglog::Error> {
                if self.database != database {
                    return Err(::tir_egglog::Error::ForeignClass);
                }
                Ok(self.value)
            }
        }

        type EncodeKey<K> = fn(&::tir_egglog::Database, K) -> Result<::tir_egglog::Value, ::tir_egglog::Error>;
        type DecodeRow<R> = fn(&::tir_egglog::Database, ::tir_egglog::Row<'_>) -> DecodeResult<R>;

        /// Index over one declared column of a relation.
        pub struct Index<K, R> {
            index: ::tir_egglog::IndexId,
            database: ::tir_egglog::DatabaseId,
            key: EncodeKey<K>,
            decode: DecodeRow<R>,
        }

        static CATALOG: &[u8] = #catalog;
        #code_declaration
        #(#accessors)*

        /// Install the declared catalog on the caller's database.
        pub fn install_schema(db: &mut ::tir_egglog::Database) -> Result<(), ::tir_egglog::LoadError> {
            #native_imports
            ::tir_egglog::Program::from_static(CATALOG, db)?;
            Ok(())
        }

        /// A loaded CDSL program operating on the caller's database.
        pub struct Program<'db> {
            db: &'db mut ::tir_egglog::Database,
            #code_field
            #(#fields: ::tir_egglog::TableId,)*
        }

        // Method types include the full checked signatures from CDSL.
        #[allow(clippy::type_complexity)]
        impl<'db> Program<'db> {
            /// Install the schema and bind this program to the existing database.
            pub fn load(db: &'db mut ::tir_egglog::Database) -> Result<Self, Box<dyn std::error::Error>> {
                install_schema(db)?;
                #load_code
                #(let #fields = db.table(#names)?;)*
                Ok(Self { db, #code_init #(#fields,)* })
            }

            /// Create a color that inherits facts from the given parents.
            pub fn color(&mut self, parents: &[::tir_egglog::Color]) -> ::tir_egglog::Color {
                self.db.color(parents)
            }

            /// Equate two classes of the same declared sort in a color.
            pub fn union_in<T>(&mut self, color: ::tir_egglog::Color, left: Class<T>, right: Class<T>) -> Result<(), ::tir_egglog::Error> {
                let database = self.db.id();
                self.db.union_in(color, left.value_for(database)?, right.value_for(database)?);
                Ok(())
            }

            /// Read the rows matching a key in the typed index.
            pub fn listed<'a, K, R>(&'a self, index: &Index<K, R>, key: K) -> Result<impl Iterator<Item=DecodeResult<R>> + use<'a, K, R>, ::tir_egglog::Error> {
                let db: &'a ::tir_egglog::Database = self.db;
                if index.database != db.id() {
                    return Err(::tir_egglog::Error::ForeignClass);
                }
                let key = (index.key)(db, key)?;
                let decode = index.decode;
                Ok(db.listed(index.index, &[key]).map(move |row| decode(db, row)))
            }

            /// Apply pending insertions and rebuild the engine's equivalence classes.
            pub fn rebuild(&mut self) -> Result<(), ::tir_egglog::Error> {
                self.db.rebuild()
            }
            #(#methods)*
        }
    };
    Ok(prettyplease::unparse(&syn::parse2(source).map_err(error)?))
}
