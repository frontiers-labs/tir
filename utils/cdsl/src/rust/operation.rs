use std::{collections::BTreeMap, fmt::Write};

use crate::{
    Error, Program,
    ast::{Binding, Declaration, Expr, Expression, Literal, Node, OpField, Path},
};

pub(super) fn emit(
    program: &Program,
    export: &str,
    module: &[String],
    binding: &Binding,
    fields: &[Node<OpField>],
    output: &mut String,
) -> Result<(), Error> {
    emit_fixed_operation(program, export, module, binding, fields, output)
        .map_err(|error| Error(error.to_string()))
}

fn emit_fixed_operation(
    program: &Program,
    export: &str,
    module: &[String],
    binding: &Binding,
    fields: &[Node<OpField>],
    output: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity = binding
        .operation_name(module)
        .ok_or("operation has no name")?;
    let (dialect, name) = identity.split_once('.').ok_or("operation has no dialect")?;
    let host_path: Path = module
        .iter()
        .cloned()
        .chain([format!("{name}_rust")])
        .collect();
    let host = match program.find_rust_export(&host_path) {
        Some(Declaration::Let { binding, .. }) => match &binding.value.value {
            Expression::Record(fields) => fields.as_slice(),
            _ => return Err("Rust operation options must be a record".into()),
        },
        _ => &[],
    };
    let option = |name: &str| {
        host.iter()
            .find_map(|(key, expr)| (key == name).then_some(expr))
    };
    let enabled = |name: &str| {
        matches!(
            option(name).map(|expr| &expr.value),
            Some(Expression::Literal(Literal::Bool(true)))
        )
    };
    let inherited = match option("interfaces").map(|expr| &expr.value) {
        Some(Expression::Record(fields)) => fields
            .iter()
            .map(|(name, value)| match value.value {
                Expression::Literal(Literal::Bool(true)) => Ok(name.as_str()),
                _ => Err("a Rust interface must name an existing implementation"),
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
        _ => return Err("Rust interfaces must be a record".into()),
    };
    let (parameters, result, results) = fields
        .iter()
        .find_map(|field| match &field.value {
            OpField::Signature {
                parameters,
                result,
                results,
            } => Some((parameters, result, results)),
            _ => None,
        })
        .ok_or("missing operation signature")?;
    let mut uses = BTreeMap::new();
    for ty in parameters.iter().map(|operand| &operand.ty).chain([result]) {
        if let Some((_, indices)) = fp_slot(ty) {
            for index in indices.iter().filter_map(variable) {
                *uses.entry(index).or_insert(0) += 1;
            }
        } else if let Some(generic) = width_variable(ty) {
            *uses.entry(generic).or_insert(0) += 1;
        }
    }
    let variables: BTreeMap<_, _> = binding
        .generics
        .iter()
        .filter(|generic| {
            uses.get(generic.name.as_str()).is_some_and(|count| {
                *count > 1 || path_is(&generic.ty, &["dialects", "fp", "model", "Format"])
            })
        })
        .map(|generic| (generic.name.as_str(), generic))
        .collect();
    let slot = |ty| {
        if enabled("widened") {
            Ok("_".to_owned())
        } else {
            slot_type(ty, &variables)
        }
    };
    let operand_types = parameters
        .iter()
        .map(|operand| slot(&operand.ty))
        .collect::<Result<Vec<_>, _>>()?;
    let operands = parameters
        .iter()
        .zip(&operand_types)
        .map(|(operand, ty)| (operand.name.as_str(), ty.as_str()))
        .collect::<Vec<_>>();
    let result_type = match &result.value {
        Expression::Unit => None,
        _ => Some(slot(result)?),
    };
    let quantified = variables
        .values()
        .map(|generic| {
            let name = generic.name.to_ascii_uppercase();
            if path_is(&generic.ty, &["Nat"]) || path_is(&generic.ty, &["std", "nat", "Nat"]) {
                Ok(format!("{name}: nat"))
            } else if path_is(&generic.ty, &["dialects", "fp", "model", "Format"]) {
                Ok(format!(
                    "{name}: float<5, 10> | float<8, 23> | float<11, 52>"
                ))
            } else if path_is(&generic.ty, &["dialects", "fp", "model", "Rounding"])
                || path_is(&generic.ty, &["dialects", "fp", "model", "Contraction"])
            {
                Ok(format!("{name}: sym"))
            } else if path_is(&generic.ty, &["Type"]) {
                Ok(name)
            } else {
                Err("unsupported operation binder")
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut scheme = String::new();
    if !quantified.is_empty() && !enabled("widened") {
        write!(scheme, "forall {}. ", quantified.join(", "))?;
    }
    write!(
        scheme,
        "({}) -> ({})",
        operand_types.join(", "),
        result_type.as_deref().unwrap_or("")
    )?;

    let attributes = attributes(program, module, fields, option("attributes"))?;
    let region = fields.iter().find_map(|field| match &field.value {
        OpField::Field { name, value } if name == "regions" => Some(value),
        _ => None,
    });
    let region = match region.map(|expr| &expr.value) {
        Some(Expression::Record(regions)) => {
            let [(name, region)] = regions.as_slice() else {
                return Err("expected one region in the operation signature".into());
            };
            if !matches!(&region.value, Expression::Generic { value, arguments }
                if path_is(value, &["std", "ir", "Region"])
                    && arguments.first().is_some_and(|kind| path_is(kind, &["std", "ir", "Cfg"])))
            {
                return Err("expected a CFG region".into());
            }
            Some(name.as_str())
        }
        Some(_) => return Err("operation regions must be a record".into()),
        None => None,
    };

    if matches!(result.value, Expression::Tuple(_)) || results.len() > 1 {
        return Err("expected one fixed result".into());
    }
    let entries = fields
        .iter()
        .find_map(|field| match &field.value {
            OpField::Interfaces { entries, .. } => Some(entries),
            _ => None,
        })
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut interfaces = Vec::new();
    for (name, path, methods) in entries {
        let Expression::Record(methods) = &methods.value else {
            return Err("expected an interface method record".into());
        };
        let canonical = path.last().ok_or("interface has no declaration name")?;
        if let Some(native) = rust_trait(program, path)? {
            let implementation = if inherited.contains(&native) {
                None
            } else {
                Some(native_methods(native, methods)?)
            };
            interfaces.push((native, format!("tir::{native}"), implementation));
        } else if methods.is_empty() {
            interfaces.push((
                name.as_str(),
                format!("super::__cdsl_values::r#{canonical}"),
                Some(String::new()),
            ));
        } else {
            return Err("generated interface methods require Rust lowering".into());
        }
    }
    for interface in inherited {
        if !interfaces.iter().any(|(name, _, _)| *name == interface) {
            interfaces.push((interface, format!("tir::{interface}"), None));
        }
    }

    let rust_name = operation_name(name);
    let signature = super::emit_scheme(&scheme)?;
    let generated = emit_fixed_result_op(
        (dialect, name, &rust_name),
        (&scheme, &signature),
        &operands,
        result_type
            .as_deref()
            .map(|ty| (results.first().map_or("result", String::as_str), ty)),
        &attributes,
        &interfaces,
        (region, enabled("format")),
    )?;
    writeln!(output, "mod __cdsl_{name} {{\n{generated}}}")?;
    writeln!(
        output,
        "pub use __cdsl_{name}::{{{name} as r#{export}, {rust_name}, {rust_name}Builder}};"
    )?;
    Ok(())
}

pub(super) fn rust_trait<'a>(
    program: &'a Program,
    path: &[String],
) -> Result<Option<&'a str>, Error> {
    let (name, module) = path
        .split_last()
        .ok_or_else(|| Error("interface has no declaration name".into()))?;
    let bindings: Path = module
        .iter()
        .cloned()
        .chain(["rust_interfaces".into()])
        .collect();
    let Some(Declaration::Let { binding, .. }) = program.find_rust_export(&bindings) else {
        return Ok(None);
    };
    let Expression::Record(fields) = &binding.value.value else {
        return Err(Error("Rust interface bindings must be a record".into()));
    };
    let Some((_, bound)) = fields.iter().find(|(interface, _)| interface == name) else {
        return Ok(None);
    };
    let Expression::Literal(Literal::String(rust_name)) = &bound.value else {
        return Err(Error(format!(
            "Rust interface binding for {name} must name a trait"
        )));
    };
    let mut chars = rust_name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(Error(format!(
            "invalid Rust trait identifier {rust_name:?}"
        )));
    }
    Ok(Some(rust_name))
}

fn native_methods(interface: &str, methods: &[(String, Expr)]) -> Result<String, &'static str> {
    let unsupported = "interface methods require a native implementation";
    let mut implementation = String::new();
    for (method, expression) in methods {
        let Expression::Function(function) = &expression.value else {
            return Err(unsupported);
        };
        if !function.parameters.is_empty() || !function.body.bindings.is_empty() {
            return Err(unsupported);
        }
        let body = &function.body.value.value;
        match ((interface, method.as_str()), body) {
            (
                ("Commutative", "is_commutative") | ("Speculatable", "is_speculatable"),
                Expression::Literal(Literal::Bool(true)),
            ) => {}
            (("OpCost", "cost"), Expression::Call { value, arguments })
                if type_argument(value, &["std", "bits", "literal"]).is_some_and(|width| {
                    matches!(&width.value, Expression::Literal(Literal::Natural(width)) if width == "32")
                }) =>
            {
                let [argument] = arguments.as_slice() else {
                    return Err(unsupported);
                };
                let Expression::Literal(Literal::Natural(cost)) = &argument.value else {
                    return Err(unsupported);
                };
                let cost = cost.parse::<u32>().map_err(|_| "operation cost exceeds u32")?;
                implementation = format!("fn cost(&self) -> u32 {{ {cost} }}");
            }
            _ => return Err(unsupported),
        }
    }
    Ok(implementation)
}

fn attributes<'a>(
    program: &'a Program,
    module: &[String],
    fields: &'a [Node<OpField>],
    host_attributes: Option<&'a Expr>,
) -> Result<Vec<(&'a str, &'static str)>, &'static str> {
    let Some(attrs) = host_attributes.or_else(|| {
        fields.iter().find_map(|field| match &field.value {
            OpField::Field { name, value } if name == "attributes" => Some(value),
            _ => None,
        })
    }) else {
        return Ok(Vec::new());
    };
    let attrs = match &attrs.value {
        Expression::Path(path) => {
            let target: Path = module.iter().chain(path).cloned().collect();
            match program
                .find_rust_export(path)
                .or_else(|| program.find_rust_export(&target))
            {
                Some(Declaration::Let { binding, .. }) => &binding.value,
                _ => return Err("missing named attribute definition"),
            }
        }
        _ => attrs,
    };
    let Expression::Record(attrs) = &attrs.value else {
        return Err("operation attributes must be a record");
    };
    attrs
        .iter()
        .map(|(name, ty)| match &ty.value {
            Expression::Generic { value, arguments }
                if path_is(value, &["std", "option", "Option"])
                    && matches!(arguments.as_slice(), [ty] if path_is(ty, &["Bool"])) =>
            {
                Ok((name.as_str(), "?Flag"))
            }
            Expression::Path(path) if path == &["Int"] => Ok((name.as_str(), "Int")),
            Expression::Path(path) if path == &["String"] => Ok((name.as_str(), "Str")),
            Expression::Path(path)
                if path.last().is_some_and(|name| name == "IntegerPredicate") =>
            {
                Ok((name.as_str(), "Predicate"))
            }
            Expression::Path(path) if path == &["dialects", "fp", "FloatPredicate"] => {
                Ok((name.as_str(), "FloatPredicate"))
            }
            _ => Err("unsupported operation attribute type"),
        })
        .collect()
}

