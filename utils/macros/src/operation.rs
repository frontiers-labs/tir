use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Expr, ExprStruct, Ident, Member, Path, TypePath, parse::Parse, parse_macro_input};

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
        custom_verifier,
        state,
    } = parse_macro_input!(item as Operation);

    let builder_name = format_ident!("{}Builder", struct_name.to_string());
    // `state:` names which single dependency ports memory order threads through
    // the op — absent in un-threaded IR — and only decides which accessors the
    // op gets; every builder takes dependencies the same way.
    let state_accessors = make_state_accessors(&state);
    let same_type = interfaces.iter().any(|path| {
        path.segments
            .last()
            .is_some_and(|segment| segment.ident == "SameOperandAndResultType")
    });
    let has_results = !results.is_empty();
    // A `?`-prefixed result type makes the single result optional: the op may be built
    // with or without it. Used by structured control flow, whose value is absent when
    // the construct is purely side-effecting.
    let result_optional = results.iter().any(|r| r.ty.starts_with('?'));
    // A `*`-prefixed result makes the op n-ary: it produces one value per type given
    // to the builder. Used by structured control flow, which carries n values.
    let result_variadic = results.iter().any(|r| r.variadic);
    assert!(
        !result_variadic || results.len() == 1,
        "a variadic result must be the only declared result"
    );
    let op_fn_name = op_fn_ident(&name);
    let operand_names: Vec<String> = operands.iter().map(|o| o.name.clone()).collect();

    let printer = if custom_format {
        make_custom_printer()
    } else {
        make_generic_printer(&dialect, &name, &operand_names, &regions, has_results)
    };

    let region_accessors = make_region_accessors(&regions);
    let region_pieces = make_region_pieces(&regions);

    let parser = if custom_format {
        make_custom_parser()
    } else {
        make_parser(
            &builder_name,
            &regions,
            &operand_names,
            &attributes,
            has_results,
            result_variadic,
        )
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
    let mut interfaces = interfaces;
    if derive_constant_fold {
        interfaces.push(syn::parse_quote!(tir::ConstantFold));
    }

    let (sem_hooks_impl, semantic_expr_method, as_sem_expr_impl) =
        make_sem_impls(&sem, &struct_name, &operands, has_results);

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

    let result_pieces = make_result_pieces(has_results, result_variadic, result_optional);
    let attr_fn_params: Vec<_> = attributes
        .iter()
        .map(|attr| {
            let name = op_fn_ident(&attr.name);
            quote! { #name: impl Into<tir::attributes::AttributeValue> }
        })
        .collect();

    let attr_fn_builders: Vec<_> = attributes
        .iter()
        .map(|attr| {
            let name_ident = op_fn_ident(&attr.name);
            let name_str = attr.name.clone();
            quote! {
                builder = builder.attr(#name_str, #name_ident.into());
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
        &operands,
        &results,
        &attributes,
        &interfaces,
    );
    let opdef_verifier = emit_opdef_verifier(
        &struct_name,
        &schema_ident,
        &operands,
        &results,
        &regions,
        same_type,
    );

    let predicate_setters: Vec<_> = attributes
        .iter()
        .filter(|attr| attr.ty == "Predicate")
        .map(|attr| {
            let method = op_fn_ident(&attr.name);
            let name = attr.name.clone();
            quote! {
                pub fn #method(self, #method: tir::attributes::Predicate) -> Self {
                    self.attr(#name, tir::attributes::AttributeValue::Predicate(#method))
                }
            }
        })
        .collect();
    let attribute_pieces = AttributePieces {
        verifier: attribute_verifier,
        predicate_setters,
    };
    let builder_code = emit_builder(
        &builder_name,
        &struct_name,
        &region_pieces,
        &operand_pieces,
        &result_pieces,
        &attribute_pieces,
    );
    let result_accessor = &result_pieces.accessor;
    let op_fn_code = emit_op_fn(
        &op_fn_name,
        &builder_name,
        &region_pieces,
        &operand_pieces,
        &result_pieces,
        &attr_fn_params,
        &attr_fn_builders,
    );

    quote! {
        pub struct #struct_name(tir::OpHandle);

        #schema_registration
        #opdef_verifier

        #(#interface_impls)*
        #verifiable_impl
        #sem_hooks_impl
        #as_sem_expr_impl
        #constant_fold_impl

        #builder_code

        impl #struct_name {
            #region_accessors
            #result_accessor
            #state_accessors
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
    operands: &[ValueSpec],
    results: &[ValueSpec],
    attributes: &[AttrSpec],
    interfaces: &[Path],
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
        };
    }
}

fn emit_opdef_verifier(
    struct_name: &Ident,
    schema_ident: &Ident,
    operands: &[ValueSpec],
    results: &[ValueSpec],
    regions: &[Region],
    same_type: bool,
) -> proc_macro2::TokenStream {
    let region_kinds: Vec<_> = regions
        .iter()
        .map(|region| format_ident!("{}", region.kind))
        .collect();
    let operand_constraint_checkers: Vec<_> = operands
        .iter()
        .map(|operand| normalize_constraint_name(&operand.ty))
        .map(|name| parse_constraint_tokens(&name))
        .collect();

    let result_constraint_checkers: Vec<_> = results
        .iter()
        .map(|result| normalize_constraint_name(&result.ty))
        .map(|name| parse_constraint_tokens(&name))
        .collect();

    quote! {
        impl tir::OpDefVerifiable for #struct_name {
            fn verify_operands(&self, context: &tir::Context) -> Result<(), tir::Error> {
                fn __satisfies_constraint<C: tir::TypeConstraint + 'static>(ty: &dyn tir::Type) -> bool {
                    C::satisfies(ty)
                }
                static SPEC: tir::OpDefSpec = tir::OpDefSpec {
                    schema: &#schema_ident,
                    operand_checkers: &[#(__satisfies_constraint::<#operand_constraint_checkers>),*],
                    result_checkers: &[#(__satisfies_constraint::<#result_constraint_checkers>),*],
                    region_kinds: &[#(tir::RegionKind::#region_kinds),*],
                    same_type: #same_type,
                };
                tir::verify_opdef_operands(context, &self.0, <Self as tir::Operation>::name(), &SPEC)
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

/// What an op's attribute declarations contribute to its builder: the
/// required-attribute check `build` runs, and a typed setter per `Predicate`.
struct AttributePieces {
    verifier: proc_macro2::TokenStream,
    predicate_setters: Vec<proc_macro2::TokenStream>,
}

fn emit_builder(
    builder_name: &Ident,
    struct_name: &Ident,
    regions: &RegionPieces,
    operands: &OperandPieces,
    results: &ResultPieces,
    attributes: &AttributePieces,
) -> proc_macro2::TokenStream {
    let (attribute_verifier, predicate_setters) =
        (&attributes.verifier, &attributes.predicate_setters);
    let (region_fields, region_defaults, region_builders, region_fills) = (
        &regions.fields,
        &regions.defaults,
        &regions.builders,
        &regions.fills,
    );
    let (operand_fields, operand_defaults, operand_builders, operand_collect) = (
        &operands.fields,
        &operands.defaults,
        &operands.builders,
        &operands.collect,
    );
    let segment_sizes_setup = &operands.segment_sizes_setup;
    let attributes_binding = &operands.attributes_binding;
    let segment_sizes_attr = &operands.segment_sizes_attr;
    let (result_builder_field, result_builder_default, result_builder_method, result_build) = (
        &results.builder_field,
        &results.builder_default,
        &results.builder_method,
        &results.build,
    );
    quote! {
        pub struct #builder_name {
            context: tir::Context,
            parts: tir::NewOpParts,
            #(#region_fields,)*
            #(#operand_fields,)*
            #result_builder_field
        }

        impl #builder_name {
            pub fn new(context: &tir::Context) -> #builder_name {
                Self {
                    context: context.clone(),
                    parts: tir::NewOpParts::default(),
                    #(#region_defaults,)*
                    #(#operand_defaults,)*
                    #result_builder_default
                }
            }

            #(#region_builders)*
            #(#operand_builders)*
            #(#predicate_setters)*
            #result_builder_method

            /// Observe one more dependency: the chain this op is ordered after.
            pub fn dep_operand(mut self, v: tir::ValueId) -> Self {
                self.parts.dep_operands.push(v);
                self
            }

            /// Produce one more dependency: a chain this op leaves behind.
            pub fn dep_result(mut self) -> Self {
                self.parts.dep_results += 1;
                self
            }

            pub fn attr(mut self, name: &str, value: tir::attributes::AttributeValue) -> Self {
                let attribute = self.context.named_attribute(name, value);
                self.parts.attributes.push(attribute);
                self
            }

            /// [`Self::attr`] for a name already interned in the context.
            pub fn attr_sym(mut self, name: tir::Sym, value: tir::attributes::AttributeValue) -> Self {
                self.parts.attributes.push(tir::attributes::NamedAttribute::new(name, value));
                self
            }

            pub fn build(self) -> #struct_name {
                let mut regions = vec![];

                #(#region_fills)*

                #attribute_verifier

                let mut operand_vec: Vec<tir::ValueId> = vec![];
                #segment_sizes_setup
                #(#operand_collect)*

                #result_build

                #attributes_binding
                #segment_sizes_attr

                let instance = tir::NewOp::new::<#struct_name>(
                    self.context.as_context_ref(),
                    operand_vec,
                    result_vec,
                    regions,
                    tir::NewOpParts { attributes, ..parts },
                );

                let instance = self.context.add_operation(instance);

                #struct_name(instance)
            }
        }
    }
}

fn emit_op_fn(
    op_fn_name: &Ident,
    builder_name: &Ident,
    regions: &RegionPieces,
    operands: &OperandPieces,
    results: &ResultPieces,
    attr_fn_params: &TokenStreams,
    attr_fn_builders: &TokenStreams,
) -> proc_macro2::TokenStream {
    let (region_fn_params, region_fn_builders) = (&regions.fn_params, &regions.fn_builders);
    let (operand_fn_params, operand_fn_builders) = (&operands.fn_params, &operands.fn_builders);
    let (result_fn_param, result_fn_builder) = (&results.fn_param, &results.fn_builder);

    quote! {
        // Generated: one parameter per declared port, which for a machine
        // instruction is however many registers it names.
        #[allow(clippy::too_many_arguments)]
        pub fn #op_fn_name(
            context: &tir::Context,
            #(#operand_fn_params,)*
            #(#attr_fn_params,)*
            #result_fn_param
            #(#region_fn_params,)*
        ) -> #builder_name {
            let mut builder = #builder_name::new(context);
            #(#operand_fn_builders)*
            #(#attr_fn_builders)*
            #result_fn_builder
            #(#region_fn_builders)*
            builder
        }
    }
}

struct RegionPieces {
    fields: TokenStreams,
    defaults: TokenStreams,
    builders: TokenStreams,
    fills: TokenStreams,
    fn_params: TokenStreams,
    fn_builders: TokenStreams,
}

fn make_region_pieces(regions: &[Region]) -> RegionPieces {
    let mut region_fills = vec![];
    let mut region_fields = vec![];
    let mut region_defaults = vec![];
    let mut region_builders = vec![];

    for r in regions {
        let name = format_ident!("{}", r.name);

        let name_str = r.name.clone();

        if r.variadic {
            region_fields.push(quote! {
               #name: Vec<tir::RegionId>
            });
            region_defaults.push(quote! {
               #name: Vec::new()
            });
            region_builders.push(quote! {
               pub fn #name(mut self, ids: Vec<tir::RegionId>) -> Self {
                   self.#name = ids;
                   self
               }
            });
            region_fills.push(quote! {
                regions.extend(self.#name.iter().copied());
            });
            continue;
        }

        region_fields.push(quote! {
           #name: Option<tir::RegionId>
        });

        region_defaults.push(quote! {
           #name: None
        });

        region_builders.push(quote! {
           pub fn #name(mut self, id: tir::RegionId) -> Self {
               self.#name = Some(id);
               self
           }
        });

        if r.blocked() {
            region_fills.push(quote! {
                let region = if self.#name.is_some() {
                    self.#name.unwrap()
                } else {
                    let region = self.context.create_region();
                    let block = self.context.create_block(vec![]);
                    region.add_block(block.id());
                    region.id()
                };
                regions.push(region);
            });
        } else {
            region_fills.push(quote! {
                if self.#name.is_some() {
                    regions.push(self.#name.unwrap());
                } else {
                    panic!("Region '{}' is not set", #name_str);
                }
            });
        }
    }

    let region_fn_params: Vec<_> = regions
        .iter()
        .map(|region| {
            let name = format_ident!("{}", region.name);
            if region.variadic {
                quote! { #name: Vec<tir::RegionId> }
            } else {
                quote! { #name: Option<tir::RegionId> }
            }
        })
        .collect();

    let region_fn_builders: Vec<_> = regions
        .iter()
        .map(|region| {
            let name = format_ident!("{}", region.name);
            if region.variadic {
                quote! { builder = builder.#name(#name); }
            } else {
                quote! {
                    if let Some(region) = #name {
                        builder = builder.#name(region);
                    }
                }
            }
        })
        .collect();

    RegionPieces {
        fields: region_fields,
        defaults: region_defaults,
        builders: region_builders,
        fills: region_fills,
        fn_params: region_fn_params,
        fn_builders: region_fn_builders,
    }
}

struct OperandPieces {
    fields: TokenStreams,
    defaults: TokenStreams,
    builders: TokenStreams,
    fn_params: TokenStreams,
    fn_builders: TokenStreams,
    collect: TokenStreams,
    segment_sizes_setup: proc_macro2::TokenStream,
    attributes_binding: proc_macro2::TokenStream,
    segment_sizes_attr: proc_macro2::TokenStream,
}

fn make_operand_pieces(operands: &[ValueSpec]) -> OperandPieces {
    let mut operand_fields = vec![];
    let mut operand_defaults = vec![];
    let mut operand_builders = vec![];
    let mut operand_fn_params = vec![];
    let mut operand_fn_builders = vec![];

    for operand in operands {
        let field = format_ident!("{}", operand.name);
        if operand.variadic {
            operand_fields.push(quote! {
                #field: Vec<tir::ValueId>
            });
            operand_defaults.push(quote! {
                #field: Vec::new()
            });
            operand_builders.push(quote! {
                pub fn #field(mut self, v: Vec<tir::ValueId>) -> Self {
                    self.#field = v;
                    self
                }
            });
            operand_fn_params.push(quote! {
                #field: Vec<tir::ValueId>
            });
            operand_fn_builders.push(quote! {
                builder = builder.#field(#field);
            });
        } else {
            operand_fields.push(quote! {
                #field: Option<tir::ValueId>
            });
            operand_defaults.push(quote! {
                #field: None
            });
            operand_builders.push(quote! {
                pub fn #field(mut self, v: tir::ValueId) -> Self {
                    self.#field = Some(v);
                    self
                }
            });
            operand_fn_params.push(quote! {
                #field: impl Into<tir::Operand>
            });
            operand_fn_builders.push(quote! {
                let #field = #field.into();
                if let Some(value) = #field.into_option() {
                    builder = builder.#field(value);
                }
            });
        }
    }

    let has_variadic = operands.iter().any(|o| o.variadic);
    // Collect operands in declaration order. Variadic ops additionally record each
    // declared operand's segment size in the `operand_segment_sizes` attribute so
    // groups can be recovered; fixed-arity ops keep the original simple collection.
    let operand_collect: Vec<_> = operands
        .iter()
        .map(|operand| {
            let field = format_ident!("{}", operand.name);
            if !has_variadic {
                quote! {
                    if let Some(v) = self.#field {
                        operand_vec.push(v);
                    }
                }
            } else if operand.variadic {
                quote! {
                    operand_segment_sizes.push(self.#field.len() as u64);
                    operand_vec.extend(self.#field.iter().copied());
                }
            } else {
                quote! {
                    if let Some(v) = self.#field {
                        operand_segment_sizes.push(1);
                        operand_vec.push(v);
                    } else {
                        operand_segment_sizes.push(0);
                    }
                }
            }
        })
        .collect();

    let segment_sizes_setup = if has_variadic {
        quote! { let mut operand_segment_sizes: Vec<u64> = vec![]; }
    } else {
        quote! {}
    };

    // `attributes` only needs to be mutable when a variadic op appends its segment
    // sizes, so bind it accordingly to avoid an `unused_mut` warning otherwise.
    let attributes_binding = if has_variadic {
        quote! { let parts = self.parts; let mut attributes = parts.attributes; }
    } else {
        quote! { let parts = self.parts; let attributes = parts.attributes; }
    };

    let segment_sizes_attr = if has_variadic {
        quote! {
            attributes.push(tir::attributes::NamedAttribute::new(
                self.context.intern("operand_segment_sizes"),
                tir::attributes::AttributeValue::Array(
                    operand_segment_sizes
                        .iter()
                        .map(|n| tir::attributes::AttributeValue::UInt(*n))
                        .collect::<Vec<_>>()
                .into()),
            ));
        }
    } else {
        quote! {}
    };

    OperandPieces {
        fields: operand_fields,
        defaults: operand_defaults,
        builders: operand_builders,
        fn_params: operand_fn_params,
        fn_builders: operand_fn_builders,
        collect: operand_collect,
        segment_sizes_setup,
        attributes_binding,
        segment_sizes_attr,
    }
}

