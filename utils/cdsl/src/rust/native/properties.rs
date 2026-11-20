use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::Ident;

use super::{Operation, boolean, entries, field, find_field, natural, parse_rust, string};
use crate::{Error, ast::Expr};

pub(super) struct Attribute<'a> {
    pub(super) name: &'a str,
    pub(super) method: Ident,
    ty: &'a str,
    vocabulary: Option<Ident>,
    pub(super) immediate: Option<Immediate>,
}

pub(super) struct Immediate {
    pub(super) brand: Ident,
    bits: u16,
    align: u32,
    nonzero: bool,
    allow_fixups: bool,
    allow_text: bool,
    unsigned: bool,
}

impl Immediate {
    fn constraint(&self) -> TokenStream {
        let Self {
            bits,
            align,
            nonzero,
            ..
        } = self;
        quote! { tir::backend::operand::ImmediateConstraint {
            bits: #bits, align: #align, nonzero: #nonzero,
        } }
    }

    pub(super) fn declaration(&self) -> TokenStream {
        let brand = &self.brand;
        let constraint = self.constraint();
        let (ty, variant) = if self.unsigned {
            (quote! { u64 }, quote! { UInt })
        } else {
            (quote! { i64 }, quote! { Int })
        };
        quote! {
            /// A numeric immediate within the declared domain of this instruction slot.
            #[derive(Clone, Copy, Debug, Eq, PartialEq)]
            pub struct #brand(#ty);

            impl #brand {
                pub(super) const CONSTRAINT: tir::backend::operand::ImmediateConstraint = #constraint;
            }

            impl TryFrom<#ty> for #brand {
                type Error = tir::Error;

                fn try_from(value: #ty) -> Result<Self, Self::Error> {
                    tir::backend::operand::check_immediate(
                        Self::CONSTRAINT, &tir::attributes::AttributeValue::#variant(value),
                    )?;
                    Ok(Self(value))
                }
            }

            impl From<#brand> for tir::attributes::AttributeValue {
                fn from(value: #brand) -> Self { Self::#variant(value.0) }
            }
        }
    }
}

impl<'a> Attribute<'a> {
    pub(super) fn parse(name: &'a str, source: &'a str) -> Result<Self, Error> {
        let (ty, vocabulary) = match source.split_once(" in ") {
            Some((ty, vocabulary)) => (ty, Some(parse_rust(vocabulary)?)),
            None => (source, None),
        };
        Ok(Self {
            name,
            method: parse_rust(&format!("r#{name}"))?,
            ty,
            vocabulary,
            immediate: None,
        })
    }

    pub(super) fn set_immediate(&mut self, op: &Ident, metadata: &Expr) -> Result<(), Error> {
        let unsigned = match self.ty.strip_prefix('?').unwrap_or(self.ty) {
            "Int" => false,
            "UInt" => true,
            _ => {
                return Err(Error(format!(
                    "immediate {} must have an integer type",
                    self.name
                )));
            }
        };
        let fields = entries(metadata)?;
        let suffix = self
            .name
            .split('_')
            .filter_map(|word| {
                let mut chars = word.chars();
                Some(chars.next()?.to_uppercase().chain(chars))
            })
            .flatten()
            .collect::<String>();
        let brand = format_ident!("{op}{suffix}");
        self.immediate = Some(Immediate {
            brand,
            bits: natural(field(fields, "bits")?)?,
            align: natural(field(fields, "align")?)?,
            nonzero: boolean(field(fields, "nonzero")?)?,
            allow_fixups: find_field(fields, "allow_fixups")
                .map(boolean)
                .transpose()?
                .unwrap_or_default(),
            allow_text: find_field(fields, "allow_text")
                .map(boolean)
                .transpose()?
                .unwrap_or_default(),
            unsigned,
        });
        Ok(())
    }

    pub(super) fn optional(&self) -> bool {
        self.ty.starts_with('?')
    }

