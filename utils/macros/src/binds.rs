//! `binds:` and `counted:` in `operation!`: the declared alignment of an op's
//! operands, region ports, region results and results, from which the
//! `Theta`/`Gamma` impls, the alignment verifier and the generic syntax derive.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    Ident, LitInt, Token, braced, bracketed, parenthesized,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

pub enum BindKind {
    Theta,
    Gamma,
}

pub struct Binds {
    pub kind: BindKind,
    pub predicate: Term,
    pub chains: Vec<(String, Chain)>,
}

pub struct Chain(pub Vec<Term>);

pub enum Term {
    /// Consecutive operand groups, by declared name.
    Operands(Vec<Ident>),
    Results(Slice),
    Region {
        name: Ident,
        ports: bool,
        slice: Slice,
    },
}

/// `a` or `n + a`, where `n` is the length of the chain's first term.
pub struct Idx {
    pub n: bool,
    pub offset: usize,
}

pub enum Slice {
    All,
    One(Idx),
    Range(Idx, Option<Idx>),
}

/// `counted: { induction: 0, lb, ub, step }`: which port carries the counter
/// and which operands bound and advance it.
pub struct Counted {
    pub induction: usize,
    pub lb: Ident,
    pub ub: Ident,
    pub step: Ident,
}

impl Parse for Binds {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kind: Ident = input.parse()?;
        let kind = match kind.to_string().as_str() {
            "Theta" => BindKind::Theta,
            "Gamma" => BindKind::Gamma,
            other => {
                return Err(syn::Error::new(
                    kind.span(),
                    format!("binds: expects Theta or Gamma, got {other}"),
                ));
            }
        };
        let content;
        braced!(content in input);
        let mut predicate = None;
        let mut chains = vec![];
        while !content.is_empty() {
            let key: Ident = content.parse()?;
            if key == "predicate" {
                predicate = Some(if content.peek(Token![:]) {
                    content.parse::<Token![:]>()?;
                    content.parse()?
                } else {
                    Term::Operands(vec![key])
                });
            } else {
                content.parse::<Token![:]>()?;
                chains.push((key.to_string(), content.parse()?));
            }
            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }
        let predicate = predicate.ok_or_else(|| input.error("binds: names no predicate"))?;
        Ok(Binds {
            kind,
            predicate,
            chains,
        })
    }
}

impl Parse for Chain {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let terms: Punctuated<Term, Token![~]> = Punctuated::parse_separated_nonempty(input)?;
        Ok(Chain(terms.into_iter().collect()))
    }
}

impl Parse for Term {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.peek(syn::token::Paren) {
            let content;
            parenthesized!(content in input);
            let groups: Punctuated<Ident, Token![,]> =
                content.parse_terminated(Ident::parse, Token![,])?;
            return Ok(Term::Operands(groups.into_iter().collect()));
        }
        let name: Ident = input.parse()?;
        if input.peek(Token![.]) {
            input.parse::<Token![.]>()?;
            let list: Ident = input.parse()?;
            let ports = match list.to_string().as_str() {
                "ports" => true,
                "results" => false,
                other => {
                    return Err(syn::Error::new(
                        list.span(),
                        format!("a region has ports or results, not {other}"),
                    ));
                }
            };
            return Ok(Term::Region {
                name,
                ports,
                slice: Slice::parse_optional(input)?,
            });
        }
        if name == "results" {
            return Ok(Term::Results(Slice::parse_optional(input)?));
        }
        Ok(Term::Operands(vec![name]))
    }
}

impl Slice {
    fn parse_optional(input: ParseStream) -> syn::Result<Self> {
        if !input.peek(syn::token::Bracket) {
            return Ok(Slice::All);
        }
        let content;
        bracketed!(content in input);
        let start: Idx = content.parse()?;
        if !content.peek(Token![..]) {
            return Ok(Slice::One(start));
        }
        content.parse::<Token![..]>()?;
        let end = if content.is_empty() {
            None
        } else {
            Some(content.parse()?)
        };
        Ok(Slice::Range(start, end))
    }
}

impl Parse for Idx {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.peek(LitInt) {
            let literal: LitInt = input.parse()?;
            return Ok(Idx {
                n: false,
                offset: literal.base10_parse()?,
            });
        }
        let n: Ident = input.parse()?;
        if n != "n" {
            return Err(syn::Error::new(
                n.span(),
                "an index is a number, n, or n + number",
            ));
        }
        let offset = if input.peek(Token![+]) {
            input.parse::<Token![+]>()?;
            input.parse::<LitInt>()?.base10_parse()?
        } else {
            0
        };
        Ok(Idx { n: true, offset })
    }
}