/// The `sem = "..."` declaration is lowered at run time by tir-symbolic's graph
/// builder. The op provides the operand-symbol map plus the hooks the builder
/// needs: `$splice` atoms call op methods, and width-changing ops read the op's
/// result width.
fn make_sem_impls(
    sem: &Option<Sem>,
    struct_name: &Ident,
    operands: &[ValueSpec],
    has_results: bool,
) -> (
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
) {
    if let Some(sem) = sem {
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
        let splice_arms: Vec<proc_macro2::TokenStream> = sem
            .splices
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
                let __ctx = self.0.context.upgrade();
                let __ty = __ctx.get_value(self.0.results()[0]).ty();
                Some(
                    (__ctx.get_type_data(__ty).as_ref() as &dyn std::any::Any)
                        .downcast_ref::<tir::builtin::IntegerType>()
                        .map(|t| t.width() as u64)
                        .unwrap_or(0),
                )
            }
        } else {
            quote! { None }
        };
        let actual_type_setter = if has_results {
            quote! {
                let __tir_ctx = self.0.context.upgrade();
                let __tir_ty = __tir_ctx.get_value(self.result()).ty();
                g.set_actual_type(__tir_sem_root, __tir_ty);
            }
        } else {
            quote! {}
        };

        let hooks = quote! {
            impl<__G> tir_symbolic::lang::SemBuilderHooks<__G> for #struct_name
            where
                __G: tir::graph::MutDag<
                    Node = tir_symbolic::lang::SymKind,
                    Leaf = tir_symbolic::lang::SymPayload<tir::ValueId>,
                >,
            {
                fn splice(&self, __name: &str, g: &mut __G) -> Option<tir::graph::NodeId> {
                    match __name {
                        #(#splice_arms)*
                        _ => None,
                    }
                }
                fn result_width(&self) -> Option<u64> {
                    #result_width_body
                }
            }
        };
        let sem_method = quote! {
            fn semantic_expr(&self, g: &mut tir::sem::SemGraph) -> Option<tir::graph::NodeId> {
                use tir::graph::MetaMutDag;
                let __tir_sem_root = tir_symbolic::lang::build(g, #src, &[#(#sym_pairs),*], self).ok()?;
                g.set_original_op(__tir_sem_root, <Self as tir::Operation>::id(self));
                #actual_type_setter
                Some(__tir_sem_root)
            }
        };
        let as_impl = quote! {
            impl tir::sem::AsSemExpr for #struct_name {
                fn convert(
                    &self,
                    g: &mut impl tir::graph::MutDag<
                        Node = tir_symbolic::lang::SymKind,
                        Leaf = tir_symbolic::lang::SymPayload<tir::ValueId>,
                        Annotation = tir::graph::NodeMeta,
                    >,
                ) -> tir::graph::NodeId {
                    use tir::graph::MetaMutDag;
                    let __tir_sem_root = tir_symbolic::lang::build(g, #src, &[#(#sym_pairs),*], self)
                        .expect("semantic expression should build");
                    g.set_original_op(__tir_sem_root, <Self as tir::Operation>::id(self));
                    #actual_type_setter
                    __tir_sem_root
                }
            }
        };
        (hooks, sem_method, as_impl)
    } else {
        (quote! {}, quote! {}, quote! {})
    }
}