    pub(super) fn schema(&self) -> TokenStream {
        let Self {
            name,
            ty,
            vocabulary,
            ..
        } = self;
        let ty = match (self.immediate.is_some(), self.optional()) {
            (true, true) => "?any",
            (true, false) => "any",
            (false, _) => ty,
        };
        let vocabulary = match vocabulary {
            Some(name) => quote! { tir::attributes::Predicate::#name },
            None => quote! { &[] },
        };
        quote! { tir::AttrSchema { name: #name, ty: #ty, vocabulary: #vocabulary } }
    }

    pub(super) fn immediate_check(&self, op: &str) -> Option<TokenStream> {
        let immediate = self.immediate.as_ref()?;
        let brand = &immediate.brand;
        let name = self.name;
        let invalid = format!("{op} immediate '{name}'");
        let string = if immediate.allow_text {
            quote! {
                tir::attributes::AttributeValue::Str(value) =>
                    tir::backend::operand::check_text_immediate(#brand::CONSTRAINT, value),
            }
        } else if immediate.allow_fixups {
            quote! { tir::attributes::AttributeValue::Str(_) => Ok(()), }
        } else {
            TokenStream::new()
        };
        let block = immediate.allow_fixups.then(|| {
            quote! {
                tir::attributes::AttributeValue::Block(_) => Ok(()),
            }
        });
        Some(quote! {
            if let Some(attribute) = tir::attributes::find_attribute(context, attributes, #name) {
                match &attribute.value {
                    #string
                    #block
                    value => tir::backend::operand::check_immediate(#brand::CONSTRAINT, value),
                }.map_err(|error| tir::Error::VerificationError(format!("{}: {}", #invalid, error)))?;
            }
        })
    }

    pub(super) fn accessors(&self, dialect: &str, op: &str) -> (TokenStream, TokenStream) {
        if let Some(immediate) = &self.immediate {
            let brand = &immediate.brand;
            let method = &self.method;
            let name = self.name;
            let (ty, read) = if immediate.unsigned {
                (
                    quote! { u64 },
                    quote! {
                        match tir::Operation::attr(self, #name)? {
                            tir::attributes::AttributeValue::UInt(value) => Some(value),
                            tir::attributes::AttributeValue::Int(value) => u64::try_from(value).ok(),
                            _ => None,
                        }
                    },
                )
            } else {
                (
                    quote! { i64 },
                    quote! { tir::Operation::attr(self, #name)?.as_int() },
                )
            };
            return (
                quote! {
                    pub fn #method(self, value: #brand) -> Self { self.attr(#name, value.into()) }
                },
                quote! {
                    pub fn #method(&self) -> Option<#ty> { #read }
                },
            );
        }
        let ty = self.ty.strip_prefix('?').unwrap_or(self.ty);
        let (variant, getter_type, setter_type, read, write) = if ty == "Str" {
            (
                "Str",
                quote! { String },
                quote! { impl Into<Box<str>> },
                quote! { value.to_string() },
                quote! { value.into() },
            )
        } else {
            let (variant, value_type) = match ty {
                "Int" => ("Int", quote! { i64 }),
                "UInt" => ("UInt", quote! { u64 }),
                "Bool" | "Flag" => ("Bool", quote! { bool }),
                "Type" => ("Type", quote! { tir::TypeId }),
                "Block" => ("Block", quote! { tir::BlockId }),
                "Predicate" => ("Predicate", quote! { tir::attributes::Predicate }),
                _ => return (TokenStream::new(), TokenStream::new()),
            };
            (
                variant,
                value_type.clone(),
                value_type,
                quote! { value },
                quote! { value },
            )
        };
        let variant = format_ident!("{variant}");
        let method = &self.method;
        let name = self.name;
        let missing = format!("{dialect}.{op} must carry a {} attribute '{name}'", self.ty);
        let setter = quote! {
            pub fn #method(self, value: #setter_type) -> Self {
                self.attr(#name, tir::attributes::AttributeValue::#variant(#write))
            }
        };
        let getter = if self.optional() {
            quote! {
                pub fn #method(&self) -> Option<#getter_type> {
                    match tir::Operation::attr(self, #name) {
                        Some(tir::attributes::AttributeValue::#variant(value)) => Some(#read),
                        _ => None,
                    }
                }
            }
        } else {
            quote! {
                pub fn #method(&self) -> #getter_type {
                    match tir::Operation::attr(self, #name) {
                        Some(tir::attributes::AttributeValue::#variant(value)) => #read,
                        _ => panic!(#missing),
                    }
                }
            }
        };
        (setter, getter)
    }
}

pub(super) enum Region {
    Blocks(Ident),
    Nodes(Ident),
    Any(Ident),
    VariadicNodes(Ident),
}

pub(super) struct RegionBuilder {
    pub(super) field: TokenStream,
    pub(super) setter: TokenStream,
    pub(super) getter: TokenStream,
    pub(super) collect: TokenStream,
    pub(super) parameter: TokenStream,
    pub(super) argument: TokenStream,
}

impl Region {
    pub(super) fn parse(name: &str, value: &Expr) -> Result<Self, Error> {
        let name = parse_rust(&format!("r#{name}"))?;
        let fields = entries(value)?;
        let kind = string(field(fields, "kind")?)?;
        let variadic = boolean(field(fields, "variadic")?)?;
        match (kind, variadic) {
            ("Blocks", false) => Ok(Self::Blocks(name)),
            ("Nodes", false) => Ok(Self::Nodes(name)),
            ("Any", false) => Ok(Self::Any(name)),
            ("Nodes", true) => Ok(Self::VariadicNodes(name)),
            _ => Err(Error(format!("unsupported Rust operation region {kind}"))),
        }
    }

    pub(super) fn name(&self) -> &Ident {
        match self {
            Self::Blocks(name)
            | Self::Nodes(name)
            | Self::Any(name)
            | Self::VariadicNodes(name) => name,
        }
    }

    pub(super) fn kind(&self) -> TokenStream {
        match self {
            Self::Blocks(_) => quote! { tir::RegionKind::Blocks },
            Self::Nodes(_) | Self::VariadicNodes(_) => quote! { tir::RegionKind::Nodes },
            Self::Any(_) => quote! { tir::RegionKind::Any },
        }
    }

    pub(super) fn builder(&self, index: usize) -> RegionBuilder {
        let name = self.name();
        if matches!(self, Self::VariadicNodes(_)) {
            return RegionBuilder {
                field: quote! { #name: Vec<tir::RegionId>, },
                setter: quote! {
                    pub fn #name(mut self, ids: Vec<tir::RegionId>) -> Self {
                        self.ports.#name = ids;
                        self
                    }
                },
                getter: quote! {
                    pub fn #name(&self) -> tir::RegionIds { self.0.regions()[#index..].into() }
                },
                collect: quote! { regions.extend(self.ports.#name); },
                parameter: quote! { #name: Vec<tir::RegionId>, },
                argument: quote! { builder = builder.#name(#name); },
            };
        }

        let (getter, collect) = if matches!(self, Self::Nodes(_)) {
            let missing = format!(
                "Region '{}' is not set",
                name.to_string().trim_start_matches("r#")
            );
            (
                quote! {
                    pub fn #name(&self) -> tir::RegionHandle {
                        tir::Operation::regions(self).nth(#index).unwrap()
                    }
                },
                quote! { regions.push(self.ports.#name.expect(#missing)); },
            )
        } else {
            (
                quote! {
                    pub fn #name(&self) -> tir::BlockHandle {
                        tir::Operation::regions(self).nth(#index).unwrap()
                            .iter(self.0.context.clone()).next().unwrap()
                    }
                },
                quote! {
                    regions.push(self.ports.#name.unwrap_or_else(|| {
                        let region = self.context.create_region();
                        let block = self.context.create_block(vec![]);
                        region.add_block(block.id());
                        region.id()
                    }));
                },
            )
        };
        RegionBuilder {
            field: quote! { #name: Option<tir::RegionId>, },
            setter: quote! {
                pub fn #name(mut self, id: tir::RegionId) -> Self {
                    self.ports.#name = Some(id);
                    self
                }
            },
            getter,
            collect,
            parameter: quote! { #name: Option<tir::RegionId>, },
            argument: quote! {
                if let Some(region) = #name { builder = builder.#name(region); }
            },
        }
    }
}

impl Operation<'_> {
    pub(super) fn generic_parse(&self, builder: &Ident) -> TokenStream {
        let mut operand_parser = TokenStream::new();
        let mut position = 0;
        let mut state_parser = TokenStream::new();
        for operand in &self.operands {
            let name = format_ident!("r#{}", operand.name);
            if operand.state() {
                let method = if operand.name == "state" {
                    format_ident!("state_operands")
                } else {
                    name
                };
                state_parser =
                    quote! { builder = builder.#method(parser.parse_state_operands(context)?); };
                continue;
            }
            if position > 0 {
                operand_parser.extend(quote! { parser.parse_token(","); });
            }
            position += 1;
            if operand.variadic() {
                operand_parser.extend(quote! {
                    let mut values = Vec::new();
                    while let Some(reference) = parser.parse_value_ref() {
                        values.push(parser.resolve_value(context, reference));
                        if !parser.parse_token(",") { break; }
                    }
                    builder = builder.#name(values);
                });
            } else {
                operand_parser.extend(quote! {
                    if let Some(reference) = parser.parse_value_ref() {
                        builder = builder.#name(parser.resolve_value(context, reference));
                    }
                });
            }
        }
        let result_parser = self.results.iter().find(|slot| !slot.state()).map(|result| {
            let set_result = if result.variadic() {
                quote! { builder = builder.result_types(vec![ty]); }
            } else {
                quote! { builder = builder.result_type(ty); }
            };
            quote! {
                if !parser.parse_token(":") {
                    return Err((tir::parse::common::Cursor::span(parser), tir::Error::ExpectedToken(":")));
                }
                let ty = parser.parse_type(context)?.ok_or_else(|| (tir::parse::common::Cursor::span(parser), tir::Error::ExpectedType))?;
                #set_result
            }
        });
        let region_parser = match self.regions.as_slice() {
            [region] if !matches!(region, Region::VariadicNodes(_)) => {
                let name = region.name();
                quote! { builder = builder.#name(parser.parse_region(context)?.id()); }
            }
            _ => TokenStream::new(),
        };
        let attribute_parser = self.attribute_parser();
        let build = if self.assembly.is_some() {
            quote! {
                builder.try_build().map_err(|error| (tir::parse::common::Cursor::span(parser), error))?
            }
        } else {
            quote! { builder.build() }
        };
        quote! {
            let mut builder = #builder::new(context);
            #operand_parser
            #state_parser
            #attribute_parser
            #result_parser
            #region_parser
            for attribute in parsed_attributes {
                builder = builder.attr_sym(attribute.name, attribute.value);
            }
            Ok(Box::new(#build))
        }
    }

    fn attribute_parser(&self) -> TokenStream {
        let unsigned = self
            .attributes
            .iter()
            .filter(|attribute| attribute.ty == "UInt")
            .map(|attribute| attribute.name);
        let mut normalize = TokenStream::new();
        for name in unsigned {
            normalize.extend(quote! {
                if name == #name && let tir::attributes::AttributeValue::Int(integer) = value {
                    let Ok(integer) = u64::try_from(integer) else { break false; };
                    value = tir::attributes::AttributeValue::UInt(integer);
                }
            });
        }
        quote! {
            let mark = parser.pos();
            let mut parsed_attributes = Vec::new();
            if parser.parse_token("{") && !parser.parse_token("}") {
                let complete = loop {
                    let Some(name) = parser.parse_attribute_name() else { break false; };
                    if !parser.parse_token("=") { break false; }
                    #[allow(unused_mut)]
                    let Some(mut value) = parser.parse_attribute_value(context)? else { break false; };
                    #normalize
                    parsed_attributes.push(context.named_attribute(&name, value));
                    if parser.parse_token("}") { break true; }
                    if !parser.parse_token(",") { break false; }
                };
                if !complete {
                    parser.set_pos(mark);
                    parsed_attributes.clear();
                }
            }
        }
    }
}
