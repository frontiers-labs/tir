use quote::{format_ident, quote};
use syn::{Ident, Path};

use super::binds::{Binds, BindsCode, Counted, OpShape};
use super::utils::op_fn_ident;

/// Emit Rust bindings from one operation definition.
pub fn emit_operation(operation: Operation) -> proc_macro2::TokenStream {
    let Operation {
        name,
        dialect,
        regions,
        attributes,
        mut operands,
        mut results,
        mut interfaces,
        format,
        sem,
        semantic_hook,
        poison,
        requires,
        custom_verifier,
        verify_hook,
        state,
        binds,
        counted,
        scheme,
        interface_callbacks,
        splice_callbacks,
    } = operation;
    let op_fn_name = op_fn_ident(&name);
    let prefix = op_fn_name.to_string().trim_start_matches("r#").to_owned();
    let definition_name = format_ident!("{}", prefix.to_uppercase());
    let schema_name = format_ident!("__TIR_SCHEMA_{}", prefix.to_uppercase());
    if state.input {
        operands.push(ValueSpec::state());
    }
    if state.output {
        results.push(ValueSpec::state());
    }
    let custom_format = !matches!(format, RustFormat::Generic);
    let handle: syn::Type = syn::parse_quote!(tir::OpHandle);
    let binds_code = binds
        .as_ref()
        .map(|binding| {
            super::binds::emit(
                binding,
                counted.as_ref(),
                &OpShape {
                    struct_name: &handle,
                    spelled: format!("{dialect}.{name}"),
                    operands: &operands
                        .iter()
                        .map(|port| (port.name.clone(), port.variadic))
                        .collect::<Vec<_>>(),
                    regions: &regions
                        .iter()
                        .map(|region| (region.name.clone(), region.variadic))
                        .collect::<Vec<_>>(),
                    custom_format,
                    state_results: results.iter().any(ValueSpec::is_state),
                },
            )
        })
        .unwrap_or_default();
    let BindsCode {
        interfaces: binding_interfaces,
        impls: binding_impls,
        verify: binding_verify,
        printer: binding_printer,
        parser: binding_parser,
    } = binds_code;
    interfaces.extend(binding_interfaces);
    let has_results = results.iter().any(|result| !result.is_state());
    let value_results = results
        .iter()
        .filter(|result| !result.is_state())
        .cloned()
        .collect::<Vec<_>>();
    let result_variadic = value_results.len() > 1
        || value_results
            .iter()
            .any(|result| result.variadic || result.ty.starts_with('?'));
    let region_pieces = make_region_pieces(&regions);
    let operand_pieces = make_operand_pieces(&operands);
    let result_pieces = make_result_pieces(has_results, result_variadic);
    let state_result_pieces =
        make_state_result_pieces(results.iter().find(|result| result.is_state()));
    let required = attributes
        .iter()
        .filter(|attribute| !attribute.ty.starts_with('?'))
        .collect::<Vec<_>>();
    let attr_params = required
        .iter()
        .map(|attribute| {
            let name = op_fn_ident(&attribute.name);
            quote! {#name:impl Into<tir::attributes::AttributeValue>}
        })
        .collect::<Vec<_>>();
    let attr_args = required
        .iter()
        .map(|attribute| op_fn_ident(&attribute.name))
        .collect::<Vec<_>>();
    let attr_fills = required
        .iter()
        .map(|attribute| {
            let id = op_fn_ident(&attribute.name);
            let name = &attribute.name;
            quote! {parts.attributes.push(context.named_attribute(#name,#id.into()));}
        })
        .collect::<Vec<_>>();
    let (_, constructor) = emit_constructors(
        &op_fn_name,
        &handle,
        &region_pieces,
        &operand_pieces,
        &result_pieces,
        &state_result_pieces,
        (&attr_params, &attr_args),
        &attr_fills,
        &make_attribute_verifier(&attributes),
        true,
    );
    let constructor = replace_definition_constructor(constructor, &definition_name);
    let accessors = [
        make_region_accessors(&regions),
        result_pieces.accessor,
        make_attr_accessors(&attributes, &format!("{dialect}.{name}")),
    ]
    .into_iter()
    .map(|tokens| free_accessors(tokens, &prefix));
    let print_name = format_ident!("__tir_{}_print", prefix);
    let parse_name = format_ident!("__tir_{}_parse", prefix);
    let printer=binding_printer.unwrap_or_else(||match &format {
        RustFormat::Generic=>make_generic_printer(&dialect,&name),
        RustFormat::Custom=>{let function=format_ident!("{}_custom_print",prefix);quote!{fn definition_print(&self,fmt:&mut tir::IRFormatter)->Result<(),std::fmt::Error>{#function(self.0,fmt)}}},
        RustFormat::Callbacks{print,..}=>quote!{fn definition_print(&self,fmt:&mut tir::IRFormatter)->Result<(),std::fmt::Error>{let callback=#print;({callback})(self.0,fmt)}},
    });
    let parser=binding_parser.unwrap_or_else(||match &format {
        RustFormat::Generic=>make_parser(&name,&regions,&operands,&attributes,has_results),
        RustFormat::Custom=>{let function=format_ident!("{}_custom_parse",prefix);quote!{fn definition_parse<'src>(parser:&mut tir::parse::text::Parser<'src>,context:&tir::Context)->Result<tir::OpHandle,(tir::parse::Span,tir::Error)>{#function(parser,context)}}},
        RustFormat::Callbacks{parse,..}=>quote!{fn definition_parse<'src>(parser:&mut tir::parse::text::Parser<'src>,context:&tir::Context)->Result<tir::OpHandle,(tir::parse::Span,tir::Error)>{let callback=#parse;({callback})(parser,context,definition)}},
    });
    let printer = free_method(printer, print_name.clone(), None);
    let parser = free_method(parser, parse_name.clone(), Some(&definition_name));
    let region_kinds = regions
        .iter()
        .map(|region| format_ident!("{}", region.kind));
    let verify = if let Some(hook) = verify_hook {
        quote! {Some(#hook)}
    } else if custom_verifier {
        let function = format_ident!("{}_verify_impl", prefix);
        quote! {Some(#function)}
    } else {
        quote! {None}
    };
    let (verify_binding_code, verify_bindings) = if binding_verify.is_empty() {
        (quote! {}, quote! {None})
    } else {
        let function = format_ident!("__tir_{}_verify_bindings", prefix);
        let body = rewrite_tokens(binding_verify, Some(&prefix), None, None, false);
        (
            quote! {fn #function(operation:&tir::OpHandle,context:&tir::Context)->Result<(),tir::Error>{#body Ok(())}},
            quote! {Some(#function)},
        )
    };
    let mut callback_bindings = interface_callbacks;
    let (derived_callbacks, derived_bindings) = free_binding_callbacks(binding_impls, &prefix);
    callback_bindings.extend(derived_bindings);
    let semantic = semantic_callback(&sem, &prefix, "semantic", has_results);
    let poison_callback = semantic_callback(&poison, &prefix, "poison", false);
    let requires_callback = semantic_callback(&requires, &prefix, "requires", false);
    let splices = [&sem, &poison, &requires]
        .into_iter()
        .flatten()
        .flat_map(|semantic| semantic.splices.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    let splice_name = format_ident!("__tir_{}_splice", prefix);
    let splice_arms = splices.iter().map(|name| {
        if let Some((_, callback)) = splice_callbacks.iter().find(|(label, _)| label == name) {
            quote! {#name=>(#callback)(operation,graph).into(),}
        } else {
            let function = format_ident!("{}_{}", prefix, name);
            quote! {#name=>#function(operation,graph).into(),}
        }
    });
    let (splice_code, splice) = if splices.is_empty() {
        (quote! {}, quote! {None})
    } else {
        (
            quote! {fn #splice_name(operation:&tir::OpHandle,name:&str,graph:&mut tir::sem::SemGraph)->Option<tir::NodeId>{
                match name {#(#splice_arms)*_=>None}
            }},
            quote! {Some(#splice_name)},
        )
    };
    if sem.is_some()
        && !interfaces.iter().any(|path| {
            path.segments
                .last()
                .is_some_and(|part| part.ident == "ConstantFold")
        })
    {
        interfaces.push(syn::parse_quote!(tir::ConstantFold));
        callback_bindings.push((
            syn::parse_quote!(tir::ConstantFold),
            syn::parse_quote!(&tir::ConstantFoldCallbacks {
                fold: |operation, operands| tir::sem::fold_with_sem(operation, operands)
            }),
        ));
    }
    let mut registrations = Vec::new();
    for interface in &interfaces {
        let Some(last) = interface.segments.last().map(|part| &part.ident) else {
            return syn::Error::new(
                proc_macro2::Span::call_site(),
                "empty operation interface path",
            )
            .into_compile_error();
        };
        let callbacks = callback_bindings
            .iter()
            .find(|(path, _)| path.segments.last().is_some_and(|part| part.ident == *last))
            .map(|(_, expression)| expression.clone())
            .unwrap_or_else(|| {
                let mut callbacks: syn::Path = if interface.segments.len() == 1 {
                    syn::parse_quote!(tir::#last)
                } else {
                    interface.clone()
                };
                if let Some(last) = callbacks.segments.last_mut() {
                    last.ident = format_ident!("{}Callbacks", last.ident);
                }
                syn::parse_quote!(&#callbacks::DEFAULT)
            });
        registrations.push(quote!{(std::any::TypeId::of::<dyn #interface>,std::any::type_name::<dyn #interface>,|operation|{let callbacks=#callbacks;tir::erase_op_interface::<dyn #interface>(callbacks.view(operation))})});
    }
    let schema = emit_schema(
        &schema_name,
        &dialect,
        &name,
        (&operands, &results),
        &attributes,
        &interfaces,
        (&scheme, sem.is_some()),
    );
    let semantic_pointer = semantic_hook.map_or(semantic.1, |hook| quote! {Some(#hook)});
    let poison_pointer = poison_callback.1;
    let requires_pointer = requires_callback.1;
    let semantic_code = semantic.0;
    let poison_code = poison_callback.0;
    let requires_code = requires_callback.0;
    quote! {
        #schema
        #[tir::linkme::distributed_slice(tir::OP_DEFINITIONS)]
        #[linkme(crate=tir::linkme)]
        pub static #definition_name:tir::OpDefSpec=tir::OpDefSpec {
            schema:&#schema_name,region_kinds:&[#(tir::RegionKind::#region_kinds),*],
            print:#print_name,parse:#parse_name,verify:#verify,verify_bindings:#verify_bindings,
            semantic:#semantic_pointer,poison:#poison_pointer,requires:#requires_pointer,
            interfaces:&[#(#registrations),*],splice:#splice,
        };
        #printer
        #parser
        #constructor
        #(#accessors)*
        #derived_callbacks
        #verify_binding_code
        #semantic_code
        #poison_code
        #requires_code
        #splice_code
    }
}

type TokenStreams = Vec<proc_macro2::TokenStream>;

fn emit_schema(
    schema_ident: &Ident,
    dialect: &str,
    name: &str,
    (operands, results): (&[ValueSpec], &[ValueSpec]),
    attributes: &[AttrSpec],
    interfaces: &[Path],
    (scheme, semantics): (&str, bool),
) -> proc_macro2::TokenStream {
    let operand_schema_entries = operands.iter().map(|o| {
        let (n, t, v) = (&o.name, &o.ty, o.variadic);
        quote! { tir::FieldSchema { name: #n, ty: #t, variadic: #v } }
    });
    let result_schema_entries = results.iter().map(|r| {
        let (n, t, v) = (&r.name, &r.ty, r.variadic);
        quote! { tir::FieldSchema { name: #n, ty: #t, variadic: #v } }
    });
    let attr_schema_entries = attributes.iter().map(|a| {
        let (n, t) = (&a.name, &a.ty);
        let vocabulary = match &a.vocabulary {
            Some(name) => {
                let ident = format_ident!("{}", name);
                quote! { tir::attributes::Predicate::#ident }
            }
            None => quote! { &[] },
        };
        quote! { tir::AttrSchema { name: #n, ty: #t, vocabulary: #vocabulary } }
    });
    let interface_schema_entries = interfaces
        .iter()
        .map(|p| p.segments.last().unwrap().ident.to_string());

    quote! {
        #[allow(non_upper_case_globals)]
        static #schema_ident: tir::OpSchema = tir::OpSchema {
            dialect: #dialect,
            name: #name,
            operands: &[#(#operand_schema_entries),*],
            results: &[#(#result_schema_entries),*],
            attributes: &[#(#attr_schema_entries),*],
            interfaces: &[#(#interface_schema_entries),*],
            scheme: #scheme,
            semantics: #semantics,
        };
    }
}

/// The Rust getter type and corresponding IR attribute variant.
struct AttrAccessor {
    getter_ty: proc_macro2::TokenStream,
    variant: Ident,
    read: proc_macro2::TokenStream,
}

fn attr_accessor(ty: &str) -> Option<AttrAccessor> {
    let scalar = |rust: proc_macro2::TokenStream, variant: &str| AttrAccessor {
        getter_ty: rust,
        variant: format_ident!("{}", variant),
        read: quote! { value },
    };
    Some(match ty {
        "Str" => AttrAccessor {
            getter_ty: quote! { String },
            variant: format_ident!("Str"),
            read: quote! { value.to_string() },
        },
        "Int" => scalar(quote! { i64 }, "Int"),
        "UInt" => scalar(quote! { u64 }, "UInt"),
        "F32" => scalar(quote! { f32 }, "F32"),
        "F64" => scalar(quote! { f64 }, "F64"),
        "Bool" | "Flag" => scalar(quote! { bool }, "Bool"),
        "Type" => scalar(quote! { tir::TypeId }, "Type"),
        "Block" => scalar(quote! { tir::BlockId }, "Block"),
        "Value" => scalar(quote! { tir::ValueId }, "Value"),
        "Predicate" => scalar(quote! { tir::attributes::Predicate }, "Predicate"),
        "FpSemantics" => AttrAccessor {
            getter_ty: quote! { std::sync::Arc<tir::fp::Semantics> },
            variant: format_ident!("FpSemantics"),
            read: quote! { value },
        },
        "EvaluationContract" => AttrAccessor {
            getter_ty: quote! { std::sync::Arc<tir::fp::EvaluationContract> },
            variant: format_ident!("EvaluationContract"),
            read: quote! { value },
        },
        _ => return None,
    })
}

fn make_attr_accessors(attributes: &[AttrSpec], spelled: &str) -> proc_macro2::TokenStream {
    let mut getters = vec![];
    for attr in attributes {
        let optional = attr.ty.strip_prefix('?');
        let Some(AttrAccessor {
            getter_ty,
            variant,
            read,
        }) = attr_accessor(optional.unwrap_or(&attr.ty))
        else {
            continue;
        };
        let method = op_fn_ident(&attr.name);
        let name = attr.name.clone();
        let missing = format!(
            "{spelled} must carry a {} attribute '{}'",
            attr.ty, attr.name
        );
        if optional.is_some() {
            getters.push(quote! {
                pub fn #method(&self) -> Option<#getter_ty> {
                    match tir::Operation::attr(self, #name) {
                        Some(tir::attributes::AttributeValue::#variant(value)) => Some(#read),
                        _ => None,
                    }
                }
            });
            continue;
        }
        getters.push(quote! {
            pub fn #method(&self) -> #getter_ty {
                match tir::Operation::attr(self, #name) {
                    Some(tir::attributes::AttributeValue::#variant(value)) => #read,
                    _ => panic!(#missing),
                }
            }
        });
    }
    quote! { #(#getters)* }
}

fn constructor_attributes(
    attr_fills: &TokenStreams,
    segment_sizes_attr: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    if !attr_fills.is_empty() {
        quote! {
            let supplied_attributes = parts.attributes;
            let mut parts = tir::NewOpParts::default();
            #(#attr_fills)*
            parts.attributes.extend(supplied_attributes);
        }
    } else if !segment_sizes_attr.is_empty() {
        quote! { let mut parts = parts; }
    } else {
        quote! {}
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_constructors(
    op_fn_name: &Ident,
    _struct_name: &syn::Type,
    regions: &RegionPieces,
    operands: &OperandPieces,
    results: &ResultPieces,
    state_results: &StateResultPieces,
    (attr_params, _attr_args): (&TokenStreams, &[Ident]),
    attr_fills: &TokenStreams,
    attribute_verifier: &proc_macro2::TokenStream,
    _functional: bool,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let (region_params, _region_args, region_fills) =
        (&regions.params, &regions.args, &regions.fills);
    let (operand_params, _operand_args, operand_setup, operand_collect) = (
        &operands.params,
        &operands.args,
        &operands.setup,
        &operands.collect,
    );
    let (state_params, _state_args) = (&operands.state_params, &operands.state_args);
    let (result_param, _result_arg, result_build) = (&results.param, &results.arg, &results.build);
    let (state_result_param, _state_result_arg, state_result_build) = (
        &state_results.param,
        &state_results.arg,
        &state_results.build,
    );
    let segment_sizes_setup = &operands.segment_sizes_setup;
    let segment_sizes_attr = &operands.segment_sizes_attr;
    let parts_setup = constructor_attributes(attr_fills, segment_sizes_attr);
    let params = quote! {
        context: &tir::Context,
        #(#operand_params,)*
        #(#attr_params,)*
        #result_param
        #(#region_params,)*
        #(#state_params,)*
        #state_result_param
        parts: tir::NewOpParts,
        insertion: tir::InsertionPoint,
    };
    let constructor = quote! {};
    let return_type = quote! {tir::OpHandle};
    let inserted = quote! {context.add_operation(insertion,instance)};
    let region_mut = (!region_fills.is_empty()).then(|| quote! {mut});
    let operand_mut = (!operand_collect.is_empty()).then(|| quote! {mut});
    let function = quote! {
        // Generated: one parameter per declared port.
        #[allow(dead_code, clippy::too_many_arguments)]
        pub fn #op_fn_name(#params) -> #return_type {
            #parts_setup
            let #region_mut regions = vec![];
            #(#region_fills)*
            #attribute_verifier
            #(#operand_setup)*
            let #operand_mut operand_vec: Vec<tir::ValueId> = vec![];
            #segment_sizes_setup
            #(#operand_collect)*
            #result_build
            #state_result_build
            #segment_sizes_attr
            let instance = tir::NewOp::for_definition(__TIR_DEFINITION,
                context.clone(), operand_vec, result_vec, regions, parts,
            );
            #inserted
        }
    };
    (constructor, function)
}

struct RegionPieces {
    params: TokenStreams,
    args: TokenStreams,
    defaults: TokenStreams,
    fills: TokenStreams,
}

fn make_region_pieces(regions: &[Region]) -> RegionPieces {
    let mut params = vec![];
    let mut args = vec![];
    let mut defaults = vec![];
    let mut fills = vec![];
    for region in regions {
        let name = format_ident!("{}", region.name);
        args.push(quote! { #name });
        if region.variadic {
            params.push(quote! { #name: Vec<tir::RegionId> });
            defaults.push(quote! { let #name = vec![]; });
            fills.push(quote! { regions.extend(#name); });
        } else {
            params.push(quote! { #name: Option<tir::RegionId> });
            defaults.push(quote! { let #name = None; });
            if region.blocked() {
                fills.push(quote! {
                    let region = #name.unwrap_or_else(|| {
                        let region = context.create_region();
                        let block = context.create_block(vec![]);
                        region.add_block(block.id());
                        region.id()
                    });
                    regions.push(region);
                });
            } else {
                let missing = format!("Region '{}' is not set", region.name);
                fills.push(quote! { regions.push(#name.expect(#missing)); });
            }
        }
    }
    RegionPieces {
        params,
        args,
        defaults,
        fills,
    }
}

struct OperandPieces {
    params: TokenStreams,
    args: TokenStreams,
    state_params: TokenStreams,
    state_args: TokenStreams,
    setup: TokenStreams,
    collect: TokenStreams,
    segment_sizes_setup: proc_macro2::TokenStream,
    segment_sizes_attr: proc_macro2::TokenStream,
}

fn make_operand_pieces(operands: &[ValueSpec]) -> OperandPieces {
    let mut params = vec![];
    let mut args = vec![];
    let mut state_params = vec![];
    let mut state_args = vec![];
    let mut setup = vec![];
    let mut collect = vec![];
    let has_variadic = operands.iter().any(|operand| operand.variadic);
    for operand in operands {
        let field = format_ident!("{}", operand.name);
        if operand.is_state() {
            state_params.push(quote! { #field: Vec<tir::ValueId> });
            state_args.push(quote! { #field });
        } else {
            args.push(quote! { #field });
            if operand.variadic {
                params.push(quote! { #field: Vec<tir::ValueId> });
            } else {
                params.push(quote! { #field: impl Into<tir::Operand> });
                setup.push(quote! { let #field = #field.into().into_option(); });
            }
        }
        if !has_variadic {
            collect.push(quote! { if let Some(value) = #field { operand_vec.push(value); } });
        } else if operand.variadic {
            collect.push(quote! {
                operand_segment_sizes.push(#field.len() as u64);
                operand_vec.extend(#field);
            });
        } else {
            collect.push(quote! {
                operand_segment_sizes.push(u64::from(#field.is_some()));
                operand_vec.extend(#field);
            });
        }
    }
    let segment_sizes_setup = if has_variadic {
        quote! { let mut operand_segment_sizes: Vec<u64> = vec![]; }
    } else {
        quote! {}
    };
    let segment_sizes_attr = if has_variadic {
        quote! {
            parts.attributes.push(context.named_attribute(
                "operand_segment_sizes",
                tir::attributes::AttributeValue::Array(
                    operand_segment_sizes.into_iter()
                        .map(tir::attributes::AttributeValue::UInt)
                        .collect::<Vec<_>>().into(),
                ),
            ));
        }
    } else {
        quote! {}
    };
    OperandPieces {
        params,
        args,
        state_params,
        state_args,
        setup,
        collect,
        segment_sizes_setup,
        segment_sizes_attr,
    }
}

struct ResultPieces {
    accessor: proc_macro2::TokenStream,
    param: proc_macro2::TokenStream,
    arg: proc_macro2::TokenStream,
    build: proc_macro2::TokenStream,
}

fn make_result_pieces(has_results: bool, result_variadic: bool) -> ResultPieces {
    let accessor = if has_results {
        quote! { pub fn result(&self) -> tir::ValueId { self.0.results()[0] } }
    } else {
        quote! {}
    };
    let (param, arg, build) = if !has_results {
        (
            quote! {},
            quote! {},
            quote! { let result_vec: Vec<tir::ValueId> = vec![]; },
        )
    } else if result_variadic {
        (
            quote! { result_types: Vec<tir::TypeId>, },
            quote! { result_types, },
            quote! {
                let result_vec: Vec<tir::ValueId> = result_types.into_iter()
                    .map(|ty| context.create_value(ty, None).id()).collect();
            },
        )
    } else {
        (
            quote! { result_type: tir::TypeId, },
            quote! { result_type, },
            quote! {
                let result_vec = vec![context.create_value(result_type, None).id()];
            },
        )
    };
    ResultPieces {
        accessor,
        param,
        arg,
        build,
    }
}

/// The memory-order ports an op declares with `state: "in" | "out" | "in_out"`.
#[derive(Clone, Copy, Default)]
pub struct StatePorts {
    pub input: bool,
    pub output: bool,
}

struct StateResultPieces {
    param: proc_macro2::TokenStream,
    arg: proc_macro2::TokenStream,
    build: proc_macro2::TokenStream,
}

fn make_state_result_pieces(spec: Option<&ValueSpec>) -> StateResultPieces {
    if spec.is_some() {
        StateResultPieces {
            param: quote! { state_result_types: Vec<tir::TypeId>, },
            arg: quote! { state_result_types, },
            build: quote! {
                let mut result_vec = result_vec;
                result_vec.extend(state_result_types.into_iter()
                    .map(|ty| context.create_value(ty, None).id()));
            },
        }
    } else {
        StateResultPieces {
            param: quote! {},
            arg: quote! {},
            build: quote! {},
        }
    }
}

/// Explicit Rust formatting bindings, checked by the generated callback call.
pub enum RustFormat {
    Generic,
    Custom,
    Callbacks {
        print: syn::Expr,
        parse: Box<syn::Expr>,
    },
}

pub struct Operation {
    pub name: String,
    pub dialect: String,
    pub regions: Vec<Region>,
    pub attributes: Vec<AttrSpec>,
    pub operands: Vec<ValueSpec>,
    pub results: Vec<ValueSpec>,
    pub interfaces: Vec<Path>,
    pub interface_callbacks: Vec<(Path, syn::Expr)>,
    pub splice_callbacks: Vec<(String, syn::Expr)>,
    pub format: RustFormat,
    pub sem: Option<Sem>,
    pub semantic_hook: Option<syn::Expr>,
    pub poison: Option<Sem>,
    pub requires: Option<Sem>,
    pub custom_verifier: bool,
    pub verify_hook: Option<syn::Expr>,
    pub state: StatePorts,
    pub binds: Option<Binds>,
    pub counted: Option<Counted>,
    pub scheme: String,
}

/// A parsed `sem = "..."` declaration: the raw s-expression source plus the
/// `$splice` method names referenced inside it (discovered via tir-symbolic's
/// parser), which the codegen needs to generate the builder hooks.
pub struct Sem {
    pub recipe: proc_macro2::TokenStream,
    pub splices: Vec<String>,
}

pub struct Region {
    pub name: String,
    /// What the region's body may be: `Blocks` (a control-flow graph), `Nodes`
    /// (an unordered dependence graph), or `Any` for an op that accepts either.
    pub kind: String,
    /// A `variadic: true` region declares a group of zero or more regions rather than
    /// one, for an op whose arity is decided per instance (an n-ary conditional).
    pub variadic: bool,
}

impl Region {
    fn blocked(&self) -> bool {
        self.kind != "Nodes"
    }
}

#[derive(Clone)]
pub struct ValueSpec {
    pub name: String,
    pub ty: String,
    /// A `*`-prefixed operand accepts zero or more values (an MLIR-style variadic
    /// segment). Operand grouping is then recovered from the stored
    /// `operand_segment_sizes` attribute.
    pub variadic: bool,
}

impl ValueSpec {
    fn state() -> Self {
        Self {
            name: "state".to_string(),
            ty: format!("*{STATE_PATTERN}"),
            variadic: true,
        }
    }

    fn is_state(&self) -> bool {
        self.ty == format!("*{STATE_PATTERN}")
    }
}

const STATE_PATTERN: &str = "state<_>";

#[derive(Clone)]
pub struct AttrSpec {
    pub name: String,
    pub ty: String,
    pub vocabulary: Option<String>,
}

fn make_attribute_verifier(specs: &[AttrSpec]) -> proc_macro2::TokenStream {
    if specs.is_empty() {
        return quote! {};
    }
    let checks = specs.iter().filter(|s| !s.ty.starts_with('?')).map(|s| {
        let n = s.name.clone();
        quote! {
            if !parts.attributes.iter().any(|a| Some(a.name) == context.sym(#n)) {
                panic!(concat!("Missing required attribute: ", #n));
            }
        }
    });
    quote! { #(#checks)* }
}

fn make_region_accessors(regions: &[Region]) -> proc_macro2::TokenStream {
    if regions.is_empty() {
        return quote! {};
    }

    let accessors = regions.iter().enumerate().map(|(index, region)| {
        if region.variadic {
            make_variadic_region_accessor(region, index)
        } else if region.blocked() {
            make_entry_block_region_accessor(region, index)
        } else {
            make_region_accessor(region, index)
        }
    });

    quote! { #(#accessors)* }
}

fn make_region_accessor(region: &Region, index: usize) -> proc_macro2::TokenStream {
    let func_name = format_ident!("{}", region.name);
    quote! {
        pub fn #func_name(&self) -> tir::RegionHandle {
            use tir::Operation;
            self.0.context.get_region(self.0.regions()[#index])
        }
    }
}

fn make_variadic_region_accessor(region: &Region, index: usize) -> proc_macro2::TokenStream {
    let func_name = format_ident!("{}", region.name);
    quote! {
        pub fn #func_name(&self) -> tir::RegionIds {
            self.0.regions()[#index..].into()
        }
    }
}

fn make_entry_block_region_accessor(region: &Region, index: usize) -> proc_macro2::TokenStream {
    let func_name = format_ident!("{}", region.name);

    quote! {
        pub fn #func_name(&self) -> tir::BlockHandle {
            use tir::Operation;
            let context = self.0.context.clone();
            let region = self.0.context.get_region(self.0.regions()[#index]);
            let block = region.iter(context).next().unwrap();
            block
        }
    }
}

fn make_generic_printer(dialect: &str, name: &str) -> proc_macro2::TokenStream {
    let op_name = if dialect == "builtin" {
        name.to_string()
    } else {
        format!("{}.{}", dialect, name)
    };

    quote! {
        fn definition_print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
            tir::region_format::print_generic(fmt, self.0, #op_name)
        }
    }
}

fn make_parser(
    name: &str,
    regions: &[Region],
    operands: &[ValueSpec],
    attributes: &[AttrSpec],
    has_results: bool,
) -> proc_macro2::TokenStream {
    let operand_pieces = make_operand_pieces(operands);
    let (state_operands, operands): (Vec<ValueSpec>, Vec<ValueSpec>) =
        operands.iter().cloned().partition(ValueSpec::is_state);
    assert!(
        operands
            .iter()
            .rev()
            .skip(1)
            .all(|operand| !operand.variadic),
        "the generic syntax reads a variadic operand group only as the last one"
    );
    assert!(
        state_operands.len() <= 1,
        "the generic syntax reads one state operand group"
    );
    let state_parser = state_operands.first().map(|operand| {
        let field = format_ident!("{}", operand.name);
        quote! { let #field = parser.parse_state_operands(context)?; }
    });
    let operand_collect = &operand_pieces.collect;
    let segment_sizes_setup = &operand_pieces.segment_sizes_setup;
    let segment_sizes_attr = &operand_pieces.segment_sizes_attr;
    let region_pieces = make_region_pieces(regions);
    let region_fills = &region_pieces.fills;
    let attribute_verifier = make_attribute_verifier(attributes);
    let attr_spec_literals: Vec<_> = attributes
        .iter()
        .map(|attr| {
            let name = proc_macro2::Literal::string(&attr.name);
            let ty = proc_macro2::Literal::string(&attr.ty);
            quote! { (#name, #ty) }
        })
        .collect();
    let region_parsers = if regions.len() == 1 && !regions[0].variadic {
        let region_name = format_ident!("{}", regions[0].name);
        quote! { let #region_name = Some(parser.parse_region(context)?.id()); }
    } else {
        let defaults = &region_pieces.defaults;
        quote! { #(#defaults)* }
    };

    let operand_parsers: Vec<_> = operands
        .iter()
        .enumerate()
        .map(|(i, operand)| {
            let field = format_ident!("{}", operand.name);
            let comma = if i > 0 {
                quote! { parser.parse_token(","); }
            } else {
                quote! {}
            };
            if operand.variadic {
                quote! {
                    #comma
                    let mut group = vec![];
                    while let Some(ref_name) = parser.parse_value_ref() {
                        group.push(parser.resolve_value(context, ref_name));
                        if !parser.parse_token(",") {
                            break;
                        }
                    }
                    let #field = group;
                }
            } else {
                quote! {
                    #comma
                    let #field = parser.parse_value_ref()
                        .map(|ref_name| parser.resolve_value(context, ref_name));
                }
            }
        })
        .collect();

    let result_parser = if !has_results {
        quote! {}
    } else {
        // The generic form spells one result, including for a variadic group.
        quote! {
            if !parser.parse_token(":") {
                return Err((parser.span(), tir::Error::ExpectedToken(":")));
            }
            let result_ty = parser.parse_type(context)?
                .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?;
        }
    };
    let region_mut = (!region_fills.is_empty()).then(|| quote! { mut });
    let operand_mut = (!operand_collect.is_empty()).then(|| quote! { mut });
    let result_build = if has_results {
        quote! { let result_vec = vec![context.create_value(result_ty, None).id()]; }
    } else {
        quote! { let result_vec: Vec<tir::ValueId> = vec![]; }
    };

    quote! {
        fn definition_parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
        -> Result<tir::OpHandle, (tir::parse::Span, tir::Error)> {
           use tir::parse::common::Cursor;

           let mut parsed_attrs: Vec<tir::attributes::NamedAttribute> = vec![];
           let attr_specs: &[(&str, &str)] = &[#(#attr_spec_literals),*];

           #(#operand_parsers)*
           #state_parser

           // Parse optional generic attribute dict: { key = value, ... }
           let mark = parser.pos();
           if parser.parse_token("{") {
               let mut ok = true;
               if !parser.parse_token("}") {
                   loop {
                       if let Some(name) = parser.parse_attribute_name() {
                           if !parser.parse_token("=") { ok = false; break; }
                           let mut val = if let Some(value) = parser.parse_attribute_value(context)? {
                               value
                           } else {
                               ok = false; break;
                           };
                           if attr_specs.iter().any(|(attr_name, ty)| *attr_name == name && *ty == "UInt") {
                               val = match val {
                                   tir::attributes::AttributeValue::Int(value) if value >= 0 => {
                                       tir::attributes::AttributeValue::UInt(value as u64)
                                   }
                                   tir::attributes::AttributeValue::Int(_) => { ok = false; break; }
                                   other => other,
                               };
                           }
                           if attr_specs.iter().any(|(attr_name, ty)| *attr_name == name && *ty == "FpSemantics") {
                               val = match val {
                                   tir::attributes::AttributeValue::FpSemantics(_) => val,
                                   other => match tir::fp::Semantics::parse_for_operation(&other, #name) {
                                       Ok(semantics) => tir::attributes::AttributeValue::FpSemantics(
                                           context.intern_fp_semantics(semantics),
                                       ),
                                       Err(error) => return Err((parser.span(), error)),
                                   },
                               };
                           }
                           if attr_specs.iter().any(|(attr_name, ty)| *attr_name == name && *ty == "EvaluationContract") {
                               val = match val {
                                   tir::attributes::AttributeValue::EvaluationContract(_) => val,
                                   other => match tir::fp::EvaluationContract::parse_attribute(&other) {
                                       Ok(contract) => tir::attributes::AttributeValue::EvaluationContract(
                                           context.intern_evaluation_contract(contract),
                                       ),
                                       Err(error) => return Err((parser.span(), error)),
                                   },
                               };
                           }
                           parsed_attrs.push(tir::attributes::NamedAttribute::new(context.intern(&name), val));
                           if parser.parse_token("}") { break; }
                           if !parser.parse_token(",") { ok = false; break; }
                       } else { ok = false; break; }
                   }
               }
               if !ok {
                   parser.set_pos(mark);
                   parsed_attrs.clear();
               }
           }

           #result_parser

           #region_parsers

            let mut parts = tir::NewOpParts::default();
            for &(attr_name, ty) in attr_specs {
                if ty == "FpSemantics"
                    && !parsed_attrs.iter().any(|attr| attr.name == context.intern(attr_name))
                {
                    parts.attributes.push(context.named_attribute(
                        attr_name, tir::attributes::AttributeValue::FpSemantics(
                            context.intern_fp_semantics(tir::fp::Semantics::default_for_operation(#name)),
                        ),
                    ));
                }
            }
            parts.attributes.extend(parsed_attrs);
            let #region_mut regions = vec![];
            #(#region_fills)*
            #attribute_verifier
            let #operand_mut operand_vec: Vec<tir::ValueId> = vec![];
            #segment_sizes_setup
            #(#operand_collect)*
            #result_build
            #segment_sizes_attr
            let instance = tir::NewOp::for_definition(definition,
                context.clone(), operand_vec, result_vec, regions, parts,
            );
            Ok(context.add_operation(tir::InsertionPoint::Root, instance))
        }
    }
}

fn rewrite_tokens(
    tokens: proc_macro2::TokenStream,
    prefix: Option<&str>,
    constructor: Option<&Ident>,
    definition: Option<&Ident>,
    unbox: bool,
) -> proc_macro2::TokenStream {
    use proc_macro2::{Group, TokenTree};
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    let mut output = proc_macro2::TokenStream::new();
    let mut index = 0;
    let is = |at: usize, text: &str| {
        tokens
            .get(at)
            .is_some_and(|token| token.to_string() == text)
    };
    while index < tokens.len() {
        if is(index, "self") && is(index + 1, ".") && is(index + 2, "0") {
            output.extend(quote! {operation});
            index += 3;
            continue;
        }
        if is(index, "self") {
            output.extend(quote! {operation});
            index += 1;
            continue;
        }
        if is(index, "__TIR_DEFINITION")
            && let Some(definition) = definition
        {
            output.extend(quote! {&#definition});
            index += 1;
            continue;
        }
        if is(index, "Self")
            && is(index + 1, ":")
            && is(index + 2, ":")
            && is(index + 3, "new")
            && let Some(constructor) = constructor
        {
            output.extend(quote! {#constructor});
            index += 4;
            continue;
        }
        if unbox
            && is(index, "Box")
            && is(index + 1, ":")
            && is(index + 2, ":")
            && is(index + 3, "new")
            && let Some(TokenTree::Group(group)) = tokens.get(index + 4)
        {
            output.extend(rewrite_tokens(
                group.stream(),
                prefix,
                constructor,
                definition,
                unbox,
            ));
            index += 5;
            continue;
        }
        if is(index, "<")
            && is(index + 1, "Self")
            && is(index + 2, "as")
            && let Some(prefix) = prefix
            && let Some(end) = (index + 3..tokens.len()).find(|&at| is(at, ">"))
            && is(end + 1, ":")
            && is(end + 2, ":")
            && let (Some(TokenTree::Ident(interface)), Some(TokenTree::Ident(method))) =
                (tokens.get(end - 1), tokens.get(end + 3))
        {
            let function = format_ident!(
                "{}_{}_{}",
                prefix,
                interface.to_string().to_lowercase(),
                method
            );
            output.extend(quote! {#function});
            index = end + 4;
            continue;
        }
        match &tokens[index] {
            TokenTree::Group(group) => {
                let mut rewritten = Group::new(
                    group.delimiter(),
                    rewrite_tokens(group.stream(), prefix, constructor, definition, unbox),
                );
                rewritten.set_span(group.span());
                output.extend([TokenTree::Group(rewritten)]);
            }
            token => output.extend([token.clone()]),
        }
        index += 1;
    }
    output
}

fn replace_definition_constructor(
    tokens: proc_macro2::TokenStream,
    definition: &Ident,
) -> proc_macro2::TokenStream {
    rewrite_tokens(tokens, None, None, Some(definition), false)
}
fn free_method(
    tokens: proc_macro2::TokenStream,
    name: Ident,
    definition: Option<&Ident>,
) -> proc_macro2::TokenStream {
    let mut method = match syn::parse2::<syn::ImplItemFn>(tokens) {
        Ok(method) => method,
        Err(error) => return error.into_compile_error(),
    };
    let printer = method.sig.ident == "definition_print";
    let binding_prefix = name.to_string();
    let binding_prefix = binding_prefix
        .strip_prefix("__tir_")
        .and_then(|name| {
            name.strip_suffix("_print")
                .or_else(|| name.strip_suffix("_parse"))
        })
        .map(str::to_owned);
    method.sig.ident = name;
    if printer {
        method.sig.generics = syn::Generics::default();
        method.sig.inputs =
            syn::parse_quote!(operation:&tir::OpHandle,fmt:&mut tir::IRFormatter<'_>);
    }
    for argument in &mut method.sig.inputs {
        if matches!(argument, syn::FnArg::Receiver(_)) {
            *argument = syn::parse_quote!(operation:&tir::OpHandle);
        }
    }
    let constructor =
        definition.map(|definition| format_ident!("r#{}", definition.to_string().to_lowercase()));
    if definition.is_some() {
        method
            .sig
            .inputs
            .push(syn::parse_quote!(definition:&'static tir::OpDefSpec));
        method.sig.output =
            syn::parse_quote!(->Result<tir::OpHandle,(tir::parse::Span,tir::Error)>);
    }
    // Quote the block separately; an ImplItemFn is not a field projection in quote.
    let block = &method.block;
    let body = rewrite_tokens(
        quote! {#block},
        binding_prefix.as_deref(),
        constructor.as_ref(),
        definition,
        definition.is_some(),
    );
    method.block = match syn::parse2(body) {
        Ok(block) => block,
        Err(error) => return error.into_compile_error(),
    };
    quote! {#method}
}
fn free_accessors(tokens: proc_macro2::TokenStream, prefix: &str) -> proc_macro2::TokenStream {
    let implementation = match syn::parse2::<syn::ItemImpl>(quote! {impl tir::OpHandle {#tokens}}) {
        Ok(implementation) => implementation,
        Err(error) => return error.into_compile_error(),
    };
    let mut output = proc_macro2::TokenStream::new();
    for item in implementation.items {
        if let syn::ImplItem::Fn(function) = item {
            let name = format_ident!(
                "{}_{}",
                prefix,
                function.sig.ident.to_string().trim_start_matches("r#")
            );
            output.extend(free_method(quote! {#function}, name, None));
        }
    }
    output
}
fn free_binding_callbacks(
    tokens: proc_macro2::TokenStream,
    prefix: &str,
) -> (proc_macro2::TokenStream, Vec<(Path, syn::Expr)>) {
    let file = match syn::parse2::<syn::File>(tokens) {
        Ok(file) => file,
        Err(error) => return (error.into_compile_error(), Vec::new()),
    };
    let mut output = proc_macro2::TokenStream::new();
    let mut bindings = Vec::new();
    for item in file.items {
        let syn::Item::Impl(implementation) = item else {
            continue;
        };
        let Some((_, interface, _)) = implementation.trait_ else {
            continue;
        };
        let Some(last) = interface.segments.last() else {
            continue;
        };
        let interface_name = last.ident.to_string();
        let mut callbacks: syn::Path = if interface.segments.len() == 1 {
            syn::parse_quote!(tir::#last)
        } else {
            interface.clone()
        };
        if let Some(last) = callbacks.segments.last_mut() {
            last.ident = format_ident!("{}Callbacks", interface_name);
        }
        let optional: &[&str] = match interface_name.as_str() {
            "Gamma" => &["indexes_arms", "arm_for"],
            "CountedLoop" => &["induction"],
            _ => &[],
        };
        let mut fields = Vec::new();
        let mut supplied = std::collections::BTreeSet::new();
        for item in implementation.items {
            let syn::ImplItem::Fn(method) = item else {
                continue;
            };
            let field = method.sig.ident.clone();
            supplied.insert(field.to_string());
            let function = format_ident!("{}_{}_{}", prefix, interface_name.to_lowercase(), field);
            output.extend(free_method(quote! {#method}, function.clone(), None));
            fields.push(if optional.contains(&field.to_string().as_str()) {
                quote! {#field:Some(#function)}
            } else {
                quote! {#field:#function}
            });
        }
        for name in optional.iter().filter(|name| !supplied.contains(**name)) {
            let name = format_ident!("{}", name);
            fields.push(quote! {#name:None});
        }
        let expression = match syn::parse2(quote! {&#callbacks{#(#fields),*}}) {
            Ok(expression) => expression,
            Err(error) => return (error.into_compile_error(), Vec::new()),
        };
        bindings.push((interface, expression));
    }
    (output, bindings)
}
fn semantic_callback(
    semantic: &Option<Sem>,
    prefix: &str,
    which: &str,
    has_results: bool,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let Some(semantic) = semantic else {
        return (quote! {}, quote! {None});
    };
    let function = format_ident!("__tir_{}_{}", prefix, which);
    let recipe = &semantic.recipe;
    let provenance = (which == "semantic")
        .then(|| quote! {graph.annotation_mut(root).original_op=Some(operation.id);});
    let type_annotation=has_results.then(||quote!{graph.annotation_mut(root).actual_type=operation.results().first().map(|value|operation.context.get_value(*value).ty());});
    (
        quote! {fn #function(operation:&tir::OpHandle,graph:&mut tir::sem::SemGraph)->Option<tir::NodeId>{
            let root=tir::sem::build_defined_recipe(operation,graph,&#recipe)?;
            #provenance
            #type_annotation
            Some(root)
        }},
        quote! {Some(#function)},
    )
}