struct ResultPieces {
    accessor: proc_macro2::TokenStream,
    builder_field: proc_macro2::TokenStream,
    builder_default: proc_macro2::TokenStream,
    builder_method: proc_macro2::TokenStream,
    fn_param: proc_macro2::TokenStream,
    fn_builder: proc_macro2::TokenStream,
    build: proc_macro2::TokenStream,
}

fn make_result_pieces(
    has_results: bool,
    result_variadic: bool,
    result_optional: bool,
) -> ResultPieces {
    let result_accessor = if has_results {
        quote! {
            pub fn result(&self) -> tir::ValueId {
                self.0.results()[0]
            }
        }
    } else {
        quote! {}
    };

    let result_builder_field = if !has_results {
        quote! {}
    } else if result_variadic {
        quote! { result_types: Vec<tir::TypeId>, result_values: Vec<tir::ValueId>, }
    } else {
        quote! { result_type: Option<tir::TypeId>, }
    };

    let result_builder_default = if !has_results {
        quote! {}
    } else if result_variadic {
        quote! { result_types: Vec::new(), result_values: Vec::new(), }
    } else {
        quote! { result_type: None, }
    };

    let result_builder_method = if !has_results {
        quote! {}
    } else if result_variadic {
        quote! {
            pub fn result_types(mut self, types: Vec<tir::TypeId>) -> Self {
                self.result_types = types;
                self
            }

            /// Adopt values that already exist as this op's results, instead of
            /// minting fresh ones from [`Self::result_types`]. A rewrite that
            /// moves a definition from one op to another (a two-address tie
            /// lowered to a copy) keeps the value it was reaching.
            pub fn result_values(mut self, values: Vec<tir::ValueId>) -> Self {
                self.result_values = values;
                self
            }
        }
    } else {
        quote! {
            pub fn result_type(mut self, ty: tir::TypeId) -> Self {
                self.result_type = Some(ty);
                self
            }
        }
    };

    let result_fn_param = if !has_results {
        quote! {}
    } else if result_variadic {
        quote! { result_types: Vec<tir::TypeId>, }
    } else if result_optional {
        quote! { result_type: Option<tir::TypeId>, }
    } else {
        quote! { result_type: tir::TypeId, }
    };

    let result_fn_builder = if !has_results {
        quote! {}
    } else if result_variadic {
        quote! { builder = builder.result_types(result_types); }
    } else if result_optional {
        quote! {
            if let Some(result_type) = result_type {
                builder = builder.result_type(result_type);
            }
        }
    } else {
        quote! { builder = builder.result_type(result_type); }
    };

    let result_build = if !has_results {
        quote! {
            let result_vec: Vec<tir::ValueId> = vec![];
        }
    } else if result_variadic {
        quote! {
            let result_vec: Vec<tir::ValueId> = if self.result_values.is_empty() {
                self.result_types
                    .iter()
                    .map(|ty| self.context.create_value(*ty, None).id())
                    .collect()
            } else {
                self.result_values
            };
        }
    } else if result_optional {
        quote! {
            let result_vec = match self.result_type {
                Some(ty) => vec![self.context.create_value(ty, None).id()],
                None => vec![],
            };
        }
    } else {
        quote! {
            let result_vec = {
                let ty = self.result_type.expect("result_type must be set for ops with results");
                let val = self.context.create_value(ty, None);
                vec![val.id()]
            };
        }
    };

    ResultPieces {
        accessor: result_accessor,
        builder_field: result_builder_field,
        builder_default: result_builder_default,
        builder_method: result_builder_method,
        fn_param: result_fn_param,
        fn_builder: result_fn_builder,
        build: result_build,
    }
}