impl Parse for Counted {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        braced!(content in input);
        let (mut induction, mut lb, mut ub, mut step) = (None, None, None, None);
        while !content.is_empty() {
            let key: Ident = content.parse()?;
            let named = |content: ParseStream| -> syn::Result<Ident> {
                if content.peek(Token![:]) {
                    content.parse::<Token![:]>()?;
                    content.parse()
                } else {
                    Ok(key.clone())
                }
            };
            match key.to_string().as_str() {
                "induction" => {
                    content.parse::<Token![:]>()?;
                    induction = Some(content.parse::<LitInt>()?.base10_parse()?);
                }
                "lb" => lb = Some(named(&content)?),
                "ub" => ub = Some(named(&content)?),
                "step" => step = Some(named(&content)?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("counted: takes induction, lb, ub and step, not {other}"),
                    ));
                }
            }
            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }
        let missing = |what| input.error(format!("counted: names no {what}"));
        Ok(Counted {
            induction: induction.ok_or_else(|| missing("induction"))?,
            lb: lb.ok_or_else(|| missing("lb"))?,
            ub: ub.ok_or_else(|| missing("ub"))?,
            step: step.ok_or_else(|| missing("step"))?,
        })
    }
}

/// What the generator needs to know about the op around the declaration.
pub struct OpShape<'a> {
    pub struct_name: &'a Ident,
    pub builder_name: &'a Ident,
    /// `dialect.name` as printed.
    pub spelled: String,
    /// Declared operand groups: name and whether variadic.
    pub operands: &'a [(String, bool)],
    /// Declared regions: name and whether variadic.
    pub regions: &'a [(String, bool)],
    pub custom_format: bool,
}

/// The code a `binds:` declaration contributes to the op.
#[derive(Default)]
pub struct BindsCode {
    /// Interfaces the op now implements, for registration.
    pub interfaces: Vec<syn::Path>,
    pub impls: TokenStream,
    /// Checks appended to the generated `verify_operands`.
    pub verify: TokenStream,
    /// The generic printer and parser, unless the op spells its own.
    pub printer: Option<TokenStream>,
    pub parser: Option<TokenStream>,
}

