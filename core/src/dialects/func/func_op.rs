use crate::Any;
use crate::builtin::UnitType;
use crate::operation;
use crate::symbol_table::{symbol_name_of, visibility_of};

use crate as tir;
use crate::{Callable, Context, Error, Operation, Symbol, Terminator, Visibility};

operation! {
    FuncOp {
        name: "func",
        dialect: "func",
        format: "custom",
        verifier: "true",
        interfaces: [Symbol, Callable],
        attributes: A {
            sym_name: "Str",
            ret_type: "Type",
        },
        results: R {
            result: "crate::builtin::FnType",
        },
        regions: R {
            body: Region {
                kind: Any,
            }
        }
    }
}

/// A λ definition named `name`, its `!fn` type read off the entry block of
/// `body` so the signature can never disagree with the parameters.
pub fn lambda(
    context: &Context,
    name: &str,
    ret_type: tir::TypeId,
    body: &tir::RegionHandle,
) -> FuncOpBuilder {
    let parameters: Vec<_> = body.ports().iter().map(tir::Value::ty).collect();
    FuncOpBuilder::new(context)
        .sym_name(name)
        .ret_type(ret_type)
        .result_type(tir::builtin::FnType::new(context, &parameters, ret_type))
        .body(body.id())
}

impl FuncOpBuilder {
    /// Declare `count` trailing body arguments that are supplied by the ABI and
    /// are absent from the public function type.
    pub fn implicit_arguments(mut self, count: usize) -> Self {
        if let Some(body) = self.body {
            let arguments = self.context.get_region(body).value_arguments();
            if count <= arguments.len() {
                self = self.attr(
                    "implicit_argument_types",
                    tir::attributes::AttributeValue::Array(
                        arguments[arguments.len() - count..]
                            .iter()
                            .map(|argument| tir::attributes::AttributeValue::Type(argument.ty()))
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                );
            }
        }
        self.attr(
            "implicit_arguments",
            tir::attributes::AttributeValue::UInt(count as u64),
        )
    }

    /// Mark an implicit pointer argument that receives the stack pointer at
    /// function entry, before any frame adjustment.
    pub fn entry_sp(mut self, argument: tir::ValueId) -> Self {
        if let Some(body) = self.body
            && let Some(index) = self
                .context
                .get_region(body)
                .value_arguments()
                .iter()
                .position(|value| value.id() == argument)
        {
            self = self.attr(
                "entry_sp_index",
                tir::attributes::AttributeValue::UInt(index as u64),
            );
        }
        self.attr("entry_sp", tir::attributes::AttributeValue::Value(argument))
    }

    pub fn result_address(self) -> Self {
        self.attr(
            "result_address",
            tir::attributes::AttributeValue::Bool(true),
        )
    }

    pub fn noalias(self, arguments: &[usize]) -> Self {
        self.attr(
            "noalias",
            tir::attributes::AttributeValue::Array(
                arguments
                    .iter()
                    .map(|&argument| tir::attributes::AttributeValue::UInt(argument as u64))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
    }

    pub fn argument_alignments(self, alignments: &[u64]) -> Self {
        self.attr(
            "argument_alignments",
            tir::attributes::AttributeValue::Array(
                alignments
                    .iter()
                    .copied()
                    .map(tir::attributes::AttributeValue::UInt)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
    }

    pub fn stack_arguments(self, arguments: &[usize]) -> Self {
        self.attr(
            "stack_arguments",
            tir::attributes::AttributeValue::Array(
                arguments
                    .iter()
                    .map(|&argument| tir::attributes::AttributeValue::UInt(argument as u64))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
    }
}

impl Callable for FuncOp {
    fn body(&self) -> Option<tir::RegionId> {
        Some(self.body_region().id())
    }

    fn value(&self) -> tir::ValueId {
        self.fn_value()
    }

    fn params(&self) -> Vec<tir::TypeId> {
        self.public_parameters()
            .iter()
            .map(tir::Value::ty)
            .collect()
    }

    fn result(&self) -> tir::TypeId {
        self.ret_type()
    }
}

impl FuncOp {
    /// The region holding the body, whichever kind it is. [`FuncOp::body`]
    /// answers with the entry block, which only an ordered body has.
    pub fn body_region(&self) -> tir::RegionHandle {
        use tir::Operation;
        self.regions().next().expect("a function owns its body")
    }

    /// The function's parameters: the body region's arguments.
    pub fn parameters(&self) -> Vec<tir::Value> {
        self.body_region().value_arguments()
    }

    pub fn implicit_argument_count(&self) -> usize {
        match self.attr("implicit_arguments") {
            Some(tir::attributes::AttributeValue::UInt(count)) => count as usize,
            _ => 0,
        }
    }

    /// The body arguments represented by the public function type.
    pub fn public_parameters(&self) -> Vec<tir::Value> {
        let parameters = self.parameters();
        let public_count = parameters
            .len()
            .saturating_sub(self.implicit_argument_count());
        parameters[..public_count].to_vec()
    }

    pub fn implicit_argument_types(&self) -> Vec<tir::TypeId> {
        match self.attr("implicit_argument_types") {
            Some(tir::attributes::AttributeValue::Array(types)) => types
                .iter()
                .filter_map(|ty| match ty {
                    tir::attributes::AttributeValue::Type(ty) => Some(*ty),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn entry_sp_argument(&self) -> Option<tir::ValueId> {
        let marked = match self.attr("entry_sp") {
            Some(tir::attributes::AttributeValue::Value(value)) => value,
            _ => return None,
        };
        let parameters = self.parameters();
        if parameters.iter().any(|argument| argument.id() == marked) {
            return Some(marked);
        }
        match self.attr("entry_sp_index") {
            Some(tir::attributes::AttributeValue::UInt(index)) => parameters
                .get(usize::try_from(index).ok()?)
                .map(tir::Value::id),
            _ => None,
        }
    }

    /// The λ value this definition produces: what a call to it takes as callee.
    pub fn fn_value(&self) -> tir::ValueId {
        self.result()
    }

    pub fn has_result_address(&self) -> bool {
        self.attr("result_address") == Some(tir::attributes::AttributeValue::Bool(true))
    }

    pub fn argument_alignments(&self) -> Vec<u64> {
        super::argument_alignments(self)
    }

    pub fn stack_arguments(&self) -> Vec<usize> {
        super::stack_arguments(self)
    }

    /// The parameters the caller guarantees name memory nothing else the
    /// function reaches names: a `restrict`-qualified pointer, by index.
    pub fn noalias_arguments(&self) -> Vec<usize> {
        super::noalias_arguments(self)
    }
}

impl Symbol for FuncOp {
    fn symbol_name(&self) -> String {
        symbol_name_of(self)
    }

    fn symbol_signature(&self) -> Option<Vec<tir::TypeId>> {
        tir::builtin::FnType::signature_of(&self.0.context, self.fn_value())
            .map(|(parameters, _)| parameters)
    }

    fn symbol_result_type(&self) -> Option<tir::TypeId> {
        Some(self.ret_type())
    }

    fn symbol_visibility(&self) -> Visibility {
        visibility_of(self)
    }

    fn is_definition(&self) -> bool {
        true
    }
}

impl FuncOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        use tir::Operation;

        fmt.write(format!("%{} = func.func", self.fn_value().number()))?;
        if self.symbol_visibility() == Visibility::Private {
            fmt.write(" private")?;
        }

        let sym_name = match self.attr("sym_name") {
            Some(tir::attributes::AttributeValue::Str(s)) => s.to_string(),
            Some(_) => panic!("sym_name must be a string"),
            None => "unknown".to_string(),
        };

        fmt.write(format!(" @{}", sym_name))?;

        let context = self.0.context.clone();
        let args = self.parameters();

        fmt.write("(")?;
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                fmt.write(", ")?;
            }
            fmt.write(format!("%{}: ", arg.id().number()))?;
            context.print_type(arg.ty(), fmt)?;
        }
        let variadic = tir::builtin::FnType::signature_of(&context, self.fn_value())
            .and_then(|(params, _)| params.last().copied())
            == Some(tir::builtin::VarArgsType::new(&context));
        if variadic {
            if !args.is_empty() {
                fmt.write(", ")?;
            }
            fmt.write("...")?;
        }
        fmt.write(")")?;

        let ret_type = self.ret_type();

        if ret_type != UnitType::new(&context) {
            fmt.write(" -> ")?;
            context.print_type(ret_type, fmt)?;
        }
        if self.has_result_address() {
            fmt.write(" result_address")?;
        }
        if self.implicit_argument_count() > 0 {
            fmt.write(format!(
                " implicit_arguments {}",
                self.implicit_argument_count()
            ))?;
        }
        if let Some(entry_sp) = self.entry_sp_argument() {
            fmt.write(format!(" entry_sp %{}", entry_sp.number()))?;
        }
        super::print_keyed_list(fmt, "argument_alignments", &self.argument_alignments())?;
        super::print_keyed_list(fmt, "stack_arguments", &self.stack_arguments())?;
        super::print_keyed_list(fmt, "noalias", &self.noalias_arguments())?;

        tir::region_format::print_op_region(fmt, &context, self, 0)?;

        Ok(())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        use tir::parse::common::Cursor;

        let is_private = parser.parse_token("private");

        let sym_name = parser
            .parse_symbol_name()
            .ok_or_else(|| (parser.span(), tir::Error::ExpectedSymbolName))?
            .to_string();

        let parsed_args = parser
            .parse_delimited("(", ")", |parser| {
                if parser.parse_token("...") {
                    return Ok(None);
                }
                let val_name = parser
                    .parse_value_ref()
                    .ok_or_else(|| (parser.span(), tir::Error::ExpectedValueRef))?
                    .to_string();

                if !parser.parse_token(":") {
                    return Err((parser.span(), tir::Error::ExpectedToken(":")));
                }

                let ty = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?;

                let value = context.create_value(ty, None);
                parser.define_value(&val_name, value.id());
                Ok(Some(value))
            })?
            .ok_or_else(|| (parser.span(), tir::Error::ExpectedToken("(")))?;
        let variadic = parsed_args.last().is_some_and(Option::is_none);
        if parsed_args
            .iter()
            .take(parsed_args.len().saturating_sub(1))
            .any(Option::is_none)
        {
            return Err((
                parser.span(),
                tir::Error::ExpectedToken("final variadic marker"),
            ));
        }
        let block_args: Vec<_> = parsed_args.into_iter().flatten().collect();

        let ret_type = if parser.parse_token("->") {
            parser
                .parse_type(context)?
                .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?
        } else {
            UnitType::new(context)
        };
        let result_address = parser.parse_token("result_address");
        let implicit_arguments = if parser.parse_token("implicit_arguments") {
            let count = parser.parse_number().ok_or_else(|| {
                (
                    parser.span(),
                    tir::Error::ExpectedToken("implicit argument count"),
                )
            })?;
            Some(u64::try_from(count).map_err(|_| {
                (
                    parser.span(),
                    tir::Error::ExpectedToken("nonnegative implicit argument count"),
                )
            })?)
        } else {
            None
        };
        let entry_sp = if parser.parse_token("entry_sp") {
            let name = parser
                .parse_value_ref()
                .ok_or_else(|| (parser.span(), tir::Error::ExpectedValueRef))?
                .to_string();
            Some(parser.resolve_value(context, &name))
        } else {
            None
        };
        let argument_alignments =
            super::parse_keyed_array(parser, context, "argument_alignments", "alignment list")?;
        let stack_arguments =
            super::parse_keyed_array(parser, context, "stack_arguments", "argument list")?;
        let noalias = super::parse_keyed_array(parser, context, "noalias", "argument list")?;

        let public_count = block_args
            .len()
            .saturating_sub(implicit_arguments.unwrap_or(0) as usize);
        let mut parameters: Vec<tir::TypeId> = block_args[..public_count]
            .iter()
            .map(tir::Value::ty)
            .collect();
        if variadic {
            parameters.push(tir::builtin::VarArgsType::new(context));
        }
        let body_region = parser.parse_region_with_entry_args(context, block_args)?;

        let mut builder = FuncOpBuilder::new(context)
            .sym_name(sym_name.as_str())
            .ret_type(ret_type)
            .result_type(tir::builtin::FnType::new(context, &parameters, ret_type))
            .body(body_region.id());
        if result_address {
            builder = builder.result_address();
        }
        if let Some(count) = implicit_arguments {
            builder = builder.implicit_arguments(count as usize);
        }
        if let Some(entry_sp) = entry_sp {
            builder = builder.entry_sp(entry_sp);
        }
        if let Some(argument_alignments) = argument_alignments {
            builder = builder.attr("argument_alignments", argument_alignments);
        }
        if let Some(stack_arguments) = stack_arguments {
            builder = builder.attr("stack_arguments", stack_arguments);
        }
        if let Some(noalias) = noalias {
            builder = builder.attr("noalias", noalias);
        }
        if is_private {
            builder = builder.attr(
                "sym_visibility",
                tir::attributes::AttributeValue::Str("private".to_string().into()),
            );
        }

        Ok(Box::new(builder.build()))
    }
}

impl FuncOp {
    /// An unordered body names the values it produces, so what it names is what
    /// the signature says the function returns. An ordered body says the same
    /// thing through its [`RegionExit`] operations.
    fn verify_nodes_results(&self, context: &Context) -> Result<(), Error> {
        let body = self.body_region();
        if !body.is_nodes() {
            return Ok(());
        }
        let produced: Vec<tir::TypeId> = body
            .value_results()
            .iter()
            .map(|result| context.get_value(*result).ty())
            .collect();
        let declared = match self.ret_type() {
            unit if unit == UnitType::new(context) => vec![],
            ty => vec![ty],
        };
        if produced == declared {
            return Ok(());
        }
        let spell = |types: &[tir::TypeId]| match types {
            [one] => context.type_to_string(*one),
            many => format!(
                "({})",
                many.iter()
                    .map(|ty| context.type_to_string(*ty))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        Err(Error::VerificationError(format!(
            "function '@{}' produces {}, but its signature returns {}",
            self.symbol_name(),
            spell(&produced),
            spell(&declared)
        )))
    }
}

impl tir::Verifiable for FuncOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        super::verify_argument_alignments(self, self.parameters().len(), "function")?;
        super::verify_stack_arguments(self, self.parameters().len(), "function")?;
        let body_parameters = self.parameters();
        let implicit_count = match self.attr("implicit_arguments") {
            None => 0,
            Some(tir::attributes::AttributeValue::UInt(count)) => {
                usize::try_from(count).map_err(|_| {
                    Error::VerificationError(
                        "function implicit argument count is too large".to_string(),
                    )
                })?
            }
            Some(_) => {
                return Err(Error::VerificationError(
                    "function implicit argument count must be an unsigned integer".to_string(),
                ));
            }
        };
        if implicit_count > body_parameters.len() {
            return Err(Error::VerificationError(
                "function implicit argument count exceeds its body arguments".to_string(),
            ));
        }
        let public_count = body_parameters.len() - implicit_count;
        let mut parameters: Vec<_> = body_parameters[..public_count]
            .iter()
            .map(tir::Value::ty)
            .collect();
        super::verify_noalias_arguments(self, context, &parameters)?;
        let variadic = tir::builtin::FnType::signature_of(context, self.fn_value())
            .and_then(|(params, _)| params.last().copied())
            == Some(tir::builtin::VarArgsType::new(context));
        if implicit_count > 0 && !variadic {
            return Err(Error::VerificationError(
                "only variadic functions may have implicit arguments".to_string(),
            ));
        }
        if self.attr("entry_sp").is_some()
            && !matches!(
                self.attr("entry_sp"),
                Some(tir::attributes::AttributeValue::Value(_))
            )
        {
            return Err(Error::VerificationError(
                "function entry stack pointer must be a value".to_string(),
            ));
        }
        if self.attr("entry_sp").is_some() && self.entry_sp_argument().is_none() {
            return Err(Error::VerificationError(
                "function entry stack pointer must name an implicit argument".to_string(),
            ));
        }
        if let Some(entry_sp) = self.entry_sp_argument() {
            if !body_parameters[public_count..]
                .iter()
                .any(|argument| argument.id() == entry_sp)
            {
                return Err(Error::VerificationError(
                    "function entry stack pointer must name an implicit argument".to_string(),
                ));
            }
            let ty = context.get_type_data(context.get_value(entry_sp).ty());
            if (ty.as_ref() as &dyn std::any::Any)
                .downcast_ref::<crate::ptr::PtrType>()
                .is_none()
            {
                return Err(Error::VerificationError(
                    "function entry stack pointer argument must have pointer type".to_string(),
                ));
            }
        }
        if variadic {
            parameters.push(tir::builtin::VarArgsType::new(context));
        }
        let expected = tir::builtin::FnType::new(context, &parameters, self.ret_type());
        if context.get_value(self.fn_value()).ty() != expected {
            return Err(Error::VerificationError(format!(
                "function '@{}' produces {}, but its signature is {}",
                self.symbol_name(),
                context.type_to_string(context.get_value(self.fn_value()).ty()),
                context.type_to_string(expected)
            )));
        }
        self.verify_nodes_results(context)?;
        if !self.has_result_address() {
            return Ok(());
        }
        let Some(argument) = self.parameters().first().cloned() else {
            return Err(Error::VerificationError(
                "result-address function requires a destination argument".to_string(),
            ));
        };
        let ty = context.get_type_data(argument.ty());
        if (ty.as_ref() as &dyn std::any::Any)
            .downcast_ref::<crate::ptr::PtrType>()
            .is_none()
        {
            return Err(Error::VerificationError(
                "result-address function destination must have pointer type".to_string(),
            ));
        }
        Ok(())
    }
}

operation! {
    ReturnOp {
        name: "return",
        dialect: "func",
        operands: O {
            value: "?Any",
        },
        interfaces: [Terminator],
        state: "in",
    }
}

impl ReturnOp {
    /// The value the function returns, or `None` for a void return. The
    /// state operand names the memory handed back to the caller, not a
    /// value.
    pub fn returned_value(&self) -> Option<crate::ValueId> {
        self.value_operands().first().copied()
    }
}

impl Terminator for ReturnOp {}
