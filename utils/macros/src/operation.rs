use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, Ident, Path, Token, braced, parse::Parse, parse_macro_input};
use tir_symbolic::scheme::{Arity, SchemeSyntax, SlotSyntax};

use crate::binds::{Binds, BindsCode, Counted, OpShape};
use crate::utils::{expr_as_path_vec, expr_as_string, field_name, op_fn_ident};

pub fn construct_operation(item: TokenStream) -> TokenStream {
    let Operation {
        struct_name,
        name,
        dialect,
        regions,
        attributes,
        operands,
        results,
        interfaces,
        custom_format,
        sem,
        poison,
        requires,
        custom_verifier,
        state,
        binds,
        counted,
        mut scheme,
    } = parse_macro_input!(item as Operation);

    // `state:` appends optional resource-state ports. Effects are declared by
    // operation interfaces, independently of whether a threading pass has
    // supplied those ports.
    let mut operands = operands;
    let mut results = results;
    if state.input {
        operands.push(ValueSpec::state());
        scheme.operands.push(ValueSpec::state_slot());
    }
    if state.output {
        results.push(ValueSpec::state());
        scheme.results.push(ValueSpec::state_slot());
    }
    let mut interfaces = interfaces;
    let binds_code = binds.as_ref().map(|binds| {
        assert!(
            operands.iter().all(|operand| !operand.ty.starts_with('?')),
            "binds: aligns fixed or variadic operand groups, not optional ones"
        );
        assert!(
            attributes.is_empty() || custom_format,
            "binds: the generic syntax spells no attributes; declare a custom format"
        );
        let operand_shape: Vec<(String, bool)> = operands
            .iter()
            .map(|operand| (operand.name.clone(), operand.variadic))
            .collect();
        let region_shape: Vec<(String, bool)> = regions
            .iter()
            .map(|region| (region.name.clone(), region.variadic))
            .collect();
        crate::binds::emit(
            binds,
            counted.as_ref(),
            &OpShape {
                struct_name: &struct_name,
                spelled: format!("{dialect}.{name}"),
                operands: &operand_shape,
                regions: &region_shape,
                custom_format,
                state_results: results.iter().any(ValueSpec::is_state),
            },
        )
    });
    assert!(
        counted.is_none() || binds.is_some(),
        "counted: needs the Theta binding it pins"
    );
    // State results follow the value results, independently of value arity.
    let (state_results, value_results): (Vec<ValueSpec>, Vec<ValueSpec>) =
        results.iter().cloned().partition(ValueSpec::is_state);
    assert!(
        state_results.len() <= 1,
        "an op declares at most one state result group"
    );
    let has_results = !value_results.is_empty();
    // A `*`-prefixed result makes the op n-ary: it produces one value per type given
    // to the constructor. Used by structured control flow, which carries n values.
    let result_variadic = value_results.len() > 1
        || value_results
            .iter()
            .any(|r| r.variadic || r.ty.starts_with('?'));
    assert!(
        !value_results.iter().any(|r| r.variadic) || value_results.len() == 1,
        "a variadic result must be the only declared result"
    );
    let state_result_pieces = make_state_result_pieces(state_results.first());
    let op_fn_name = op_fn_ident(&name);

    let BindsCode {
        interfaces: binds_interfaces,
        impls: binds_impls,
        verify: binds_verify,
        printer: binds_printer,
        parser: binds_parser,
    } = binds_code.unwrap_or_default();

    let printer = match binds_printer {
        Some(printer) => printer,
        None if custom_format => make_custom_printer(),
        None => make_generic_printer(&dialect, &name),
    };

    let region_accessors = make_region_accessors(&regions);
    let region_pieces = make_region_pieces(&regions);

    let parser = match binds_parser {
        Some(parser) => parser,
        None if custom_format => make_custom_parser(),
        None => make_parser(&name, &regions, &operands, &attributes, has_results),
    };

    let attribute_verifier = make_attribute_verifier(&attributes);

    let operand_pieces = make_operand_pieces(&operands);

    // An op that declares `sem` can be folded over constant operands by evaluating
    // that expression, so derive `ConstantFold` for it automatically (unless the op
    // already lists the interface, e.g. a hand-written fold).
    let has_sem = sem.is_some();
    let already_lists_fold = interfaces.iter().any(|path| {
        path.segments
            .last()
            .is_some_and(|seg| seg.ident == "ConstantFold")
    });
    let derive_constant_fold = has_sem && !already_lists_fold;
    let constant_fold_impl = if derive_constant_fold {
        quote! {
            impl tir::ConstantFold for #struct_name {
                fn fold(&self, operands: &[tir::sem::Value]) -> Option<tir::sem::Value> {
                    tir::sem::fold_with_sem(self, operands)
                }
            }
        }
    } else {
        quote! {}
    };
    if derive_constant_fold {
        interfaces.push(syn::parse_quote!(tir::ConstantFold));
    }
    interfaces.extend(binds_interfaces);

    let (semantic_expr_method, as_sem_expr_impl) =
        make_sem_impls(&sem, &struct_name, &operands, has_results);
    let undefined_methods = make_undefined_methods(&poison, &requires, &operands);
    let splices: Vec<String> = [&sem, &poison, &requires]
        .into_iter()
        .flatten()
        .flat_map(|sem| sem.splices.iter().cloned())
        .fold(Vec::new(), |mut all, splice| {
            if !all.contains(&splice) {
                all.push(splice);
            }
            all
        });
    let any_sem = sem.is_some() || poison.is_some() || requires.is_some();
    let sem_hooks_impl = if any_sem {
        make_sem_hooks(&splices, &struct_name, has_results)
    } else {
        quote! {}
    };

    let interface_registration_method = if interfaces.is_empty() {
        quote! {}
    } else {
        let registrations = interfaces.iter().map(|interface| {
            quote! {
                context.register_operation_interface::<#struct_name, dyn #interface>();
            }
        });
        quote! {
            fn register_interfaces(context: &tir::Context) {
                #(#registrations)*
            }
        }
    };

    let interface_impls = interfaces.iter().map(|interface| {
        quote! {
            impl tir::ImplementsOpInterface<dyn #interface> for #struct_name {
                fn into_interface(self: Box<Self>) -> Box<dyn #interface> {
                    self
                }
            }
        }
    });

    let result_pieces = make_result_pieces(has_results, result_variadic);
    // Optional attributes are supplied through NewOpParts.
    let required = || attributes.iter().filter(|attr| !attr.ty.starts_with('?'));
    let attr_fn_params: Vec<_> = required()
        .map(|attr| {
            let name = op_fn_ident(&attr.name);
            quote! { #name: impl Into<tir::attributes::AttributeValue> }
        })
        .collect();

    let attr_fn_args: Vec<_> = required().map(|attr| op_fn_ident(&attr.name)).collect();
    let attr_fills: Vec<_> = required()
        .map(|attr| {
            let name_ident = op_fn_ident(&attr.name);
            let name_str = attr.name.clone();
            quote! {
                parts.attributes.push(context.named_attribute(#name_str, #name_ident.into()));
            }
        })
        .collect();

    let verifiable_impl = if custom_verifier {
        quote! {}
    } else {
        quote! { impl tir::Verifiable for #struct_name {} }
    };

    let schema_ident = format_ident!("__TIR_OP_SCHEMA_{}", struct_name);
    let schema_registration = emit_schema(
        &struct_name,
        &dialect,
        &name,
        (&operands, &results),
        &attributes,
        &interfaces,
        (&scheme.to_string(), has_sem),
    );
    let opdef_verifier = emit_opdef_verifier(&struct_name, &schema_ident, &regions, &binds_verify);

    let attr_getters = make_attr_accessors(&attributes, &format!("{dialect}.{name}"));
    let result_accessor = &result_pieces.accessor;
    let (constructor, op_fn_code) = emit_constructors(
        &op_fn_name,
        &struct_name,
        &region_pieces,
        &operand_pieces,
        &result_pieces,
        &state_result_pieces,
        (&attr_fn_params, &attr_fn_args),
        &attr_fills,
        &attribute_verifier,
    );

    quote! {
        pub struct #struct_name(tir::OpHandle);

        #schema_registration
        #opdef_verifier

        #(#interface_impls)*
        #verifiable_impl
        #binds_impls
        #sem_hooks_impl
        #as_sem_expr_impl
        #constant_fold_impl

        impl #struct_name {
            #constructor
            #region_accessors
            #result_accessor
            #attr_getters
        }

        impl tir::Operation for #struct_name {
            fn name() -> &'static str
            where
                Self: Sized
            {
                #name
            }

            fn dialect() -> &'static str
            where
                Self: Sized
            {
                #dialect
            }

            fn id(&self) -> tir::OpId {
                self.0.id
            }

            fn handle(&self) -> &tir::OpHandle {
                &self.0
            }

            fn from_op_instance(instance: tir::OpHandle) -> Self {
                assert_eq!(instance.name(), tir::OperationName::of::<Self>());
                #struct_name(instance)
            }

            fn from_op_instance_dyn(instance: tir::OpHandle) -> Box<dyn tir::Operation> {
                assert_eq!(instance.name(), tir::OperationName::of::<Self>());
                Box::new(#struct_name(instance))
            }

            fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
                self
            }

            #printer

            #parser

            #semantic_expr_method
            #undefined_methods
            #interface_registration_method
        }

        impl From<&#struct_name> for tir::OpId {
            fn from(v: &#struct_name) -> tir::OpId {
                use tir::Operation;
                v.id()
            }
        }

        impl From<#struct_name> for tir::OpId {
            fn from(v: #struct_name) -> tir::OpId {
                use tir::Operation;
                v.id()
            }
        }

        #op_fn_code
    }
    .into()
}
type TokenStreams = Vec<proc_macro2::TokenStream>;