fn operation_name(name: &str) -> String {
    let (stem, suffix) = ["si", "ui", "i"]
        .into_iter()
        .find_map(|suffix| name.strip_suffix(suffix).map(|stem| (stem, suffix)))
        .unwrap_or((name, ""));
    let mut rust_name = String::new();
    for word in stem.split('_') {
        if word == "xor" {
            rust_name.push_str("XOr");
        } else {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                rust_name.extend(first.to_uppercase());
                rust_name.push_str(chars.as_str());
            }
        }
    }
    rust_name.push_str(&suffix.to_ascii_uppercase());
    rust_name.push_str("Op");
    rust_name
}

fn path_is(expression: &Expr, path: &[&str]) -> bool {
    matches!(&expression.value, Expression::Path(actual) if actual == path)
}

fn width_variable(expression: &Expr) -> Option<&str> {
    let expression = type_argument(expression, &["std", "ir", "El"]).unwrap_or(expression);
    let index = type_argument(expression, &["std", "ir", "Bits"]).unwrap_or(expression);
    variable(index)
}

fn variable(index: &Expr) -> Option<&str> {
    if let Expression::Path(path) = &index.value
        && let [variable] = path.as_slice()
    {
        Some(variable)
    } else {
        None
    }
}

fn fp_slot(expression: &Expr) -> Option<(&str, &[Expr])> {
    let Expression::Generic { value, arguments } = &expression.value else {
        return None;
    };
    let Expression::Path(path) = &value.value else {
        return None;
    };
    match path.as_slice() {
        [dialects, fp, model, name]
            if dialects == "dialects"
                && fp == "fp"
                && model == "model"
                && matches!(name.as_str(), "Float" | "Computation") =>
        {
            Some((name, arguments))
        }
        _ => None,
    }
}

