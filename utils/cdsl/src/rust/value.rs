use std::{cell::RefCell, collections::BTreeSet, fmt::Write};

use crate::{
    Error, Program,
    ast::{
        BinaryOp, Binder, Binding, Declaration, Expr, Expression, Function, Literal, Path, Pattern,
        PatternKind, UnaryOp, Variant,
    },
    totality::free_paths,
};

pub(super) fn emit(
    program: &Program,
    output: &mut String,
    name: &str,
    target: &[String],
    declaration: &Declaration,
) -> Result<BTreeSet<Path>, Error> {
    let (_, module) = target
        .split_last()
        .ok_or_else(|| Error("missing declaration path".into()))?;
    let scope = Scope {
        program,
        target,
        module,
        dependencies: RefCell::new(BTreeSet::new()),
    };
    scope
        .emit(output, name, declaration)
        .map_err(|error| Error(error.to_string()))?;
    Ok(scope.dependencies.into_inner())
}

pub(super) fn interface_methods(
    program: &Program,
    module: &[String],
    methods: &[(String, Expr)],
    generics: &[Binder],
) -> Result<(String, BTreeSet<Path>)> {
    let scope = Scope {
        program,
        target: &[],
        module,
        dependencies: RefCell::new(BTreeSet::new()),
    };
    let parameters = generics
        .iter()
        .filter(|binder| path_is(&binder.ty, &["Type"]))
        .map(|binder| format!("r#{}", binder.name))
        .collect::<Vec<_>>();
    let mut output = String::new();
    for (name, method) in methods {
        let Expression::Function(function) = &method.value else {
            return Err("interface implementation must be a function".into());
        };
        scope.method(&mut output, name, function, &parameters)?;
    }
    Ok((output, scope.dependencies.into_inner()))
}

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

const WITNESS_PREFIX: &str = "__cdsl_runtime_index_";

struct Scope<'a> {
    program: &'a Program,
    target: &'a [String],
    module: &'a [String],
    dependencies: RefCell<BTreeSet<Path>>,
}