/// The memory-order ports an op declares with `state: "in" | "out" | "in_out"`:
/// which of the single dependency operand and result accessors it carries.
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

fn make_state_accessors(state: &StatePorts) -> proc_macro2::TokenStream {
    let operand = if state.input {
        quote! {
            /// The memory state this op observes, once a threading pass has set it.
            pub fn state_operand(&self) -> Option<tir::ValueId> {
                self.0.dep_operands().first().copied()
            }
        }
    } else {
        quote! {}
    };
    let result = if state.output {
        quote! {
            /// The memory state this op leaves behind, once a threading pass has set it.
            pub fn state_result(&self) -> Option<tir::ValueId> {
                self.0.dep_results().first().copied()
            }
        }
    } else {
        quote! {}
    };
    quote! { #operand #result }
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
    custom_verifier: bool,
    state: StatePorts,
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
    /// Whether the region's body is a block list, so the op's accessor hands
    /// back the entry block rather than the region.
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

impl Parse for Operation {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let struct_: ExprStruct = input.parse()?;

        let struct_name = struct_.path.require_ident()?.clone();

        let name = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "name" {
                        Some(expr_as_string(&f.expr))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap();

        let dialect = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "dialect" {
                        Some(expr_as_string(&f.expr))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap();

        let regions = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "regions" {
                        get_regions(&f.expr)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        let attributes = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "attributes" {
                        get_attributes(&f.expr)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        let operands = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "operands" {
                        get_value_specs(&f.expr)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        let results = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "results" {
                        get_value_specs(&f.expr)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        let interfaces = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "interfaces" {
                        Some(expr_as_path_vec(&f.expr))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or_default();

        let custom_format = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "format" {
                        Some(expr_as_string(&f.expr) == "custom")
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or(false);

        let custom_verifier = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "verifier" {
                        Some(expr_as_string(&f.expr) == "true")
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or(false);

        let state = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) if ident == "state" => {
                    Some(StatePorts::parse(&expr_as_string(&f.expr)))
                }
                _ => None,
            })
            .unwrap_or_default();

        let sem = struct_.fields.iter().find_map(|f| match &f.member {
            Member::Named(ident) if ident == "sem" => parse_sem(&f.expr),
            _ => None,
        });

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
            custom_verifier,
            state,
        })
    }
}

/// Parse a `sem = "..."` field into its raw source and the `$splice` names it
/// references. Validation (and splice discovery) reuses tir-symbolic's parser so
/// the macro and the runtime builder agree on the grammar.
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

/// The `kind:` a `Region { .. }` declares, defaulting to `Blocks`.
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

/// Whether a `Region { .. }` declaration sets `flag: true`.
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

fn get_value_specs(expr: &Expr) -> Option<Vec<ValueSpec>> {
    match expr {
        Expr::Struct(s) => Some(
            s.fields
                .iter()
                .map(|f| {
                    let ty = expr_as_string(&f.expr);
                    ValueSpec {
                        name: field_name(f),
                        variadic: ty.starts_with('*'),
                        ty,
                    }
                })
                .collect(),
        ),
        // Backward-compatible form: operands/results: [lhs, rhs]
        Expr::Array(arr) => Some(
            arr.elems
                .iter()
                .map(|e| {
                    let Expr::Path(p) = e else {
                        unreachable!();
                    };
                    ValueSpec {
                        name: p.path.get_ident().unwrap().to_string(),
                        ty: "Any".to_string(),
                        variadic: false,
                    }
                })
                .collect(),
        ),
        _ => None,
    }
}

fn normalize_constraint_name(spec: &str) -> String {
    spec.strip_prefix('?')
        .or_else(|| spec.strip_prefix('*'))
        .unwrap_or(spec)
        .to_string()
}

fn parse_constraint_tokens(spec: &str) -> proc_macro2::TokenStream {
    let path: TypePath =
        syn::parse_str(spec).unwrap_or_else(|_| panic!("Invalid type constraint '{}'", spec));
    let path = path.path;
    quote! { #path }
}

fn make_attribute_verifier(specs: &[AttrSpec]) -> proc_macro2::TokenStream {
    if specs.is_empty() {
        return quote! {};
    }
    let checks = specs.iter().map(|s| {
        let n = s.name.clone();
        quote! {
            if !self.parts.attributes.iter().any(|a| Some(a.name) == self.context.sym(#n)) {
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

/// A `variadic` region group holds every region from its declaration position on, so it
/// must be declared last.
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
            let context = self.0.context.upgrade();
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

fn make_generic_printer(
    dialect: &str,
    name: &str,
    operands: &[String],
    regions: &[Region],
    has_results: bool,
) -> proc_macro2::TokenStream {
    let op_name = if dialect == "builtin" {
        name.to_string()
    } else {
        format!("{}.{}", dialect, name)
    };

    let operand_printer = if !operands.is_empty() {
        quote! {
            let printed_operands = self.0.value_operands();
            if !printed_operands.is_empty() {
                fmt.write(" ")?;
                tir::dependency::print_value_list(fmt, &printed_operands)?;
            }
        }
    } else {
        quote! {}
    };

    let result_suffix = if has_results {
        quote! {
            if let Some(result) = self.0.value_results().first() {
                let context = self.0.context.upgrade();
                let result_val = context.get_value(*result);
                fmt.write(" : ")?;
                context.print_type(result_val.ty(), fmt)?;
            }
        }
    } else {
        quote! {}
    };

    let regions = if regions.len() == 1 && !regions[0].variadic {
        make_region_printer(&regions[0], 0)
    } else {
        quote! {}
    };

    quote! {
        fn print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
            tir::dependency::print_result_prefix(fmt, &self.0)?;
            fmt.write(#op_name)?;
            #operand_printer
            tir::dependency::print_dep_operands(fmt, &self.0)?;
            // Print generic attribute dict if any
            if !self.attributes().is_empty() {
                fmt.write(" ")?;
                fmt.write("{")?;
                let mut first = true;
                for attr in self.attributes() {
                    if !first { fmt.write(", ")?; }
                    first = false;
                    let context = self.0.context.upgrade();
                    fmt.write(context.resolve(attr.name))?;
                    fmt.write(" = ")?;
                    attr.value.print(fmt, &context)?;
                }
                fmt.write("}")?;
            }

            #result_suffix

            if self.regions().len() == 0 {
                fmt.write("\n")?;
            }

            #regions

            Ok(())
        }
    }
}

fn make_region_printer(region: &Region, index: usize) -> proc_macro2::TokenStream {
    let _ = region;
    quote! {
        {
            let context = self.0.context.upgrade();
            tir::region_format::print_op_region(fmt, &context, self, #index)?;
        }
    }
}

fn make_parser(
    builder_name: &Ident,
    regions: &[Region],
    operands: &[String],
    attributes: &[AttrSpec],
    has_results: bool,
    result_variadic: bool,
) -> proc_macro2::TokenStream {
    let attr_spec_literals: Vec<_> = attributes
        .iter()
        .map(|attr| {
            let name = proc_macro2::Literal::string(&attr.name);
            let ty = proc_macro2::Literal::string(&attr.ty);
            quote! { (#name, #ty) }
        })
        .collect();
    let (region_parsers, region_builders) = if regions.len() == 1 && !regions[0].variadic {
        let region_name = format_ident!("{}", regions[0].name);
        (
            quote! {
               let #region_name = parser.parse_region(context)?.id();
            },
            quote! {
                .#region_name(#region_name)
            },
        )
    } else {
        (quote! {}, quote! {})
    };

    let operand_parsers: Vec<_> = operands
        .iter()
        .enumerate()
        .map(|(i, op_name)| {
            let field = format_ident!("{}", op_name);
            let comma = if i > 0 {
                quote! { parser.parse_token(","); }
            } else {
                quote! {}
            };
            quote! {
                #comma
                if let Some(ref_name) = parser.parse_value_ref() {
                    builder = builder.#field(parser.resolve_value(context, ref_name));
                }
            }
        })
        .collect();

    let result_parser = if !has_results {
        quote! {}
    } else {
        // A variadic result group is written as the one result the generic form
        // prints; an op with several is spelled by its own format.
        let bind = if result_variadic {
            quote! { builder = builder.result_types(vec![result_ty]); }
        } else {
            quote! { builder = builder.result_type(result_ty); }
        };
        quote! {
            if !parser.parse_token(":") {
                return Err((parser.span(), tir::Error::ExpectedToken(":")));
            }
            let result_ty = parser.parse_type(context)
                ?
                .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?;
            #bind
        }
    };

    quote! {
        fn parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
        -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
           use tir::parse::common::Cursor;

           let mut parsed_attrs: Vec<tir::attributes::NamedAttribute> = vec![];
           let attr_specs: &[(&str, &str)] = &[#(#attr_spec_literals),*];

           let mut builder = #builder_name::new(context);

           #(#operand_parsers)*
           builder.parts.dep_operands = tir::dependency::parse_dep_operands(parser, context)?;

           // Parse optional generic attribute dict: { key = value, ... }
           let mark = parser.pos();
           if parser.parse_token("{") {
               let mut ok = true;
               if !parser.parse_token("}") {
                   loop {
                       if let Some(name) = parser.parse_ident() {
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

            for a in parsed_attrs { builder = builder.attr_sym(a.name, a.value); }

            let op = builder
                #region_builders
                .build();
            Ok(Box::new(op))
        }
    }
}
