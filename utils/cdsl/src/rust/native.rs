use std::{collections::BTreeSet, fmt::Write};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, Path, ext::IdentExt};

use crate::{
    Error, Program,
    ast::{Declaration, Expr, Expression, Literal, Record},
};

mod bindings;
mod properties;

use bindings::Bindings;
use properties::{Attribute, Region};

struct Slot<'a> {
    name: &'a str,
    ty: &'a str,
}

impl Slot<'_> {
    fn variadic(&self) -> bool {
        self.ty.starts_with('*')
    }

    fn state(&self) -> bool {
        self.ty == "*state<_>"
    }

    fn schema(&self) -> TokenStream {
        let Self { name, ty } = self;
        let variadic = self.variadic();
        quote! { tir::FieldSchema { name: #name, ty: #ty, variadic: #variadic } }
    }
}

const STATE: Slot<'static> = Slot {
    name: "state",
    ty: "*state<_>",
};

struct Operation<'a> {
    rust_name: Ident,
    constructor: Ident,
    name: &'a str,
    dialect: &'a str,
    quantified: &'a str,
    operands: Vec<Slot<'a>>,
    results: Vec<Slot<'a>>,
    attributes: Vec<Attribute<'a>>,
    assembly: Option<AssemblyLayout<'a>>,
    interfaces: Vec<Path>,
    regions: Vec<Region>,
    custom_format: bool,
}

struct AssemblyLayout<'a> {
    operands: Vec<Slot<'a>>,
    results: Vec<Slot<'a>>,
    attributes: Vec<Attribute<'a>>,
}

pub(super) fn emit(
    program: &Program,
    output: &mut String,
    dependencies: &mut BTreeSet<crate::ast::Path>,
    declaration: &Declaration,
) -> Result<bool, Error> {
    let Declaration::Let { binding, .. } = declaration else {
        return Ok(false);
    };
    let Expression::Record(record) = &binding.value.value else {
        return Ok(false);
    };
    if !record.iter().any(|(name, _)| name == "rust_name") {
        return Ok(false);
    }
    let interface = |expression: &Expr| {
        let Expression::Path(path) = &expression.value else {
            return Err(Error("expected a declared interface".into()));
        };
        super::interface_path(program, &[], path, "self", dependencies)
            .and_then(|path| parse_rust(&path))
    };
    let operation = Operation::parse(record, interface)?;
    let binding = |name| find_field(record, name).map(string).transpose();
    let binds = Bindings::parse(
        binding("binds")?.unwrap_or_default(),
        binding("counted")?.unwrap_or_default(),
        &operation,
    )?;
    write_operation(output, operation, binds)?;
    Ok(true)
}

fn write_operation(
    output: &mut String,
    operation: Operation<'_>,
    bindings: Bindings,
) -> Result<(), Error> {
    let scheme = operation.scheme();
    let signature = super::emit_scheme(&scheme)?
        .parse::<TokenStream>()
        .map_err(|error| Error(error.to_string()))?;
    writeln!(output, "{}", operation.render(bindings, &scheme, signature))
        .map_err(|error| Error(error.to_string()))
}

fn field<'a>(record: &'a [(String, Expr)], name: &str) -> Result<&'a Expr, Error> {
    find_field(record, name).ok_or_else(|| Error(format!("missing Rust operation field {name}")))
}

fn find_field<'a>(record: &'a [(String, Expr)], name: &str) -> Option<&'a Expr> {
    record
        .iter()
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn string(expression: &Expr) -> Result<&str, Error> {
    match &expression.value {
        Expression::Literal(Literal::String(value)) => Ok(value),
        _ => Err(Error("expected a Rust operation string".into())),
    }
}

fn boolean(expression: &Expr) -> Result<bool, Error> {
    match &expression.value {
        Expression::Literal(Literal::Bool(value)) => Ok(*value),
        _ => Err(Error("expected a Rust operation flag".into())),
    }
}