fn slot_type(
    expression: &Expr,
    variables: &BTreeMap<&str, &crate::ast::Binder>,
) -> Result<String, &'static str> {
    let expression = type_argument(expression, &["std", "ir", "El"]).unwrap_or(expression);
    if let Some((family, indices)) = fp_slot(expression) {
        let index = |expr| {
            variable(expr)
                .map(|name| {
                    if variables.contains_key(name) {
                        name.to_ascii_uppercase()
                    } else {
                        "_".into()
                    }
                })
                .ok_or("unsupported index in an FP operation signature")
        };
        return match (family, indices) {
            ("Float", [format]) => index(format),
            ("Computation", [format, rounding, contraction]) => Ok(format!(
                "fp.computation<{}, {}, {}>",
                index(format)?,
                index(rounding)?,
                index(contraction)?
            )),
            _ => Err("unsupported FP operation signature slot"),
        };
    }
    if let Some(index) = type_argument(expression, &["std", "ir", "Bits"]) {
        let width = match &index.value {
            Expression::Literal(Literal::Natural(width)) => width.clone(),
            Expression::Path(path) if path.len() == 1 => variable_spelling(path, variables)?,
            _ => return Err("unsupported bit width in an operation signature"),
        };
        return Ok(format!("int<{width}>"));
    }
    match &expression.value {
        Expression::Generic { value, .. } if path_is(value, &["std", "ir", "Ptr"]) => {
            Ok("ptr.p<_>".into())
        }
        Expression::Generic { value, .. } if path_is(value, &["std", "ir", "Function"]) => {
            Ok("fn<_, _>".into())
        }
        Expression::Path(path) if path.len() == 1 => variable_spelling(path, variables),
        _ => Err("unsupported operation signature slot"),
    }
}