fn emit_schema(
    struct_name: &Ident,
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

    let schema_ident = format_ident!("__TIR_OP_SCHEMA_{}", struct_name);
    quote! {
        #[allow(non_upper_case_globals)]
        #[tir::linkme::distributed_slice(tir::OP_SCHEMAS)]
        #[linkme(crate = tir::linkme)]
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

fn emit_opdef_verifier(
    struct_name: &Ident,
    schema_ident: &Ident,
    regions: &[Region],
    binds_verify: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let region_kinds: Vec<_> = regions
        .iter()
        .map(|region| format_ident!("{}", region.kind))
        .collect();

    quote! {
        impl tir::OpDefVerifiable for #struct_name {
            fn verify_operands(&self, context: &tir::Context) -> Result<(), tir::Error> {
                static SPEC: tir::OpDefSpec = tir::OpDefSpec {
                    schema: &#schema_ident,
                    region_kinds: &[#(tir::RegionKind::#region_kinds),*],
                };
                tir::verify_opdef_operands(context, &self.0, <Self as tir::Operation>::name(), &SPEC)?;
                #binds_verify
                Ok(())
            }
            fn verify_attributes(&self, context: &tir::Context) -> Result<(), tir::Error> {
                tir::verify_opdef_attributes(
                    context,
                    &self.0,
                    <Self as tir::Operation>::name(),
                    #schema_ident.attributes,
                )
            }

        }
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
    struct_name: &Ident,
    regions: &RegionPieces,
    operands: &OperandPieces,
    results: &ResultPieces,
    state_results: &StateResultPieces,
    (attr_params, attr_args): (&TokenStreams, &[Ident]),
    attr_fills: &TokenStreams,
    attribute_verifier: &proc_macro2::TokenStream,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let (region_params, region_args, region_fills) =
        (&regions.params, &regions.args, &regions.fills);
    let (operand_params, operand_args, operand_setup, operand_collect) = (
        &operands.params,
        &operands.args,
        &operands.setup,
        &operands.collect,
    );
    let (state_params, state_args) = (&operands.state_params, &operands.state_args);
    let (result_param, result_arg, result_build) = (&results.param, &results.arg, &results.build);
    let (state_result_param, state_result_arg, state_result_build) = (
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
    let constructor = quote! {
        #[allow(clippy::too_many_arguments)]
        pub fn new(#params) -> Self {
            self::#op_fn_name(
                context, #(#operand_args,)* #(#attr_args,)*
                #result_arg #(#region_args,)* #(#state_args,)* #state_result_arg parts, insertion,
            )
        }
    };
    let function = quote! {
        // Generated: one parameter per declared port.
        #[allow(clippy::too_many_arguments)]
        pub fn #op_fn_name(#params) -> #struct_name {
            #parts_setup
            let mut regions = vec![];
            #(#region_fills)*
            #attribute_verifier
            #(#operand_setup)*
            let mut operand_vec: Vec<tir::ValueId> = vec![];
            #segment_sizes_setup
            #(#operand_collect)*
            #result_build
            #state_result_build
            #segment_sizes_attr
            let instance = tir::NewOp::new::<#struct_name>(
                context.clone(), operand_vec, result_vec, regions, parts,
            );
            #struct_name(context.add_operation(insertion, instance))
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

fn make_sem_impls(
    sem: &Option<Sem>,
    struct_name: &Ident,
    operands: &[ValueSpec],
    has_results: bool,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let Some(sem) = sem else {
        return (quote! {}, quote! {});
    };
    let src = &sem.src;
    let sym_pairs: Vec<proc_macro2::TokenStream> = operands
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let name = &o.name;
            let idx = i as u32;
            quote! { (#name, #idx) }
        })
        .collect();
    let actual_type_setter = if has_results {
        quote! {
            let __tir_ctx = self.0.context.clone();
            let __tir_ty = __tir_ctx.get_value(self.result()).ty();
            g.annotation_mut(__tir_sem_root).actual_type = Some(__tir_ty);
        }
    } else {
        quote! {}
    };
    let sem_method = quote! {
        fn semantic_expr(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
            let __tir_sem_root = tir_symbolic::lang::build(g, #src, &[#(#sym_pairs),*], self).ok()?;
            g.annotation_mut(__tir_sem_root).original_op = Some(<Self as tir::Operation>::id(self));
            #actual_type_setter
            Some(__tir_sem_root)
        }
    };
    let as_impl = quote! {
        impl tir::sem::AsSemExpr for #struct_name {
            fn convert(&self, g: &mut tir::sem::SemGraph) -> tir::NodeId {
                let __tir_sem_root = tir_symbolic::lang::build(g, #src, &[#(#sym_pairs),*], self)
                    .expect("semantic expression should build");
                g.annotation_mut(__tir_sem_root).original_op = Some(<Self as tir::Operation>::id(self));
                #actual_type_setter
                __tir_sem_root
            }
        }
    };
    (sem_method, as_impl)
}

fn make_sem_hooks(
    splices: &[String],
    struct_name: &Ident,
    has_results: bool,
) -> proc_macro2::TokenStream {
    let splice_arms: Vec<proc_macro2::TokenStream> = splices
        .iter()
        .map(|name| {
            let method = format_ident!("{}", name);
            // `.into()` accepts both `NodeId` and `Option<NodeId>` splice
            // methods, so a splice can signal "un-lowerable" with `None`.
            quote! { #name => self.#method(g).into(), }
        })
        .collect();
    let result_width_body = if has_results {
        quote! {
            let __ctx = self.0.context.clone();
            let __ty = __ctx.get_value(self.0.results()[0]).ty();
            Some(
                tir::builtin::IntegerType::view(&__ctx, __ty)
                    .map(|t| t.width() as u64)
                    .unwrap_or(0),
            )
        }
    } else {
        quote! { None }
    };
    quote! {
        impl tir_symbolic::lang::SemBuilderHooks<tir::sem::SemGraph> for #struct_name {
            fn splice(&self, __name: &str, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
                match __name {
                    #(#splice_arms)*
                    _ => None,
                }
            }
            fn result_width(&self) -> Option<u64> {
                #result_width_body
            }
        }
    }
}

fn make_undefined_methods(
    poison: &Option<Sem>,
    requires: &Option<Sem>,
    operands: &[ValueSpec],
) -> proc_macro2::TokenStream {
    let sym_pairs: Vec<proc_macro2::TokenStream> = operands
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let name = &o.name;
            let idx = i as u32;
            quote! { (#name, #idx) }
        })
        .collect();
    let method = |name: &str, condition: &Option<Sem>| {
        let Some(condition) = condition else {
            return quote! {};
        };
        let name = format_ident!("{}", name);
        let src = &condition.src;
        quote! {
            fn #name(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
                tir_symbolic::lang::build(g, #src, &[#(#sym_pairs),*], self).ok()
            }
        }
    };
    let poison_method = method("poison_expr", poison);
    let requires_method = method("requires_expr", requires);
    quote! {
        #poison_method
        #requires_method
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
struct StatePorts {
    input: bool,
    output: bool,
}

impl StatePorts {
    fn parse(spec: &str) -> Self {
        match spec {
            "in" => Self {
                input: true,
                output: false,
            },
            "out" => Self {
                input: false,
                output: true,
            },
            "in_out" => Self {
                input: true,
                output: true,
            },
            other => panic!("state must be one of \"in\", \"out\", \"in_out\", got '{other}'"),
        }
    }
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

struct Operation {
    struct_name: Ident,
    name: String,
    dialect: String,
    regions: Vec<Region>,
    attributes: Vec<AttrSpec>,
    operands: Vec<ValueSpec>,
    results: Vec<ValueSpec>,
    interfaces: Vec<Path>,
    custom_format: bool,
    sem: Option<Sem>,
    poison: Option<Sem>,
    requires: Option<Sem>,
    custom_verifier: bool,
    state: StatePorts,
    binds: Option<Binds>,
    counted: Option<Counted>,
    scheme: SchemeSyntax,
}

/// One `key: value` of the declaration. `binds:` and `counted:` are read as
/// their own grammars (`~` is no Rust operator); everything else is an
/// expression.
enum Field {
    Expr(Expr),
    Binds(Binds),
    Counted(Counted),
}

/// The declaration as written: the op's struct name and its fields in order.
struct Fields {
    struct_name: Ident,
    fields: Vec<(String, Field)>,
}

impl Fields {
    fn expr(&self, key: &str) -> Option<&Expr> {
        self.fields.iter().find_map(|(name, field)| match field {
            Field::Expr(expr) if name == key => Some(expr),
            _ => None,
        })
    }
}

impl Parse for Fields {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let struct_name: Ident = input.parse()?;
        let content;
        braced!(content in input);
        let mut fields = vec![];
        while !content.is_empty() {
            let key: Ident = content.parse()?;
            content.parse::<Token![:]>()?;
            let field = match key.to_string().as_str() {
                "binds" => Field::Binds(content.parse()?),
                "counted" => Field::Counted(content.parse()?),
                _ => Field::Expr(content.parse()?),
            };
            fields.push((key.to_string(), field));
            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }
        Ok(Fields {
            struct_name,
            fields,
        })
    }
}

/// A parsed `sem = "..."` declaration: the raw s-expression source plus the
/// `$splice` method names referenced inside it (discovered via tir-symbolic's
/// parser), which the codegen needs to generate the builder hooks.
struct Sem {
    src: String,
    splices: Vec<String>,
}

struct Region {
    name: String,
    /// What the region's body may be: `Blocks` (a control-flow graph), `Nodes`
    /// (an unordered dependence graph), or `Any` for an op that accepts either.
    kind: String,
    /// A `variadic: true` region declares a group of zero or more regions rather than
    /// one, for an op whose arity is decided per instance (an n-ary conditional).
    variadic: bool,
}

impl Region {
    fn blocked(&self) -> bool {
        self.kind != "Nodes"
    }
}

#[derive(Clone)]
struct ValueSpec {
    name: String,
    ty: String,
    /// A `*`-prefixed operand accepts zero or more values (an MLIR-style variadic
    /// segment). Operand grouping is then recovered from the stored
    /// `operand_segment_sizes` attribute.
    variadic: bool,
}

impl ValueSpec {
    fn state() -> Self {
        Self {
            name: "state".to_string(),
            ty: format!("*{STATE_PATTERN}"),
            variadic: true,
        }
    }

    fn state_slot() -> SlotSyntax {
        SchemeSyntax::slot(STATE_PATTERN, Arity::Many).expect("a state pattern")
    }

    fn is_state(&self) -> bool {
        self.ty == format!("*{STATE_PATTERN}")
    }

    fn typed(names: Vec<String>, slots: &[SlotSyntax], what: &str) -> Vec<Self> {
        assert_eq!(
            names.len(),
            slots.len(),
            "the scheme types {} {what}, the op declares {}",
            slots.len(),
            names.len()
        );
        names
            .into_iter()
            .zip(slots)
            .map(|(name, slot)| {
                let marker = match slot.arity {
                    Arity::One => "",
                    Arity::Opt => "?",
                    Arity::Many => "*",
                };
                ValueSpec {
                    name,
                    ty: format!("{marker}{}", slot.pattern()),
                    variadic: slot.arity == Arity::Many,
                }
            })
            .collect()
    }
}

const STATE_PATTERN: &str = "state<_>";

impl Parse for Operation {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut declared: Fields = input.parse()?;
        let struct_name = declared.struct_name.clone();
        let name = expr_as_string(declared.expr("name").expect("an operation has a name"));
        let dialect = expr_as_string(
            declared
                .expr("dialect")
                .expect("an operation belongs to a dialect"),
        );
        let regions = declared
            .expr("regions")
            .and_then(get_regions)
            .unwrap_or_default();
        let attributes = declared
            .expr("attributes")
            .and_then(get_attributes)
            .unwrap_or_default();
        let scheme = declared
            .expr("scheme")
            .map(expr_as_string)
            .unwrap_or_else(|| panic!("operation `{dialect}.{name}` declares no `scheme:`"));
        let scheme = SchemeSyntax::parse(&scheme)
            .unwrap_or_else(|error| panic!("operation `{dialect}.{name}`: {error}"));
        let operands = ValueSpec::typed(
            declared
                .expr("operands")
                .map(get_value_names)
                .unwrap_or_default(),
            &scheme.operands,
            "operands",
        );
        let results = ValueSpec::typed(
            declared
                .expr("results")
                .map(get_value_names)
                .unwrap_or_default(),
            &scheme.results,
            "results",
        );
        let interfaces = declared
            .expr("interfaces")
            .map(expr_as_path_vec)
            .unwrap_or_default();
        let custom_format = declared
            .expr("format")
            .is_some_and(|expr| expr_as_string(expr) == "custom");
        let custom_verifier = declared
            .expr("verifier")
            .is_some_and(|expr| expr_as_string(expr) == "true");
        let state = declared
            .expr("state")
            .map(|expr| StatePorts::parse(&expr_as_string(expr)))
            .unwrap_or_default();
        let sem = declared.expr("sem").and_then(parse_sem);
        let poison = declared
            .expr("poison")
            .map(|expr| parse_condition("poison", expr));
        let requires = declared
            .expr("requires")
            .map(|expr| parse_condition("requires", expr));
        let (mut binds, mut counted) = (None, None);
        for (_, field) in declared.fields.drain(..) {
            match field {
                Field::Binds(spec) => binds = Some(spec),
                Field::Counted(spec) => counted = Some(spec),
                Field::Expr(_) => {}
            }
        }

        Ok(Operation {
            struct_name,
            name,
            dialect,
            regions,
            attributes,
            operands,
            results,
            interfaces,
            custom_format,
            sem,
            poison,
            requires,
            custom_verifier,
            state,
            binds,
            counted,
            scheme,
        })
    }
}

fn parse_sem(expr: &Expr) -> Option<Sem> {
    let src = match expr {
        Expr::Lit(lit) => match &lit.lit {
            syn::Lit::Str(s) => s.value(),
            _ => return None,
        },
        _ => return None,
    };
    let ast = tir_symbolic::lang::parse(&src)?;
    let splices = ast.splice_names();
    Some(Sem { src, splices })
}

fn parse_condition(field: &str, expr: &Expr) -> Sem {
    let condition = expr_as_string(expr);
    let src = format!("(set {field} {condition})");
    let ast = tir_symbolic::lang::parse(&src)
        .unwrap_or_else(|| panic!("`{field}:` is no s-expression: {condition}"));
    Sem {
        splices: ast.splice_names(),
        src,
    }
}

fn get_regions(expr: &Expr) -> Option<Vec<Region>> {
    if let Expr::Struct(s) = expr {
        Some(
            s.fields
                .iter()
                .map(|f| {
                    let name = field_name(f);
                    Region {
                        name,
                        kind: region_kind(&f.expr),
                        variadic: has_true_flag(&f.expr, "variadic"),
                    }
                })
                .collect(),
        )
    } else {
        None
    }
}

fn region_kind(expr: &Expr) -> String {
    let Expr::Struct(s) = expr else {
        return "Blocks".to_string();
    };
    s.fields
        .iter()
        .find(|f| field_name(f) == "kind")
        .and_then(|f| match &f.expr {
            Expr::Path(path) => Some(path.path.require_ident().ok()?.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "Blocks".to_string())
}

fn has_true_flag(expr: &Expr, flag: &str) -> bool {
    let Expr::Struct(s) = expr else {
        return false;
    };
    s.fields.iter().any(|f| {
        field_name(f) == flag
            && matches!(&f.expr, Expr::Lit(lit) if matches!(&lit.lit, syn::Lit::Bool(b) if b.value))
    })
}

#[derive(Clone)]
struct AttrSpec {
    name: String,
    ty: String,
    /// The `Predicate` vocabulary named by `"Predicate in INTEGER"`, if any.
    vocabulary: Option<String>,
}

fn get_attributes(expr: &Expr) -> Option<Vec<AttrSpec>> {
    if let Expr::Struct(s) = expr {
        Some(
            s.fields
                .iter()
                .map(|f| {
                    let name = field_name(f);
                    let spelled = expr_as_string(&f.expr);
                    let (ty, vocabulary) = match spelled.split_once(" in ") {
                        Some((ty, vocabulary)) => {
                            (ty.to_string(), Some(vocabulary.trim().to_string()))
                        }
                        None => (spelled, None),
                    };
                    AttrSpec {
                        name,
                        ty,
                        vocabulary,
                    }
                })
                .collect(),
        )
    } else {
        None
    }
}

fn get_value_names(expr: &Expr) -> Vec<String> {
    let Expr::Struct(s) = expr else {
        panic!("operands and results are listed as `O {{ name, .. }}`");
    };
    s.fields
        .iter()
        .map(|f| {
            let name = field_name(f);
            assert!(
                f.colon_token.is_none(),
                "`{name}` declares a type: an op's types go in its `scheme:`"
            );
            name
        })
        .collect()
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
            self.regions().nth(#index).unwrap()
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
            let region = self.regions().nth(#index).unwrap();
            let block = region.iter(context).next().unwrap();
            block
        }
    }
}

fn make_custom_printer() -> proc_macro2::TokenStream {
    quote! {
        fn print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
            Self::custom_print(self, fmt)
        }
    }
}

fn make_custom_parser() -> proc_macro2::TokenStream {
    quote! {
        fn parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
        -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
            Self::custom_parse(parser, context)
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
        fn print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
            tir::region_format::print_generic(fmt, &self.0, #op_name)
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
    let result_build = if has_results {
        quote! { let result_vec = vec![context.create_value(result_ty, None).id()]; }
    } else {
        quote! { let result_vec: Vec<tir::ValueId> = vec![]; }
    };

    quote! {
        fn parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
        -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
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
            let mut regions = vec![];
            #(#region_fills)*
            #attribute_verifier
            let mut operand_vec: Vec<tir::ValueId> = vec![];
            #segment_sizes_setup
            #(#operand_collect)*
            #result_build
            #segment_sizes_attr
            let instance = tir::NewOp::new::<Self>(
                context.clone(), operand_vec, result_vec, regions, parts,
            );
            Ok(Box::new(Self(context.add_operation(tir::InsertionPoint::Root, instance))))
        }
    }
}
