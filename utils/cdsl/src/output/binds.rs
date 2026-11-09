//! Checked alignment of an operation's
//! operands, region ports, region results and results, from which the
//! `Theta`/`Gamma` impls, the alignment verifier and the generic syntax derive.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

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

/// What the generator needs to know about the op around the declaration.
pub struct OpShape<'a> {
    pub struct_name: &'a syn::Type,
    /// `dialect.name` as printed.
    pub spelled: String,
    /// Declared operand groups: name and whether variadic.
    pub operands: &'a [(String, bool)],
    /// Declared regions: name and whether variadic.
    pub regions: &'a [(String, bool)],
    pub custom_format: bool,
    pub state_results: bool,
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
            fn predicate_slot(&self) -> usize { #predicate }
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
        let region_args: Vec<_> = shape
            .regions
            .iter()
            .map(|(name, variadic)| {
                if name == &body.to_string() {
                    quote! { Some(parsed.body) }
                } else if *variadic {
                    quote! { vec![] }
                } else {
                    quote! { None }
                }
            })
            .collect();
        let state_result_arg = shape.state_results.then(|| quote! { vec![], });
        (
            Some(quote! {
                fn definition_print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
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
                fn definition_parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
                -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                    let parsed = tir::binding::parse_theta(parser, context)?;
                    Ok(Box::new(Self::new(
                        context, parsed.inits,
                        parsed.result_types, #(#region_args,)* #state_result_arg
                        tir::NewOpParts::default(), tir::InsertionPoint::Root,
                    )))
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
    let predicate_group = shape.operand_index(&predicate[0]);
    let predicate_groups = shape.operands.len();
    let predicate = shape.operand_value(&predicate[0]);
    let (operands, ports, exit, results) = (
        range(shape, &forwarded[0]),
        range(shape, &forwarded[1]),
        range(shape, &joined[0]),
        range(shape, &joined[1]),
    );

    let impls = quote! {
        impl tir::Gamma for #struct_name {
            fn predicate_slot(&self) -> usize {
                tir::binding::operand_segments(&self.0, #predicate_groups)[#predicate_group].start
            }
            fn predicate(&self) -> tir::ValueId {
                #predicate
            }

            fn arms(&self) -> Vec<tir::RegionId> {
                self.0.regions()[#arms_index..#arms_end].to_vec()
            }

            fn cases(&self) -> Vec<u64> {
                tir::binding::gamma_cases(&self.0, #arms_end - #arms_index)
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
            <Self as tir::Gamma>::predicate(self),
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
        let operand_args: Vec<_> = shape
            .operands
            .iter()
            .map(|(name, _)| {
                if name == &predicate[0].to_string() {
                    quote! { parsed.predicate }
                } else {
                    quote! { parsed.inputs }
                }
            })
            .collect();
        let region_args: Vec<_> = shape
            .regions
            .iter()
            .map(|(name, variadic)| {
                if name == &arms.to_string() {
                    quote! { parsed.arms }
                } else if *variadic {
                    quote! { vec![] }
                } else {
                    quote! { None }
                }
            })
            .collect();
        let state_result_arg = shape.state_results.then(|| quote! { vec![], });
        (
            Some(quote! {
                fn definition_print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
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
                fn definition_parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
                -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
                    let parsed = tir::binding::parse_gamma(parser, context)?;
                    let attributes = parsed.cases.into_iter().map(|cases| {
                        context.named_attribute(tir::binding::GAMMA_CASES, cases)
                    }).collect();
                    Ok(Box::new(Self::new(
                        context, #(#operand_args,)*
                        parsed.result_types, #(#region_args,)* #state_result_arg
                        tir::NewOpParts { attributes }, tir::InsertionPoint::Root,
                    )))
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
