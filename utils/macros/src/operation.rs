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
        roles,
        operands,
        results,
        interfaces,
        custom_format,
        as_sem_expr_body,
        custom_verifier,
    } = parse_macro_input!(item as Operation);

    let builder_name = format_ident!("{}Builder", struct_name.to_string());
    let has_results = !results.is_empty();
    let op_fn_name = op_fn_ident(&name);
    let operand_names: Vec<String> = operands.iter().map(|o| o.name.clone()).collect();

    let printer = if custom_format {
        make_custom_printer()
    } else {
        make_generic_printer(&dialect, &name, &operand_names, &regions, has_results)
    };

    let mut region_fills = vec![];
    let mut region_fields = vec![];
    let mut region_defaults = vec![];
    let mut region_builders = vec![];

    let region_accessors = make_region_accessors(&regions);

    for r in &regions {
        let name = format_ident!("{}", r.name);

        let name_str = r.name.clone();

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

        if r.single_block {
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

    let parser = if custom_format {
        make_custom_parser()
    } else {
        make_parser(&builder_name, &regions, &operand_names, has_results)
    };

    let attribute_verifier = make_attribute_verifier(&attributes);
    let roles_table = make_roles_table(&struct_name, &roles);

    // Operand support in builder
    let mut operand_fields = vec![];
    let mut operand_defaults = vec![];
    let mut operand_builders = vec![];
    let mut operand_fn_params = vec![];
    let mut operand_fn_builders = vec![];

    let has_variadic = operands.iter().any(|o| o.variadic);

    for operand in &operands {
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
        quote! { let mut attributes = self.attributes; }
    } else {
        quote! { let attributes = self.attributes; }
    };

    let segment_sizes_attr = if has_variadic {
        quote! {
            attributes.push(tir::attributes::NamedAttribute::new(
                "operand_segment_sizes",
                tir::attributes::AttributeValue::Array(
                    operand_segment_sizes
                        .iter()
                        .map(|n| tir::attributes::AttributeValue::UInt(*n))
                        .collect(),
                ),
            ));
        }
    } else {
        quote! {}
    };

    // Result support
    let result_accessor = if has_results {
        quote! {
            pub fn result(&self) -> tir::ValueId {
                self.0.results[0]
            }
        }
    } else {
        quote! {}
    };

    let operand_name_literals: Vec<_> = operands
        .iter()
        .map(|operand| {
            let lit = proc_macro2::Literal::string(&operand.name);
            quote! { #lit }
        })
        .collect();

    let operand_spec_literals: Vec<_> = operands
        .iter()
        .map(|operand| {
            let name = proc_macro2::Literal::string(&operand.name);
            let ty = proc_macro2::Literal::string(&operand.ty);
            quote! { (#name, #ty) }
        })
        .collect();

    let result_spec_literals: Vec<_> = results
        .iter()
        .map(|result| {
            let name = proc_macro2::Literal::string(&result.name);
            let ty = proc_macro2::Literal::string(&result.ty);
            quote! { (#name, #ty) }
        })
        .collect();

    let operand_constraint_name_literals: Vec<_> = operands
        .iter()
        .map(|operand| normalize_constraint_name(&operand.ty))
        .map(|name| proc_macro2::Literal::string(&name))
        .collect();

    let operand_constraint_checkers: Vec<_> = operands
        .iter()
        .map(|operand| normalize_constraint_name(&operand.ty))
        .map(|name| parse_constraint_tokens(&name))
        .collect();

    let result_constraint_name_literals: Vec<_> = results
        .iter()
        .map(|result| normalize_constraint_name(&result.ty))
        .map(|name| proc_macro2::Literal::string(&name))
        .collect();

    let result_constraint_checkers: Vec<_> = results
        .iter()
        .map(|result| normalize_constraint_name(&result.ty))
        .map(|name| parse_constraint_tokens(&name))
        .collect();

    let attr_spec_literals: Vec<_> = attributes
        .iter()
        .map(|attr| {
            let name = proc_macro2::Literal::string(&attr.name);
            let ty = proc_macro2::Literal::string(&attr.ty);
            quote! { (#name, #ty) }
        })
        .collect();

    let semantic_expr_method = if let Some(body) = &as_sem_expr_body {
        let actual_type_setter = if has_results {
            quote! {
                let __tir_sem_expr_context = self.0.context.upgrade();
                let __tir_sem_expr_actual_type = __tir_sem_expr_context.get_value(self.result()).ty();
                g.set_actual_type(__tir_sem_expr_root, __tir_sem_expr_actual_type);
            }
        } else {
            quote! {}
        };
        quote! {
            fn semantic_expr(&self, g: &mut tir::sem_expr::ExprPostGraph) -> Option<tir::graph::NodeId> {
                use tir::graph::MutDag;
                let __tir_sem_expr_root = { #body };
                g.set_original_op(__tir_sem_expr_root, <Self as tir::Operation>::id(self));
                #actual_type_setter
                Some(__tir_sem_expr_root)
            }
        }
    } else {
        quote! {}
    };

    let as_sem_expr_impl = if let Some(body) = as_sem_expr_body {
        let actual_type_setter = if has_results {
            quote! {
                let __tir_sem_expr_context = self.0.context.upgrade();
                let __tir_sem_expr_actual_type = __tir_sem_expr_context.get_value(self.result()).ty();
                g.set_actual_type(__tir_sem_expr_root, __tir_sem_expr_actual_type);
            }
        } else {
            quote! {}
        };
        quote! {
            impl tir::sem_expr::AsSemExpr for #struct_name {
                fn convert(&self, g: &mut impl tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>) -> tir::graph::NodeId {
                    let __tir_sem_expr_root = { #body };
                    g.set_original_op(__tir_sem_expr_root, <Self as tir::Operation>::id(self));
                    #actual_type_setter
                    __tir_sem_expr_root
                }
            }
        }
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

    let interface_verifiers: Vec<_> = interfaces
        .iter()
        .map(|interface| {
            quote! {
                {
                    let iface: &dyn #interface = self;
                    iface.verify_interface(self, context)?;
                }
            }
        })
        .collect();

    let result_builder_field = if has_results {
        quote! { result_type: Option<tir::TypeId>, }
    } else {
        quote! {}
    };

    let result_builder_default = if has_results {
        quote! { result_type: None, }
    } else {
        quote! {}
    };

    let result_builder_method = if has_results {
        quote! {
            pub fn result_type(mut self, ty: tir::TypeId) -> Self {
                self.result_type = Some(ty);
                self
            }
        }
    } else {
        quote! {}
    };

    let result_fn_param = if has_results {
        quote! { result_type: tir::TypeId, }
    } else {
        quote! {}
    };

    let result_fn_builder = if has_results {
        quote! { builder = builder.result_type(result_type); }
    } else {
        quote! {}
    };

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

    let region_fn_params: Vec<_> = regions
        .iter()
        .map(|region| {
            let name = format_ident!("{}", region.name);
            quote! { #name: Option<tir::RegionId> }
        })
        .collect();

    let region_fn_builders: Vec<_> = regions
        .iter()
        .map(|region| {
            let name = format_ident!("{}", region.name);
            quote! {
                if let Some(region) = #name {
                    builder = builder.#name(region);
                }
            }
        })
        .collect();

    let result_build = if has_results {
        quote! {
            let result_vec = {
                let ty = self.result_type.expect("result_type must be set for ops with results");
                let val = self.context.create_value(ty, None);
                vec![val.id()]
            };
        }
    } else {
        quote! {
            let result_vec: Vec<tir::ValueId> = vec![];
        }
    };

    let verifiable_impl = if custom_verifier {
        quote! {}
    } else {
        quote! { impl tir::Verifiable for #struct_name {} }
    };

    // Per-operand value checks (existence, defining-op/block-arg, type constraint),
    // shared by the fixed-arity and variadic validation loops. Expects `value_id`,
    // `operand_name`, and `idx` in scope.
    let operand_value_checks = quote! {
        if !context.has_value(value_id) {
            return Err(tir::Error::VerificationError(format!(
                "{} operand '{}' references unknown value %{id}",
                <Self as tir::Operation>::name(),
                operand_name,
                id = value_id.number()
            )));
        }

        let value = context.get_value(value_id);
        match value.defining_op() {
            Some(def_op) => {
                if !context.has_operation(def_op) {
                    return Err(tir::Error::VerificationError(format!(
                        "{} operand '{}' value %{id} references missing defining op",
                        <Self as tir::Operation>::name(),
                        operand_name,
                        id = value_id.number()
                    )));
                }
            }
            None => {
                if !context.is_block_argument(value_id) {
                    return Err(tir::Error::VerificationError(format!(
                        "{} operand '{}' value %{id} has no defining op and is not a block argument",
                        <Self as tir::Operation>::name(),
                        operand_name,
                        id = value_id.number()
                    )));
                }
            }
        }

        let actual_ty = value.ty();
        let actual_ty_data = context.get_type_data(actual_ty);
        if !operand_constraint_checkers[idx](actual_ty_data.as_ref()) {
            return Err(tir::Error::VerificationError(format!(
                "{} operand '{}' expected constraint {}, got {}",
                <Self as tir::Operation>::name(),
                operand_name,
                operand_constraint_names[idx],
                context.type_to_string(actual_ty)
            )));
        }
    };

    let operand_validation = if has_variadic {
        quote! {
            // Variadic ops recover their operand grouping from the segment sizes
            // recorded at build time, then validate each declared operand's segment.
            let segment_sizes: Vec<usize> = match <Self as tir::Operation>::attributes(self)
                .iter()
                .find(|a| a.name == "operand_segment_sizes")
                .map(|a| &a.value)
            {
                Some(tir::attributes::AttributeValue::Array(items)) => items
                    .iter()
                    .map(|v| match v {
                        tir::attributes::AttributeValue::UInt(n) => *n as usize,
                        _ => 0usize,
                    })
                    .collect(),
                _ => {
                    return Err(tir::Error::VerificationError(format!(
                        "{} missing operand_segment_sizes attribute",
                        <Self as tir::Operation>::name()
                    )));
                }
            };

            if segment_sizes.len() != operand_specs.len() {
                return Err(tir::Error::VerificationError(format!(
                    "{} expects {} operand segments, got {}",
                    <Self as tir::Operation>::name(),
                    operand_specs.len(),
                    segment_sizes.len()
                )));
            }

            let total: usize = segment_sizes.iter().sum();
            if total != operands.len() {
                return Err(tir::Error::VerificationError(format!(
                    "{} operand segment sizes sum to {}, but it has {} operands",
                    <Self as tir::Operation>::name(),
                    total,
                    operands.len()
                )));
            }

            let mut __cursor = 0usize;
            for (idx, (operand_name, _type_spec)) in operand_specs.iter().enumerate() {
                let __count = segment_sizes[idx];
                for __k in 0..__count {
                    let value_id = operands[__cursor + __k];
                    #operand_value_checks
                }
                __cursor += __count;
            }
        }
    } else {
        quote! {
            if operands.len() > operand_specs.len() {
                return Err(tir::Error::VerificationError(format!(
                    "{} expects at most {} operands, got {}",
                    <Self as tir::Operation>::name(),
                    operand_specs.len(),
                    operands.len()
                )));
            }

            for (idx, (operand_name, type_spec)) in operand_specs.iter().enumerate() {
                let is_optional = type_spec.starts_with('?');

                let Some(value_id) = operands.get(idx).copied() else {
                    if is_optional {
                        continue;
                    }
                    return Err(tir::Error::VerificationError(format!(
                        "{} missing required operand '{}'",
                        <Self as tir::Operation>::name(),
                        operand_name
                    )));
                };

                #operand_value_checks
            }
        }
    };

    quote! {
        pub struct #struct_name(std::sync::Arc<tir::OpInstance>);

        #(#interface_impls)*
        #verifiable_impl
        #as_sem_expr_impl

        impl tir::OpDefVerifiable for #struct_name {
            fn verify_operands(&self, context: &tir::Context) -> Result<(), tir::Error> {
                let operand_specs: &[(&str, &str)] = &[#(#operand_spec_literals),*];
                let result_specs: &[(&str, &str)] = &[#(#result_spec_literals),*];
                let operand_constraint_names: &[&str] = &[#(#operand_constraint_name_literals),*];
                let result_constraint_names: &[&str] = &[#(#result_constraint_name_literals),*];
                fn __satisfies_constraint<C: tir::TypeConstraint + 'static>(ty: &dyn tir::Type) -> bool {
                    C::satisfies(ty)
                }
                let operand_constraint_checkers: &[fn(&dyn tir::Type) -> bool] = &[
                    #(__satisfies_constraint::<#operand_constraint_checkers>),*
                ];
                let result_constraint_checkers: &[fn(&dyn tir::Type) -> bool] = &[
                    #(__satisfies_constraint::<#result_constraint_checkers>),*
                ];
                let operands = <Self as tir::Operation>::operands(self);

                #operand_validation

                if self.0.results.len() != result_specs.len() {
                    return Err(tir::Error::VerificationError(format!(
                        "{} expects {} results, got {}",
                        <Self as tir::Operation>::name(),
                        result_specs.len(),
                        self.0.results.len()
                    )));
                }

                for (idx, (result_name, _type_spec)) in result_specs.iter().enumerate() {
                    let value_id = self.0.results[idx];
                    if !context.has_value(value_id) {
                        return Err(tir::Error::VerificationError(format!(
                            "{} result '{}' references unknown value %{id}",
                            <Self as tir::Operation>::name(),
                            result_name,
                            id = value_id.number()
                        )));
                    }

                    let value = context.get_value(value_id);
                    match value.defining_op() {
                        Some(def_op) => {
                            if !context.has_operation(def_op) {
                                return Err(tir::Error::VerificationError(format!(
                                    "{} result '{}' value %{id} references missing defining op",
                                    <Self as tir::Operation>::name(),
                                    result_name,
                                    id = value_id.number()
                                )));
                            }
                        }
                        None => {
                            return Err(tir::Error::VerificationError(format!(
                                "{} result '{}' value %{id} has no defining op",
                                <Self as tir::Operation>::name(),
                                result_name,
                                id = value_id.number()
                            )));
                        }
                    }

                    let actual_ty = value.ty();
                    let actual_ty_data = context.get_type_data(actual_ty);
                    if !result_constraint_checkers[idx](actual_ty_data.as_ref()) {
                        return Err(tir::Error::VerificationError(format!(
                            "{} result '{}' expected constraint {}, got {}",
                            <Self as tir::Operation>::name(),
                            result_name,
                            result_constraint_names[idx],
                            context.type_to_string(actual_ty)
                        )));
                    }
                }

                Ok(())
            }
            fn verify_attributes(&self, context: &tir::Context) -> Result<(), tir::Error> {
                let attr_specs: &[(&str, &str)] = &[#(#attr_spec_literals),*];

                for (attr_name, attr_type) in attr_specs {
                    let Some(attr) = <Self as tir::Operation>::attributes(self).iter().find(|a| a.name == *attr_name) else {
                        return Err(tir::Error::VerificationError(format!(
                            "{} missing required attribute '{}'",
                            <Self as tir::Operation>::name(),
                            attr_name
                        )));
                    };

                    let matches = match *attr_type {
                        "any" => true,
                        "Str" => matches!(attr.value, tir::attributes::AttributeValue::Str(_)),
                        "Int" => matches!(attr.value, tir::attributes::AttributeValue::Int(_)),
                        "UInt" => matches!(attr.value, tir::attributes::AttributeValue::UInt(_)),
                        "F32" => matches!(attr.value, tir::attributes::AttributeValue::F32(_)),
                        "F64" => matches!(attr.value, tir::attributes::AttributeValue::F64(_)),
                        "Bool" => matches!(attr.value, tir::attributes::AttributeValue::Bool(_)),
                        "Array" => matches!(attr.value, tir::attributes::AttributeValue::Array(_)),
                        "Dict" => matches!(attr.value, tir::attributes::AttributeValue::Dict(_)),
                        "Register" => matches!(attr.value, tir::attributes::AttributeValue::Register(_)),
                        "Type" => matches!(attr.value, tir::attributes::AttributeValue::Type(_)),
                        "Block" => matches!(attr.value, tir::attributes::AttributeValue::Block(_)),
                        _ => false,
                    };

                    if !matches {
                        return Err(tir::Error::VerificationError(format!(
                            "{} attribute '{}' expected type '{}'",
                            <Self as tir::Operation>::name(),
                            attr_name,
                            attr_type
                        )));
                    }
                }

                Ok(())
            }

            fn verify_interfaces(&self, context: &tir::Context) -> Result<(), tir::Error> {
                #(#interface_verifiers)*
                Ok(())
            }
        }

        pub struct #builder_name {
            context: tir::Context,
            attributes: Vec<tir::attributes::NamedAttribute>,
            #(#region_fields,)*
            #(#operand_fields,)*
            #result_builder_field
        }

        impl #struct_name {
            #region_accessors
            #roles_table
            #result_accessor
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

            fn from_op_instance(instance: std::sync::Arc<tir::OpInstance>) -> Self {
                assert_eq!(instance.name(), #name);
                #struct_name(instance)
            }

            fn from_op_instance_dyn(instance: std::sync::Arc<tir::OpInstance>) -> Box<dyn tir::Operation> {
                assert_eq!(instance.name(), #name);
                Box::new(#struct_name(instance))
            }

            fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
                self
            }

            #printer

            #parser

            fn regions(&self) -> tir::ContextIterator<tir::RegionId> {
                let context = self.0.context.upgrade();
                tir::ContextIterator::new(context, self.0.regions.clone())
            }

            fn operands(&self) -> &[tir::ValueId] {
                &self.0.operands
            }

            fn attributes(&self) -> &[tir::attributes::NamedAttribute] {
                &self.0.attributes
            }

            fn operand_names(&self) -> &'static [&'static str] {
                &[#(#operand_name_literals),*]
            }

            #semantic_expr_method
            #interface_registration_method
        }

        impl #builder_name {
            pub fn new(context: &tir::Context) -> #builder_name {
                Self {
                    context: context.clone(),
                    attributes: vec![],
                    #(#region_defaults,)*
                    #(#operand_defaults,)*
                    #result_builder_default
                }
            }

            #(#region_builders)*
            #(#operand_builders)*
            #result_builder_method

            pub fn attr(mut self, name: &str, value: tir::attributes::AttributeValue) -> Self {
                self.attributes.push(tir::attributes::NamedAttribute::new(name, value));
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

                let instance = tir::OpInstance {
                    id: tir::OpId::invalid(),
                    name: #name,
                    dialect: #dialect,
                    context: self.context.as_context_ref(),
                    operands: operand_vec,
                    results: result_vec,
                    regions,
                    attributes,
                    attribute_roles: #struct_name::attribute_roles(),
                };

                let instance = self.context.add_operation(instance);

                #struct_name(instance)
            }
        }

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
    .into()
}

struct Operation {
    struct_name: Ident,
    name: String,
    dialect: String,
    regions: Vec<Region>,
    attributes: Vec<AttrSpec>,
    roles: Vec<RoleSpec>,
    operands: Vec<ValueSpec>,
    results: Vec<ValueSpec>,
    interfaces: Vec<Path>,
    custom_format: bool,
    as_sem_expr_body: Option<proc_macro2::TokenStream>,
    custom_verifier: bool,
}

struct Region {
    name: String,
    single_block: bool,
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

        let roles = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "roles" {
                        get_roles(&f.expr)
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

        let as_sem_expr_body = struct_
            .fields
            .iter()
            .find_map(|f| match &f.member {
                Member::Named(ident) => {
                    if ident.to_string().as_str() == "sem" {
                        let new = expr_as_sem_expr_body(&f.expr, &operands);
                        Some(new)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .unwrap_or(None);

        Ok(Operation {
            struct_name,
            name,
            dialect,
            regions,
            attributes,
            roles,
            operands,
            results,
            interfaces,
            custom_format,
            as_sem_expr_body,
            custom_verifier,
        })
    }
}

#[derive(Clone)]
enum SemNode {
    Atom(String),
    List(Vec<SemNode>),
}

fn parse_sem_expr(input: &str) -> Option<SemNode> {
    fn parse_list(chars: &[char], pos: &mut usize) -> Option<SemNode> {
        if *pos >= chars.len() || chars[*pos] != '(' {
            return None;
        }
        *pos += 1;
        let mut items = Vec::new();
        loop {
            while *pos < chars.len() && chars[*pos].is_whitespace() {
                *pos += 1;
            }
            if *pos >= chars.len() {
                return None;
            }
            if chars[*pos] == ')' {
                *pos += 1;
                break;
            }
            if chars[*pos] == '(' {
                items.push(parse_list(chars, pos)?);
                continue;
            }
            let start = *pos;
            while *pos < chars.len()
                && !chars[*pos].is_whitespace()
                && chars[*pos] != '('
                && chars[*pos] != ')'
            {
                *pos += 1;
            }
            items.push(SemNode::Atom(chars[start..*pos].iter().collect()));
        }
        Some(SemNode::List(items))
    }

    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0usize;
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }
    let expr = parse_list(&chars, &mut pos)?;
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }
    if pos == chars.len() { Some(expr) } else { None }
}

fn sem_node_to_dag_stmts(
    node: &SemNode,
    operand_symbols: &std::collections::HashMap<String, u32>,
    counter: &mut u32,
) -> Option<(Vec<proc_macro2::TokenStream>, proc_macro2::Ident)> {
    match node {
        SemNode::Atom(name) => {
            if let Some(&idx) = operand_symbols.get(name) {
                let var = format_ident!("__sem_node_{}", *counter);
                *counter += 1;
                let idx_lit = proc_macro2::Literal::u32_unsuffixed(idx);
                let stmt = quote! {
                    let #var = g.add_node(tir::sem_expr::ExprKind::Symbol);
                    g.set_leaf_data(#var, tir::sem_expr::ExprPayload::SymbolId(#idx_lit));
                };
                Some((vec![stmt], var))
            } else if let Ok(i) = name.parse::<i64>() {
                let var = format_ident!("__sem_node_{}", *counter);
                *counter += 1;
                let val = proc_macro2::Literal::i64_unsuffixed(i);
                let stmt = quote! {
                    let #var = g.add_node(tir::sem_expr::ExprKind::Constant);
                    g.set_leaf_data(#var, tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new_signed(64, #val)));
                };
                Some((vec![stmt], var))
            } else {
                None
            }
        }
        SemNode::List(items) => {
            // Unary width-changing ops take the result width from the op's result
            // type (read through the context the generated body already holds), so
            // `(sext x)`/`(zext x)`/`(trunc x)` need no explicit width operand.
            if let [SemNode::Atom(op), arg] = items.as_slice() {
                let ext_kind = match op.as_str() {
                    "sext" => Some(quote! { tir::sem_expr::ExprKind::SExt }),
                    "zext" => Some(quote! { tir::sem_expr::ExprKind::ZExt }),
                    "trunc" => None,
                    _ => return None,
                };
                let (mut stmts, arg_var) = sem_node_to_dag_stmts(arg, operand_symbols, counter)?;
                let width_var = format_ident!("__sem_node_{}", *counter);
                *counter += 1;
                stmts.push(quote! {
                    let __tir_result_width = {
                        let __ctx = self.0.context.upgrade();
                        let __ty = __ctx.get_value(self.0.results[0]).ty();
                        (__ctx.get_type_data(__ty).as_ref() as &dyn std::any::Any)
                            .downcast_ref::<tir::builtin::IntegerType>()
                            .map(|t| t.width())
                            .unwrap_or(0) as u64
                    };
                    let #width_var = g.add_node(tir::sem_expr::ExprKind::Constant);
                    g.set_leaf_data(
                        #width_var,
                        tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new(16, __tir_result_width)),
                    );
                });
                let var = format_ident!("__sem_node_{}", *counter);
                *counter += 1;
                if let Some(kind) = ext_kind {
                    stmts.push(quote! {
                        let #var = g.add_node(#kind);
                        g.add_edge(#var, #arg_var);
                        g.add_edge(#var, #width_var);
                    });
                } else {
                    // trunc x  ==  extract(x, result_width - 1, 0)
                    let low_var = format_ident!("__sem_node_{}", *counter);
                    *counter += 1;
                    stmts.push(quote! {
                        let #low_var = g.add_node(tir::sem_expr::ExprKind::Constant);
                        g.set_leaf_data(
                            #low_var,
                            tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new(16, 0)),
                        );
                        // Reuse the width constant as the (high = width - 1) bound.
                        g.set_leaf_data(
                            #width_var,
                            tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new(
                                16,
                                __tir_result_width.saturating_sub(1),
                            )),
                        );
                        let #var = g.add_node(tir::sem_expr::ExprKind::Extract);
                        g.add_edge(#var, #arg_var);
                        g.add_edge(#var, #width_var);
                        g.add_edge(#var, #low_var);
                    });
                }
                return Some((stmts, var));
            }

            let [SemNode::Atom(op), lhs, rhs] = items.as_slice() else {
                return None;
            };
            let kind = match op.as_str() {
                "add" => quote! { tir::sem_expr::ExprKind::Add },
                "sub" => quote! { tir::sem_expr::ExprKind::Sub },
                "mul" => quote! { tir::sem_expr::ExprKind::Mul },
                "div" => quote! { tir::sem_expr::ExprKind::Div },
                "and" => quote! { tir::sem_expr::ExprKind::And },
                "or" => quote! { tir::sem_expr::ExprKind::Or },
                "xor" => quote! { tir::sem_expr::ExprKind::Xor },
                "shl" => quote! { tir::sem_expr::ExprKind::ShiftLeft },
                "lshr" => quote! { tir::sem_expr::ExprKind::ShiftRightLogic },
                "ashr" => quote! { tir::sem_expr::ExprKind::ShiftRightArithmetic },
                _ => return None,
            };
            let (mut stmts, lhs_var) = sem_node_to_dag_stmts(lhs, operand_symbols, counter)?;
            let (rhs_stmts, rhs_var) = sem_node_to_dag_stmts(rhs, operand_symbols, counter)?;
            stmts.extend(rhs_stmts);
            let var = format_ident!("__sem_node_{}", *counter);
            *counter += 1;
            stmts.push(quote! {
                let #var = g.add_node(#kind);
                g.add_edge(#var, #lhs_var);
                g.add_edge(#var, #rhs_var);
            });
            Some((stmts, var))
        }
    }
}

fn expr_as_sem_expr_body(expr: &Expr, operands: &[ValueSpec]) -> Option<proc_macro2::TokenStream> {
    let sem_src = match expr {
        Expr::Lit(lit) => {
            if let syn::Lit::Str(s) = &lit.lit {
                s.value()
            } else {
                return None;
            }
        }
        _ => return None,
    };

    let mut symbols = std::collections::HashMap::new();
    for (idx, operand) in operands.iter().enumerate() {
        symbols.insert(operand.name.clone(), idx as u32);
    }

    let parsed = parse_sem_expr(&sem_src)?;
    let SemNode::List(items) = parsed else {
        return None;
    };
    let [SemNode::Atom(set_kw), SemNode::Atom(_dst), rhs] = items.as_slice() else {
        return None;
    };
    if set_kw != "set" {
        return None;
    }

    let mut counter = 0u32;
    let (stmts, root_var) = sem_node_to_dag_stmts(rhs, &symbols, &mut counter)?;
    Some(quote! {
        #(#stmts)*
        #root_var
    })
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
                        single_block: true,
                    }
                })
                .collect(),
        )
    } else {
        None
    }
}

#[derive(Clone)]
struct AttrSpec {
    name: String,
    ty: String,
}

fn get_attributes(expr: &Expr) -> Option<Vec<AttrSpec>> {
    if let Expr::Struct(s) = expr {
        Some(
            s.fields
                .iter()
                .map(|f| {
                    let name = field_name(f);
                    let ty = expr_as_string(&f.expr);
                    AttrSpec { name, ty }
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
            if !self.attributes.iter().any(|a| a.name == #n) {
                panic!(concat!("Missing required attribute: ", #n));
            }
        }
    });
    quote! { #(#checks)* }
}

#[derive(Clone)]
struct RoleSpec {
    name: String,
    role: String,
}

fn get_roles(expr: &Expr) -> Option<Vec<RoleSpec>> {
    if let Expr::Struct(s) = expr {
        Some(
            s.fields
                .iter()
                .map(|f| {
                    let name = field_name(f);
                    let role = expr_as_string(&f.expr);
                    RoleSpec { name, role }
                })
                .collect(),
        )
    } else {
        None
    }
}

fn make_roles_table(_op_ident: &Ident, roles: &[RoleSpec]) -> proc_macro2::TokenStream {
    // Always emit `attribute_roles()` (empty when no roles) so `build()` can thread
    // the table onto every `OpInstance` uniformly, and the core can read register
    // def/use roles without resolving the op back to its concrete type.
    let mut pairs = Vec::new();
    for r in roles {
        let name = r.name.clone();
        let role_ts = match r.role.as_str() {
            "Def" => quote! { tir::attributes::AttributeRole::Def },
            "Use" => quote! { tir::attributes::AttributeRole::Use },
            "Clobber" => quote! { tir::attributes::AttributeRole::Clobber },
            "ReadWrite" => quote! { tir::attributes::AttributeRole::ReadWrite },
            _ => quote! { tir::attributes::AttributeRole::None },
        };
        pairs.push(quote! { ( #name, #role_ts ) });
    }
    let len = pairs.len();
    quote! {
        pub fn attribute_roles() -> &'static [(&'static str, tir::attributes::AttributeRole)] {
            const ROLES: [(&str, tir::attributes::AttributeRole); #len] = [ #(#pairs),* ];
            &ROLES
        }
    }
}

fn make_region_accessors(regions: &[Region]) -> proc_macro2::TokenStream {
    if regions.is_empty() {
        return quote! {};
    }

    let accessors = regions.iter().enumerate().map(|(index, region)| {
        if region.single_block {
            make_single_block_region_accessor(region, index)
        } else {
            make_region_accessor(region, index)
        }
    });

    quote! { #(#accessors)* }
}

fn make_region_accessor(region: &Region, index: usize) -> proc_macro2::TokenStream {
    let func_name = format_ident!("{}", region.name);
    quote! {
        pub fn #func_name(&self) -> std::sync::Arc<tir::Region> {
            use tir::Operation;
            self.regions().nth(#index).unwrap()
        }
    }
}

fn make_single_block_region_accessor(region: &Region, index: usize) -> proc_macro2::TokenStream {
    let func_name = format_ident!("{}", region.name);

    quote! {
        pub fn #func_name(&self) -> std::sync::Arc<tir::Block> {
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

    let result_prefix = if has_results {
        quote! {
            if !self.0.results.is_empty() {
                fmt.write(format!("%{} = ", self.0.results[0].number()))?;
            }
        }
    } else {
        quote! {}
    };

    let operand_printer = if !operands.is_empty() {
        quote! {
            if !self.0.operands.is_empty() {
                fmt.write(" ")?;
                let mut first = true;
                for op_id in &self.0.operands {
                    if !first { fmt.write(", ")?; }
                    first = false;
                    fmt.write(format!("%{}", op_id.number()))?;
                }
            }
        }
    } else {
        quote! {}
    };

    let result_suffix = if has_results {
        quote! {
            if !self.0.results.is_empty() {
                let context = self.0.context.upgrade();
                let result_val = context.get_value(self.0.results[0]);
                fmt.write(" : ")?;
                context.print_type(result_val.ty(), fmt)?;
            }
        }
    } else {
        quote! {}
    };

    let regions = if regions.len() == 1 && regions[0].single_block {
        make_single_block_region_printer(&regions[0], 0)
    } else {
        quote! {}
    };

    quote! {
        fn print<'a, 'b: 'a>(&'a self, fmt: &'a mut tir::IRFormatter<'b>) -> Result<(), std::fmt::Error> {
            #result_prefix
            fmt.write(#op_name)?;
            #operand_printer
            // Print generic attribute dict if any
            if !self.attributes().is_empty() {
                fmt.write(" ")?;
                fmt.write("{")?;
                let mut first = true;
                for attr in self.attributes() {
                    if !first { fmt.write(", ")?; }
                    first = false;
                    fmt.write(&attr.name)?;
                    fmt.write(" = ")?;
                    let context = self.0.context.upgrade();
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

fn make_single_block_region_printer(region: &Region, index: usize) -> proc_macro2::TokenStream {
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
    has_results: bool,
) -> proc_macro2::TokenStream {
    let (region_parsers, region_builders) = if regions.len() == 1 && regions[0].single_block {
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
                    if let Ok(num) = ref_name.parse::<u32>() {
                        builder = builder.#field(tir::ValueId::from_number(num));
                    }
                }
            }
        })
        .collect();

    let result_parser = if has_results {
        quote! {
            if !parser.parse_token(":") {
                return Err((parser.span(), tir::Error::ExpectedToken(":")));
            }
            let result_ty = parser.parse_type(context)
                ?
                .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?;
            builder = builder.result_type(result_ty);
        }
    } else {
        quote! {}
    };

    quote! {
        fn parse<'src>(parser: &mut tir::parse::text::Parser<'src>, context: &tir::Context)
        -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
           use tir::parse::common::Cursor;

           let mut parsed_attrs: Vec<tir::attributes::NamedAttribute> = vec![];

           let mut builder = #builder_name::new(context);

           #(#operand_parsers)*

           // Parse optional generic attribute dict: { key = value, ... }
           let mark = parser.pos();
           if parser.parse_token("{") {
               let mut ok = true;
               if !parser.parse_token("}") {
                   loop {
                       if let Some(name) = parser.parse_ident() {
                           if !parser.parse_token("=") { ok = false; break; }
                           let val = if let Some(s) = parser.parse_string() {
                               tir::attributes::AttributeValue::Str(s.to_string())
                           } else if let Some(ty) = parser.parse_type(context)? {
                               tir::attributes::AttributeValue::Type(ty)
                           } else if parser.parse_token("%virt") {
                               if let Some(id) = parser.parse_number() {
                                   let mut class = None;
                                   if parser.parse_token(":") {
                                       if let Some(cls) = parser.parse_ident() { class = Some(cls.to_string()); } else { ok = false; }
                                   }
                                   tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Virtual { id: id as u32, class: class })
                               } else { ok = false; break; }
                           } else if let Some(n) = parser.parse_number() {
                               tir::attributes::AttributeValue::Int(n)
                           } else {
                               ok = false; break;
                           };
                           parsed_attrs.push(tir::attributes::NamedAttribute::new(name, val));
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

            for a in parsed_attrs { builder = builder.attr(&a.name, a.value); }

            Ok(Box::new(builder
                #region_builders
                .build()))
        }
    }
}