impl Scope<'_> {
    fn emit(&self, output: &mut String, name: &str, declaration: &Declaration) -> Result<()> {
        match declaration {
            Declaration::Enum {
                generics, variants, ..
            } => {
                let parameters = self.type_parameters(generics)?;
                if is_fieldless(variants) {
                    output.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n");
                }
                writeln!(output, "pub enum r#{name}{} {{", angles(&parameters))?;
                for variant in variants {
                    if !self.type_parameters(&variant.generics)?.is_empty()
                        || (!parameters.is_empty() && variant.ty.is_some())
                    {
                        return Err("enum variant refines a Rust type parameter".into());
                    }
                    write!(output, "    r#{}", variant.name)?;
                    if let Some(arguments) = &variant.arguments {
                        let payload = arguments
                            .iter()
                            .map(|argument| {
                                let ty = self.ty(argument, &parameters)?;
                                Ok(if is_recursive_field(argument, self.target) {
                                    format!("Box<{ty}>")
                                } else {
                                    ty
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        write!(output, "({})", payload.join(", "))?;
                    }
                    writeln!(output, ",")?;
                }
                writeln!(output, "}}")?;
            }
            Declaration::Interface { binding, .. } => {
                let parameters = self.type_parameters(&binding.generics)?;
                let Expression::Record(methods) = &binding.value.value else {
                    return Err("interface contract must be a record".into());
                };
                writeln!(output, "pub trait r#{name}{} {{", angles(&parameters))?;
                for (method, signature) in methods {
                    if let Expression::Function(function) = &signature.value {
                        self.method(output, method, function, &parameters)?;
                        continue;
                    }
                    let Expression::Binary {
                        op: BinaryOp::Arrow,
                        left,
                        right,
                    } = &signature.value
                    else {
                        return Err(format!("interface method {method} needs a signature").into());
                    };
                    let arguments = self.types(arrow_parameters(left), &parameters)?;
                    let result = self.ty(right, &parameters)?;
                    write!(output, "    fn r#{method}(&self")?;
                    for (index, argument) in arguments.iter().enumerate() {
                        write!(output, ", arg{index}: {argument}")?;
                    }
                    writeln!(output, ") -> {result};")?;
                }
                writeln!(output, "}}")?;
            }
            Declaration::Let { binding, .. } => self.emit_binding(output, name, binding)?,
            _ => return Err("unsupported Rust export declaration".into()),
        }
        Ok(())
    }

    fn method(
        &self,
        output: &mut String,
        name: &str,
        function: &Function,
        parameters: &[String],
    ) -> Result<()> {
        if !function.generics.is_empty() || !function.body.bindings.is_empty() {
            return Err("unsupported Rust interface method definition".into());
        }
        let result = self.ty(
            function
                .result
                .as_ref()
                .ok_or("interface method needs a result type")?,
            parameters,
        )?;
        write!(output, "fn r#{name}(&self")?;
        for parameter in &function.parameters {
            let ty = self.ty(&parameter.ty, parameters)?;
            write!(output, ", r#{}: {ty}", parameter.name)?;
        }
        let body = self.expression(&function.body.value, &[])?;
        writeln!(output, ") -> {result} {{ {body} }}")?;
        Ok(())
    }

    fn emit_binding(&self, output: &mut String, name: &str, binding: &Binding) -> Result<()> {
        let parameters = self.type_parameters(&binding.generics)?;
        match &binding.value.value {
            Expression::Record(fields) => {
                if fields.iter().all(|(_, ty)| {
                    matches!(
                        ty.value,
                        Expression::Binary {
                            op: BinaryOp::Arrow,
                            ..
                        }
                    )
                }) {
                    output.push_str("#[derive(Clone, Copy)]\n");
                }
                writeln!(output, "pub struct r#{name}{} {{", angles(&parameters))?;
                for (field, ty) in fields {
                    let ty = self.ty(ty, &parameters)?;
                    writeln!(output, "    pub r#{field}: {ty},")?;
                }
                writeln!(output, "}}")?;
            }
            Expression::Generic { value, arguments } if path_is(value, &["std", "ir", "Bits"]) => {
                let [width] = arguments.as_slice() else {
                    return Err("Bits requires one width".into());
                };
                if !parameters.is_empty() {
                    return Err("unsupported Rust type parameter for Bits".into());
                }
                write!(
                    output,
                    "#[allow(non_snake_case)]\npub fn r#{name}(context: &crate::Context"
                )?;
                for binder in &binding.generics {
                    write!(output, ", r#{}: u64", binder.name)?;
                }
                let width = self.expression(width, &[])?;
                writeln!(
                    output,
                    ") -> crate::TypeId {{ context.terms().int({width}) }}"
                )?;
            }
            Expression::Generic { value, arguments }
                if path_is(value, &["std", "ir", "State"]) && binding.generics.is_empty() =>
            {
                let [resource] = arguments.as_slice() else {
                    return Err("State requires one resource".into());
                };
                let Expression::Generic { value, arguments } = &resource.value else {
                    return Err("expected a State resource constructor".into());
                };
                let [resource] = arguments.as_slice() else {
                    return Err("expected one name for the State resource".into());
                };
                let Expression::Literal(Literal::String(resource)) = &resource.value else {
                    return Err("expected a literal State resource name".into());
                };
                if !path_is(value, &["std", "ir", "ExclusiveResource"]) {
                    return Err("unsupported State resource constructor".into());
                }
                writeln!(
                    output,
                    "#[allow(non_snake_case)]\npub fn r#{name}(context: &crate::Context) -> Result<crate::TypeId, String> {{"
                )?;
                output.push_str("    let terms = context.terms();\n");
                output.push_str("    let ctor = terms.ctor_named(\"builtin\", \"state\")\n");
                output.push_str("        .ok_or(\"missing state type constructor\")?;\n");
                writeln!(
                    output,
                    "    let resource = crate::TypeArg::Sym(terms.name({resource:?}));"
                )?;
                output.push_str("    terms.intern(ctor, vec![resource])\n}\n");
            }
            Expression::Function(function)
                if function.generics.is_empty() && function.body.bindings.is_empty() =>
            {
                let result = function
                    .result
                    .as_ref()
                    .ok_or("function requires a result type")?;
                let result = self.ty(result, &parameters)?;
                let indices = runtime_indices(binding, function)?;
                if !indices.is_empty()
                    && binding
                        .generics
                        .iter()
                        .chain(&function.parameters)
                        .any(|binder| binder.name.starts_with(WITNESS_PREFIX))
                {
                    return Err("Rust function binder uses the runtime index namespace".into());
                }
                let body = self.expression(&function.body.value, &indices)?;
                let arguments = indices
                    .iter()
                    .copied()
                    .chain(&function.parameters)
                    .map(|param| {
                        let ty = self.ty(&param.ty, &parameters)?;
                        Ok(format!("r#{}: {ty}", param.name))
                    })
                    .collect::<Result<Vec<_>>>()?;
                write!(
                    output,
                    "pub fn r#{name}{}({}) -> {result} {{ ",
                    angles(&parameters),
                    arguments.join(", ")
                )?;
                // A pattern can shadow the CDSL index name inside a recursive arm.
                for (index, binder) in indices.iter().enumerate() {
                    write!(output, "let {WITNESS_PREFIX}{index} = r#{}; ", binder.name)?;
                }
                writeln!(output, "{body} }}")?;
            }
            _ if binding.ty.as_ref().is_some_and(|ty| path_is(ty, &["Type"])) => {
                let alias = self.ty(&binding.value, &parameters)?;
                writeln!(
                    output,
                    "pub type r#{name}{} = {alias};",
                    angles(&parameters)
                )?;
            }
            Expression::Literal(literal) if binding.generics.is_empty() => {
                let (ty, value) = match literal {
                    Literal::Bool(value) => ("bool", value.to_string()),
                    Literal::String(value) => ("&str", format!("{value:?}")),
                    Literal::Natural(value) => {
                        let value = value.replace('_', "");
                        if binding.ty.as_ref().is_some_and(|ty| path_is(ty, &["Int"])) {
                            ("i64", value.parse::<i64>()?.to_string())
                        } else {
                            ("u64", value.parse::<u64>()?.to_string())
                        }
                    }
                };
                writeln!(
                    output,
                    "#[allow(non_upper_case_globals)]\npub const r#{name}: {ty} = {value};"
                )?;
            }
            _ => return Err("unsupported Rust export value".into()),
        }
        Ok(())
    }

    fn ty(&self, expression: &Expr, parameters: &[String]) -> Result<String> {
        if is_nat(expression) {
            return Ok("u64".into());
        }
        match &expression.value {
            Expression::Path(path) => match path.as_slice() {
                [name]
                    if parameters
                        .iter()
                        .any(|parameter| parameter.strip_prefix("r#") == Some(name)) =>
                {
                    Ok(format!("r#{name}"))
                }
                [name] if name == "Int" => Ok("i64".into()),
                [name] if name == "Bool" => Ok("bool".into()),
                [name] if name == "String" => Ok("String".into()),
                [std, ir, ty] if std == "std" && ir == "ir" && ty == "Ty" => {
                    Ok("::tir::TypeId".into())
                }
                [std, ir, ty]
                    if std == "std"
                        && ir == "ir"
                        && matches!(
                            ty.as_str(),
                            "BlockId"
                                | "RegionId"
                                | "ValueId"
                                | "Binding"
                                | "ResourceEffect"
                                | "ExitTarget"
                                | "ExitScopeKind"
                        ) =>
                {
                    Ok(format!("::tir::{ty}"))
                }
                [std, ir, ty] if std == "std" && ir == "ir" && ty == "OperandIndex" => {
                    Ok("usize".into())
                }
                [std, ir, ty] if std == "std" && ir == "ir" && ty == "OperandRange" => {
                    Ok("::std::ops::Range<usize>".into())
                }
                _ => self.named_type(path, &[], parameters),
            },
            Expression::Unit => Ok("()".into()),
            Expression::Tuple(fields) => {
                let fields = self.types(fields, parameters)?;
                Ok(format!("({},)", fields.join(", ")))
            }
            Expression::Binary {
                op: BinaryOp::Arrow,
                left,
                right,
            } => {
                let left = self.types(arrow_parameters(left), parameters)?;
                let right = self.ty(right, parameters)?;
                Ok(format!("fn({}) -> {right}", left.join(", ")))
            }
            Expression::Generic { value, arguments } => {
                if path_is(value, &["std", "ir", "El"])
                    && let [argument] = arguments.as_slice()
                    && let Expression::Generic { value, arguments } = &argument.value
                    && path_is(value, &["std", "ir", "Bits"])
                    && let [width] = arguments.as_slice()
                {
                    let width = match &width.value {
                        Expression::Literal(Literal::Natural(width)) => {
                            width.replace('_', "").parse::<u32>().ok()
                        }
                        _ => None,
                    };
                    let carrier = match width {
                        Some(8) => "u8",
                        Some(16) => "u16",
                        Some(32) => "u32",
                        Some(64) => "u64",
                        Some(128) => "u128",
                        _ => "::tir::utils::APInt",
                    };
                    return Ok(carrier.into());
                }
                let (container, element) = match arguments.as_slice() {
                    [element] if path_is(value, &["std", "option", "Option"]) => {
                        ("Option", element)
                    }
                    [element] if path_is(value, &["std", "list", "List"]) => ("Vec", element),
                    [element, _] if path_is(value, &["std", "ir", "Vec"]) => ("Vec", element),
                    _ => {
                        let Expression::Path(path) = &value.value else {
                            return Err("unsupported Rust type constructor expression".into());
                        };
                        return self.named_type(path, arguments, parameters);
                    }
                };
                Ok(format!("{container}<{}>", self.ty(element, parameters)?))
            }
            _ => Err("unsupported Rust data type expression".into()),
        }
    }

    fn types(&self, fields: &[Expr], parameters: &[String]) -> Result<Vec<String>> {
        fields
            .iter()
            .map(|field| self.ty(field, parameters))
            .collect()
    }

    fn named_type(
        &self,
        path: &[String],
        arguments: &[Expr],
        parameters: &[String],
    ) -> Result<String> {
        let owned;
        let path = if path.len() == 1 && !self.module.is_empty() {
            owned = self.module.iter().chain(path).cloned().collect::<Path>();
            owned.as_slice()
        } else {
            path
        };
        let name = path.last().ok_or("missing type name")?;
        let (Declaration::Enum { generics, .. }
        | Declaration::Let {
            binding: Binding { generics, .. },
            ..
        }) = self
            .program
            .find_rust_export(path)
            .ok_or_else(|| format!("missing Rust type {}", path.join("::")))?
        else {
            return Err(format!("unsupported Rust type {}", path.join("::")).into());
        };
        if generics.len() != arguments.len() {
            return Err(format!("wrong number of type arguments for {}", path.join("::")).into());
        }
        let mut types = Vec::new();
        for (binder, argument) in generics.iter().zip(arguments) {
            if path_is(&binder.ty, &["Type"]) {
                types.push(self.ty(argument, parameters)?);
            }
        }
        self.dependencies.borrow_mut().insert(path.to_vec());
        Ok(format!("r#{name}{}", angles(&types)))
    }

    fn type_parameters(&self, binders: &[Binder]) -> Result<Vec<String>> {
        // Checked value indices constrain CDSL values but carry no Rust type parameter.
        let mut parameters = Vec::new();
        for binder in binders {
            if path_is(&binder.ty, &["Type"]) {
                parameters.push(format!("r#{}", binder.name));
            } else if !is_nat(&binder.ty)
                && !matches!(&binder.ty.value, Expression::Path(path)
                    if matches!(self.program.find_rust_export(path),
                        Some(Declaration::Enum { generics, variants, .. })
                            if generics.is_empty() && is_fieldless(variants)))
            {
                return Err(format!("unsupported Rust parameter type for {}", binder.name).into());
            }
        }
        Ok(parameters)
    }

    fn expression(&self, expression: &Expr, indices: &[&Binder]) -> Result<String> {
        Ok(match &expression.value {
            Expression::Literal(Literal::Bool(value)) => value.to_string(),
            Expression::Literal(Literal::String(value)) => format!("String::from({value:?})"),
            Expression::Literal(Literal::Natural(value)) => {
                value.replace('_', "").parse::<u64>()?.to_string()
            }
            Expression::Path(path) if path == &["self"] => "self".into(),
            Expression::Path(path) => self.value_path(path)?,
            Expression::Function(function)
                if function.generics.is_empty() && function.body.bindings.is_empty() =>
            {
                let parameters = function
                    .parameters
                    .iter()
                    .map(|parameter| format!("r#{}", parameter.name))
                    .collect::<Vec<_>>();
                let body = self.expression(&function.body.value, indices)?;
                format!("|{}| {{ {body} }}", parameters.join(", "))
            }
            Expression::Unary {
                op: UnaryOp::Not,
                value,
            } => format!("(!{})", self.expression(value, indices)?),
            Expression::Binary { op, left, right } => {
                let op = match op {
                    BinaryOp::Or => "||",
                    BinaryOp::And => "&&",
                    BinaryOp::Equal => "==",
                    BinaryOp::NotEqual => "!=",
                    _ => return Err("unsupported Rust scalar operator".into()),
                };
                format!(
                    "({} {op} {})",
                    self.expression(left, indices)?,
                    self.expression(right, indices)?
                )
            }
            Expression::Unit => "()".into(),
            Expression::Generic { value, .. }
                if path_is(value, &["std", "list", "List", "Nil"]) =>
            {
                "Vec::new()".into()
            }
            Expression::Generic { value, .. } if path_is(value, &["std", "option", "none"]) => {
                "None".into()
            }
            Expression::Member { value, name } => {
                format!("{}.r#{name}", self.expression(value, indices)?)
            }
            Expression::Call { value, arguments } => self.call(value, arguments, indices)?,
            Expression::Match { value, arms } => {
                let value = self.expression(value, indices)?;
                let mut result = format!("match {value} {{\n");
                for (pattern, value) in arms {
                    let mut bindings = String::new();
                    let pattern = self.pattern(pattern, &mut bindings, !indices.is_empty())?;
                    let value = self.expression(value, indices)?;
                    writeln!(result, "{pattern} => {{ {bindings}{value} }},")?;
                }
                result.push('}');
                result
            }
            _ => return Err("unsupported Rust value expression".into()),
        })
    }

    fn value_path(&self, path: &[String]) -> Result<String> {
        if let [name] = path {
            Ok(format!("r#{name}"))
        } else {
            Ok(self.constructor(path)?.0)
        }
    }

    fn constructor<'path>(
        &self,
        path: &'path [String],
    ) -> Result<(String, &'path [String], &Variant)> {
        let resolve = || {
            let (name, owner) = path.split_last()?;
            let owner_name = owner.last()?;
            let Declaration::Enum { variants, .. } = self.program.find_rust_export(owner)? else {
                return None;
            };
            let variant = variants.iter().find(|variant| &variant.name == name)?;
            self.dependencies.borrow_mut().insert(owner.to_vec());
            Some((format!("r#{owner_name}::r#{name}"), owner, variant))
        };
        resolve().ok_or_else(|| format!("unsupported Rust constructor {}", path.join("::")).into())
    }

    fn call(&self, value: &Expr, arguments: &[Expr], indices: &[&Binder]) -> Result<String> {
        if let Expression::Generic { value, .. } = &value.value
            && path_is(value, &["std", "bits", "literal"])
            && let [argument] = arguments
        {
            return self.expression(argument, indices);
        }
        if let Some(call) = self.standard_call(value, arguments, indices)? {
            return Ok(call);
        }
        let mut args = Vec::new();
        let mut fields = None;
        let callee = match &value.value {
            Expression::Path(path) if path == self.target => {
                args.extend((0..indices.len()).map(|index| format!("{WITNESS_PREFIX}{index}")));
                let name = path.last().ok_or("missing Rust function name")?;
                format!("r#{name}")
            }
            Expression::Path(path) => {
                let (constructor, owner, variant) = self.constructor(path)?;
                fields = Some((owner, variant.arguments.as_deref().unwrap_or_default()));
                constructor
            }
            Expression::Member {
                value: receiver, ..
            } if path_is(receiver, &["self"]) => self.expression(value, indices)?,
            Expression::Member { .. } => format!("({})", self.expression(value, indices)?),
            _ => return Err("unsupported Rust function expression".into()),
        };
        for (index, argument) in arguments.iter().enumerate() {
            let argument = self.expression(argument, indices)?;
            let boxed = fields.is_some_and(|(owner, fields)| {
                fields
                    .get(index)
                    .is_some_and(|field| is_recursive_field(field, owner))
            });
            args.push(if boxed {
                format!("Box::new({argument})")
            } else {
                argument
            });
        }
        Ok(format!("{callee}({})", args.join(", ")))
    }

    fn standard_call(
        &self,
        value: &Expr,
        arguments: &[Expr],
        indices: &[&Binder],
    ) -> Result<Option<String>> {
        let render = |expression| self.expression(expression, indices);
        Ok(match arguments {
            [left, right] if path_is(value, &["std", "bits", "eq"]) => {
                Some(format!("{} == {}", render(left)?, render(right)?))
            }
            [list, test] if path_is(value, &["std", "list", "all_indexed"]) => Some(format!(
                "{{ let values = {}; let test = {}; values.into_iter().enumerate().all(|(index, value)| test(index as u64, value)) }}",
                render(list)?,
                render(test)?,
            )),
            [list, test] if path_is(value, &["std", "list", "first_index_or_length"]) => {
                Some(format!(
                    "{{ let values = {}; let len = values.len(); values.into_iter().position({}).unwrap_or(len) as u64 }}",
                    render(list)?,
                    render(test)?,
                ))
            }
            [index] if path_is(value, &["std", "ir", "operand_index"]) => Some(format!(
                "usize::try_from({}).expect(\"operand index fits usize\")",
                render(index)?
            )),
            _ => None,
        })
    }

    fn pattern(
        &self,
        pattern: &Pattern,
        bindings: &mut String,
        has_runtime_indices: bool,
    ) -> Result<String> {
        match &pattern.value {
            PatternKind::Wildcard => Ok("_".into()),
            PatternKind::Path(path) => {
                if has_runtime_indices
                    && matches!(path.as_slice(), [name] if name.starts_with(WITNESS_PREFIX))
                {
                    return Err("Rust pattern binder uses the runtime index namespace".into());
                }
                self.value_path(path)
            }
            PatternKind::Constructor { path, arguments } => {
                let (constructor, owner, variant) = self.constructor(path)?;
                let fields = variant.arguments.as_deref().unwrap_or_default();
                let arguments = arguments
                    .iter()
                    .zip(fields)
                    .map(|(pattern, field)| {
                        if is_recursive_field(field, owner) {
                            match &pattern.value {
                                PatternKind::Wildcard => {}
                                PatternKind::Path(path) if path.len() == 1 => {
                                    let name = &path[0];
                                    write!(bindings, "let r#{name} = *r#{name}; ")?;
                                }
                                _ => {
                                    return Err(
                                        "boxed Rust pattern requires a binding or wildcard".into(),
                                    );
                                }
                            }
                        }
                        self.pattern(pattern, bindings, has_runtime_indices)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(format!("{constructor}({})", arguments.join(", ")))
            }
            _ => Err("unsupported Rust pattern".into()),
        }
    }
}

fn is_fieldless(variants: &[Variant]) -> bool {
    variants.iter().all(|variant| variant.arguments.is_none())
}

fn is_recursive_field(field: &Expr, owner: &[String]) -> bool {
    let constructor = match &field.value {
        Expression::Generic { value, .. } => value,
        _ => field,
    };
    matches!(&constructor.value, Expression::Path(path) if path == owner)
}

fn arrow_parameters(expression: &Expr) -> &[Expr] {
    match &expression.value {
        Expression::Unit => &[],
        Expression::Tuple(fields) => fields,
        _ => std::slice::from_ref(expression),
    }
}

fn angles(parameters: &[String]) -> String {
    if parameters.is_empty() {
        String::new()
    } else {
        format!("<{}>", parameters.join(", "))
    }
}

fn path_is(expression: &Expr, path: &[&str]) -> bool {
    matches!(&expression.value, Expression::Path(actual) if actual == path)
}

fn is_nat(expression: &Expr) -> bool {
    path_is(expression, &["Nat"]) || path_is(expression, &["std", "nat", "Nat"])
}

fn runtime_indices<'a>(binding: &'a Binding, function: &Function) -> Result<Vec<&'a Binder>> {
    let free = free_paths(&function.body.value)?;
    Ok(binding
        .generics
        .iter()
        .filter(|binder| {
            !path_is(&binder.ty, &["Type"])
                && !function
                    .parameters
                    .iter()
                    .any(|parameter| parameter.name == binder.name)
                && free.contains(std::slice::from_ref(&binder.name))
        })
        .collect())
}
