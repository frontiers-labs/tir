use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, Path, parse_quote};

use super::Operation;
use crate::Error;

pub(super) enum Bindings {
    None,
    Theta {
        inits: Group,
        body: Ident,
        predicate: usize,
        next: Range,
        exit: Range,
        counted: Option<Counted>,
    },
    Gamma {
        predicate: Group,
        inputs: Group,
        arms: Ident,
    },
}

pub(super) struct Group {
    first: usize,
    last: usize,
    setter: Ident,
}

pub(super) struct Counted {
    induction: usize,
    lb: Group,
    ub: Group,
    step: Group,
}

pub(super) struct Range {
    from: Index,
    to: Option<Index>,
}

struct Index {
    relative: bool,
    offset: usize,
}

#[derive(Default)]
pub(super) struct BindingCode {
    pub(super) interfaces: Vec<Path>,
    pub(super) implementation: TokenStream,
    pub(super) format: Option<(TokenStream, TokenStream)>,
}

impl Bindings {
    pub(super) fn counted_groups(&self) -> TokenStream {
        match self {
            Self::Theta {
                counted: Some(counted),
                ..
            } => {
                let induction = counted.induction;
                let upper = counted.ub.first;
                let step = counted.step.first;
                quote! { Some((#induction, #upper, #step)) }
            }
            _ => quote! { None },
        }
    }

    pub(super) fn input_groups(&self) -> TokenStream {
        match self {
            Self::None => quote! { None },
            Self::Theta { inits, .. } | Self::Gamma { inputs: inits, .. } => {
                let Group { first, last, .. } = inits;
                quote! { Some((#first, #last)) }
            }
        }
    }

    pub(super) fn parse(binds: &str, counted: &str, op: &Operation<'_>) -> Result<Self, Error> {
        if binds.is_empty() && counted.is_empty() {
            return Ok(Self::None);
        }
        let wrong = || {
            Error(format!(
                "invalid region binding for {}.{}",
                op.dialect, op.name
            ))
        };
        let (kind, body) = binds.split_once('{').ok_or_else(wrong)?;
        let body = body.trim().strip_suffix('}').ok_or_else(wrong)?;
        let mut fields = body.trim().trim_end_matches(',').split(',').map(str::trim);
        if kind.trim() == "Theta" {
            let (carried, predicate) = body.split_once("predicate:").ok_or_else(wrong)?;
            let carried = carried.trim().trim_end_matches(',').trim();
            let carried = carried.strip_prefix("carried:").ok_or_else(wrong)?;
            let terms = carried.split('~').map(str::trim).collect::<Vec<_>>();
            let [inits, ports, next, exit, "results"] = terms.as_slice() else {
                return Err(wrong());
            };
            let body = ports.strip_suffix(".ports").ok_or_else(wrong)?;
            let references = format!("{body}.results[");
            let result_index = |term: &str| -> Result<String, Error> {
                Ok(term
                    .strip_prefix(&references)
                    .and_then(|term| term.strip_suffix(']'))
                    .ok_or_else(wrong)?
                    .to_owned())
            };
            let predicate = result_index(predicate.trim().trim_end_matches(',').trim())?
                .parse()
                .map_err(|_| wrong())?;
            if op.regions.len() != 1 || op.regions[0].name() != body {
                // Region names are stored as raw Rust identifiers.
                if op.regions.len() != 1 || *op.regions[0].name() != format!("r#{body}") {
                    return Err(wrong());
                }
            }
            let body = format_ident!("r#{body}");
            let inits = Group::parse(
                inits
                    .strip_prefix('(')
                    .and_then(|term| term.strip_suffix(')'))
                    .unwrap_or(inits),
                op,
            )?;
            let next = Range::parse(&result_index(next)?)?;
            let exit = Range::parse(&result_index(exit)?)?;
            let counted = if counted.is_empty() {
                None
            } else {
                Some(Counted::parse(counted, op)?)
            };
            return Ok(Self::Theta {
                inits,
                body,
                predicate,
                next,
                exit,
                counted,
            });
        }
        if kind.trim() != "Gamma" || !counted.is_empty() {
            return Err(wrong());
        }
        let predicate = Group::parse(fields.next().ok_or_else(wrong)?, op)?;
        let forwarded = fields
            .next()
            .and_then(|field| field.strip_prefix("forwarded:"))
            .ok_or_else(wrong)?;
        let (inputs, ports) = forwarded.split_once('~').ok_or_else(wrong)?;
        let arms = ports.trim().strip_suffix(".ports").ok_or_else(wrong)?;
        let joined = fields
            .next()
            .and_then(|field| field.strip_prefix("joined:"))
            .ok_or_else(wrong)?;
        let (arm_results, results) = joined.split_once('~').ok_or_else(wrong)?;
        if arm_results.trim().strip_suffix(".results") != Some(arms)
            || results.trim() != "results"
            || op.regions.len() != 1
            || *op.regions[0].name() != format!("r#{arms}")
            || fields.next().is_some()
        {
            return Err(wrong());
        }
        let arms = format_ident!("r#{arms}");
        let inputs = Group::parse(inputs, op)?;
        Ok(Self::Gamma {
            predicate,
            inputs,
            arms,
        })
    }

    pub(super) fn render(self, op: &Operation<'_>, builder: &Ident, spelled: &str) -> BindingCode {
        let name = &op.rust_name;
        let groups = op.operands.len();
        match self {
            Self::None => BindingCode::default(),
            Self::Theta {
                inits,
                body,
                predicate,
                next,
                exit,
                counted,
            } => {
                let setter = &inits.setter;
                let inits = inits.range();
                let next = next.render(quote! { body.results().len() });
                let exit = exit.render(quote! { body.results().len() });
                let mut code = BindingCode {
                    interfaces: vec![parse_quote!(tir::Theta)],
                    implementation: quote! {
                        impl tir::Theta for #name {
                            fn predicate_slot(&self) -> usize { #predicate }
                            fn body(&self) -> tir::RegionId { self.0.regions()[0] }

                            fn binding(&self) -> tir::Binding {
                                let segments = tir::binding::operand_segments(&self.0, #groups);
                                let operands = #inits;
                                let n = operands.len();
                                let body = self.0.context.get_region(self.0.regions()[0]);
                                tir::Binding {
                                    operands,
                                    ports: 0..body.ports().len(),
                                    continue_: #next,
                                    exit: #exit,
                                    results: 0..self.0.results().len(),
                                }
                            }

                            fn predicate(&self) -> tir::ValueId {
                                self.0.context.get_region(self.0.regions()[0]).results()[#predicate]
                            }
                        }
                    },
                    format: (!op.custom_format).then(|| {
                        (
                            quote! {
                                tir::binding::print_theta(
                                    formatter, &self.0, #spelled,
                                    tir::Theta::body(self), &tir::Theta::binding(self),
                                )
                            },
                            quote! {
                                let parsed = tir::binding::parse_theta(parser, context)?;
                                Ok(Box::new(#builder::new(context)
                                    .#setter(parsed.inits)
                                    .#body(parsed.body)
                                    .result_types(parsed.result_types)
                                    .build()))
                            },
                        )
                    }),
                };
                if let Some(counted) = counted {
                    let Counted {
                        induction,
                        lb,
                        ub,
                        step,
                    } = counted;
                    let lb = lb.value(groups);
                    let ub = ub.value(groups);
                    let step = step.value(groups);
                    code.interfaces.push(parse_quote!(tir::CountedLoop));
                    code.implementation.extend(quote! {
                        impl tir::CountedLoop for #name {
                            fn lower_bound(&self) -> tir::ValueId { #lb }
                            fn upper_bound(&self) -> tir::ValueId { #ub }
                            fn step(&self) -> tir::ValueId { #step }
                            fn induction(&self) -> Option<usize> { Some(#induction) }
                        }
                    });
                }
                code
            }
            Self::Gamma {
                predicate,
                inputs,
                arms,
            } => {
                let predicate_slot = predicate.first;
                let predicate_setter = &predicate.setter;
                let inputs_setter = &inputs.setter;
                let inputs = inputs.range();
                BindingCode {
                    interfaces: vec![parse_quote!(tir::Gamma)],
                    implementation: quote! {
                        impl tir::Gamma for #name {
                            fn predicate_slot(&self) -> usize {
                                tir::binding::operand_segments(&self.0, #groups)[#predicate_slot].start
                            }

                            fn predicate(&self) -> tir::ValueId {
                                self.0.operands()[tir::Gamma::predicate_slot(self)]
                            }

                            fn arms(&self) -> Vec<tir::RegionId> { self.0.regions().to_vec() }

                            fn cases(&self) -> Vec<u64> {
                                tir::binding::gamma_cases(&self.0, self.0.regions().len())
                            }

                            fn binding(&self) -> tir::Binding {
                                let segments = tir::binding::operand_segments(&self.0, #groups);
                                tir::Binding {
                                    operands: #inputs,
                                    ports: 0..tir::binding::region_list_len(&self.0.context, &self.0, 0, true),
                                    continue_: 0..0,
                                    exit: 0..tir::binding::region_list_len(&self.0.context, &self.0, 0, false),
                                    results: 0..self.0.results().len(),
                                }
                            }
                        }
                    },
                    format: (!op.custom_format).then(|| {
                        (
                            quote! {
                                tir::binding::print_gamma(
                                    formatter, &self.0, #spelled, tir::Gamma::predicate(self),
                                    &tir::Gamma::arms(self), &tir::Gamma::binding(self),
                                )
                            },
                            quote! {
                                let parsed = tir::binding::parse_gamma(parser, context)?;
                                let builder = #builder::new(context)
                                    .#predicate_setter(parsed.predicate)
                                    .#inputs_setter(parsed.inputs)
                                    .#arms(parsed.arms)
                                    .result_types(parsed.result_types);
                                let builder = match parsed.cases {
                                    Some(cases) => builder.attr(tir::binding::GAMMA_CASES, cases),
                                    None => builder,
                                };
                                Ok(Box::new(builder.build()))
                            },
                        )
                    }),
                }
            }
        }
    }
}

impl Group {
    fn parse(input: &str, op: &Operation<'_>) -> Result<Self, Error> {
        let groups = input
            .split(',')
            .map(str::trim)
            .map(|name| {
                op.operands
                    .iter()
                    .position(|slot| slot.name == name)
                    .ok_or_else(|| Error(format!("unknown region binding operand {name}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (Some(&first), Some(&last)) = (groups.first(), groups.last()) else {
            return Err(Error("empty region binding operand group".into()));
        };
        if !groups.windows(2).all(|pair| pair[1] == pair[0] + 1) {
            return Err(Error(
                "region binding names nonconsecutive operand groups".into(),
            ));
        }
        Ok(Self {
            first,
            last,
            setter: format_ident!("r#{}", op.operands[first].name),
        })
    }

    fn range(&self) -> TokenStream {
        let Self { first, last, .. } = self;
        quote! { segments[#first].start..segments[#last].end }
    }

    fn value(&self, groups: usize) -> TokenStream {
        let first = self.first;
        quote! { self.0.operands()[tir::binding::operand_segments(&self.0, #groups)[#first].start] }
    }
}

impl Counted {
    fn parse(input: &str, op: &Operation<'_>) -> Result<Self, Error> {
        let error = || Error("invalid counted loop binding".into());
        let input = input
            .strip_prefix('{')
            .and_then(|input| input.strip_suffix('}'))
            .ok_or_else(error)?;
        let fields = input
            .trim()
            .trim_end_matches(',')
            .split(',')
            .map(str::trim)
            .collect::<Vec<_>>();
        let [induction, "lb", "ub", "step"] = fields.as_slice() else {
            return Err(error());
        };
        let induction = induction
            .strip_prefix("induction:")
            .ok_or_else(error)?
            .trim()
            .parse()
            .map_err(|_| error())?;
        Ok(Self {
            induction,
            lb: Group::parse("lb", op)?,
            ub: Group::parse("ub", op)?,
            step: Group::parse("step", op)?,
        })
    }
}

impl Range {
    fn parse(input: &str) -> Result<Self, Error> {
        let (from, to) = input
            .split_once("..")
            .ok_or_else(|| Error("invalid region binding range".into()))?;
        Ok(Self {
            from: Index::parse(from)?,
            to: if to.is_empty() {
                None
            } else {
                Some(Index::parse(to)?)
            },
        })
    }

    fn render(&self, len: TokenStream) -> TokenStream {
        let from = self.from.render();
        let to = self.to.as_ref().map_or(len, Index::render);
        quote! { #from..#to }
    }
}

impl Index {
    fn parse(input: &str) -> Result<Self, Error> {
        let input = input.trim();
        let (relative, offset) = if let Some(offset) = input.strip_prefix("n+") {
            (true, offset)
        } else if input == "n" {
            (true, "0")
        } else {
            (false, input)
        };
        let offset = offset
            .parse()
            .map_err(|_| Error(format!("invalid region binding index {input}")))?;
        Ok(Self { relative, offset })
    }

    fn render(&self) -> TokenStream {
        let offset = self.offset;
        if self.relative {
            quote! { n + #offset }
        } else {
            quote! { #offset }
        }
    }
}