impl OpShape<'_> {
    fn operand_index(&self, name: &Ident) -> usize {
        self.operands
            .iter()
            .position(|(declared, _)| declared == &name.to_string())
            .unwrap_or_else(|| panic!("binds: names no operand '{name}'"))
    }

    fn region_index(&self, name: &Ident) -> usize {
        self.regions
            .iter()
            .position(|(declared, _)| declared == &name.to_string())
            .unwrap_or_else(|| panic!("binds: names no region '{name}'"))
    }

    fn operand_value(&self, name: &Ident) -> TokenStream {
        let index = self.operand_index(name);
        let groups = self.operands.len();
        quote! {
            self.0.operands()[tir::binding::operand_segments(&self.0, #groups)[#index].start]
        }
    }
}

fn idx(index: &Idx) -> TokenStream {
    let offset = index.offset;
    if index.n {
        quote! { (n + #offset) }
    } else {
        quote! { #offset }
    }
}

/// The range `slice` names in a list of `len` entries, in terms of `n`.
fn slice(slice: &Slice) -> TokenStream {
    match slice {
        Slice::All => quote! { 0..len },
        Slice::One(at) => {
            let at = idx(at);
            quote! { #at..#at + 1 }
        }
        Slice::Range(start, end) => {
            let start = idx(start);
            let end = end.as_ref().map(idx).unwrap_or(quote! { len });
            quote! { #start..#end }
        }
    }
}

/// The range a term denotes, evaluated against the op held in `self.0` with
/// `__segments`, `__context` and `n` in scope.
fn range(shape: &OpShape, term: &Term) -> TokenStream {
    match term {
        Term::Operands(groups) => {
            let indices: Vec<usize> = groups
                .iter()
                .map(|group| shape.operand_index(group))
                .collect();
            assert!(
                indices.windows(2).all(|pair| pair[1] == pair[0] + 1),
                "binds: an operand group list names consecutive groups in declaration order"
            );
            let (first, last) = (indices[0], indices[indices.len() - 1]);
            quote! { (__segments[#first].start..__segments[#last].end) }
        }
        Term::Results(at) => {
            let at = slice(at);
            quote! { { let len = self.0.results().len(); #at } }
        }
        Term::Region {
            name,
            ports,
            slice: at,
        } => {
            let index = shape.region_index(name);
            let at = slice(at);
            quote! { {
                let len = tir::binding::region_list_len(&__context, &self.0, #index, #ports);
                #at
            } }
        }
    }
}

fn region_of(term: &Term) -> &Ident {
    match term {
        Term::Region { name, .. } => name,
        _ => panic!("binds: expected a region term here"),
    }
}

fn chain<'a>(binds: &'a Binds, key: &str, arity: usize) -> &'a [Term] {
    let terms = &binds
        .chains
        .iter()
        .find(|(name, _)| name == key)
        .unwrap_or_else(|| panic!("binds: names no {key}"))
        .1
        .0;
    assert!(
        terms.len() == arity,
        "binds: {key} aligns {arity} lists, got {}",
        terms.len()
    );
    terms
}

pub fn emit(binds: &Binds, counted: Option<&Counted>, shape: &OpShape) -> BindsCode {
    match binds.kind {
        BindKind::Theta => emit_theta(binds, counted, shape),
        BindKind::Gamma => {
            assert!(counted.is_none(), "counted: belongs to a Theta");
            emit_gamma(binds, shape)
        }
    }
}

fn emit_theta(binds: &Binds, counted: Option<&Counted>, shape: &OpShape) -> BindsCode {
    let struct_name = shape.struct_name;
    let spelled = &shape.spelled;
    let groups = shape.operands.len();
    let terms = chain(binds, "carried", 5);
    assert!(
        matches!(terms[0], Term::Operands(_)) && matches!(terms[4], Term::Results(_)),
        "binds: carried runs from operands to results"
    );
    let body = region_of(&terms[1]);
    let body_index = shape.region_index(body);
    assert!(
        matches!(terms[1], Term::Region { ports: true, .. })
            && terms[2..4]
                .iter()
                .all(|term| matches!(term, Term::Region { ports: false, .. })
                    && region_of(term) == body),
        "binds: carried runs operands ~ body.ports ~ body.results[..] ~ body.results[..] ~ results over one body"
    );
    let (operands, ports, continue_, exit, results) = (
        range(shape, &terms[0]),
        range(shape, &terms[1]),
        range(shape, &terms[2]),
        range(shape, &terms[3]),
        range(shape, &terms[4]),
    );
    let predicate = match &binds.predicate {
        Term::Region {
            name,
            ports: false,
            slice: Slice::One(Idx { n: false, offset }),
        } if name == body => *offset,
        _ => panic!("binds: a Theta predicate is one body result at a fixed index"),
    };

    let mut interfaces = vec![syn::parse_quote!(tir::Theta)];
    let mut impls = quote! {
        impl tir::Theta for #struct_name {
            fn body(&self) -> tir::RegionId {
                self.0.regions()[#body_index]
            }

            fn binding(&self) -> tir::Binding {
                let __context = self.0.context.clone();
                let __segments = tir::binding::operand_segments(&self.0, #groups);
                let operands = #operands;
                let n = operands.len();
                let ports = #ports;
                let continue_ = #continue_;
                let exit = #exit;
                let results = #results;
                tir::Binding { operands, ports, continue_, exit, results }
            }

            fn predicate(&self) -> tir::ValueId {
                let __context = self.0.context.clone();
                __context.get_region(self.0.regions()[#body_index]).results()[#predicate]
            }
        }
    };
    let mut verify = quote! {
        tir::binding::verify_theta(
            context,
            &self.0,
            #spelled,
            <Self as tir::Theta>::body(self),
            &<Self as tir::Theta>::binding(self),
            #predicate,
        )?;
    };
    if let Some(counted) = counted {
        let induction = counted.induction;
        let (lb, ub, step) = (
            shape.operand_value(&counted.lb),
            shape.operand_value(&counted.ub),
            shape.operand_value(&counted.step),
        );
        interfaces.push(syn::parse_quote!(tir::CountedLoop));
        impls.extend(quote! {
            impl tir::CountedLoop for #struct_name {
                fn lower_bound(&self) -> tir::ValueId {
                    #lb
                }
                fn upper_bound(&self) -> tir::ValueId {
                    #ub
                }
                fn step(&self) -> tir::ValueId {
                    #step
                }
                fn induction(&self) -> Option<usize> {
                    Some(#induction)
                }
            }
        });
        verify.extend(quote! {
            tir::binding::verify_counted(
                context,
                #spelled,
                <Self as tir::Theta>::body(self),
                &<Self as tir::Theta>::binding(self),
                #predicate,
                #induction,
                <Self as tir::CountedLoop>::upper_bound(self),
                <Self as tir::CountedLoop>::step(self),
            )?;
        });
    }

    let (printer, parser) = if shape.custom_format {
        (None, None)
    } else {
        let Term::Operands(inits) = &terms[0] else {
            unreachable!()
        };
        assert!(
            inits.len() == 1 && shape.operands.len() == 1 && shape.operands[0].1,
            "binds: the generic Theta syntax needs one variadic operand group; spell a custom format"
        );
        let inits = inits[0].clone();
        let body = body.clone();
        let builder = shape.builder_name;
        (
            Some(quote! {
                fn print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
                    tir::binding::print_theta(
                        fmt,
                        &self.0,
                        #spelled,
                        <Self as tir::Theta>::body(self),
                        &<Self as tir::Theta>::binding(self),
                    )
                }
            }),
            Some(quote! {
                fn parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
                -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                    let parsed = tir::binding::parse_theta(parser, context)?;
                    let builder = #builder::new(context)
                        .#inits(parsed.inits)
                        .#body(parsed.body)
                        .result_types(parsed.result_types);
                    Ok(Box::new(builder.build()))
                }
            }),
        )
    };
    BindsCode {
        interfaces,
        impls,
        verify,
        printer,
        parser,
    }
}

fn emit_gamma(binds: &Binds, shape: &OpShape) -> BindsCode {
    let struct_name = shape.struct_name;
    let spelled = &shape.spelled;
    let groups = shape.operands.len();
    let forwarded = chain(binds, "forwarded", 2);
    let joined = chain(binds, "joined", 2);
    let arms = region_of(&forwarded[1]);
    assert!(
        matches!(forwarded[0], Term::Operands(_))
            && matches!(
                forwarded[1],
                Term::Region {
                    ports: true,
                    slice: Slice::All,
                    ..
                }
            )
            && matches!(
                joined[0],
                Term::Region {
                    ports: false,
                    slice: Slice::All,
                    ..
                }
            )
            && region_of(&joined[0]) == arms
            && matches!(joined[1], Term::Results(Slice::All)),
        "binds: forwarded runs operands ~ arms.ports and joined runs arms.results ~ results, whole lists on both"
    );
    let arms_index = shape.region_index(arms);
    let arms_end = if shape.regions[arms_index].1 {
        quote! { self.0.regions().len() }
    } else {
        quote! { #arms_index + 1 }
    };
    let Term::Operands(predicate) = &binds.predicate else {
        panic!("binds: a Gamma predicate is an operand")
    };
    let predicate = shape.operand_value(&predicate[0]);
    let (operands, ports, exit, results) = (
        range(shape, &forwarded[0]),
        range(shape, &forwarded[1]),
        range(shape, &joined[0]),
        range(shape, &joined[1]),
    );

    let impls = quote! {
        impl tir::Gamma for #struct_name {
            fn predicate(&self) -> tir::ValueId {
                #predicate
            }

            fn arms(&self) -> Vec<tir::RegionId> {
                self.0.regions()[#arms_index..#arms_end].to_vec()
            }

            fn binding(&self) -> tir::Binding {
                let __context = self.0.context.clone();
                let __segments = tir::binding::operand_segments(&self.0, #groups);
                let operands = #operands;
                let ports = #ports;
                let exit = #exit;
                let results = #results;
                tir::Binding { operands, ports, continue_: 0..0, exit, results }
            }
        }
    };
    let verify = quote! {
        tir::binding::verify_gamma(
            context,
            &self.0,
            #spelled,
            &<Self as tir::Gamma>::arms(self),
            &<Self as tir::Gamma>::binding(self),
        )?;
    };
    let (printer, parser) = if shape.custom_format {
        (None, None)
    } else {
        let Term::Operands(inputs) = &forwarded[0] else {
            unreachable!()
        };
        let Term::Operands(predicate) = &binds.predicate else {
            unreachable!()
        };
        assert!(
            inputs.len() == 1 && shape.operands.len() == 2 && shape.operands[1].1,
            "binds: the generic Gamma syntax needs a predicate and one variadic operand group; spell a custom format"
        );
        let predicate = predicate[0].clone();
        let inputs = inputs[0].clone();
        let arms = arms.clone();
        let builder = shape.builder_name;
        (
            Some(quote! {
                fn print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
                    tir::binding::print_gamma(
                        fmt,
                        &self.0,
                        #spelled,
                        <Self as tir::Gamma>::predicate(self),
                        &<Self as tir::Gamma>::arms(self),
                        &<Self as tir::Gamma>::binding(self),
                    )
                }
            }),
            Some(quote! {
                fn parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
                -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                    let parsed = tir::binding::parse_gamma(parser, context)?;
                    let builder = #builder::new(context)
                        .#predicate(parsed.predicate)
                        .#inputs(parsed.inputs)
                        .#arms(parsed.arms)
                        .result_types(parsed.result_types);
                    Ok(Box::new(builder.build()))
                }
            }),
        )
    };
    BindsCode {
        interfaces: vec![syn::parse_quote!(tir::Gamma)],
        impls,
        verify,
        printer,
        parser,
    }
}