fn natural<T: std::str::FromStr>(expression: &Expr) -> Result<T, Error> {
    let Expression::Literal(Literal::Natural(value)) = &expression.value else {
        return Err(Error("expected a Rust operation natural number".into()));
    };
    value
        .parse()
        .map_err(|_| Error(format!("invalid Rust operation natural number {value}")))
}

fn entries(expression: &Expr) -> Result<&[(String, Expr)], Error> {
    match &expression.value {
        Expression::Record(fields) => Ok(fields),
        _ => Err(Error("expected a Rust operation record".into())),
    }
}

fn optional_entries<'a>(
    record: &'a [(String, Expr)],
    name: &str,
) -> Result<&'a [(String, Expr)], Error> {
    Ok(find_field(record, name)
        .map(entries)
        .transpose()?
        .unwrap_or_default())
}

fn parse_rust<T: syn::parse::Parse>(value: &str) -> Result<T, Error> {
    syn::parse_str(value)
        .map_err(|error| Error(format!("invalid Rust operation name {value}: {error}")))
}

fn slots<'a>(record: &'a [(String, Expr)], key: &str) -> Result<Vec<Slot<'a>>, Error> {
    optional_entries(record, key)?
        .iter()
        .map(|(name, ty)| {
            Ok(Slot {
                name,
                ty: string(ty)?,
            })
        })
        .collect()
}

fn attributes(record: &[(String, Expr)]) -> Result<Vec<Attribute<'_>>, Error> {
    optional_entries(record, "attributes")?
        .iter()
        .map(|(name, value)| Attribute::parse(name, string(value)?))
        .collect()
}

impl<'a> AssemblyLayout<'a> {
    fn parse(record: &'a [(String, Expr)]) -> Result<Self, Error> {
        Ok(Self {
            operands: slots(record, "operands")?,
            results: slots(record, "results")?,
            attributes: attributes(record)?,
        })
    }