fn variable_spelling(
    path: &[String],
    variables: &BTreeMap<&str, &crate::ast::Binder>,
) -> Result<String, &'static str> {
    let [name] = path else {
        return Err("expected a signature variable");
    };
    Ok(if variables.contains_key(name.as_str()) {
        name.to_ascii_uppercase()
    } else {
        "_".into()
    })
}

fn type_argument<'a>(expression: &'a Expr, constructor: &[&str]) -> Option<&'a Expr> {
    let Expression::Generic { value, arguments } = &expression.value else {
        return None;
    };
    match arguments.as_slice() {
        [argument] if path_is(value, constructor) => Some(argument),
        _ => None,
    }
}

fn emit_fixed_result_op(
    (dialect, name, rust_name): (&str, &str, &str),
    (scheme, signature): (&str, &str),
    operands: &[(&str, &str)],
    result: Option<(&str, &str)>,
    attribute_specs: &[(&str, &str)],
    interfaces: &[(&str, String, Option<String>)],
    (region, custom_format): (Option<&str>, bool),
) -> Result<String, std::fmt::Error> {
    let mut operand_schema = String::new();
    let mut fields = String::new();
    let mut defaults = String::new();
    let mut setters = String::new();
    let mut operands_to_build = String::new();
    let mut params = String::new();
    let mut params_to_build = String::new();
    let mut operand_parser = String::new();
    for (index, &(operand, ty)) in operands.iter().enumerate() {
        writeln!(
            operand_schema,
            "FieldSchema {{ name: {operand:?}, ty: {ty:?}, variadic: false }},"
        )?;
        writeln!(fields, "{operand}: Option<ValueId>,")?;
        writeln!(defaults, "{operand}: None,")?;
        writeln!(
            setters,
            "pub fn {operand}(mut self, value: ValueId) -> Self {{ self.{operand} = Some(value); self }}"
        )?;
        writeln!(operands_to_build, "self.{operand},")?;
        writeln!(params, "{operand}: ValueId,")?;
        write!(params_to_build, ".{operand}({operand})")?;
        if index > 0 {
            writeln!(operand_parser, "parser.parse_token(\",\");")?;
        }
        writeln!(
            operand_parser,
            "if let Some(reference) = parser.parse_value_ref() {{ builder = builder.{operand}(parser.resolve_value(context, reference)); }}"
        )?;
    }

    let mut attributes = String::new();
    let mut getters = String::new();
    let mut required_attributes = String::new();
    for &(attribute, kind) in attribute_specs {
        let (variant, setter_ty, getter_ty, read, write, vocabulary) = match kind {
            "?Flag" => ("Bool", "bool", "bool", "value", "value", "&[]"),
            "Int" => ("Int", "i64", "i64", "value", "value", "&[]"),
            "Str" => (
                "Str",
                "impl Into<Box<str>>",
                "String",
                "value.to_string()",
                "value.into()",
                "&[]",
            ),
            "Predicate" => (
                "Predicate",
                "tir::attributes::Predicate",
                "tir::attributes::Predicate",
                "value",
                "value",
                "tir::attributes::Predicate::INTEGER",
            ),
            "FloatPredicate" => (
                "Predicate",
                "tir::attributes::Predicate",
                "tir::attributes::Predicate",
                "value",
                "value",
                "tir::attributes::Predicate::FLOAT",
            ),
            _ => return Err(std::fmt::Error),
        };
        let kind = if kind == "FloatPredicate" {
            "Predicate"
        } else {
            kind
        };
        writeln!(
            attributes,
            "AttrSchema {{ name: {attribute:?}, ty: {kind:?}, vocabulary: {vocabulary} }},"
        )?;
        writeln!(
            setters,
            "pub fn {attribute}(self, value: {setter_ty}) -> Self {{ self.attr({attribute:?}, AttributeValue::{variant}({write})) }}"
        )?;
        if kind.starts_with('?') {
            writeln!(
                getters,
                "pub fn {attribute}(&self) -> Option<{getter_ty}> {{ match self.0.attr({attribute:?}) {{ Some(AttributeValue::{variant}(value)) => Some({read}), _ => None }} }}"
            )?;
        } else {
            let missing = format!("{dialect}.{name} must carry a {kind} attribute '{attribute}'");
            writeln!(
                getters,
                "pub fn {attribute}(&self) -> {getter_ty} {{ match self.0.attr({attribute:?}) {{ Some(AttributeValue::{variant}(value)) => {read}, _ => panic!({missing:?}) }} }}"
            )?;
            writeln!(
                required_attributes,
                "assert!(self.parts.attributes.iter().any(|attr| Some(attr.name) == self.context.sym({attribute:?})), \"Missing required attribute: {attribute}\");"
            )?;
            writeln!(params, "{attribute}: impl Into<AttributeValue>,")?;
            write!(params_to_build, ".attr({attribute:?}, {attribute}.into())")?;
        }
    }

    let mut interface_names = String::new();
    let mut implementations = String::new();
    let mut registrations = String::new();
    for (interface, rust_path, implementation) in interfaces {
        writeln!(interface_names, "{interface:?},")?;
        if let Some(body) = implementation {
            writeln!(
                implementations,
                "impl {rust_path} for {rust_name} {{{body}}}"
            )?;
        }
        writeln!(
            implementations,
            "impl tir::ImplementsOpInterface<dyn {rust_path}> for {rust_name} {{ fn into_interface(self: Box<Self>) -> Box<dyn {rust_path}> {{ self }} }}"
        )?;
        writeln!(
            registrations,
            "context.register_operation_interface::<Self, dyn {rust_path}>();"
        )?;
    }
    if !registrations.is_empty() {
        registrations = format!("fn register_interfaces(context: &Context) {{ {registrations} }}");
    }

    let result_schema = result.map_or_else(String::new, |(name, ty)| {
        format!("FieldSchema {{ name: {name:?}, ty: {ty:?}, variadic: false }}")
    });
    let mut result_fragments = [
        ("@result_getter@", ""),
        ("@result_field@", ""),
        ("@result_default@", ""),
        ("@result_setter@", ""),
        ("@parse_result@", ""),
        ("@build_results@", "let results = Vec::new();"),
        ("@result_parameter@", ""),
        ("@result_argument@", ""),
    ];
    if result.is_some() {
        result_fragments = [
            (
                "@result_getter@",
                "pub fn result(&self) -> ValueId { self.0.results()[0] }",
            ),
            ("@result_field@", "result_type: Option<tir::TypeId>,"),
            ("@result_default@", "result_type: None,"),
            (
                "@result_setter@",
                "pub fn result_type(mut self, ty: tir::TypeId) -> Self { self.result_type = Some(ty); self }",
            ),
            (
                "@parse_result@",
                "if !parser.parse_token(\":\") { return Err((parser.span(), Error::ExpectedToken(\":\"))); } let ty = parser.parse_type(context)?.ok_or_else(|| (parser.span(), Error::ExpectedType))?; builder = builder.result_type(ty);",
            ),
            (
                "@build_results@",
                "let ty = self.result_type.expect(\"result_type must be set for ops with results\"); let results = vec![self.context.create_value(ty, None).id()];",
            ),
            ("@result_parameter@", "result_type: tir::TypeId,"),
            ("@result_argument@", ".result_type(result_type)"),
        ];
    }
    let printer = if custom_format {
        "Self::custom_print(self, formatter)"
    } else {
        "tir::region_format::print_generic(formatter, &self.0, \"@spelled@\")"
    };
    let parser = if custom_format {
        "Self::custom_parse(parser, context)"
    } else {
        r#"let mut builder = @RustName@Builder::new(context);
           @operand_parser@
           let attributes = parse_optional_attributes(parser, context)?;
           for field in SCHEMA.attributes.iter().filter(|field| !field.ty.starts_with('?')) {
               if !attributes.iter().any(|attribute| Some(attribute.name) == context.sym(field.name)) {
                   let error = Error::VerificationError(format!("@spelled@ missing required attribute '{}'", field.name));
                   return Err((parser.span(), error));
               }
           }
           @parse_result@
           @parse_region@
           builder.parts.attributes.extend(attributes);
           Ok(Box::new(builder.build()))"#
    };
    let region_schema = if region.is_some() {
        "(tir::RegionKind::Blocks, false)"
    } else {
        ""
    };
    let mut region_field = String::new();
    let mut region_default = String::new();
    let mut region_arguments = String::new();
    let mut region_setter = String::new();
    let mut region_getter = String::new();
    let mut region_parser = String::new();
    let mut region_builder = String::from("let regions = Vec::new();");
    let mut region_parameter = String::new();
    let mut builder_binding = "";
    if let Some(region) = region {
        write!(region_field, "{region}: Option<tir::RegionId>,")?;
        write!(region_default, "{region}: None,")?;
        write!(region_parameter, "{region}: Option<tir::RegionId>,")?;
        write!(
            region_arguments,
            "; if let Some(id) = {region} {{ return builder.{region}(id); }} builder"
        )?;
        builder_binding = "let builder = ";
        write!(
            region_setter,
            "pub fn {region}(mut self, id: tir::RegionId) -> Self {{ self.{region} = Some(id); self }}"
        )?;
        write!(
            region_getter,
            "pub fn {region}(&self) -> tir::BlockHandle {{ self.regions().next().unwrap().iter(self.0.context.clone()).next().unwrap() }}"
        )?;
        write!(
            region_parser,
            "builder = builder.{region}(parser.parse_region(context)?.id());"
        )?;
        region_builder = format!(
            "let regions = vec![self.{region}.unwrap_or_else(|| {{ let region = self.context.create_region(); let block = self.context.create_block(vec![]); region.add_block(block.id()); region.id() }})];"
        );
    }
    let spelled = if dialect == "builtin" {
        name.to_owned()
    } else {
        format!("{dialect}.{name}")
    };
    let fragments = [
        ("@parse_body@", parser),
        ("@print_body@", printer),
        ("@spelled@", &spelled),
        ("@dialect@", dialect),
        ("@RustName@", rust_name),
        ("@name@", name),
        ("@scheme@", &format!("{scheme:?}")),
        ("@region_schema@", region_schema),
        ("@signature@", signature),
        ("@operands@", &operand_schema),
        ("@result@", &result_schema),
        ("@attributes@", &attributes),
        ("@required_attributes@", &required_attributes),
        ("@interfaces@", &interface_names),
        ("@implementations@", &implementations),
        ("@registrations@", &registrations),
        ("@fields@", &fields),
        ("@defaults@", &defaults),
        ("@setters@", &setters),
        ("@getters@", &getters),
        ("@operands_to_build@", &operands_to_build),
        ("@params@", &params),
        ("@params_to_build@", &params_to_build),
        ("@operand_parser@", &operand_parser),
        ("@operand_count@", &operands.len().to_string()),
        ("@region_field@", &region_field),
        ("@region_default@", &region_default),
        ("@region_parameter@", &region_parameter),
        ("@region_arguments@", &region_arguments),
        ("@builder_binding@", builder_binding),
        ("@region_setter@", &region_setter),
        ("@region_getter@", &region_getter),
        ("@parse_region@", &region_parser),
        ("@region_builder@", &region_builder),
    ];
    let mut source = include_str!("fixed-result-operation.rs.in").to_owned();
    for (marker, fragment) in fragments.into_iter().chain(result_fragments) {
        source = source.replace(marker, fragment);
    }
    Ok(source)
}