    fn schema(&self) -> TokenStream {
        let operands = self.operands.iter().map(Slot::schema);
        let results = self.results.iter().map(Slot::schema);
        let attributes = self.attributes.iter().map(Attribute::schema);
        quote! { tir::schema::OpLayout {
            operands: &[#(#operands),*],
            results: &[#(#results),*],
            attributes: &[#(#attributes),*],
        } }
    }
}

impl<'a> Operation<'a> {
    fn parse(
        record: &'a Record,
        interface: impl FnMut(&Expr) -> Result<Path, Error>,
    ) -> Result<Self, Error> {
        let read = |name| field(record, name).and_then(string);
        let flag = |name| find_field(record, name).map(boolean).transpose();
        let rust_name = parse_rust(read("rust_name")?)?;
        let name = read("name")?;
        let constructor = parse_rust(&format!("r#{}", name.replace('.', "_")))?;
        let dialect = read("dialect")?;
        if dialect == "fp" {
            return Err(Error(
                "FP uses the checked coercion and refinement model".into(),
            ));
        }
        let quantified = find_field(record, "quantified")
            .map(string)
            .transpose()?
            .unwrap_or_default();
        let mut operands = slots(record, "operands")?;
        let mut results = slots(record, "results")?;
        let state = read("state")?;
        match state {
            "none" => {}
            "in" => operands.push(STATE),
            "out" => results.push(STATE),
            "in_out" => {
                operands.push(STATE);
                results.push(STATE);
            }
            _ => {
                return Err(Error(format!(
                    "unknown Rust operation state direction {state}"
                )));
            }
        }
        let mut attributes = attributes(record)?;
        let machine = flag("machine")?.unwrap_or_default();
        let immediates = optional_entries(record, "immediates")?;
        if !machine && !immediates.is_empty() {
            return Err(Error("immediates require a machine operation".into()));
        }
        for (name, constraints) in immediates {
            let attribute = attributes
                .iter_mut()
                .find(|attribute| attribute.name == name)
                .ok_or_else(|| Error(format!("unknown immediate attribute {name}")))?;
            attribute.set_immediate(&rust_name, constraints)?;
        }
        let assembly = machine
            .then(|| {
                let layout = find_field(record, "assembly")
                    .map(entries)
                    .transpose()?
                    .unwrap_or_default();
                AssemblyLayout::parse(layout)
            })
            .transpose()?;
        let interfaces = super::interface_entries(find_field(record, "interfaces"))
            .iter()
            .map(interface)
            .collect::<Result<Vec<_>, _>>()?;
        let regions = optional_entries(record, "regions")?
            .iter()
            .map(|(name, value)| Region::parse(name, value))
            .collect::<Result<Vec<_>, _>>()?;
        let custom_format = flag("custom_format")?.unwrap_or_default();
        Self {
            rust_name,
            constructor,
            name,
            dialect,
            quantified,
            operands,
            results,
            attributes,
            assembly,
            interfaces,
            regions,
            custom_format,
        }
        .validate()
    }

    fn validate(mut self) -> Result<Self, Error> {
        let _: Ident = parse_rust(&format!("r#{}", self.dialect))?;
        self.quantified = self.quantified.trim_end();
        let quantified = self.quantified;
        if !quantified.is_empty()
            && !(quantified.starts_with("forall ") && quantified.ends_with('.'))
        {
            return Err(Error(format!(
                "invalid Rust operation scheme binders {quantified}"
            )));
        }
        for slot in self.operands.iter().chain(&self.results) {
            let _: Ident = parse_rust(&format!("r#{}", slot.name))?;
        }
        let mut data_results = self.results.iter().filter(|slot| !slot.state());
        let multiple_results = data_results.clone().count() > 1;
        if self.results.iter().filter(|slot| slot.state()).count() > 1
            || self.operands.iter().filter(|slot| slot.state()).count() > 1
            || (multiple_results
                && (!self.custom_format
                    || data_results.any(|slot| slot.ty.starts_with(['*', '?']))))
        {
            return Err(Error(
                "unsupported result or state groups in Rust operation".into(),
            ));
        }
        Ok(self)
    }

    fn render(mut self, bindings: Bindings, scheme: &str, signature: TokenStream) -> TokenStream {
        let op = &self.rust_name;
        let builder = format_ident!("{op}Builder");
        let dialect = self.dialect;
        let name = self.name;
        let spelled = if dialect == "builtin" {
            name.to_owned()
        } else {
            format!("{dialect}.{name}")
        };
        let macro_name = format_ident!("__cdsl_{dialect}_{op}");
        let schema = format_ident!("__TIR_OP_SCHEMA_{op}");
        let build = self.builder(&builder, &schema, &spelled);
        let immediate_code = self.immediates(&spelled);
        let assembly = self.assembly.as_ref().map_or_else(
            || quote! { None },
            |layout| {
                let layout = layout.schema();
                quote! { Some(#layout) }
            },
        );
        let assembly_accessor = self.assembly.as_ref().map(|_| {
            quote! {
                impl #op {
                    /// The attribute and port schema for assembly form.
                    pub fn assembly_layout() -> &'static tir::schema::OpLayout {
                        #schema.assembly.as_ref().expect("machine operation has an assembly schema")
                    }
                }
            }
        });
        let operand_schema = self.operands.iter().map(Slot::schema);
        let result_schema = self.results.iter().map(Slot::schema);
        let attr_schema = self.attributes.iter().map(Attribute::schema);
        let binding_inputs = bindings.input_groups();
        let counted_groups = bindings.counted_groups();
        let binding_code = bindings.render(&self, &builder, &spelled);
        let binding_impl = binding_code.implementation;
        let region_schema = self.regions.iter().map(|region| {
            let kind = region.kind();
            let variadic = matches!(region, Region::VariadicNodes(_));
            quote! { (#kind, #variadic) }
        });
        self.interfaces.extend(binding_code.interfaces);
        let interfaces = &self.interfaces;
        let interface_names: Vec<_> = interfaces
            .iter()
            .filter_map(|path| path.segments.last())
            .map(|segment| segment.ident.unraw().to_string())
            .collect();
        let (print, parse) = if self.custom_format {
            (
                quote! { self.custom_print(formatter) },
                quote! { Self::custom_parse(parser, context) },
            )
        } else if let Some(format) = binding_code.format {
            format
        } else {
            (
                quote! { tir::region_format::print_generic(formatter, &self.0, #spelled) },
                self.generic_parse(&builder),
            )
        };
        quote! {
            macro_rules! #macro_name {
                ($($host:path),* $(,)?) => {
                    #[allow(non_upper_case_globals)]
                    #[tir::linkme::distributed_slice(tir::OP_SCHEMAS)]
                    #[linkme(crate = tir::linkme)]
                    static #schema: tir::OpSchema = tir::OpSchema {
                        dialect: #dialect,
                        name: #name,
                        operands: &[#(#operand_schema),*],
                        results: &[#(#result_schema),*],
                        attributes: &[#(#attr_schema),*],
                        interfaces: &[#(#interface_names,)* $(const {
                            let name = stringify!($host);
                            let mut start = name.len();
                            while start > 0 && name.as_bytes()[start - 1] != b':' {
                                start -= 1;
                            }
                            name.split_at(start).1.trim_ascii()
                        }),*],
                        regions: &[#(#region_schema),*],
                        binding_inputs: #binding_inputs,
                        counted_groups: #counted_groups,
                        scheme: #scheme,
                        signature: #signature,
                        assembly: #assembly,
                    };

                    pub struct #op(tir::OpHandle);

                    impl tir::Operation for #op {
                        fn name() -> &'static str { #name }
                        fn dialect() -> &'static str { #dialect }
                        fn handle(&self) -> &tir::OpHandle { &self.0 }

                        fn from_op_instance(instance: tir::OpHandle) -> Self {
                            assert_eq!(instance.name(), tir::OperationName::of::<Self>());
                            Self(instance)
                        }

                        fn from_op_instance_dyn(instance: tir::OpHandle) -> Box<dyn tir::Operation> {
                            Box::new(Self::from_op_instance(instance))
                        }

                        fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> { self }

                        fn print<'a, 'b: 'a>(&'a self, formatter: &'a mut tir::IRFormatter<'b>) -> std::fmt::Result {
                            #print
                        }

                        fn parse(parser: &mut tir::parse::text::Parser<'_>, context: &tir::Context)
                            -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)>
                        {
                            #parse
                        }

                        fn register_interfaces(_context: &tir::Context) {
                            #(_context.register_operation_interface::<Self, dyn #interfaces>();)*
                            $(_context.register_operation_interface::<Self, dyn $host>();)*
                        }
                    }

                    #(impl tir::ImplementsOpInterface<dyn #interfaces> for #op {
                        fn into_interface(self: Box<Self>) -> Box<dyn #interfaces> { self }
                    })*
                    $(impl tir::ImplementsOpInterface<dyn $host> for #op {
                        fn into_interface(self: Box<Self>) -> Box<dyn $host> { self }
                    })*

                    impl From<&#op> for tir::OpId {
                        fn from(operation: &#op) -> Self { tir::Operation::id(operation) }
                    }

                    impl From<#op> for tir::OpId {
                        fn from(operation: #op) -> Self { tir::Operation::id(&operation) }
                    }

                    #binding_impl
                    #assembly_accessor
                    #immediate_code
                    #build
                };
            }
        }
    }

    fn immediates(&self, spelled: &str) -> TokenStream {
        if self.assembly.is_none() {
            return TokenStream::new();
        }
        let op = &self.rust_name;
        let immediates: Vec<_> = self
            .attributes
            .iter()
            .filter_map(|attribute| attribute.immediate.as_ref())
            .collect();
        let declarations = (!immediates.is_empty()).then(|| {
            let module = format_ident!("__cdsl_immediates_{}", self.constructor.unraw());
            let brands = immediates.iter().map(|immediate| &immediate.brand);
            let declarations = immediates.iter().map(|immediate| immediate.declaration());
            quote! {
                mod #module { #(#declarations)* }
                pub use #module::{#(#brands),*};
            }
        });
        let checks = self
            .attributes
            .iter()
            .filter_map(|attribute| attribute.immediate_check(spelled));
        quote! {
            #declarations

            impl #op {
                /// Check the declared domains of the numeric immediates present in this operation.
                pub fn check_immediates(
                    attributes: &[tir::attributes::NamedAttribute], context: &tir::Context,
                ) -> Result<(), tir::Error> {
                    let _ = (attributes, context);
                    #(#checks)*
                    Ok(())
                }
            }
        }
    }

    fn scheme(&self) -> String {
        let list = |slots: &[Slot<'_>]| {
            slots
                .iter()
                .map(|slot| match slot.ty.as_bytes().first() {
                    Some(b'*' | b'?') => format!("{}{}", &slot.ty[1..], &slot.ty[..1]),
                    _ => slot.ty.to_owned(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let prefix = if self.quantified.is_empty() {
            String::new()
        } else {
            format!("{} ", self.quantified)
        };
        format!(
            "{prefix}({}) -> ({})",
            list(&self.operands),
            list(&self.results)
        )
    }

    fn builder(&self, builder: &Ident, schema: &Ident, spelled: &str) -> TokenStream {
        let op = &self.rust_name;
        let constructor = &self.constructor;
        let mut fields = TokenStream::new();
        let mut methods = TokenStream::new();
        let mut getters = TokenStream::new();
        let mut collect_operands = TokenStream::new();
        let mut collect_results = TokenStream::new();
        let mut collect_regions = TokenStream::new();
        let mut operand_parameters = TokenStream::new();
        let mut attribute_parameters = TokenStream::new();
        let mut result_parameter = TokenStream::new();
        let mut region_parameters = TokenStream::new();
        let mut arguments = TokenStream::new();
        let has_segments = self.operands.iter().any(Slot::variadic);
        for operand in &self.operands {
            let name = format_ident!("r#{}", operand.name);
            if operand.variadic() {
                fields.extend(quote! { #name: Vec<tir::ValueId>, });
                collect_operands.extend(quote! {
                    segment_sizes.push(tir::attributes::AttributeValue::UInt(self.ports.#name.len() as u64));
                    operands.extend(self.ports.#name);
                });
            } else {
                fields.extend(quote! { #name: Option<tir::ValueId>, });
                if has_segments {
                    collect_operands.extend(quote! {
                        segment_sizes.push(tir::attributes::AttributeValue::UInt(u64::from(self.ports.#name.is_some())));
                    });
                }
                collect_operands.extend(quote! { operands.extend(self.ports.#name); });
            }
            if operand.state() && operand.name == "state" {
                methods.extend(quote! {
                    pub fn state(mut self, value: tir::ValueId) -> Self {
                        self.ports.state.push(value);
                        self
                    }
                    pub fn state_operands(mut self, values: impl IntoIterator<Item = tir::ValueId>) -> Self {
                        self.ports.state = values.into_iter().collect();
                        self
                    }
                });
            } else if operand.variadic() {
                methods.extend(quote! {
                    pub fn #name(mut self, values: Vec<tir::ValueId>) -> Self {
                        self.ports.#name = values;
                        self
                    }
                });
                if !operand.state() {
                    operand_parameters.extend(quote! { #name: Vec<tir::ValueId>, });
                    arguments.extend(quote! { builder = builder.#name(#name); });
                }
            } else {
                methods.extend(quote! {
                    pub fn #name(mut self, value: tir::ValueId) -> Self {
                        self.ports.#name = Some(value);
                        self
                    }
                });
                operand_parameters.extend(quote! { #name: impl Into<tir::Operand>, });
                arguments.extend(quote! {
                    if let Some(value) = #name.into().into_option() { builder = builder.#name(value); }
                });
            }
        }
        let mut required = TokenStream::new();
        for attribute in &self.attributes {
            let name = attribute.name;
            if !attribute.optional() {
                let method = &attribute.method;
                if let Some(immediate) = &attribute.immediate {
                    let brand = &immediate.brand;
                    attribute_parameters.extend(quote! { #method: #brand, });
                    arguments.extend(quote! { builder = builder.#method(#method); });
                } else {
                    attribute_parameters
                        .extend(quote! { #method: impl Into<tir::attributes::AttributeValue>, });
                    arguments.extend(quote! { builder = builder.attr(#name, #method.into()); });
                }
                if self.assembly.is_none() {
                    let missing = format!("Missing required attribute: {name}");
                    required.extend(quote! {
                        assert!(self.parts.attributes.iter().any(|attribute| {
                            Some(attribute.name) == self.context.sym(#name)
                        }), #missing);
                    });
                }
            }
            let (setter, getter) = attribute.accessors(self.dialect, self.name);
            methods.extend(setter);
            getters.extend(getter);
        }
        let multiple_results = self.results.iter().filter(|slot| !slot.state()).count() > 1;
        if self.results.iter().any(|slot| !slot.state()) {
            getters.extend(quote! { pub fn result(&self) -> tir::ValueId { self.0.results()[0] } });
        }
        for result in &self.results {
            if result.state() {
                fields.extend(quote! { state_result_types: Vec<tir::TypeId>, });
                collect_results.extend(quote! {
                    results.extend(self.ports.state_result_types.into_iter().map(|ty| self.context.create_value(ty, None).id()));
                });
                if result.name == "state" {
                    methods.extend(quote! {
                        pub fn state_result(mut self, ty: tir::TypeId) -> Self {
                            self.ports.state_result_types.push(ty);
                            self
                        }
                        pub fn state_results(mut self, types: impl IntoIterator<Item = tir::TypeId>) -> Self {
                            self.ports.state_result_types = types.into_iter().collect();
                            self
                        }
                    });
                } else {
                    let name = format_ident!("r#{}", result.name);
                    methods.extend(quote! {
                        pub fn #name(mut self, types: impl IntoIterator<Item = tir::TypeId>) -> Self {
                            self.ports.state_result_types = types.into_iter().collect();
                            self
                        }
                    });
                }
                continue;
            }
            if multiple_results {
                let name = format_ident!("r#{}", result.name);
                let missing = format!("result type for '{}' must be set", result.name);
                fields.extend(quote! { #name: Option<tir::TypeId>, });
                methods.extend(quote! {
                    pub fn #name(mut self, ty: tir::TypeId) -> Self {
                        self.ports.#name = Some(ty);
                        self
                    }
                });
                collect_results.extend(quote! {
                    let ty = self.ports.#name.expect(#missing);
                    results.push(self.context.create_value(ty, None).id());
                });
                result_parameter.extend(quote! { #name: tir::TypeId, });
                arguments.extend(quote! { builder = builder.#name(#name); });
            } else if result.variadic() {
                fields.extend(
                    quote! { result_types: Vec<tir::TypeId>, result_values: Vec<tir::ValueId>, },
                );
                methods.extend(quote! {
                    pub fn result_types(mut self, types: Vec<tir::TypeId>) -> Self {
                        self.ports.result_types = types;
                        self
                    }
                    pub fn result_values(mut self, values: Vec<tir::ValueId>) -> Self {
                        self.ports.result_values = values;
                        self
                    }
                });
                collect_results.extend(quote! {
                    if self.ports.result_values.is_empty() {
                        results.extend(self.ports.result_types.into_iter().map(|ty| self.context.create_value(ty, None).id()));
                    } else {
                        results.extend(self.ports.result_values);
                    }
                });
                result_parameter = quote! { result_types: Vec<tir::TypeId>, };
                arguments.extend(quote! { builder = builder.result_types(result_types); });
            } else {
                fields.extend(quote! { result_type: Option<tir::TypeId>, });
                methods.extend(quote! {
                    pub fn result_type(mut self, ty: tir::TypeId) -> Self {
                        self.ports.result_type = Some(ty);
                        self
                    }
                });
                collect_results.extend(quote! {
                    let ty = self.ports.result_type.expect("result_type must be set for ops with results");
                    results.push(self.context.create_value(ty, None).id());
                });
                result_parameter = quote! { result_type: tir::TypeId, };
                arguments.extend(quote! { builder = builder.result_type(result_type); });
            }
        }
        for (index, region) in self.regions.iter().enumerate() {
            let pieces = region.builder(index);
            fields.extend(pieces.field);
            methods.extend(pieces.setter);
            getters.extend(pieces.getter);
            collect_regions.extend(pieces.collect);
            region_parameters.extend(pieces.parameter);
            arguments.extend(pieces.argument);
        }
        let (segment_sizes, store_sizes) = if has_segments {
            (
                quote! { let mut segment_sizes = Vec::new(); },
                quote! {
                    self.parts.attributes.push(self.context.named_attribute(
                        "operand_segment_sizes",
                        tir::attributes::AttributeValue::Array(segment_sizes.into()),
                    ));
                },
            )
        } else {
            (TokenStream::new(), TokenStream::new())
        };
        let (ports_type, ports_field, ports_default) = if fields.is_empty() {
            (TokenStream::new(), TokenStream::new(), TokenStream::new())
        } else {
            let ports = format_ident!("{op}Ports");
            (
                quote! {
                    #[derive(Default)]
                    struct #ports { #fields }
                },
                quote! { ports: #ports, },
                quote! { ports: #ports::default(), },
            )
        };
        let build = quote! {
            let mut regions = Vec::new();
            #collect_regions
            #required
            let mut operands = Vec::new();
            #segment_sizes
            #collect_operands
            let mut results = Vec::new();
            #collect_results
            #store_sizes
            let instance = tir::NewOp::new::<#op>(self.context.clone(), operands, results, regions, self.parts);
            let operation = #op(self.context.add_operation(self.insertion, instance));
        };
        let build_method = if self.assembly.is_some() {
            quote! {
                /// Check the attributes, then create the operation and its results.
                #[allow(unused_mut)]
                pub fn try_build(mut self) -> Result<#op, tir::Error> {
                    tir::check_opdef_attributes(&self.context, &self.parts.attributes, #spelled, #schema.attributes)?;
                    #op::check_immediates(&self.parts.attributes, &self.context)?;
                    #build
                    Ok(operation)
                }
            }
        } else {
            quote! {
                #[allow(unused_mut)]
                pub fn build(mut self) -> #op {
                    #build
                    operation
                }
            }
        };
        quote! {
            impl #op { #getters }

            #ports_type

            pub struct #builder {
                context: tir::Context,
                parts: tir::NewOpParts,
                insertion: tir::InsertionPoint,
                #ports_field
            }

            impl #builder {
                pub fn new(context: &tir::Context) -> Self {
                    Self {
                        context: context.clone(),
                        parts: tir::NewOpParts::default(),
                        insertion: tir::InsertionPoint::Root,
                        #ports_default
                    }
                }

                #methods

                pub fn at(mut self, insertion: tir::InsertionPoint) -> Self {
                    self.insertion = insertion;
                    self
                }

                pub fn attr(mut self, name: &str, value: tir::attributes::AttributeValue) -> Self {
                    self.parts.attributes.push(self.context.named_attribute(name, value));
                    self
                }

                pub fn attr_sym(mut self, name: tir::Sym, value: tir::attributes::AttributeValue) -> Self {
                    self.parts.attributes.push(tir::attributes::NamedAttribute::new(name, value));
                    self
                }

                #build_method
            }

            #[allow(unused_mut, clippy::too_many_arguments)]
            pub fn #constructor(
                context: &tir::Context,
                #operand_parameters #attribute_parameters #result_parameter #region_parameters
            ) -> #builder {
                let mut builder = #builder::new(context);
                #arguments
                builder
            }
        }
    }
}
